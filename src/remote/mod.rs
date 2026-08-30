//! Backend-agnostic remote filesystem worker.
//!
//! A [`RemoteFs`] backend (SFTP, SMB, …) provides primitive operations; the generic
//! command loop here implements listing, recursive transfer (with atomic temp+rename),
//! recursive delete, and progress reporting on top of those primitives. The worker owns
//! the backend on its own thread and talks to the UI over channels.

pub mod sftp;
pub mod smb;
pub mod winrm;
pub mod worker;

use crate::model::FileEntry;
use anyhow::{anyhow, Result};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

pub use worker::{spawn, WorkerHandle};

const MAX_DEPTH: usize = 64;
const CHUNK: usize = 64 * 1024;
const PROGRESS_EVERY: Duration = Duration::from_millis(80);

/// Minimal stat used by transfer planning.
pub struct RStat {
    pub is_dir: bool,
    pub size: u64,
}

/// A pluggable remote filesystem backend. Lives on the worker thread (not required to be Send).
pub trait RemoteFs {
    fn home(&self) -> String;
    fn realpath(&self, path: &str) -> Result<String>;
    fn stat(&self, path: &str) -> Result<RStat>;
    fn list(&self, path: &str) -> Result<Vec<FileEntry>>;
    fn read_file(&self, path: &str, out: &mut dyn Write, progress: &mut dyn FnMut(u64)) -> Result<()>;
    /// Write a file atomically (temp + rename) from `input`.
    fn write_file(&self, path: &str, input: &mut dyn Read, progress: &mut dyn FnMut(u64)) -> Result<()>;
    fn mkdir(&self, path: &str) -> Result<()>;
    fn create_empty(&self, path: &str) -> Result<()>;
    fn rename(&self, from: &str, to: &str) -> Result<()>;
    fn remove_file(&self, path: &str) -> Result<()>;
    fn remove_dir(&self, path: &str) -> Result<()>;
    /// Join a directory and a child name with the backend's separator.
    fn join(&self, dir: &str, name: &str) -> String {
        if dir.ends_with('/') {
            format!("{dir}{name}")
        } else {
            format!("{dir}/{name}")
        }
    }

    /// Run a command, streaming stdout/stderr lines via `emit`. Returns the exit code.
    fn exec(&self, _command: &str, _emit: &dyn Fn(Event)) -> Result<i32> {
        Err(anyhow!("command execution is not supported on this backend"))
    }
}

/// Commands sent from the UI to the worker.
/// NOTE: not `Debug` — `Connect` carries a plaintext secret.
pub enum Command {
    Connect { conn: Box<crate::model::Connection>, secret: Option<String> },
    ListRemote { path: String },
    Download { id: u64, remote: Vec<String>, local_dir: PathBuf },
    Upload { id: u64, local: Vec<PathBuf>, remote_dir: String },
    MkdirRemote { path: String },
    CreateFileRemote { path: String },
    RenameRemote { from: String, to: String },
    DeleteRemote { paths: Vec<String> },
    Exec { command: String },
    Disconnect,
    Shutdown,
}

