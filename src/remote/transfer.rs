//! Planning and running transfers: remote → local, local → remote and
//! local → local. Each builds a [`TransferPlan`], runs the conflict pre-pass,
//! then copies file by file through a temp sibling + rename, reporting progress.

use super::conflicts::{
    OnConflict, local_existing, local_rel_name, local_target, must_ask, remote_existing,
    remote_rel_name, remote_target,
};
use super::{
    Event, MAX_DEPTH, RemoteFs, TransferProgress, basename, copy_with_progress, file_name_of,
    safe_component,
};
use anyhow::{Context, Result, anyhow};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const PROGRESS_EVERY: Duration = Duration::from_millis(80);

/// How a transfer command ended: finished (reported either way), or parked
/// until the user answers the `Conflicts` it reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done,
    NeedsAnswer,
}

/// What a transfer will do: the target directories to create and the
/// `(source, target, size)` file pairs, in copy order. `S` and `D` are the
/// source and destination path types (`String` remote, `PathBuf` local).
#[derive(Debug, PartialEq, Eq)]
pub struct TransferPlan<S, D> {
    pub dirs: Vec<D>,
    pub files: Vec<(S, D, u64)>,
}

impl<S, D> Default for TransferPlan<S, D> {
    fn default() -> Self {
        TransferPlan {
            dirs: Vec::new(),
            files: Vec::new(),
        }
    }
}

impl<S, D> TransferPlan<S, D> {
    fn bytes_total(&self) -> u64 {
        self.files.iter().map(|(_, _, s)| *s).sum()
    }
}

/// A local → local plan.
pub type LocalCopyPlan = TransferPlan<PathBuf, PathBuf>;

/// Throttled `Event::Progress` reporting for one transfer.
struct Reporter<'a> {
    id: u64,
    file_count: usize,
    bytes_total: u64,
    bytes_done: u64,
    last: Instant,
    emit: &'a dyn Fn(Event),
}

impl<'a> Reporter<'a> {
    fn new(id: u64, file_count: usize, bytes_total: u64, emit: &'a dyn Fn(Event)) -> Self {
        Reporter {
            id,
            file_count,
            bytes_total,
            bytes_done: 0,
            last: Instant::now(),
            emit,
        }
    }

    /// `delta` more bytes of `current_file` (the 1-based `file_index`th) went through.
    fn advance(&mut self, delta: u64, file_index: usize, current_file: &str) {
        self.bytes_done += delta;
        if self.last.elapsed() >= PROGRESS_EVERY {
            self.last = Instant::now();
            self.report(file_index, current_file);
        }
    }

    /// File `file_index` was left alone; count its bytes so the bar still fills.
    fn skip(&mut self, size: u64, file_index: usize) {
        self.advance(size, file_index, "");
    }

    /// The final, unthrottled report once every file is through.
    fn finish(&self) {
        self.report(self.file_count, "");
    }

    fn report(&self, file_index: usize, current_file: &str) {
        (self.emit)(Event::Progress(TransferProgress {
            id: self.id,
            current_file: current_file.to_string(),
            file_index,
            file_count: self.file_count,
            bytes_done: self.bytes_done,
            bytes_total: self.bytes_total,
        }));
    }
}

// ---------------------------------------------------------------------------
// Remote → local
// ---------------------------------------------------------------------------

/// Plan downloading `remote` into `local_dir`: each lands at `local_dir/<base>`
/// and directories are expanded.
fn plan_download(
    fs: &dyn RemoteFs,
    remote: &[String],
    local_dir: &Path,
) -> Result<TransferPlan<String, PathBuf>> {
    let mut plan = TransferPlan::default();
    for r in remote {
        let base = basename(r).ok_or_else(|| anyhow!("unsafe remote name: {r}"))?;
        let st = fs.stat(r)?;
        if st.is_dir {
            let root = local_dir.join(&base);
            plan.dirs.push(root.clone());
            walk_remote(fs, r, &root, &mut plan.files, &mut plan.dirs, 0)?;
        } else {
            plan.files.push((r.clone(), local_dir.join(&base), st.size));
        }
    }
    Ok(plan)
}

pub(super) fn download(
    fs: &dyn RemoteFs,
    id: u64,
    remote: &[String],
    local_dir: &Path,
    on_conflict: OnConflict,
    emit: &dyn Fn(Event),
) -> Result<Outcome> {
    let plan = plan_download(fs, remote, local_dir)?;
    let name = |d: &PathBuf| local_rel_name(d, local_dir);
    let probe = |d: &PathBuf| local_existing(d);
    if must_ask(id, on_conflict, &plan, &name, &probe, emit)? {
        return Ok(Outcome::NeedsAnswer);
    }
    for d in &plan.dirs {
        std::fs::create_dir_all(d).ok();
    }
    let mut progress = Reporter::new(id, plan.files.len(), plan.bytes_total(), emit);
    for (idx, (src, dst, size)) in plan.files.iter().enumerate() {
        let Some(dst) = local_target(dst, on_conflict) else {
            progress.skip(*size, idx + 1);
            continue;
        };
        download_file(fs, src, &dst, idx + 1, &mut progress)?;
    }
    progress.finish();
    Ok(Outcome::Done)
}

