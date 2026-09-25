//! Runtime font discovery and registration.
//!
//! Four roles (sans, medium, mono, condensed) are each resolved to the first
//! candidate file found on this machine. Nothing is bundled unless the
//! `bundled-fonts` feature is on; with nothing found, every family still falls
//! back to egui's built-in faces so the app keeps working.

use egui::{FontData, FontDefinitions, FontFamily};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// How deep to descend below each search root.
const MAX_DEPTH: usize = 5;

/// Largest font file we will load; anything bigger is skipped (CJK faces are ~20 MiB).
const MAX_FONT_BYTES: u64 = 64 * 1024 * 1024;

/// Broad-coverage symbol fallbacks appended to every family (⌂ ↑ ↻ ● ▾ ⇩ …).
const FALLBACKS: &[&str] = &["DejaVuSans.ttf", "NotoSansSymbols2-Regular.ttf"];

/// A typographic role the UI asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Sans,
    Medium,
    Mono,
    Cond,
}

impl Role {
    #[cfg(test)]
    pub(crate) const ALL: [Role; 4] = [Role::Sans, Role::Medium, Role::Mono, Role::Cond];

    /// Candidate file names, best first.
    pub(crate) fn candidates(self) -> &'static [&'static str] {
        match self {
            Role::Sans => &[
                "IBMPlexSans-Text.ttf",
                "IBMPlexSans-Regular.ttf",
                "NotoSans-Regular.ttf",
                "Cantarell-Regular.otf",
                "DejaVuSans.ttf",
            ],
            Role::Medium => &[
                "IBMPlexSans-Medium.ttf",
                "NotoSans-Medium.ttf",
                "DejaVuSans-Bold.ttf",
            ],
            Role::Mono => &[
                "IBMPlexMono-Regular.ttf",
                "JetBrainsMonoNerdFont-Regular.ttf",
                "JetBrainsMono-Regular.ttf",
                "DejaVuSansMono.ttf",
            ],
            Role::Cond => &[
                "IBMPlexSansCondensed-SemiBold.ttf",
                "IBMPlexSansCondensed-Medium.ttf",
                "DejaVuSansCondensed-Bold.ttf",
                "DejaVuSansCondensed.ttf",
            ],
        }
    }
}

/// Lower-cased file name -> first path seen for it (roots are searched in priority order).
type FontIndex = HashMap<String, PathBuf>;

/// Index of the first existing candidate among `candidates`, or `None` when none exist.
pub(crate) fn pick_font(candidates: &[&str], exists: impl Fn(&str) -> bool) -> Option<usize> {
    candidates.iter().position(|c| exists(c))
}

/// Directories to scan, best first.
fn search_roots() -> Vec<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let cwd = std::env::current_dir().ok();
    let home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
    [
        exe_dir.map(|d| d.join("assets/fonts")),
        cwd.map(|d| d.join("assets/fonts")),
        home.as_ref().map(|h| h.join(".local/share/fonts")),
        home.as_ref().map(|h| h.join(".fonts")),
        Some(PathBuf::from("/usr/local/share/fonts")),
        Some(PathBuf::from("/usr/share/fonts")),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Lower-cased file name when `path` is a TrueType/OpenType file.
fn font_file_name(path: &Path) -> Option<String> {
    let is_font = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("ttf") || e.eq_ignore_ascii_case("otf"));
    if !is_font {
        return None;
    }
    path.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_ascii_lowercase)
}

/// Recursively collects font files below `dir`, bounded by [`MAX_DEPTH`].
/// `visited` holds canonical directory paths so symlink cycles are walked once.
fn walk(dir: &Path, depth: usize, visited: &mut HashSet<PathBuf>, index: &mut FontIndex) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(real) = std::fs::canonicalize(dir) else {
        return;
    };
    if !visited.insert(real) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for path in entries.flatten().map(|e| e.path()) {
        if path.is_dir() {
            walk(&path, depth + 1, visited, index);
        } else if let Some(name) = font_file_name(&path) {
            index.entry(name).or_insert(path);
        }
    }
}

fn build_index(roots: &[PathBuf]) -> FontIndex {
    let mut index = FontIndex::new();
    let mut visited = HashSet::new();
    for root in roots {
        walk(root, 0, &mut visited, &mut index);
    }
    index
}

fn lookup<'a>(index: &'a FontIndex, file_name: &str) -> Option<&'a PathBuf> {
    index.get(&file_name.to_ascii_lowercase())
}

fn find_role(index: &FontIndex, role: Role) -> Option<PathBuf> {
    let candidates = role.candidates();
    let i = pick_font(candidates, |c| lookup(index, c).is_some())?;
    lookup(index, candidates[i]).cloned()
}

