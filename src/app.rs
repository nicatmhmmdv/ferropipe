//! Ferropipe GUI: connection sidebar + dual-pane SFTP file manager.
use crate::localfs;
use crate::model::{AuthMethod, Connection, ConnectionKind, FileEntry};
use crate::remote::{self, Command, Event, WorkerHandle};
use crate::store::Store;
use crate::vault::Vault;
use eframe::egui;
use egui::{Color32, RichText};
use egui_extras::{Column, TableBuilder};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::SystemTime;
use std::sync::mpsc;
use uuid::Uuid;

const ACCENT: Color32 = Color32::from_rgb(0xE0, 0x6C, 0x3A); // rust-orange

/// Which pane a drag/action came from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// Payload carried during a drag-and-drop between panes.
#[derive(Clone)]
pub struct DragPayload {
    pub side: Side,
    pub remote: bool, // is the source pane currently browsing a remote?
    pub paths: Vec<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum SortCol {
    Name,
    Size,
    Modified,
}

/// State for one file pane (local or remote).
struct PaneState {
    remote: bool,
    cwd: String,
    entries: Vec<FileEntry>,
    selected: HashSet<String>,
    loading: bool,
    error: Option<String>,
    path_edit: String,
    sort_col: SortCol,
    sort_asc: bool,
}

impl PaneState {
    fn new(remote: bool, cwd: String) -> Self {
        PaneState {
            remote,
            path_edit: cwd.clone(),
            cwd,
            entries: Vec::new(),
            selected: HashSet::new(),
            loading: false,
            error: None,
            sort_col: SortCol::Name,
            sort_asc: true,
        }
    }

    fn sort(&mut self) {
        let asc = self.sort_asc;
        let col = self.sort_col;
        self.entries.sort_by(|a, b| {
            let primary = a.kind_order().cmp(&b.kind_order());
            if primary != std::cmp::Ordering::Equal {
                return primary;
            }
            let ord = match col {
                SortCol::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortCol::Size => a.size.cmp(&b.size),
                SortCol::Modified => a.mtime.unwrap_or(0).cmp(&b.mtime.unwrap_or(0)),
            };
            if asc { ord } else { ord.reverse() }
        });
    }

    fn full_path(&self, name: &str) -> String {
        if self.remote {
            if self.cwd.ends_with('/') {
                format!("{}{}", self.cwd, name)
            } else {
                format!("{}/{}", self.cwd, name)
            }
        } else {
            PathBuf::from(&self.cwd)
                .join(name)
                .to_string_lossy()
                .into_owned()
        }
    }

    fn selected_paths(&self) -> Vec<String> {
        self.selected.iter().map(|n| self.full_path(n)).collect()
    }
}

/// Actions a pane emits for the app to handle.
enum PaneOutcome {
    Enter(String),
    Up,
    Home,
    Refresh,
    GoTo(String),
    Drop(DragPayload),
    Transfer,
    NewFolder,
    NewFile,
    DeleteSelected,
    Rename(String),
    Edit(String),
}

/// A remote→remote staged transfer awaiting its upload leg.
struct StageJob {
    temp_dir: PathBuf,
    names: Vec<String>,
    dest_dir: String,
    dest_left: bool,
    label: String,
}

/// A remote file being edited locally; polled for changes to auto re-upload.
struct EditWatch {
    side: Side,
    temp_file: PathBuf,
    remote_dir: String,
    name: String,
    last_mtime: Option<SystemTime>,
}

struct TransferState {
    id: u64,
    label: String,
    upload: bool,
    bytes_done: u64,
    bytes_total: u64,
    file_index: usize,
    file_count: usize,
    current: String,
    done: bool,
    error: bool,
}

/// Add/edit connection form.
struct ConnEditor {
    editing: Option<Uuid>,
    kind: ConnectionKind,
    name: String,
    host: String,
    port: String,
    username: String,
    domain: String,
    insecure_tls: bool,
    group: String,
    auth_kind: usize, // 0 password, 1 key, 2 agent
    password: String,
    key_path: String,
    passphrase: String,
    notes: String,
}

impl ConnEditor {
    fn blank() -> Self {
        ConnEditor {
            editing: None,
            kind: ConnectionKind::Ssh,
            name: String::new(),
            host: String::new(),
            port: "22".into(),
            username: String::new(),
            domain: String::new(),
            insecure_tls: false,
            group: String::new(),
            auth_kind: 0,
            password: String::new(),
            key_path: String::new(),
            passphrase: String::new(),
            notes: String::new(),
        }
    }
}

/// A modal text prompt (new folder / rename).
struct Prompt {
    title: String,
    label: String,
    value: String,
    kind: PromptKind,
}

#[derive(Clone)]
enum PromptKind {
    NewFolder(Side),
    NewFile(Side),
    Rename(Side, String),
}

pub struct FerropipeApp {
    store: Store,
    vault: Vault,
    store_path: PathBuf,
    worker: WorkerHandle,
    worker_join: Option<std::thread::JoinHandle<()>>,
    events: mpsc::Receiver<Event>,

    connected: Option<Uuid>,
    connecting: bool,
    pending_connect: Option<Uuid>,
    status: String,

    // Second session backing the LEFT pane when it hosts a remote host (for
    // remote-to-remote transfer). When `left_remote` is false the left pane is local.
    left_remote: bool,
    lworker: WorkerHandle,
    lworker_join: Option<std::thread::JoinHandle<()>>,
    levents: mpsc::Receiver<Event>,
    lconnected: Option<Uuid>,
    lconnecting: bool,
    lpending: Option<Uuid>,
    lkind: &'static str,
    /// Local cwd to restore when the left pane reverts from remote to local.
    left_saved_cwd: String,
    /// Remote→remote staging: source-download id → job describing the follow-up upload.
    stage_download: HashMap<u64, StageJob>,
    /// Upload id → temp dir to clean up once the staged upload completes.
    stage_cleanup: HashMap<u64, PathBuf>,

    local: PaneState,
    remote: PaneState,

    transfers: Vec<TransferState>,
    next_id: u64,

    editor: Option<ConnEditor>,
    prompt: Option<Prompt>,
    confirm_delete: Option<(Side, Vec<String>)>,

    toasts: Vec<(String, f64, bool)>,
    search: String,
    selected_conn: Option<Uuid>,

    // Command console (SSH exec)
    console: Vec<(String, bool)>, // (line, is_err)
    console_input: String,
    console_open: bool,
    exec_running: bool,
    active_kind: &'static str,

    // Live remote-file editing
    pending_edit: HashMap<u64, EditWatch>,
    edits: Vec<EditWatch>,
    edit_poll: f64,

    // Native RDP (in-app, via ferropipe-rdp) sessions.
    native_rdp: crate::native_rdp::NativeRdpManager,
}

impl FerropipeApp {
    pub fn new(cc: &eframe::CreationContext<'_>, store: Store, vault: Vault, store_path: PathBuf) -> Self {
        setup_fonts(&cc.egui_ctx);
        apply_theme(&cc.egui_ctx, store.settings.dark_mode);

        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
        let (ev_tx, ev_rx) = mpsc::channel::<Event>();
        let ctx = cc.egui_ctx.clone();
        let worker_join = remote::spawn(cmd_rx, ev_tx, move || ctx.request_repaint());

        // Second worker backs the left pane when it hosts a remote (remote-to-remote).
        let (lcmd_tx, lcmd_rx) = mpsc::channel::<Command>();
        let (lev_tx, lev_rx) = mpsc::channel::<Event>();
        let ctx2 = cc.egui_ctx.clone();
        let lworker_join = remote::spawn(lcmd_rx, lev_tx, move || ctx2.request_repaint());

        let home = localfs::home_dir();
        let local_start = store
            .settings
            .last_local_dir
            .clone()
            .unwrap_or_else(|| home.to_string_lossy().into_owned());

        let mut local = PaneState::new(false, local_start);
        reload_local(&mut local, store.settings.show_hidden);

        FerropipeApp {
            worker: WorkerHandle { tx: cmd_tx },
            worker_join: Some(worker_join),
            events: ev_rx,
            store,
            vault,
            store_path,
            connected: None,
            connecting: false,
            pending_connect: None,
            status: "Not connected".into(),
            left_remote: false,
            lworker: WorkerHandle { tx: lcmd_tx },
            lworker_join: Some(lworker_join),
            levents: lev_rx,
            lconnected: None,
            lconnecting: false,
            lpending: None,
            lkind: "",
            left_saved_cwd: String::new(),
            stage_download: HashMap::new(),
            stage_cleanup: HashMap::new(),
            local,
            remote: PaneState::new(true, "/".into()),
            transfers: Vec::new(),
            next_id: 1,
            editor: None,
            prompt: None,
            confirm_delete: None,
            toasts: Vec::new(),
            search: String::new(),
            selected_conn: None,
            console: Vec::new(),
            console_input: String::new(),
            console_open: false,
            exec_running: false,
            active_kind: "",
            pending_edit: HashMap::new(),
            edits: Vec::new(),
            edit_poll: 0.0,
            native_rdp: crate::native_rdp::NativeRdpManager::new(),
        }
    }