/// Events sent from the worker back to the UI.
#[derive(Debug)]
pub enum Event {
    Connected { home: String, kind: &'static str },
    RemoteListing { path: String, entries: Vec<FileEntry> },
    Progress(TransferProgress),
    TransferDone { id: u64, ok: bool },
    OpDone { refresh: Option<String> },
    ExecOutput { line: String, is_err: bool },
    ExecDone { code: i32 },
    Error { context: String, message: String },
    Disconnected,
}

#[derive(Debug, Clone)]
pub struct TransferProgress {
    pub id: u64,
    pub current_file: String,
    pub file_index: usize,
    pub file_count: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

pub fn safe_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

pub fn parent_of(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    trimmed.rfind('/').map(|i| if i == 0 { "/".to_string() } else { trimmed[..i].to_string() })
}

fn basename(path: &str) -> Option<String> {
    let t = path.trim_end_matches('/');
    let b = t.rsplit('/').next().unwrap_or(t).to_string();
    if safe_component(&b) {
        Some(b)
    } else {
        None
    }
}

/// The generic command loop, driven by any backend. Returns true on Shutdown.
pub fn serve(rx: &Receiver<Command>, emit: &dyn Fn(Event), fs: &dyn RemoteFs) -> bool {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Command::ListRemote { path } => {
                let resolved = fs.realpath(&path).unwrap_or(path);
                match fs.list(&resolved) {
                    Ok(entries) => emit(Event::RemoteListing { path: resolved, entries }),
                    Err(e) => emit(Event::Error { context: format!("list {resolved}"), message: format!("{e:#}") }),
                }
            }
            Command::Download { id, remote, local_dir } => {
                let ok = match download(fs, id, &remote, &local_dir, emit) {
                    Ok(()) => true,
                    Err(e) => {
                        emit(Event::Error { context: "download".into(), message: format!("{e:#}") });
                        false
                    }
                };
                emit(Event::TransferDone { id, ok });
            }
            Command::Upload { id, local, remote_dir } => {
                let ok = match upload(fs, id, &local, &remote_dir, emit) {
                    Ok(()) => true,
                    Err(e) => {
                        emit(Event::Error { context: "upload".into(), message: format!("{e:#}") });
                        false
                    }
                };
                emit(Event::TransferDone { id, ok });
                emit(Event::OpDone { refresh: Some(remote_dir) });
            }
            Command::MkdirRemote { path } => match fs.mkdir(&path) {
                Ok(_) => emit(Event::OpDone { refresh: parent_of(&path) }),
                Err(e) => emit(Event::Error { context: format!("mkdir {path}"), message: format!("{e:#}") }),
            },
            Command::CreateFileRemote { path } => match fs.create_empty(&path) {
                Ok(_) => emit(Event::OpDone { refresh: parent_of(&path) }),
                Err(e) => emit(Event::Error { context: format!("create {path}"), message: format!("{e:#}") }),
            },
            Command::RenameRemote { from, to } => match fs.rename(&from, &to) {
                Ok(_) => emit(Event::OpDone { refresh: parent_of(&to) }),
                Err(e) => emit(Event::Error { context: format!("rename {from}"), message: format!("{e:#}") }),
            },
            Command::DeleteRemote { paths } => {
                let refresh = paths.first().and_then(|p| parent_of(p));
                for p in &paths {
                    if let Err(e) = delete_recursive(fs, p, 0) {
                        emit(Event::Error { context: format!("delete {p}"), message: format!("{e:#}") });
                    }
                }
                emit(Event::OpDone { refresh });
            }
            Command::Exec { command } => match fs.exec(&command, emit) {
                Ok(code) => emit(Event::ExecDone { code }),
                Err(e) => {
                    emit(Event::Error { context: "exec".into(), message: format!("{e:#}") });
                    emit(Event::ExecDone { code: -1 });
                }
            },
            Command::Disconnect => return false,
            Command::Shutdown => return true,
            Command::Connect { .. } => {}
        }
    }
    false
}

fn download(fs: &dyn RemoteFs, id: u64, remote: &[String], local_dir: &Path, emit: &dyn Fn(Event)) -> Result<()> {
    let mut files: Vec<(String, PathBuf, u64)> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for r in remote {
        let base = basename(r).ok_or_else(|| anyhow!("unsafe remote name: {r}"))?;
        let st = fs.stat(r)?;
        if st.is_dir {
            let root = local_dir.join(&base);
            dirs.push(root.clone());
            walk_remote(fs, r, &root, &mut files, &mut dirs, 0)?;
        } else {
            files.push((r.clone(), local_dir.join(&base), st.size));
        }
    }
    for d in &dirs {
        std::fs::create_dir_all(d).ok();
    }
    let bytes_total: u64 = files.iter().map(|(_, _, s)| *s).sum();
    let file_count = files.len();
    let mut bytes_done = 0u64;
    let mut last = Instant::now();
    for (idx, (src, dst, _)) in files.iter().enumerate() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let fname = dst.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let tmp = dst.with_extension("ferropipe-part");
        {
            let mut out = std::fs::File::create(&tmp)?;
            fs.read_file(src, &mut out, &mut |delta| {
                bytes_done += delta;
                if last.elapsed() >= PROGRESS_EVERY {
                    last = Instant::now();
                    emit(Event::Progress(TransferProgress {
                        id, current_file: fname.clone(), file_index: idx + 1, file_count, bytes_done, bytes_total,
                    }));
                }
            })?;
            out.flush()?;
        }
        std::fs::rename(&tmp, dst)?;
    }
    emit(Event::Progress(TransferProgress { id, current_file: String::new(), file_index: file_count, file_count, bytes_done, bytes_total }));
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