/// Which file each role resolves to on this machine (bundled fonts are not consulted).
#[cfg(test)]
pub(crate) fn discover() -> Vec<(Role, Option<PathBuf>)> {
    let index = build_index(&search_roots());
    Role::ALL
        .iter()
        .map(|&r| (r, find_role(&index, r)))
        .collect()
}

/// Loads `path` into `defs` under its lower-cased file name (once) and returns that key.
fn load(defs: &mut FontDefinitions, path: &Path) -> Option<String> {
    let key = path.file_name()?.to_str()?.to_ascii_lowercase();
    if defs.font_data.contains_key(&key) {
        return Some(key);
    }
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(u64::MAX);
    if size > MAX_FONT_BYTES {
        eprintln!(
            "warning: skipping oversized font {} ({size} bytes)",
            path.display()
        );
        return None;
    }
    match std::fs::read(path) {
        Ok(bytes) => {
            defs.font_data
                .insert(key.clone(), Arc::new(FontData::from_owned(bytes)));
            Some(key)
        }
        Err(e) => {
            eprintln!("warning: could not read font {}: {e}", path.display());
            None
        }
    }
}

#[cfg(feature = "bundled-fonts")]
fn bundled(defs: &mut FontDefinitions, role: Role) -> Option<String> {
    let (key, bytes): (&str, &'static [u8]) = match role {
        Role::Sans => (
            "bundled-sans",
            include_bytes!("../../assets/fonts/IBMPlexSans-Text.ttf"),
        ),
        Role::Medium => (
            "bundled-medium",
            include_bytes!("../../assets/fonts/IBMPlexSans-Medium.ttf"),
        ),
        Role::Mono => (
            "bundled-mono",
            include_bytes!("../../assets/fonts/IBMPlexMono-Regular.ttf"),
        ),
        Role::Cond => (
            "bundled-cond",
            include_bytes!("../../assets/fonts/IBMPlexSansCondensed-SemiBold.ttf"),
        ),
    };
    defs.font_data
        .insert(key.to_owned(), Arc::new(FontData::from_static(bytes)));
    Some(key.to_owned())
}

#[cfg(not(feature = "bundled-fonts"))]
fn bundled(_defs: &mut FontDefinitions, _role: Role) -> Option<String> {
    None
}

/// Bundled face first, otherwise the best discovered candidate.
fn resolve(defs: &mut FontDefinitions, index: &FontIndex, role: Role) -> Option<String> {
    bundled(defs, role).or_else(|| find_role(index, role).and_then(|p| load(defs, &p)))
}

fn push_unique(list: &mut Vec<String>, key: &str) {
    if !list.iter().any(|k| k == key) {
        list.push(key.to_owned());
    }
}

/// `heads` (those that resolved) followed by `tail` and the symbol fallbacks, without repeats.
fn chain(heads: &[&Option<String>], tail: &[String], fallbacks: &[String]) -> Vec<String> {
    let mut list: Vec<String> = heads.iter().filter_map(|k| (*k).clone()).collect();
    for key in tail.iter().chain(fallbacks) {
        push_unique(&mut list, key);
    }
    list
}

/// Resolved font-data keys per role.
struct Keys {
    sans: Option<String>,
    medium: Option<String>,
    mono: Option<String>,
    cond: Option<String>,
}

/// The four families: egui's defaults stay as fallbacks behind our faces.
fn families(
    defaults: &BTreeMap<FontFamily, Vec<String>>,
    keys: &Keys,
    fallbacks: &[String],
) -> BTreeMap<FontFamily, Vec<String>> {
    let prop = defaults
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let mono = defaults
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();
    let mut out = defaults.clone();
    out.insert(
        FontFamily::Proportional,
        chain(&[&keys.sans], &prop, fallbacks),
    );
    out.insert(
        FontFamily::Monospace,
        chain(&[&keys.mono], &mono, fallbacks),
    );
    out.insert(
        super::cond(),
        chain(&[&keys.cond, &keys.sans], &prop, fallbacks),
    );
    out.insert(
        super::medium(),
        chain(&[&keys.medium, &keys.sans], &prop, fallbacks),
    );
    out
}

/// Discover, load and register fonts on `ctx`.
pub(crate) fn install(ctx: &egui::Context) {
    let mut defs = FontDefinitions::default();
    let index = build_index(&search_roots());
    let keys = Keys {
        sans: resolve(&mut defs, &index, Role::Sans),
        medium: resolve(&mut defs, &index, Role::Medium),
        mono: resolve(&mut defs, &index, Role::Mono),
        cond: resolve(&mut defs, &index, Role::Cond),
    };
    let fallbacks: Vec<String> = FALLBACKS
        .iter()
        .filter_map(|name| lookup(&index, name).cloned())
        .filter_map(|path| load(&mut defs, &path))
        .collect();
    defs.families = families(&defs.families, &keys, &fallbacks);
    ctx.set_fonts(defs);
}