    fn toast(&mut self, ctx: &egui::Context, msg: impl Into<String>, error: bool) {
        let now = ctx.input(|i| i.time);
        self.toasts.push((msg.into(), now + 4.0, error));
    }

    fn save_store(&mut self) {
        if let Err(e) = self.store.save(&self.store_path) {
            self.status = format!("save failed: {e:#}");
        }
    }

    fn import_ssh_config(&mut self, ctx: &egui::Context) {
        let imported = crate::sshconfig::import();
        let mut added = 0;
        for c in imported {
            let exists = self
                .store
                .connections
                .iter()
                .any(|x| x.host == c.host && x.username == c.username && x.name == c.name);
            if !exists {
                self.store.connections.push(c);
                added += 1;
            }
        }
        self.save_store();
        self.toast(ctx, format!("Imported {added} host(s) from ~/.ssh/config"), added == 0);
    }

    fn start_edit(&mut self, ctx: &egui::Context, side: Side, name: &str) {
        let remote_path = self.side_pane(side).full_path(name);
        let remote_dir = self.side_pane(side).cwd.clone();
        let dir = std::env::temp_dir().join(format!("ferropipe-edit-{}-{}", std::process::id(), self.next_id));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.toast(ctx, format!("edit: {e:#}"), true);
            return;
        }
        let temp_file = dir.join(name);
        let id = self.next_id;
        self.next_id += 1;
        self.pending_edit.insert(
            id,
            EditWatch { side, temp_file, remote_dir, name: name.to_string(), last_mtime: None },
        );
        self.side_send(side, Command::Download { id, remote: vec![remote_path], local_dir: dir });
        self.toast(ctx, format!("Opening {name} for editing…"), false);
    }

    /// Poll edited temp files; re-upload any that changed since last check.
    fn poll_edits(&mut self, ctx: &egui::Context) {
        let mut uploads: Vec<(Side, PathBuf, String, String)> = Vec::new();
        for w in &mut self.edits {
            let m = std::fs::metadata(&w.temp_file).and_then(|md| md.modified()).ok();
            match (w.last_mtime, m) {
                (Some(last), Some(now)) if now > last => {
                    w.last_mtime = Some(now);
                    uploads.push((w.side, w.temp_file.clone(), w.remote_dir.clone(), w.name.clone()));
                }
                (None, Some(now)) => w.last_mtime = Some(now),
                _ => {}
            }
        }
        for (side, temp, dir, name) in uploads {
            // Upload back to the SAME session the file was opened from.
            if !self.side_connected(side) {
                self.toast(ctx, format!("{name}: host disconnected, save not synced"), true);
                continue;
            }
            let id = self.push_transfer(format!("↑ sync {name}"), true);
            self.side_send(side, Command::Upload { id, local: vec![temp], remote_dir: dir });
            self.toast(ctx, format!("Syncing {name} → remote"), false);
        }
    }

    /// Revert the left pane from a remote session back to the local filesystem.
    fn revert_left_to_local(&mut self) {
        self.left_remote = false;
        self.lconnected = None;
        self.lconnecting = false;
        self.local.remote = false;
        self.local.loading = false;
        self.local.entries.clear();
        self.local.selected.clear();
        let cwd = if self.left_saved_cwd.is_empty() {
            localfs::home_dir().to_string_lossy().into_owned()
        } else {
            self.left_saved_cwd.clone()
        };
        self.local.cwd = cwd.clone();
        self.local.path_edit = cwd;
        let show_hidden = self.store.settings.show_hidden;
        reload_local(&mut self.local, show_hidden);
    }

    fn submit_console(&mut self) {
        let cmd = self.console_input.trim().to_string();
        if cmd.is_empty() {
            return;
        }
        if self.connected.is_none() {
            self.console.push(("(not connected)".into(), true));
            return;
        }
        self.console.push((format!("$ {cmd}"), false));
        self.exec_running = true;
        self.worker.send(Command::Exec { command: cmd });
        self.console_input.clear();
    }

    fn connect(&mut self, ctx: &egui::Context, id: Uuid) {
        let Some(conn) = self.store.connections.iter().find(|c| c.id == id).cloned() else {
            return;
        };
        // RDP connections launch the external Remmina client instead of an in-app session.
        if conn.kind == ConnectionKind::Rdp {
            self.selected_conn = Some(id);
            let password = match &conn.auth {
                AuthMethod::Password { password_enc } => self.vault.decrypt(password_enc).unwrap_or_default(),
                _ => String::new(),
            };
            let config_dir = self
                .store_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            match crate::rdp::launch(&conn, &password, &config_dir) {
                Ok(_) => self.toast(ctx, format!("Launched Remmina → {}", conn.name), false),
                Err(e) => self.toast(ctx, format!("RDP: {e:#}"), true),
            }
            return;
        }
        // Native RDP: open an in-app session via ferropipe-rdp (no external tool).
        if conn.kind == ConnectionKind::RdpNative {
            self.selected_conn = Some(id);
            let password = match &conn.auth {
                AuthMethod::Password { password_enc } => self.vault.decrypt(password_enc).unwrap_or_default(),
                _ => String::new(),
            };
            let mut params = ferropipe_rdp::session::SessionParams::new(&conn.host, &conn.username, &password);
            params.port = conn.port;
            params.domain = conn.domain.clone();
            self.native_rdp.open(params, format!("RDP — {}", conn.name));
            self.toast(ctx, format!("Opening native RDP → {}", conn.name), false);
            return;
        }
        if self.connecting {
            self.toast(ctx, "Already connecting — please wait…", true);
            return;
        }
        // If already connected, drop the current session first. Commands are processed
        // in order, so the worker handles Disconnect and then the new Connect.
        if self.connected.is_some() {
            self.worker.send(Command::Disconnect);
            self.connected = None;
            self.remote.entries.clear();
            self.remote.selected.clear();
        }
        let secret = match &conn.auth {
            AuthMethod::Password { password_enc } => self.vault.decrypt(password_enc).ok(),
            AuthMethod::Key { passphrase_enc, .. } => passphrase_enc
                .as_ref()
                .and_then(|b| self.vault.decrypt(b).ok()),
            AuthMethod::Agent => None,
        };
        self.selected_conn = Some(id);
        self.pending_connect = Some(id);
        self.connecting = true;
        self.status = format!("Connecting to {}…", conn.target());
        self.toast(ctx, format!("Connecting to {}", conn.name), false);
        self.worker.send(Command::Connect {
            conn: Box::new(conn),
            secret,
        });
    }

    fn disconnect(&mut self) {
        self.worker.send(Command::Disconnect);
        self.connected = None;
        self.remote.entries.clear();
        self.remote.selected.clear();
        self.status = "Not connected".into();
    }

    fn decrypt_secret(&self, conn: &Connection) -> Option<String> {
        match &conn.auth {
            AuthMethod::Password { password_enc } => self.vault.decrypt(password_enc).ok(),
            AuthMethod::Key { passphrase_enc, .. } => {
                passphrase_enc.as_ref().and_then(|b| self.vault.decrypt(b).ok())
            }
            AuthMethod::Agent => None,
        }
    }