fn upload(fs: &dyn RemoteFs, id: u64, local: &[PathBuf], remote_dir: &str, emit: &dyn Fn(Event)) -> Result<()> {
    let mut files: Vec<(PathBuf, String, u64)> = Vec::new();
    let mut dirs: Vec<String> = Vec::new();
    for l in local {
        let base = l.file_name().map(|n| n.to_string_lossy().into_owned()).filter(|n| safe_component(n))
            .ok_or_else(|| anyhow!("unsafe local name"))?;
        let meta = std::fs::symlink_metadata(l)?;
        if meta.is_dir() {
            let root = fs.join(remote_dir, &base);
            dirs.push(root.clone());
            walk_local(fs, l, &root, &mut files, &mut dirs, 0)?;
        } else {
            files.push((l.clone(), fs.join(remote_dir, &base), meta.len()));
        }
    }
    for d in &dirs {
        let _ = fs.mkdir(d);
    }
    let bytes_total: u64 = files.iter().map(|(_, _, s)| *s).sum();
    let file_count = files.len();
    let mut bytes_done = 0u64;
    let mut last = Instant::now();
    for (idx, (src, dst, _)) in files.iter().enumerate() {
        let fname = Path::new(dst).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let mut input = std::fs::File::open(src)?;
        fs.write_file(dst, &mut input, &mut |delta| {
            bytes_done += delta;
            if last.elapsed() >= PROGRESS_EVERY {
                last = Instant::now();
                emit(Event::Progress(TransferProgress {
                    id, current_file: fname.clone(), file_index: idx + 1, file_count, bytes_done, bytes_total,
                }));
            }
        })?;
    }
    emit(Event::Progress(TransferProgress { id, current_file: String::new(), file_index: file_count, file_count, bytes_done, bytes_total }));
    Ok(())
}

fn walk_local(
    fs: &dyn RemoteFs,
    local_dir: &Path,
    remote_dir: &str,
    files: &mut Vec<(PathBuf, String, u64)>,
    dirs: &mut Vec<String>,
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
        let rchild = fs.join(remote_dir, &name);
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            dirs.push(rchild.clone());
            walk_local(fs, &path, &rchild, files, dirs, depth + 1)?;
        } else if meta.is_file() {
            files.push((path, rchild, meta.len()));
        }
    }
    Ok(())
}

fn delete_recursive(fs: &dyn RemoteFs, path: &str, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(anyhow!("nesting exceeds {MAX_DEPTH} (symlink loop?)"));
    }
    let st = fs.stat(path)?;
    if st.is_dir {
        for e in fs.list(path)? {
            if !safe_component(&e.name) {
                continue;
            }
            delete_recursive(fs, &fs.join(path, &e.name), depth + 1)?;
        }
        fs.remove_dir(path)?;
    } else {
        fs.remove_file(path)?;
    }
    Ok(())
}

/// Copy from a reader to a writer in chunks, reporting per-chunk byte deltas.
pub(crate) fn copy_with_progress(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    progress: &mut dyn FnMut(u64),
) -> Result<()> {
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        progress(n as u64);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_of_works() {
        assert_eq!(parent_of("/a/b/c"), Some("/a/b".into()));
        assert_eq!(parent_of("/a"), Some("/".into()));
        assert_eq!(parent_of("x"), None);
    }

    #[test]
    fn safe_component_rejects_traversal() {
        assert!(safe_component("ok.txt"));
        assert!(!safe_component(".."));
        assert!(!safe_component("a/b"));
    }

    #[test]
    fn basename_sanitizes() {
        assert_eq!(basename("/a/b/file.txt"), Some("file.txt".into()));
        assert_eq!(basename("/a/b/"), Some("b".into()));
    }
}