/// Fetch one remote file into place through a `.ferropipe-part` sibling.
fn download_file(
    fs: &dyn RemoteFs,
    src: &str,
    dst: &Path,
    file_index: usize,
    progress: &mut Reporter,
) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let fname = file_name_of(dst);
    let tmp = dst.with_extension("ferropipe-part");
    {
        let mut out = std::fs::File::create(&tmp)?;
        fs.read_file(src, &mut out, &mut |delta| {
            progress.advance(delta, file_index, &fname);
        })?;
        out.flush()?;
    }
    std::fs::rename(&tmp, dst)?;
    Ok(())
}

fn walk_remote(
    fs: &dyn RemoteFs,
    remote_dir: &str,
    local_dir: &Path,
    files: &mut Vec<(String, PathBuf, u64)>,
    dirs: &mut Vec<PathBuf>,
    depth: usize,
) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(anyhow!("nesting exceeds {MAX_DEPTH} (symlink loop?)"));
    }
    for e in fs.list(remote_dir)? {
        if !safe_component(&e.name) {
            continue;
        }
        let rchild = fs.join(remote_dir, &e.name);
        let lchild = local_dir.join(&e.name);
        if e.is_dir && !e.symlink {
            dirs.push(lchild.clone());
            walk_remote(fs, &rchild, &lchild, files, dirs, depth + 1)?;
        } else if !e.is_dir {
            files.push((rchild, lchild, e.size));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Local → remote
// ---------------------------------------------------------------------------

/// Plan uploading `local` into `remote_dir`: each lands at `remote_dir/<base>`
/// and directories are expanded.
fn plan_upload(
    fs: &dyn RemoteFs,
    local: &[PathBuf],
    remote_dir: &str,
) -> Result<TransferPlan<PathBuf, String>> {
    let mut plan = TransferPlan::default();
    for l in local {
        let base = l
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| safe_component(n))
            .ok_or_else(|| anyhow!("unsafe local name"))?;
        let meta = std::fs::symlink_metadata(l)?;
        if meta.is_dir() {
            let root = fs.join(remote_dir, &base);
            plan.dirs.push(root.clone());
            let join = |dir: &String, name: &str| fs.join(dir, name);
            walk_local(&join, l, &root, &mut plan.files, &mut plan.dirs, 0)?;
        } else {
            plan.files
                .push((l.clone(), fs.join(remote_dir, &base), meta.len()));
        }
    }
    Ok(plan)
}

pub(super) fn upload(
    fs: &dyn RemoteFs,
    id: u64,
    local: &[PathBuf],
    remote_dir: &str,
    on_conflict: OnConflict,
    emit: &dyn Fn(Event),
) -> Result<Outcome> {
    let plan = plan_upload(fs, local, remote_dir)?;
    let name = |d: &String| remote_rel_name(d, remote_dir);
    let probe = |d: &String| remote_existing(fs, d);
    if must_ask(id, on_conflict, &plan, &name, &probe, emit)? {
        return Ok(Outcome::NeedsAnswer);
    }
    for d in &plan.dirs {
        let _ = fs.mkdir(d);
    }
    let mut progress = Reporter::new(id, plan.files.len(), plan.bytes_total(), emit);
    for (idx, (src, dst, size)) in plan.files.iter().enumerate() {
        let Some(dst) = remote_target(fs, dst, on_conflict) else {
            progress.skip(*size, idx + 1);
            continue;
        };
        let fname = file_name_of(Path::new(&dst));
        let mut input = std::fs::File::open(src)?;
        fs.write_file(&dst, &mut input, &mut |delta| {
            progress.advance(delta, idx + 1, &fname);
        })?;
    }
    progress.finish();
    Ok(Outcome::Done)
}

/// Expand a local directory into `files` (`(source, target, size)`) and the
/// target `dirs` to create, `join` building each child's target path.
/// Symlinks inside are skipped; a directory is not entered through one.
fn walk_local<D: Clone>(
    join: &dyn Fn(&D, &str) -> D,
    local_dir: &Path,
    target_dir: &D,
    files: &mut Vec<(PathBuf, D, u64)>,
    dirs: &mut Vec<D>,
    depth: usize,
) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(anyhow!("nesting exceeds {MAX_DEPTH} (symlink loop?)"));
    }
    for entry in std::fs::read_dir(local_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !safe_component(&name) {
            continue;
        }
        let path = entry.path();
        let child = join(target_dir, &name);
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            dirs.push(child.clone());
            walk_local(join, &path, &child, files, dirs, depth + 1)?;
        } else if meta.is_file() {
            files.push((path, child, meta.len()));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Local → local
// ---------------------------------------------------------------------------

/// Where `src` lands when copied into `dest_dir`, or why it can't be: a
/// directory can't go into itself or a descendant, and nothing onto its own
/// path. Purely lexical; callers pass normalized paths.
pub fn copy_target(src: &Path, dest_dir: &Path) -> Result<PathBuf> {
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| safe_component(n))
        .ok_or_else(|| anyhow!("unsafe name: {}", src.display()))?;
    if dest_dir.starts_with(src) {
        return Err(anyhow!("can't copy {} into itself", src.display()));
    }
    let target = dest_dir.join(name);
    if target == src {
        return Err(anyhow!("{} is already there", src.display()));
    }
    Ok(target)
}