    fn connect_left(&mut self, ctx: &egui::Context, id: Uuid) {
        let Some(conn) = self.store.connections.iter().find(|c| c.id == id).cloned() else {
            return;
        };
        if matches!(conn.kind, ConnectionKind::Rdp | ConnectionKind::RdpNative) {
            self.toast(ctx, "RDP can't be used as a file pane — use SSH/SMB/WinRM", true);
            return;
        }
        if self.lconnecting {
            self.toast(ctx, "Left pane already connecting…", true);
            return;
        }
        if !self.left_remote {
            self.left_saved_cwd = self.local.cwd.clone();
        }
        if self.lconnected.is_some() {
            self.lworker.send(Command::Disconnect);
            self.lconnected = None;
        }
        let name = conn.name.clone();
        let secret = self.decrypt_secret(&conn);
        self.left_remote = true;
        self.local.remote = true;
        self.local.entries.clear();
        self.local.selected.clear();
        self.lpending = Some(id);
        self.lconnecting = true;
        self.lworker.send(Command::Connect { conn: Box::new(conn), secret });
        self.toast(ctx, format!("Left pane ← {name}"), false);
    }

    fn render_left_picker(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ui.add_space(2.0);
        let mut choice: Option<Option<Uuid>> = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Source:").small().weak());
            let current = if self.left_remote {
                self.lconnected
                    .and_then(|id| self.store.connections.iter().find(|c| c.id == id))
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| "remote".into())
            } else {
                "This computer".into()
            };
            egui::ComboBox::from_id_salt("leftpick")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    if ui.selectable_label(!self.left_remote, "🖥 This computer").clicked() {
                        choice = Some(None);
                    }
                    ui.separator();
                    for c in &self.store.connections {
                        if c.kind.browses_files() {
                            if ui
                                .selectable_label(false, format!("{} ({})", c.name, c.kind.label()))
                                .clicked()
                            {
                                choice = Some(Some(c.id));
                            }
                        }
                    }
                });
            if self.left_remote && ui.small_button("⏏").on_hover_text("Back to local").clicked() {
                choice = Some(None);
            }
        });
        match choice {
            Some(None) => {
                if self.left_remote || self.lconnecting {
                    self.lworker.send(Command::Disconnect);
                    self.revert_left_to_local(); // revert immediately, don't wait for the event
                }
            }
            Some(Some(id)) => self.connect_left(ctx, id),
            None => {}
        }
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        // Collect first (try_recv borrows the receiver) then handle with &mut self.
        let mut right = Vec::new();
        while let Ok(ev) = self.events.try_recv() {
            right.push(ev);
        }
        let mut left = Vec::new();
        while let Ok(ev) = self.levents.try_recv() {
            left.push(ev);
        }
        for ev in right {
            self.handle_event(ctx, Side::Right, ev);
        }
        for ev in left {
            self.handle_event(ctx, Side::Left, ev);
        }
    }

    fn handle_event(&mut self, ctx: &egui::Context, side: Side, ev: Event) {
        let left = side == Side::Left;
        match ev {
            Event::Connected { home, kind } => {
                if left {
                    self.lconnecting = false;
                    self.lconnected = self.lpending.take();
                    self.lkind = kind;
                    self.local.remote = true;
                    self.local.loading = true;
                    self.lworker.send(Command::ListRemote { path: home });
                } else {
                    self.connecting = false;
                    self.connected = self.pending_connect.take().or(self.selected_conn);
                    self.active_kind = kind;
                    self.remote.loading = true;
                    self.status = format!("Connected ({kind}) — {home}");
                    self.worker.send(Command::ListRemote { path: home });
                }
            }
            Event::RemoteListing { path, mut entries } => {
                let pane = if left { &mut self.local } else { &mut self.remote };
                pane.cwd = path.clone();
                pane.path_edit = path;
                std::mem::swap(&mut pane.entries, &mut entries);
                pane.selected.clear();
                pane.loading = false;
                pane.error = None;
                pane.sort();
            }
            Event::Progress(p) => self.on_progress(p),
            Event::TransferDone { id, ok } => self.on_transfer_done(ctx, id, ok),
            Event::OpDone { refresh } => {
                if left {
                    if self.lconnected.is_some() {
                        let path = refresh.unwrap_or_else(|| self.local.cwd.clone());
                        self.local.loading = true;
                        self.lworker.send(Command::ListRemote { path });
                    }
                } else if self.connected.is_some() {
                    let path = refresh.unwrap_or_else(|| self.remote.cwd.clone());
                    self.worker.send(Command::ListRemote { path });
                }
            }
            Event::ExecOutput { line, is_err, .. } => {
                self.console.push((line, is_err));
                if self.console.len() > 5000 {
                    self.console.drain(0..self.console.len() - 5000);
                }
            }
            Event::ExecDone { code, .. } => {
                self.exec_running = false;
                self.console.push((format!("[exit {code}]"), code != 0));
            }
            Event::Error { context, message } => {
                let msg = format!("{context}: {message}");
                if left {
                    self.lconnecting = false;
                    self.local.loading = false;
                    // A failed left-pane connect never emits Disconnected — recover here.
                    if self.lconnected.is_none() {
                        self.revert_left_to_local();
                    }
                } else {
                    self.connecting = false;
                    self.remote.loading = false;
                    self.status = msg.clone();
                }
                self.toast(ctx, msg, true);
            }
            Event::Disconnected => {
                if left {
                    self.revert_left_to_local();
                } else {
                    self.connected = None;
                    self.remote.entries.clear();
                    self.remote.selected.clear();
                    self.remote.cwd = "/".into();
                    self.remote.path_edit = String::new();
                    self.remote.error = None;
                    self.status = "Disconnected".into();
                }
            }
        }
    }

    fn on_progress(&mut self, p: crate::remote::TransferProgress) {
        if let Some(t) = self.transfers.iter_mut().find(|t| t.id == p.id) {
            t.bytes_done = p.bytes_done;
            t.bytes_total = p.bytes_total;
            t.file_index = p.file_index;
            t.file_count = p.file_count;
            t.current = p.current_file;
        }
    }

    fn on_transfer_done(&mut self, ctx: &egui::Context, id: u64, ok: bool) {
        // 1) Live-edit download → open the editor and watch for saves.
        if let Some(watch) = self.pending_edit.remove(&id) {
            if ok {
                let name = watch.name.clone();
                match crate::external::open_editor(&watch.temp_file) {
                    Ok(_) => {
                        let mtime = std::fs::metadata(&watch.temp_file).and_then(|m| m.modified()).ok();
                        self.edits.push(EditWatch { last_mtime: mtime, ..watch });
                        self.toast(ctx, format!("Editing {name} — saves auto-upload"), false);
                    }
                    Err(e) => self.toast(ctx, format!("editor: {e:#}"), true),
                }
            } else {
                self.toast(ctx, "edit: download failed", true);
            }
            return;
        }
        // 2) Remote→remote staged download leg → kick off the upload leg.
        if let Some(job) = self.stage_download.remove(&id) {
            if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                t.done = true;
                t.error = !ok;
            }
            if !ok {
                let _ = std::fs::remove_dir_all(&job.temp_dir);
                self.toast(ctx, "remote→remote: source download failed", true);
                return;
            }
            let ul_id = self.next_id;
            self.next_id += 1;
            let locals: Vec<PathBuf> = job.names.iter().map(|n| job.temp_dir.join(n)).collect();
            self.transfers.push(TransferState {
                id: ul_id,
                label: format!("{} ▸ upload", job.label),
                upload: true,
                bytes_done: 0,
                bytes_total: 0,
                file_index: 0,
                file_count: 0,
                current: String::new(),
                done: false,
                error: false,
            });
            let cmd = Command::Upload { id: ul_id, local: locals, remote_dir: job.dest_dir.clone() };
            if job.dest_left {
                self.lworker.send(cmd);
            } else {
                self.worker.send(cmd);
            }
            self.stage_cleanup.insert(ul_id, job.temp_dir);
            return;
        }
        // 3) Remote→remote staged upload leg → clean up temp.
        if let Some(temp) = self.stage_cleanup.remove(&id) {
            if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                t.done = true;
                t.error = !ok;
            }
            let _ = std::fs::remove_dir_all(&temp);
            self.toast(
                ctx,
                if ok { "Remote → remote transfer complete" } else { "Remote → remote transfer failed" },
                !ok,
            );
            return;
        }
        // 4) Normal transfer.
        let mut upload = false;
        if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
            t.done = true;
            t.error = !ok;
            upload = t.upload;
        }
        if ok {
            if !upload && !self.left_remote {
                reload_local(&mut self.local, self.store.settings.show_hidden);
            }
            self.toast(ctx, "Transfer complete", false);
        } else {
            self.toast(ctx, "Transfer failed — see status bar", true);
        }
    }

    fn side_is_remote(&self, side: Side) -> bool {
        match side {
            Side::Left => self.left_remote,
            Side::Right => true,
        }
    }

    fn side_pane(&self, side: Side) -> &PaneState {
        match side {
            Side::Left => &self.local,
            Side::Right => &self.remote,
        }
    }

    fn side_pane_mut(&mut self, side: Side) -> &mut PaneState {
        match side {
            Side::Left => &mut self.local,
            Side::Right => &mut self.remote,
        }
    }

    fn side_send(&self, side: Side, cmd: Command) {
        match side {
            Side::Left => self.lworker.send(cmd),
            Side::Right => self.worker.send(cmd),
        }
    }

    fn side_connected(&self, side: Side) -> bool {
        match side {
            Side::Left => self.lconnected.is_some(),
            Side::Right => self.connected.is_some(),
        }
    }

    fn navigate(&mut self, side: Side, path: String) {
        if self.side_is_remote(side) {
            if !self.side_connected(side) {
                return;
            }
            let pane = self.side_pane_mut(side);
            pane.loading = true;
            pane.path_edit = path.clone();
            self.side_send(side, Command::ListRemote { path });
        } else {
            let pane = self.side_pane_mut(side);
            pane.cwd = path.clone();
            pane.path_edit = path;
            let show_hidden = self.store.settings.show_hidden;
            reload_local(self.side_pane_mut(side), show_hidden);
        }
    }

    fn handle_pane(&mut self, ctx: &egui::Context, side: Side, outcomes: Vec<PaneOutcome>) {
        let remote = self.side_is_remote(side);
        for o in outcomes {
            match o {
                PaneOutcome::Enter(path) => self.navigate(side, path),
                PaneOutcome::Up => {
                    let parent = parent_dir(&self.side_pane(side).cwd, remote);
                    self.navigate(side, parent);
                }
                PaneOutcome::Home => {
                    if remote {
                        if self.side_connected(side) {
                            self.side_pane_mut(side).loading = true;
                            self.side_send(side, Command::ListRemote { path: ".".into() });
                        }
                    } else {
                        let h = localfs::home_dir().to_string_lossy().into_owned();
                        self.navigate(side, h);
                    }
                }
                PaneOutcome::Refresh => {
                    if remote {
                        if self.side_connected(side) {
                            let cwd = self.side_pane(side).cwd.clone();
                            self.side_pane_mut(side).loading = true;
                            self.side_send(side, Command::ListRemote { path: cwd });
                        }
                    } else {
                        let show_hidden = self.store.settings.show_hidden;
                        reload_local(self.side_pane_mut(side), show_hidden);
                    }
                }
                PaneOutcome::GoTo(path) => self.navigate(side, path),
                PaneOutcome::Drop(payload) => self.start_transfer(ctx, payload, side),
                PaneOutcome::Transfer => {
                    let payload = DragPayload {
                        side,
                        remote,
                        paths: self.side_pane(side).selected_paths(),
                    };
                    let other = if side == Side::Left { Side::Right } else { Side::Left };
                    self.start_transfer(ctx, payload, other);
                }
                PaneOutcome::NewFolder => {
                    self.prompt = Some(Prompt {
                        title: "New folder".into(),
                        label: "Folder name".into(),
                        value: String::new(),
                        kind: PromptKind::NewFolder(side),
                    });
                }
                PaneOutcome::NewFile => {
                    self.prompt = Some(Prompt {
                        title: "New file".into(),
                        label: "File name".into(),
                        value: String::new(),
                        kind: PromptKind::NewFile(side),
                    });
                }
                PaneOutcome::DeleteSelected => {
                    let paths = self.side_pane(side).selected_paths();
                    if paths.is_empty() {
                        continue;
                    }
                    if self.store.settings.confirm_delete {
                        self.confirm_delete = Some((side, paths));
                    } else {
                        self.do_delete(ctx, side, paths);
                    }
                }
                PaneOutcome::Rename(name) => {
                    self.prompt = Some(Prompt {
                        title: "Rename".into(),
                        label: "New name".into(),
                        value: name.clone(),
                        kind: PromptKind::Rename(side, name),
                    });
                }
                PaneOutcome::Edit(name) => {
                    if remote && self.side_connected(side) {
                        self.start_edit(ctx, side, &name);
                    }
                }
            }
        }
    }

    fn push_transfer(&mut self, label: String, upload: bool) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.transfers.push(TransferState {
            id,
            label,
            upload,
            bytes_done: 0,
            bytes_total: 0,
            file_index: 0,
            file_count: 0,
            current: String::new(),
            done: false,
            error: false,
        });
        id
    }

    fn start_transfer(&mut self, ctx: &egui::Context, payload: DragPayload, dest: Side) {
        if payload.side == dest || payload.paths.is_empty() {
            return;
        }
        let src = payload.side;
        let src_remote = payload.remote;
        let dst_remote = self.side_is_remote(dest);
        let src_ok = !src_remote || self.side_connected(src);
        let dst_ok = !dst_remote || self.side_connected(dest);
        if !src_ok || !dst_ok {
            self.toast(ctx, "Both sides must be connected to transfer", true);
            return;
        }
        let n = payload.paths.len();
        match (src_remote, dst_remote) {
            (true, false) => {
                // remote → local: download into the destination local pane
                let dir = PathBuf::from(&self.side_pane(dest).cwd);
                let id = self.push_transfer(format!("↓ Download {n} item(s)"), false);
                self.side_send(src, Command::Download { id, remote: payload.paths, local_dir: dir });
            }
            (false, true) => {
                // local → remote: upload into the destination remote pane
                let dir = self.side_pane(dest).cwd.clone();
                let id = self.push_transfer(format!("↑ Upload {n} item(s)"), true);
                self.side_send(dest, Command::Upload {
                    id,
                    local: payload.paths.iter().map(PathBuf::from).collect(),
                    remote_dir: dir,
                });
            }
            (true, true) => {
                // remote → remote: stage through a local temp dir (download then upload)
                let temp_dir = std::env::temp_dir()
                    .join(format!("ferropipe-r2r-{}-{}", std::process::id(), self.next_id));
                if let Err(e) = std::fs::create_dir_all(&temp_dir) {
                    self.toast(ctx, format!("stage: {e:#}"), true);
                    return;
                }
                let names: Vec<String> = payload.paths.iter().map(|p| base_name(p)).collect();
                let dest_dir = self.side_pane(dest).cwd.clone();
                let dest_left = dest == Side::Left;
                let label = format!("⇄ remote→remote {n} item(s)");
                let id = self.push_transfer(label.clone(), false);
                self.side_send(src, Command::Download {
                    id,
                    remote: payload.paths,
                    local_dir: temp_dir.clone(),
                });
                self.stage_download.insert(id, StageJob { temp_dir, names, dest_dir, dest_left, label });
            }
            (false, false) => { /* local → local: not supported here */ }
        }
    }

    fn do_delete(&mut self, ctx: &egui::Context, side: Side, paths: Vec<String>) {
        if self.side_is_remote(side) {
            self.side_send(side, Command::DeleteRemote { paths });
        } else {
            for p in &paths {
                if let Err(e) = localfs::delete(std::path::Path::new(p)) {
                    self.toast(ctx, format!("delete failed: {e:#}"), true);
                }
            }
            let show_hidden = self.store.settings.show_hidden;
            reload_local(self.side_pane_mut(side), show_hidden);
        }
    }
}

