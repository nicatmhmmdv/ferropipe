//! Backend-agnostic remote filesystem worker.
//!
//! A [`RemoteFs`] backend (SFTP, SMB, …) provides primitive operations; the generic
//! command loop here implements listing, recursive transfer (with atomic temp+rename),
//! recursive delete, and progress reporting on top of those primitives. The worker owns
//! the backend on its own thread and talks to the UI over channels.
//!
//! Transfer planning and execution live in `transfer`, the overwrite policy
//! (`OnConflict`) and its pre-pass in `conflicts`.

mod conflicts;
pub mod sftp;
pub mod smb;
mod transfer;
pub mod winrm;
pub mod worker;

use crate::model::FileEntry;
use anyhow::{Result, anyhow};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

pub use conflicts::{Conflict, OnConflict};
use transfer::{Outcome, copy_local, download, upload};
pub use worker::{WorkerHandle, spawn};

const MAX_DEPTH: usize = 64;
const CHUNK: usize = 64 * 1024;

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
    fn read_file(
        &self,
        path: &str,
        out: &mut dyn Write,
        progress: &mut dyn FnMut(u64),
    ) -> Result<()>;
    /// Write a file atomically (temp + rename) from `input`.
    fn write_file(
        &self,
        path: &str,
        input: &mut dyn Read,
        progress: &mut dyn FnMut(u64),
    ) -> Result<()>;
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
        Err(anyhow!(
            "command execution is not supported on this backend"
        ))
    }
}

/// Commands sent from the UI to the worker.
/// NOTE: not `Debug` — `Connect` carries a plaintext secret.
pub enum Command {
    Connect {
        conn: Box<crate::model::Connection>,
        secret: Option<String>,
    },
    ListRemote {
        path: String,
    },
    Download {
        id: u64,
        remote: Vec<String>,
        local_dir: PathBuf,
        on_conflict: OnConflict,
    },
    Upload {
        id: u64,
        local: Vec<PathBuf>,
        remote_dir: String,
        on_conflict: OnConflict,
    },
    /// Copy local `paths` into the local `dest_dir`; needs no session, so an
    /// idle worker runs it too. Reports like an upload (minus the `OpDone`).
    CopyLocal {
        id: u64,
        paths: Vec<PathBuf>,
        dest_dir: PathBuf,
        on_conflict: OnConflict,
    },
    MkdirRemote {
        path: String,
    },
    CreateFileRemote {
        path: String,
    },
    RenameRemote {
        from: String,
        to: String,
    },
    DeleteRemote {
        paths: Vec<String>,
    },
    Exec {
        command: String,
    },
    Disconnect,
    Shutdown,
}

/// Events sent from the worker back to the UI.
#[derive(Debug)]
pub enum Event {
    Connected {
        home: String,
        kind: &'static str,
    },
    RemoteListing {
        path: String,
        entries: Vec<FileEntry>,
    },
    Progress(TransferProgress),
    TransferDone {
        id: u64,
        ok: bool,
    },
    /// Transfer `id` would overwrite these and was sent with `OnConflict::Ask`:
    /// nothing was written and no `TransferDone` follows; re-issue it with an answer.
    Conflicts {
        id: u64,
        conflicts: Vec<Conflict>,
    },
    OpDone {
        refresh: Option<String>,
    },
    ExecOutput {
        line: String,
        is_err: bool,
    },
    ExecDone {
        code: i32,
    },
    Error {
        context: String,
        message: String,
    },
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
    trimmed.rfind('/').map(|i| {
        if i == 0 {
            "/".to_string()
        } else {
            trimmed[..i].to_string()
        }
    })
}

fn basename(path: &str) -> Option<String> {
    let t = path.trim_end_matches('/');
    let b = t.rsplit('/').next().unwrap_or(t).to_string();
    if safe_component(&b) { Some(b) } else { None }
}

/// The last path component as a display string.
fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Report a transfer's outcome: an `Error` under `context` on failure, then
/// `TransferDone`. A transfer waiting on a conflict answer already reported
/// itself and gets neither.
fn finish_transfer(id: u64, context: &str, result: Result<Outcome>, emit: &dyn Fn(Event)) {
    let ok = match result {
        Ok(Outcome::NeedsAnswer) => return,
        Ok(Outcome::Done) => true,
        Err(e) => {
            emit(Event::Error {
                context: context.into(),
                message: format!("{e:#}"),
            });
            false
        }
    };
    emit(Event::TransferDone { id, ok });
}