/// The temp sibling a file is written through: `<name>.ferropipe-part`.
pub(super) fn part_path(dst: &Path) -> PathBuf {
    dst.with_file_name(format!("{}.ferropipe-part", file_name_of(dst)))
}

/// `path` with its directory made canonical and its own name kept, so `..`
/// and linked parents resolve while a symlink keeps its name.
fn normalize_source(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("no name: {}", path.display()))?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let parent = parent
        .canonicalize()
        .with_context(|| format!("resolve {}", parent.display()))?;
    Ok(parent.join(name))
}

/// Refuse `src` → `dest_real` when the copy would land on or inside itself,
/// checking both by name (a symlink keeps its own) and by what it resolves to.
fn check_copy_apart(src: &Path, dest_real: &Path) -> Result<()> {
    copy_target(&normalize_source(src)?, dest_real)?;
    let real = src
        .canonicalize()
        .with_context(|| format!("resolve {}", src.display()))?;
    if real.is_dir() && dest_real.starts_with(&real) {
        return Err(anyhow!("can't copy {} into itself", src.display()));
    }
    Ok(())
}

/// Plan copying `sources` into `dest_dir`: each lands at `dest_dir/<name>`
/// and directories are expanded. A top-level symlink is followed (the copy
/// opens through it); containment is checked on resolved paths.
pub fn plan_local_copy(sources: &[PathBuf], dest_dir: &Path) -> Result<LocalCopyPlan> {
    let dest_real = dest_dir
        .canonicalize()
        .with_context(|| format!("resolve {}", dest_dir.display()))?;
    let join = |dir: &PathBuf, name: &str| dir.join(name);
    let mut plan = LocalCopyPlan::default();
    for src in sources {
        check_copy_apart(src, &dest_real)?;
        // Targets are built from the paths as given, so the panes recognize them.
        let target = copy_target(src, dest_dir)?;
        let meta = std::fs::metadata(src).with_context(|| format!("stat {}", src.display()))?;
        if meta.is_dir() {
            plan.dirs.push(target.clone());
            walk_local(&join, src, &target, &mut plan.files, &mut plan.dirs, 0)?;
        } else {
            plan.files.push((src.clone(), target, meta.len()));
        }
    }
    Ok(plan)
}

/// Copy one file into place through a `.ferropipe-part` sibling, keeping the
/// source's permissions, so a failure never leaves a half-written target.
fn copy_file_atomic(src: &Path, dst: &Path, progress: &mut dyn FnMut(u64)) -> Result<()> {
    let tmp = part_path(dst);
    let mut input = std::fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
    let written: Result<()> = (|| {
        let mut out = std::fs::File::create(&tmp)?;
        copy_with_progress(&mut input, &mut out, progress)?;
        out.flush()?;
        std::fs::set_permissions(&tmp, input.metadata()?.permissions())?;
        std::fs::rename(&tmp, dst)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.with_context(|| format!("copy {}", src.display()))
}

/// Copy local `sources` into the local `dest_dir`, reporting progress per file.
pub(super) fn copy_local(
    id: u64,
    sources: &[PathBuf],
    dest_dir: &Path,
    on_conflict: OnConflict,
    emit: &dyn Fn(Event),
) -> Result<Outcome> {
    let plan = plan_local_copy(sources, dest_dir)?;
    let name = |d: &PathBuf| local_rel_name(d, dest_dir);
    let probe = |d: &PathBuf| local_existing(d);
    if must_ask(id, on_conflict, &plan, &name, &probe, emit)? {
        return Ok(Outcome::NeedsAnswer);
    }
    for d in &plan.dirs {
        std::fs::create_dir_all(d).with_context(|| format!("mkdir {}", d.display()))?;
    }
    let mut progress = Reporter::new(id, plan.files.len(), plan.bytes_total(), emit);
    for (idx, (src, dst, size)) in plan.files.iter().enumerate() {
        let Some(dst) = local_target(dst, on_conflict) else {
            progress.skip(*size, idx + 1);
            continue;
        };
        let fname = file_name_of(&dst);
        copy_file_atomic(src, &dst, &mut |delta| {
            progress.advance(delta, idx + 1, &fname);
        })?;
    }
    progress.finish();
    Ok(Outcome::Done)
}