impl eframe::App for FerropipeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_events(&ctx);

        // Draw any open native RDP session windows.
        self.native_rdp.show(&ctx);

        // Top bar
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("⬢ Ferropipe").strong().size(18.0).color(ACCENT));
                ui.separator();
                if self.connected.is_some() {
                    ui.label(RichText::new("● connected").color(Color32::from_rgb(0x5B, 0xC8, 0x7A)));
                    if ui.button("Disconnect").clicked() {
                        self.disconnect();
                    }
                } else if self.connecting {
                    ui.label(RichText::new("● connecting…").color(ACCENT));
                    ui.spinner();
                } else {
                    ui.label(RichText::new("● offline").color(Color32::GRAY));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut dark = self.store.settings.dark_mode;
                    if ui
                        .selectable_label(dark, if dark { "☾" } else { "☀" })
                        .on_hover_text("Toggle theme")
                        .clicked()
                    {
                        dark = !dark;
                        self.store.settings.dark_mode = dark;
                        apply_theme(&ctx, dark);
                        self.save_store();
                    }
                    if ui
                        .selectable_label(self.console_open, "⌨ Console")
                        .on_hover_text("Run commands on the SSH host")
                        .clicked()
                    {
                        self.console_open = !self.console_open;
                    }
                    let mut hidden = self.store.settings.show_hidden;
                    if ui.checkbox(&mut hidden, "Hidden files").changed() {
                        self.store.settings.show_hidden = hidden;
                        reload_local(&mut self.local, hidden);
                        if self.connected.is_some() {
                            self.worker.send(Command::ListRemote {
                                path: self.remote.cwd.clone(),
                            });
                        }
                        self.save_store();
                    }
                });
            });
            ui.add_space(2.0);
        });

        // Bottom status + transfer queue
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(&self.status).small());
            });
            for i in 0..self.transfers.len() {
                let t = &self.transfers[i];
                let frac = if t.error {
                    1.0
                } else if t.bytes_total > 0 {
                    t.bytes_done as f32 / t.bytes_total as f32
                } else if t.done {
                    1.0
                } else {
                    0.0
                };
                ui.horizontal(|ui| {
                    let label = if t.error {
                        RichText::new(&t.label).small().color(Color32::LIGHT_RED)
                    } else {
                        RichText::new(&t.label).small()
                    };
                    ui.label(label);
                    let mut bar = egui::ProgressBar::new(frac).desired_width(240.0).text(if t.error {
                        "failed".to_string()
                    } else if t.done {
                        "done".to_string()
                    } else if t.file_count > 0 {
                        format!("{}/{}  {}", t.file_index, t.file_count, localfs::human_size(t.bytes_done))
                    } else {
                        "…".to_string()
                    });
                    if t.error {
                        bar = bar.fill(Color32::from_rgb(0x8B, 0x2E, 0x2E));
                    }
                    ui.add(bar);
                    if !t.current.is_empty() && !t.done {
                        ui.label(RichText::new(&t.current).small().weak());
                    }
                });
            }
            // Prune finished successful transfers; keep failures visible for the session.
            self.transfers.retain(|t| !(t.done && !t.error));
            ui.add_space(2.0);
        });

        // Command console (SSH exec), toggled from the top bar.
        if self.console_open {
            egui::Panel::bottom("console")
                .resizable(true)
                .default_size(190.0)
                .size_range(egui::Rangef::new(120.0, 480.0))
                .show(ui, |ui| {
                    ui.add_space(3.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Console").strong().color(ACCENT));
                        if self.exec_running {
                            ui.spinner();
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Clear").clicked() {
                                self.console.clear();
                            }
                        });
                    });
                    let mut submit = false;
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("$").monospace().color(ACCENT));
                        let hint = if self.active_kind == "SFTP" {
                            "run a command… (Enter)"
                        } else {
                            "connect via SSH to run commands"
                        };
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.console_input)
                                .desired_width(f32::INFINITY)
                                .hint_text(hint)
                                .font(egui::TextStyle::Monospace),
                        );
                        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            submit = true;
                        }
                    });
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            let normal = ui.visuals().text_color();
                            for (line, is_err) in &self.console {
                                let color = if *is_err { Color32::from_rgb(0xE0, 0x6C, 0x6C) } else { normal };
                                ui.label(RichText::new(line).monospace().color(color));
                            }
                        });
                    if submit {
                        self.submit_console();
                    }
                });
        }

        // Left: connections sidebar
        egui::Panel::left("connections")
            .resizable(true)
            .default_size(250.0)
            .size_range(180.0..=420.0)
            .show(ui, |ui| {
                self.render_sidebar(&ctx, ui);
            });

        // Center: dual panes
        egui::CentralPanel::default().show(ui, |ui| {
            let avail = ui.available_width();
            egui::Panel::left("localpane")
                .resizable(true)
                .default_size(avail * 0.5)
                .size_range(egui::Rangef::new(260.0, (avail - 220.0).max(300.0)))
                .show(ui, |ui| {
                    self.render_left_picker(&ctx, ui);
                    let (title, connected) = if self.left_remote {
                        (
                            self.lconnected
                                .and_then(|id| self.store.connections.iter().find(|c| c.id == id))
                                .map(|c| c.name.clone())
                                .unwrap_or_else(|| "Remote (connecting…)".into()),
                            self.lconnected.is_some(),
                        )
                    } else {
                        ("This computer".to_string(), true)
                    };
                    let out = render_pane(ui, &mut self.local, Side::Left, &title, connected);
                    self.handle_pane(&ctx, Side::Left, out);
                });
            egui::CentralPanel::default().show(ui, |ui| {
                let title = if self.connected.is_some() {
                    "Remote server"
                } else {
                    "Remote (not connected)"
                };
                let connected = self.connected.is_some();
                let out = render_pane(ui, &mut self.remote, Side::Right, title, connected);
                self.handle_pane(&ctx, Side::Right, out);
            });
        });

        self.render_dialogs(&ctx);
        self.render_toasts(&ctx);

        // Keep animating while transfers run.
        // Poll edited files for changes (auto re-upload on save).
        if !self.edits.is_empty() {
            let now = ctx.input(|i| i.time);
            if now - self.edit_poll > 1.0 {
                self.edit_poll = now;
                self.poll_edits(&ctx);
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(1000));
        }

        if self.transfers.iter().any(|t| !t.done) || self.connecting {
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }
    }
}

