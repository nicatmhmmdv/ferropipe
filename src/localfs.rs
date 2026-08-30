//! Local filesystem browsing + mutations for the local pane.
use crate::model::FileEntry;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// List a local directory into FileEntry rows.
pub fn list_dir(path: &Path, show_hidden: bool) -> Result<Vec<FileEntry>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path).with_context(|| format!("read_dir {}", path.display()))? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        #[cfg(unix)]
        let perm = {
            use std::os::unix::fs::PermissionsExt;
            Some(meta.permissions().mode())
        };
        #[cfg(not(unix))]
        let perm = None;
        out.push(FileEntry {
            name,
            is_dir: meta.is_dir(),
            size: meta.len(),
            mtime,
            perm,
            symlink: meta.file_type().is_symlink(),
        });
    }
    Ok(out)
}

pub fn home_dir() -> PathBuf {
    directories::UserDirs::new()
        .map(|u| u.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn mkdir(parent: &Path, name: &str) -> Result<()> {
    std::fs::create_dir(parent.join(name)).context("create_dir")?;
    Ok(())
}

/// Create a new empty file, failing if it already exists.
pub fn create_file(path: &Path) -> Result<()> {
    if path.exists() {
        return Err(anyhow::anyhow!("already exists"));
    }
    std::fs::File::create(path).context("create file")?;
    Ok(())
}

pub fn rename(from: &Path, to: &Path) -> Result<()> {
    std::fs::rename(from, to).context("rename")?;
    Ok(())
}

pub fn delete(path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path).context("remove_dir_all")?;
    } else {
        std::fs::remove_file(path).context("remove_file")?;
    }
    Ok(())
}

/// Human-readable byte size.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes == 0 {
        return "0 B".into();
    }
    let mut val = bytes as f64;
    let mut i = 0;
    while val >= 1024.0 && i < UNITS.len() - 1 {
        val /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{val:.1} {}", UNITS[i])
    }
}

/// Format a unix mtime as a compact local datetime string.
pub fn format_mtime(mtime: Option<u64>) -> String {
    match mtime {
        Some(secs) => {
            use chrono::{Local, TimeZone};
            match Local.timestamp_opt(secs as i64, 0).single() {
                Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
                None => "-".into(),
            }
        }
        None => "-".into(),
    }
}
