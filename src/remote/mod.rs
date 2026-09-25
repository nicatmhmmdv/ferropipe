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
use anyhow::{Context, Result, anyhow};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

pub use worker::{WorkerHandle, spawn};

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
    },
    Upload {
        id: u64,
        local: Vec<PathBuf>,
        remote_dir: String,
    },
    /// Copy local `paths` into the local `dest_dir`; needs no session, so an
    /// idle worker runs it too. Reports like an upload (minus the `OpDone`).
    CopyLocal {
        id: u64,
        paths: Vec<PathBuf>,
        dest_dir: PathBuf,
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

/// What a local copy will do: the target directories to create and the
/// `(source, target, size)` file pairs, in copy order.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LocalCopyPlan {
    pub dirs: Vec<PathBuf>,
    pub files: Vec<(PathBuf, PathBuf, u64)>,
}

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

/// Report a transfer's outcome: an `Error` under `context` on failure, then `TransferDone`.
fn finish_transfer(id: u64, context: &str, result: Result<()>, emit: &dyn Fn(Event)) {
    let ok = match result {
        Ok(()) => true,
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

/// Run an `Upload` command, then have the UI relist the destination.
fn upload_command(
    fs: &dyn RemoteFs,
    id: u64,
    local: &[PathBuf],
    remote_dir: String,
    emit: &dyn Fn(Event),
) {
    let result = upload(fs, id, local, &remote_dir, emit);
    finish_transfer(id, "upload", result, emit);
    emit(Event::OpDone {
        refresh: Some(remote_dir),
    });
}

/// Run a `CopyLocal` command, reporting like an upload.
pub(crate) fn copy_local_command(
    id: u64,
    paths: &[PathBuf],
    dest_dir: &Path,
    emit: &dyn Fn(Event),
) {
    let result = copy_local(id, paths, dest_dir, emit);
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
            } => {
                let result = download(fs, id, &remote, &local_dir, emit);
                finish_transfer(id, "download", result, emit);
            }
            Command::Upload {
                id,
                local,
                remote_dir,
            } => upload_command(fs, id, &local, remote_dir, emit),
            Command::CopyLocal {
                id,
                paths,
                dest_dir,
            } => copy_local_command(id, &paths, &dest_dir, emit),
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

fn download(
    fs: &dyn RemoteFs,
    id: u64,
    remote: &[String],
    local_dir: &Path,
    emit: &dyn Fn(Event),
) -> Result<()> {
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
    let mut progress = Reporter::new(id, files.len(), bytes_total, emit);
    for (idx, (src, dst, _)) in files.iter().enumerate() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let fname = file_name_of(dst);
        let tmp = dst.with_extension("ferropipe-part");
        {
            let mut out = std::fs::File::create(&tmp)?;
            fs.read_file(src, &mut out, &mut |delta| {
                progress.advance(delta, idx + 1, &fname);
            })?;
            out.flush()?;
        }
        std::fs::rename(&tmp, dst)?;
    }
    progress.finish();
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

fn upload(
    fs: &dyn RemoteFs,
    id: u64,
    local: &[PathBuf],
    remote_dir: &str,
    emit: &dyn Fn(Event),
) -> Result<()> {
    let mut files: Vec<(PathBuf, String, u64)> = Vec::new();
    let mut dirs: Vec<String> = Vec::new();
    for l in local {
        let base = l
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| safe_component(n))
            .ok_or_else(|| anyhow!("unsafe local name"))?;
        let meta = std::fs::symlink_metadata(l)?;
        if meta.is_dir() {
            let root = fs.join(remote_dir, &base);
            dirs.push(root.clone());
            let join = |dir: &String, name: &str| fs.join(dir, name);
            walk_local(&join, l, &root, &mut files, &mut dirs, 0)?;
        } else {
            files.push((l.clone(), fs.join(remote_dir, &base), meta.len()));
        }
    }
    for d in &dirs {
        let _ = fs.mkdir(d);
    }
    let bytes_total: u64 = files.iter().map(|(_, _, s)| *s).sum();
    let mut progress = Reporter::new(id, files.len(), bytes_total, emit);
    for (idx, (src, dst, _)) in files.iter().enumerate() {
        let fname = file_name_of(Path::new(dst));
        let mut input = std::fs::File::open(src)?;
        fs.write_file(dst, &mut input, &mut |delta| {
            progress.advance(delta, idx + 1, &fname);
        })?;
    }
    progress.finish();
    Ok(())
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
fn part_path(dst: &Path) -> PathBuf {
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
fn copy_local(id: u64, sources: &[PathBuf], dest_dir: &Path, emit: &dyn Fn(Event)) -> Result<()> {
    let plan = plan_local_copy(sources, dest_dir)?;
    for d in &plan.dirs {
        std::fs::create_dir_all(d).with_context(|| format!("mkdir {}", d.display()))?;
    }
    let bytes_total: u64 = plan.files.iter().map(|(_, _, s)| *s).sum();
    let mut progress = Reporter::new(id, plan.files.len(), bytes_total, emit);
    for (idx, (src, dst, _)) in plan.files.iter().enumerate() {
        let fname = file_name_of(dst);
        copy_file_atomic(src, dst, &mut |delta| {
            progress.advance(delta, idx + 1, &fname);
        })?;
    }
    progress.finish();
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
mod tests;