impl FerropipeApp {
    fn render_sidebar(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Connections").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("+ New").on_hover_text("New connection").clicked() {
                    self.editor = Some(ConnEditor::blank());
                }
                if ui.button("⇩").on_hover_text("Import hosts from ~/.ssh/config").clicked() {
                    self.import_ssh_config(ctx);
                }
            });
        });
        ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search…").desired_width(f32::INFINITY));
        ui.separator();

        let groups = self.store.groups();
        let search = self.search.to_lowercase();
        let matches = |c: &Connection| {
            search.is_empty()
                || c.name.to_lowercase().contains(&search)
                || c.host.to_lowercase().contains(&search)
        };

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            // Ungrouped first
            let ungrouped: Vec<Connection> = self
                .store
                .connections
                .iter()
                .filter(|c| c.group.is_empty() && matches(c))
                .cloned()
                .collect();
            for c in ungrouped {
                self.conn_row(ctx, ui, &c);
            }
            for g in &groups {
                // only render leaf groups' connections under their full path header
                let conns: Vec<Connection> = self
                    .store
                    .connections
                    .iter()
                    .filter(|c| &c.group == g && matches(c))
                    .cloned()
                    .collect();
                if conns.is_empty() {
                    continue;
                }
                egui::CollapsingHeader::new(RichText::new(g).strong())
                    .default_open(!search.is_empty())
                    .show(ui, |ui| {
                        for c in conns {
                            self.conn_row(ctx, ui, &c);
                        }
                    });
            }
        });
    }

    fn conn_row(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, c: &Connection) {
        let selected = self.selected_conn == Some(c.id);
        let active = self.connected == Some(c.id);
        let dot = if active {
            "●"
        } else if c.kind != ConnectionKind::Ssh {
            "▷"
        } else {
            "•"
        };
        let tag = if c.kind == ConnectionKind::Ssh {
            String::new()
        } else {
            format!("  {}", c.kind.label())
        };
        let text = format!("{dot} {}{tag}", c.name);
        let resp = ui.selectable_label(selected, RichText::new(text));
        resp.clone().on_hover_text(c.target());
        if resp.clicked() {
            self.selected_conn = Some(c.id);
        }
        if resp.double_clicked() {
            self.selected_conn = Some(c.id);
            self.connect(ctx, c.id);
        }
        resp.context_menu(|ui| {
            if ui.button("Connect").clicked() {
                self.selected_conn = Some(c.id);
                self.connect(ctx, c.id);
                ui.close();
            }
            if c.kind == ConnectionKind::Ssh && ui.button("Open terminal").clicked() {
                let target = format!("{}@{}", c.username, c.host);
                if let Err(e) = crate::external::open_terminal_ssh(&target, c.port) {
                    self.toast(ctx, format!("terminal: {e:#}"), true);
                }
                ui.close();
            }
            if ui.button("Edit").clicked() {
                self.editor = Some(editor_from(c, &self.vault));
                ui.close();
            }
            if ui.button("Duplicate").clicked() {
                let mut nc = c.clone();
                nc.id = Uuid::new_v4();
                nc.name = format!("{} (copy)", c.name);
                self.store.connections.push(nc);
                self.save_store();
                ui.close();
            }
            ui.separator();
            if ui.button(RichText::new("Delete").color(Color32::LIGHT_RED)).clicked() {
                // Disconnect any live session using this connection before removing it.
                if self.connected == Some(c.id) {
                    self.worker.send(Command::Disconnect);
                    self.connected = None;
                    self.remote.entries.clear();
                }
                if self.lconnected == Some(c.id) {
                    self.lworker.send(Command::Disconnect);
                }
                self.store.connections.retain(|x| x.id != c.id);
                self.save_store();
                ui.close();
            }
        });
    }

    fn render_dialogs(&mut self, ctx: &egui::Context) {
        // Connection editor
        let mut close_editor = false;
        let mut save_editor = false;
        let existing_groups = self.store.groups();
        if let Some(ed) = &mut self.editor {
            egui::Window::new(if ed.editing.is_some() { "Edit connection" } else { "New connection" })
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    egui::Grid::new("editor_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                        ui.label("Type");
                        let prev_kind = ed.kind;
                        egui::ComboBox::from_id_salt("connkind")
                            .selected_text(match ed.kind {
                                ConnectionKind::Ssh => "SSH / SFTP",
                                ConnectionKind::Rdp => "RDP (Remmina)",
                                ConnectionKind::RdpNative => "RDP (Native)",
                                ConnectionKind::Smb => "SMB (Windows share)",
                                ConnectionKind::WinRm => "WinRM (Windows)",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut ed.kind, ConnectionKind::Ssh, "SSH / SFTP");
                                ui.selectable_value(&mut ed.kind, ConnectionKind::Smb, "SMB (Windows share)");
                                ui.selectable_value(&mut ed.kind, ConnectionKind::WinRm, "WinRM (Windows)");
                                ui.selectable_value(&mut ed.kind, ConnectionKind::Rdp, "RDP (Remmina)");
                                ui.selectable_value(&mut ed.kind, ConnectionKind::RdpNative, "RDP (Native)");
                            });
                        // When the type changes, retarget the port if it was still the old default.
                        if ed.kind != prev_kind {
                            if ed.port == prev_kind.default_port().to_string() {
                                ed.port = ed.kind.default_port().to_string();
                            }
                        }
                        ui.end_row();
                        ui.label("Name");
                        ui.text_edit_singleline(&mut ed.name);
                        ui.end_row();
                        ui.label("Host");
                        ui.text_edit_singleline(&mut ed.host);
                        ui.end_row();
                        ui.label("Port");
                        ui.text_edit_singleline(&mut ed.port);
                        ui.end_row();
                        ui.label("Username");
                        ui.text_edit_singleline(&mut ed.username);
                        ui.end_row();
                        if ed.kind != ConnectionKind::Ssh {
                            ui.label(if ed.kind == ConnectionKind::Smb { "Workgroup/Domain" } else { "Domain" });
                            ui.text_edit_singleline(&mut ed.domain).on_hover_text("Windows domain / workgroup (optional)");
                            ui.end_row();
                        }
                        if ed.kind == ConnectionKind::WinRm {
                            ui.label("TLS");
                            ui.checkbox(&mut ed.insecure_tls, "Accept invalid cert (insecure)")
                                .on_hover_text("Only for self-signed WinRM/HTTPS on :5986. Leave off for strict TLS.");
                            ui.end_row();
                        }
                        ui.label("Group");
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_salt("groedit")
                                .selected_text(if ed.group.is_empty() {
                                    "(top level)".to_string()
                                } else {
                                    ed.group.clone()
                                })
                                .show_ui(ui, |ui| {
                                    if ui.selectable_label(ed.group.is_empty(), "(top level)").clicked() {
                                        ed.group.clear();
                                    }
                                    for g in &existing_groups {
                                        if ui.selectable_label(&ed.group == g, g).clicked() {
                                            ed.group = g.clone();
                                        }
                                    }
                                });
                            ui.text_edit_singleline(&mut ed.group)
                                .on_hover_text("Pick a group above, or type a new one (use / to nest, e.g. WIM/Tovuz)");
                        });
                        ui.end_row();
                        if ed.kind != ConnectionKind::Ssh {
                            ed.auth_kind = 0; // RDP/SMB/WinRM use a password
                        } else {
                            ui.label("Auth");
                            egui::ComboBox::from_id_salt("authkind")
                                .selected_text(match ed.auth_kind {
                                    0 => "Password",
                                    1 => "Key file",
                                    _ => "SSH agent",
                                })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut ed.auth_kind, 0, "Password");
                                    ui.selectable_value(&mut ed.auth_kind, 1, "Key file");
                                    ui.selectable_value(&mut ed.auth_kind, 2, "SSH agent");
                                });
                            ui.end_row();
                        }
                        match ed.auth_kind {
                            0 => {
                                ui.label("Password");
                                ui.add(egui::TextEdit::singleline(&mut ed.password).password(true));
                                ui.end_row();
                            }
                            1 => {
                                ui.label("Key file");
                                ui.horizontal(|ui| {
                                    ui.text_edit_singleline(&mut ed.key_path);
                                    if ui.button("Browse…").clicked() {
                                        if let Some(p) = rfd::FileDialog::new().pick_file() {
                                            ed.key_path = p.to_string_lossy().into_owned();
                                        }
                                    }
                                });
                                ui.end_row();
                                ui.label("Passphrase");
                                ui.add(egui::TextEdit::singleline(&mut ed.passphrase).password(true));
                                ui.end_row();
                            }
                            _ => {}
                        }
                        ui.label("Notes");
                        ui.text_edit_singleline(&mut ed.notes);
                        ui.end_row();
                    });
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Save").color(ACCENT)).clicked() {
                            save_editor = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close_editor = true;
                        }
                    });
                });
        }
        if save_editor {
            self.commit_editor();
            close_editor = true;
        }
        if close_editor {
            self.editor = None;
        }

        // Prompt (new folder / rename)
        let mut prompt_ok = false;
        let mut prompt_cancel = false;
        if let Some(p) = &mut self.prompt {
            egui::Window::new(&p.title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(&p.label);
                    let r = ui.text_edit_singleline(&mut p.value);
                    r.request_focus();
                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked()
                            || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        {
                            prompt_ok = true;
                        }
                        if ui.button("Cancel").clicked() {
                            prompt_cancel = true;
                        }
                    });
                });
        }
        if prompt_ok {
            self.apply_prompt(ctx);
            self.prompt = None;
        }
        if prompt_cancel {
            self.prompt = None;
        }

        // Confirm delete
        let mut do_del: Option<(Side, Vec<String>)> = None;
        let mut cancel_del = false;
        if let Some((side, paths)) = &self.confirm_delete {
            egui::Window::new("Confirm delete")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!("Delete {} item(s)? This cannot be undone.", paths.len()));
                    for p in paths.iter().take(8) {
                        ui.label(RichText::new(p).small().weak());
                    }
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Delete").color(Color32::LIGHT_RED)).clicked() {
                            do_del = Some((*side, paths.clone()));
                        }
                        if ui.button("Cancel").clicked() {
                            cancel_del = true;
                        }
                    });
                });
        }
        if let Some((side, paths)) = do_del {
            self.do_delete(ctx, side, paths);
            self.confirm_delete = None;
        }
        if cancel_del {
            self.confirm_delete = None;
        }
    }

    fn apply_prompt(&mut self, ctx: &egui::Context) {
        let Some(p) = self.prompt.take() else { return };
        let value = p.value.trim().to_string();
        if value.is_empty() {
            return;
        }
        let show_hidden = self.store.settings.show_hidden;
        match p.kind {
            PromptKind::NewFolder(side) => {
                if self.side_is_remote(side) {
                    let path = self.side_pane(side).full_path(&value);
                    self.side_send(side, Command::MkdirRemote { path });
                } else {
                    let cwd = self.side_pane(side).cwd.clone();
                    if let Err(e) = localfs::mkdir(std::path::Path::new(&cwd), &value) {
                        self.toast(ctx, format!("mkdir failed: {e:#}"), true);
                    }
                    reload_local(self.side_pane_mut(side), show_hidden);
                }
            }
            PromptKind::NewFile(side) => {
                if self.side_is_remote(side) {
                    let path = self.side_pane(side).full_path(&value);
                    self.side_send(side, Command::CreateFileRemote { path });
                } else {
                    let path = PathBuf::from(&self.side_pane(side).cwd).join(&value);
                    if let Err(e) = localfs::create_file(&path) {
                        self.toast(ctx, format!("create file failed: {e:#}"), true);
                    }
                    reload_local(self.side_pane_mut(side), show_hidden);
                }
            }
            PromptKind::Rename(side, old) => {
                if self.side_is_remote(side) {
                    let from = self.side_pane(side).full_path(&old);
                    let to = self.side_pane(side).full_path(&value);
                    self.side_send(side, Command::RenameRemote { from, to });
                } else {
                    let cwd = self.side_pane(side).cwd.clone();
                    let from = PathBuf::from(&cwd).join(&old);
                    let to = PathBuf::from(&cwd).join(&value);
                    if let Err(e) = localfs::rename(&from, &to) {
                        self.toast(ctx, format!("rename failed: {e:#}"), true);
                    }
                    reload_local(self.side_pane_mut(side), show_hidden);
                }
            }
        }
    }

    fn commit_editor(&mut self) {
        let Some(ed) = self.editor.take() else { return };
        let default_port = if ed.kind == ConnectionKind::Rdp { 3389 } else { 22 };
        let port: u16 = ed.port.trim().parse().unwrap_or(default_port);
        let auth = match ed.auth_kind {
            0 => AuthMethod::Password {
                password_enc: self.vault.encrypt(&ed.password).unwrap_or_default(),
            },
            1 => AuthMethod::Key {
                private_key: ed.key_path.clone(),
                passphrase_enc: if ed.passphrase.is_empty() {
                    None
                } else {
                    Some(self.vault.encrypt(&ed.passphrase).unwrap_or_default())
                },
            },
            _ => AuthMethod::Agent,
        };
        if let Some(id) = ed.editing {
            if let Some(c) = self.store.connections.iter_mut().find(|c| c.id == id) {
                c.name = ed.name;
                c.host = ed.host;
                c.port = port;
                c.username = ed.username;
                c.kind = ed.kind;
                c.domain = ed.domain;
                c.insecure_tls = ed.insecure_tls;
                c.group = ed.group;
                c.notes = ed.notes;
                c.auth = auth;
            }
        } else {
            let mut c = Connection::new(ed.name, ed.host, ed.username);
            c.port = port;
            c.kind = ed.kind;
            c.domain = ed.domain;
            c.insecure_tls = ed.insecure_tls;
            c.group = ed.group;
            c.notes = ed.notes;
            c.auth = auth;
            self.store.connections.push(c);
        }
        self.save_store();
    }

    fn render_toasts(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        self.toasts.retain(|(_, exp, _)| *exp > now);
        if self.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, [-12.0, -12.0])
            .show(ctx, |ui| {
                for (msg, _, err) in self.toasts.iter().rev().take(5) {
                    let color = if *err { Color32::from_rgb(0xC0, 0x39, 0x2B) } else { Color32::from_rgb(0x33, 0x77, 0x55) };
                    egui::Frame::NONE
                        .fill(color)
                        .corner_radius(egui::CornerRadius::same(6))
                        .inner_margin(egui::Margin::same(8))
                        .show(ui, |ui| {
                            ui.label(RichText::new(msg).color(Color32::WHITE));
                        });
                    ui.add_space(4.0);
                }
            });
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
}