/// Run an `Upload` command, then have the UI relist the destination (unless
/// nothing was written because the user must answer first).
fn upload_command(
    fs: &dyn RemoteFs,
    id: u64,
    local: &[PathBuf],
    remote_dir: String,
    on_conflict: OnConflict,
    emit: &dyn Fn(Event),
) {
    let result = upload(fs, id, local, &remote_dir, on_conflict, emit);
    let asked = matches!(result, Ok(Outcome::NeedsAnswer));
    finish_transfer(id, "upload", result, emit);
    if !asked {
        emit(Event::OpDone {
            refresh: Some(remote_dir),
        });
    }
}

/// Run a `CopyLocal` command, reporting like an upload.
pub(crate) fn copy_local_command(
    id: u64,
    paths: &[PathBuf],
    dest_dir: &Path,
    on_conflict: OnConflict,
    emit: &dyn Fn(Event),
) {
    let result = copy_local(id, paths, dest_dir, on_conflict, emit);
    finish_transfer(id, "copy", result, emit);
}

/// Report a mutation: `OpDone` relisting `refresh` on success, else an `Error` under `context`.
fn finish_op(result: Result<()>, refresh: Option<String>, context: String, emit: &dyn Fn(Event)) {
    match result {
        Ok(()) => emit(Event::OpDone { refresh }),
        Err(e) => emit(Event::Error {
            context,
            message: format!("{e:#}"),
        }),
    }
}

/// List `path` (resolved through the backend) or report why it can't be.
fn list_remote(fs: &dyn RemoteFs, path: String, emit: &dyn Fn(Event)) {
    let resolved = fs.realpath(&path).unwrap_or(path);
    match fs.list(&resolved) {
        Ok(entries) => emit(Event::RemoteListing {
            path: resolved,
            entries,
        }),
        Err(e) => emit(Event::Error {
            context: format!("list {resolved}"),
            message: format!("{e:#}"),
        }),
    }
}

/// Delete each of `paths` recursively, reporting failures one by one, then
/// relist the first one's parent.
fn delete_remote(fs: &dyn RemoteFs, paths: &[String], emit: &dyn Fn(Event)) {
    let refresh = paths.first().and_then(|p| parent_of(p));
    for p in paths {
        if let Err(e) = delete_recursive(fs, p, 0) {
            emit(Event::Error {
                context: format!("delete {p}"),
                message: format!("{e:#}"),
            });
        }
    }
    emit(Event::OpDone { refresh });
}

/// Run `command`, streaming its output; a failure to run it exits with -1.
fn exec_remote(fs: &dyn RemoteFs, command: &str, emit: &dyn Fn(Event)) {
    match fs.exec(command, emit) {
        Ok(code) => emit(Event::ExecDone { code }),
        Err(e) => {
            emit(Event::Error {
                context: "exec".into(),
                message: format!("{e:#}"),
            });
            emit(Event::ExecDone { code: -1 });
        }
    }
}

/// The generic command loop, driven by any backend. Returns true on Shutdown.
pub fn serve(rx: &Receiver<Command>, emit: &dyn Fn(Event), fs: &dyn RemoteFs) -> bool {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Command::ListRemote { path } => list_remote(fs, path, emit),
            Command::Download {
                id,
                remote,
                local_dir,
                on_conflict,
            } => {
                let result = download(fs, id, &remote, &local_dir, on_conflict, emit);
                finish_transfer(id, "download", result, emit);
            }
            Command::Upload {
                id,
                local,
                remote_dir,
                on_conflict,
            } => upload_command(fs, id, &local, remote_dir, on_conflict, emit),
            Command::CopyLocal {
                id,
                paths,
                dest_dir,
                on_conflict,
            } => copy_local_command(id, &paths, &dest_dir, on_conflict, emit),
            Command::MkdirRemote { path } => finish_op(
                fs.mkdir(&path),
                parent_of(&path),
                format!("mkdir {path}"),
                emit,
            ),
            Command::CreateFileRemote { path } => {
                let context = format!("create {path}");
                finish_op(fs.create_empty(&path), parent_of(&path), context, emit)
            }
            Command::RenameRemote { from, to } => {
                let context = format!("rename {from}");
                finish_op(fs.rename(&from, &to), parent_of(&to), context, emit)
            }
            Command::DeleteRemote { paths } => delete_remote(fs, &paths, emit),
            Command::Exec { command } => exec_remote(fs, &command, emit),
            Command::Disconnect => return false,
            Command::Shutdown => return true,
            Command::Connect { .. } => {}
        }
    }
    false
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
mod tests;
