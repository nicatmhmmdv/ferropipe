//! Overwrite policy: what a transfer does when a destination already exists.
//!
//! [`OnConflict::Ask`] runs a pre-pass that stats every destination and reports
//! the collisions before anything is written; the other policies are applied
//! per file while copying. Everything here is pure over a `probe` closure so it
//! serves local and remote destinations alike.

use super::transfer::TransferPlan;
use super::{Event, RemoteFs, file_name_of, parent_of};
use anyhow::{Result, anyhow};
use std::path::{Path, PathBuf};

/// What to do when a transfer would write over something that exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum OnConflict {
    /// Report the collisions (`Event::Conflicts`) and write nothing.
    #[default]
    Ask,
    Overwrite,
    /// Leave the existing file; the incoming one is dropped.
    Skip,
    /// Write the incoming file under the first free `name (N).ext`.
    Rename,
}

/// One destination a transfer would write over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// Destination path relative to the transfer's destination directory.
    pub name: String,
    pub existing_size: u64,
    pub incoming_size: u64,
    /// A file sits where a directory should go, or the other way round.
    pub kind_mismatch: bool,
}

/// What the pre-pass found at a destination path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Existing {
    Missing,
    File { size: u64 },
    Dir,
}

/// `name` with a counter before its extension: `report (1).pdf`, then `(2)`,
/// … until `taken` says no. The extension starts at the LAST dot (`archive.tar
/// (1).gz`); no dot, or only a leading one, means none (`Makefile (1)`). A name
/// already ending in ` (N)` counts on from N.
pub fn renamed_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    let (base, mut n) = strip_counter(stem);
    loop {
        let candidate = format!("{base} ({n}){ext}");
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Split a trailing ` (N)` off `stem`: `("report", 2)` for `report (1)`, else
/// the stem itself and 1.
fn strip_counter(stem: &str) -> (&str, u64) {
    let counted = stem
        .strip_suffix(')')
        .and_then(|inner| inner.rsplit_once(" ("))
        .and_then(|(base, digits)| digits.parse::<u64>().ok().map(|n| (base, n + 1)));
    counted.unwrap_or((stem, 1))
}

/// Everything in `plan` that would write over something: an existing file at
/// a file's target (sizes reported), or a kind mismatch either way. An
/// existing directory at a directory's target is a merge, not a conflict.
/// `name` gives a target's path relative to the destination directory.
pub fn find_conflicts<S, D>(
    plan: &TransferPlan<S, D>,
    name: &dyn Fn(&D) -> String,
    probe: &dyn Fn(&D) -> Existing,
) -> Vec<Conflict> {
    let dirs = plan.dirs.iter().filter_map(|d| match probe(d) {
        Existing::File { size } => Some(Conflict {
            name: name(d),
            existing_size: size,
            incoming_size: 0,
            kind_mismatch: true,
        }),
        Existing::Dir | Existing::Missing => None,
    });
    let files = plan
        .files
        .iter()
        .filter_map(|(_, d, incoming)| match probe(d) {
            Existing::File { size } => Some(Conflict {
                name: name(d),
                existing_size: size,
                incoming_size: *incoming,
                kind_mismatch: false,
            }),
            Existing::Dir => Some(Conflict {
                name: name(d),
                existing_size: 0,
                incoming_size: *incoming,
                kind_mismatch: true,
            }),
            Existing::Missing => None,
        });
    dirs.chain(files).collect()
}

/// Refuse a plan whose directory target is a file or whose file target is a
/// directory: no policy can write through those, so nothing is written at all.
pub fn refuse_mismatches<S, D>(
    plan: &TransferPlan<S, D>,
    name: &dyn Fn(&D) -> String,
    probe: &dyn Fn(&D) -> Existing,
) -> Result<()> {
    for d in &plan.dirs {
        if let Existing::File { .. } = probe(d) {
            return Err(anyhow!("{}: a file is in the way", name(d)));
        }
    }
    for (_, d, _) in &plan.files {
        if probe(d) == Existing::Dir {
            return Err(anyhow!("{}: a directory is in the way", name(d)));
        }
    }
    Ok(())
}

/// The pre-pass before anything is written. Under `Ask`, report what would be
/// overwritten and return true so the caller stops without a `TransferDone`;
/// under any answer, refuse kind mismatches up front.
pub(super) fn must_ask<S, D>(
    id: u64,
    on_conflict: OnConflict,
    plan: &TransferPlan<S, D>,
    name: &dyn Fn(&D) -> String,
    probe: &dyn Fn(&D) -> Existing,
    emit: &dyn Fn(Event),
) -> Result<bool> {
    if on_conflict != OnConflict::Ask {
        refuse_mismatches(plan, name, probe)?;
        return Ok(false);
    }
    let conflicts = find_conflicts(plan, name, probe);
    if conflicts.is_empty() {
        return Ok(false);
    }
    emit(Event::Conflicts { id, conflicts });
    Ok(true)
}

/// Where a file lands under `on_conflict` when `dst` may be taken: `None` to
/// skip it, else `dst` itself or a free sibling (`sibling(dst, new_name)`).
pub(super) fn target_under<D: Clone>(
    dst: &D,
    on_conflict: OnConflict,
    probe: &dyn Fn(&D) -> Existing,
    sibling: &dyn Fn(&D, &str) -> D,
    leaf: &dyn Fn(&D) -> String,
) -> Option<D> {
    match on_conflict {
        OnConflict::Ask | OnConflict::Overwrite => Some(dst.clone()),
        _ if probe(dst) == Existing::Missing => Some(dst.clone()),
        OnConflict::Skip => None,
        OnConflict::Rename => {
            let free = renamed_name(&leaf(dst), |n| probe(&sibling(dst, n)) != Existing::Missing);
            Some(sibling(dst, &free))
        }
    }
}

pub(super) fn local_existing(path: &Path) -> Existing {
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => Existing::Dir,
        Ok(m) => Existing::File { size: m.len() },
        Err(_) => Existing::Missing,
    }
}