impl Drop for FerropipeApp {
    fn drop(&mut self) {
        self.worker.send(Command::Shutdown);
        self.lworker.send(Command::Shutdown);
        if !self.left_remote {
            self.store.settings.last_local_dir = Some(self.local.cwd.clone());
        }
        let _ = self.store.save(&self.store_path);
        // Let both workers finish any in-flight transfer and exit cleanly.
        if let Some(join) = self.worker_join.take() {
            let _ = join.join();
        }
        if let Some(join) = self.lworker_join.take() {
            let _ = join.join();
        }
        // Clean up any staging / edit temp directories that never completed.
        for (_, job) in self.stage_download.drain() {
            let _ = std::fs::remove_dir_all(&job.temp_dir);
        }
        for (_, temp) in self.stage_cleanup.drain() {
            let _ = std::fs::remove_dir_all(&temp);
        }
        for w in self.edits.drain(..) {
            if let Some(parent) = w.temp_file.parent() {
                let _ = std::fs::remove_dir_all(parent);
            }
        }
        for (_, w) in self.pending_edit.drain() {
            if let Some(parent) = w.temp_file.parent() {
                let _ = std::fs::remove_dir_all(parent);
            }
        }
    }
}

/// Render a file pane, returning the actions the app should handle.
fn render_pane(ui: &mut egui::Ui, pane: &mut PaneState, side: Side, title: &str, connected: bool) -> Vec<PaneOutcome> {
    let mut out = Vec::new();
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).strong().color(ACCENT));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let transfer_label = if pane.remote { "◀ Download" } else { "Upload ▶" };
            let can_transfer = connected && !pane.selected.is_empty();
            if ui.add_enabled(can_transfer, egui::Button::new(RichText::new(transfer_label).color(ACCENT))).clicked() {
                out.push(PaneOutcome::Transfer);
            }
        });
    });

    // Path / nav bar
    ui.horizontal(|ui| {
        if ui.button("↑").on_hover_text("Up").clicked() {
            out.push(PaneOutcome::Up);
        }
        if ui.button("⌂").on_hover_text("Home").clicked() {
            out.push(PaneOutcome::Home);
        }
        if ui.button("↻").on_hover_text("Refresh").clicked() {
            out.push(PaneOutcome::Refresh);
        }
        let r = ui.add(egui::TextEdit::singleline(&mut pane.path_edit).desired_width(f32::INFINITY));
        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            out.push(PaneOutcome::GoTo(pane.path_edit.clone()));
        }
    });

    // Toolbar
    ui.horizontal(|ui| {
        if ui.button("New folder").clicked() {
            out.push(PaneOutcome::NewFolder);
        }
        if ui.button("New file").clicked() {
            out.push(PaneOutcome::NewFile);
        }
        if ui.add_enabled(!pane.selected.is_empty(), egui::Button::new("Delete")).clicked() {
            out.push(PaneOutcome::DeleteSelected);
        }
        if ui.add_enabled(pane.selected.len() == 1, egui::Button::new("Rename")).clicked() {
            if let Some(name) = pane.selected.iter().next().cloned() {
                out.push(PaneOutcome::Rename(name));
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(format!("{} items", pane.entries.len())).small().weak());
        });
    });

    ui.separator();

    if pane.remote && !connected {
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new("Not connected.\nDouble-click a connection on the left to begin.").weak());
        });
        return out;
    }
    if pane.loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Loading…");
        });
    }
    if let Some(err) = &pane.error {
        ui.colored_label(Color32::LIGHT_RED, err);
    }

    // The whole listing is a drop target.
    let frame = egui::Frame::NONE;
    let (_, dropped) = ui.dnd_drop_zone::<DragPayload, _>(frame, |ui| {
        render_table(ui, pane, side, &mut out);
    });
    if let Some(payload) = dropped {
        out.push(PaneOutcome::Drop((*payload).clone()));
    }

    out
}

fn render_table(ui: &mut egui::Ui, pane: &mut PaneState, side: Side, out: &mut Vec<PaneOutcome>) {
    let row_h = 22.0;
    let mut clicked_sort: Option<SortCol> = None;
    let mut click: Option<(String, bool)> = None; // (name, ctrl_held) applied after the table
    let ctrl = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);

    TableBuilder::new(ui)
        .striped(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::remainder().at_least(160.0).clip(true)) // name
        .column(Column::auto().at_least(80.0)) // size
        .column(Column::auto().at_least(130.0)) // modified
        .sense(egui::Sense::click())
        .header(20.0, |mut header| {
            header.col(|ui| {
                if ui.button(sort_label("Name", pane, SortCol::Name)).clicked() {
                    clicked_sort = Some(SortCol::Name);
                }
            });
            header.col(|ui| {
                if ui.button(sort_label("Size", pane, SortCol::Size)).clicked() {
                    clicked_sort = Some(SortCol::Size);
                }
            });
            header.col(|ui| {
                if ui.button(sort_label("Modified", pane, SortCol::Modified)).clicked() {
                    clicked_sort = Some(SortCol::Modified);
                }
            });
        })
        .body(|mut body| {
            // Iterate by reference (no per-frame clone). Selection changes are deferred
            // to `click` and applied after the table so `pane` is only borrowed immutably here.
            let remote = pane.remote;
            for entry in &pane.entries {
                let name = &entry.name;
                let is_sel = pane.selected.contains(name);
                body.row(row_h, |mut row| {
                    row.set_selected(is_sel);
                    row.col(|ui| {
                        let label = if entry.is_dir {
                            RichText::new(format!("{name}/")).color(ACCENT).strong()
                        } else if entry.symlink {
                            RichText::new(format!("{name}@")).italics()
                        } else {
                            RichText::new(name.clone())
                        };
                        let full = pane.full_path(name);
                        let payload = if is_sel {
                            DragPayload { side, remote: pane.remote, paths: pane.selected_paths() }
                        } else {
                            DragPayload { side, remote: pane.remote, paths: vec![full] }
                        };
                        let id = egui::Id::new((remote, "row", name));
                        ui.dnd_drag_source(id, payload, |ui| {
                            ui.label(label);
                        });
                    });
                    row.col(|ui| {
                        if entry.is_dir {
                            ui.label(RichText::new("—").weak());
                        } else {
                            ui.label(localfs::human_size(entry.size));
                        }
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(localfs::format_mtime(entry.mtime)).weak());
                    });

                    let resp = row.response();
                    if resp.clicked() {
                        click = Some((name.clone(), ctrl));
                    }
                    if resp.double_clicked() && entry.is_dir {
                        out.push(PaneOutcome::Enter(pane.full_path(name)));
                    }
                    resp.context_menu(|ui| {
                        if entry.is_dir && ui.button("Open").clicked() {
                            out.push(PaneOutcome::Enter(pane.full_path(name)));
                            ui.close();
                        }
                        let tlabel = if remote { "◀ Download" } else { "Upload ▶" };
                        if ui.button(tlabel).clicked() {
                            if !is_sel {
                                click = Some((name.clone(), false));
                            }
                            out.push(PaneOutcome::Transfer);
                            ui.close();
                        }
                        if !entry.is_dir && remote && ui.button("Edit (live)").clicked() {
                            out.push(PaneOutcome::Edit(name.clone()));
                            ui.close();
                        }
                        if ui.button("Rename").clicked() {
                            out.push(PaneOutcome::Rename(name.clone()));
                            ui.close();
                        }
                        if ui.button(RichText::new("Delete").color(Color32::LIGHT_RED)).clicked() {
                            if !is_sel {
                                click = Some((name.clone(), false));
                            }
                            out.push(PaneOutcome::DeleteSelected);
                            ui.close();
                        }
                    });
                });
            }
        });

    // Apply the deferred selection change now that the immutable borrow is released.
    if let Some((name, ctrl_held)) = click.take() {
        if ctrl_held {
            if pane.selected.contains(&name) {
                pane.selected.remove(&name);
            } else {
                pane.selected.insert(name);
            }
        } else {
            pane.selected.clear();
            pane.selected.insert(name);
        }
    }

    if let Some(col) = clicked_sort {
        if pane.sort_col == col {
            pane.sort_asc = !pane.sort_asc;
        } else {
            pane.sort_col = col;
            pane.sort_asc = true;
        }
        pane.sort();
    }
}