pub(super) fn remote_existing(fs: &dyn RemoteFs, path: &str) -> Existing {
    match fs.stat(path) {
        Ok(st) if st.is_dir => Existing::Dir,
        Ok(st) => Existing::File { size: st.size },
        Err(_) => Existing::Missing,
    }
}

/// `path` relative to `dir`, for the conflict list.
pub(super) fn local_rel_name(path: &Path, dir: &Path) -> String {
    path.strip_prefix(dir)
        .map(|rel| rel.to_string_lossy().into_owned())
        .unwrap_or_else(|_| file_name_of(path))
}

/// `path` relative to `dir`, for the conflict list.
pub(super) fn remote_rel_name(path: &str, dir: &str) -> String {
    path.strip_prefix(dir)
        .map(|rel| rel.trim_start_matches('/').to_string())
        .unwrap_or_else(|| file_name_of(Path::new(path)))
}

/// A local file's target under the policy; `None` skips it.
pub(super) fn local_target(dst: &Path, on_conflict: OnConflict) -> Option<PathBuf> {
    let sibling = |d: &PathBuf, n: &str| d.with_file_name(n);
    let leaf = |d: &PathBuf| file_name_of(d);
    let probe = |d: &PathBuf| local_existing(d);
    target_under(&dst.to_path_buf(), on_conflict, &probe, &sibling, &leaf)
}

/// A remote file's target under the policy; `None` skips it.
pub(super) fn remote_target(
    fs: &dyn RemoteFs,
    dst: &str,
    on_conflict: OnConflict,
) -> Option<String> {
    let sibling = |d: &String, n: &str| match parent_of(d) {
        Some(parent) => fs.join(&parent, n),
        None => n.to_string(),
    };
    let leaf = |d: &String| file_name_of(Path::new(d));
    let probe = |d: &String| remote_existing(fs, d);
    target_under(&dst.to_string(), on_conflict, &probe, &sibling, &leaf)
}