fn sort_label(base: &str, pane: &PaneState, col: SortCol) -> RichText {
    if pane.sort_col == col {
        let arrow = if pane.sort_asc { " ▲" } else { " ▼" };
        RichText::new(format!("{base}{arrow}")).strong()
    } else {
        RichText::new(base)
    }
}

fn reload_local(pane: &mut PaneState, show_hidden: bool) {
    match localfs::list_dir(std::path::Path::new(&pane.cwd), show_hidden) {
        Ok(e) => {
            pane.entries = e;
            pane.error = None;
            pane.selected.clear();
            pane.sort();
        }
        Err(e) => {
            pane.error = Some(format!("{e:#}"));
        }
    }
}

fn base_name(path: &str) -> String {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path).to_string()
}

fn parent_dir(cwd: &str, remote: bool) -> String {
    if remote {
        let t = cwd.trim_end_matches('/');
        match t.rfind('/') {
            Some(0) | None => "/".to_string(),
            Some(i) => t[..i].to_string(),
        }
    } else {
        PathBuf::from(cwd)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| cwd.to_string())
    }
}

fn editor_from(c: &Connection, vault: &Vault) -> ConnEditor {
    let (auth_kind, password, key_path, passphrase) = match &c.auth {
        AuthMethod::Password { password_enc } => {
            (0, vault.decrypt(password_enc).unwrap_or_default(), String::new(), String::new())
        }
        AuthMethod::Key { private_key, passphrase_enc } => (
            1,
            String::new(),
            private_key.clone(),
            passphrase_enc.as_ref().and_then(|b| vault.decrypt(b).ok()).unwrap_or_default(),
        ),
        AuthMethod::Agent => (2, String::new(), String::new(), String::new()),
    };
    ConnEditor {
        editing: Some(c.id),
        kind: c.kind,
        name: c.name.clone(),
        host: c.host.clone(),
        port: c.port.to_string(),
        username: c.username.clone(),
        domain: c.domain.clone(),
        insecure_tls: c.insecure_tls,
        group: c.group.clone(),
        auth_kind,
        password,
        key_path,
        passphrase,
        notes: c.notes.clone(),
    }
}

/// Load broad-coverage system fonts as fallbacks so symbol glyphs render
/// instead of showing as "tofu" boxes. Gracefully no-ops if the files are absent.
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates = [
        ("dejavu", "/usr/share/fonts/TTF/DejaVuSans.ttf"),
        ("dejavu", "/usr/share/fonts/dejavu/DejaVuSans.ttf"),
        ("symbols2", "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf"),
    ];
    let mut added: Vec<String> = Vec::new();
    for (name, path) in candidates {
        if added.iter().any(|n| n == name) {
            continue;
        }
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert(name.to_string(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
            added.push(name.to_string());
        }
    }
    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let entry = fonts.families.entry(fam).or_default();
        for name in &added {
            entry.push(name.clone()); // fallback: appended after the defaults
        }
    }
    ctx.set_fonts(fonts);
}

fn apply_theme(ctx: &egui::Context, dark: bool) {
    let mut visuals = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    if !dark {
        // A muted, low-glare gray light theme (softer than egui's near-white default).
        let panel = Color32::from_rgb(0xC6, 0xCA, 0xD0);
        let window = Color32::from_rgb(0xD1, 0xD5, 0xDA);
        let field = Color32::from_rgb(0xBB, 0xBF, 0xC6);
        let faint = Color32::from_rgb(0xC0, 0xC4, 0xCB);
        let text = Color32::from_rgb(0x2E, 0x34, 0x40); // dark slate (alacritty-ish fg)
        visuals.panel_fill = panel;
        visuals.window_fill = window;
        visuals.extreme_bg_color = field;
        visuals.faint_bg_color = faint;
        visuals.override_text_color = Some(text);
        visuals.widgets.noninteractive.bg_fill = panel;
        visuals.widgets.inactive.bg_fill = Color32::from_rgb(0xB6, 0xBA, 0xC1);
        visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(0xBE, 0xC2, 0xC9);
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(0xAE, 0xB3, 0xBB);
        visuals.widgets.active.bg_fill = Color32::from_rgb(0xA6, 0xAB, 0xB4);
        visuals.window_fill = window;
    }
    visuals.selection.bg_fill = if dark {
        Color32::from_rgb(0x6E, 0x38, 0x1E)
    } else {
        Color32::from_rgb(0xD8, 0x8A, 0x5A)
    };
    visuals.hyperlink_color = ACCENT;
    ctx.set_visuals(visuals);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(6.0, 5.0);
        s.spacing.button_padding = egui::vec2(7.0, 4.0);
    });
}
