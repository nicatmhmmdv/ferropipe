//! Ferropipe GUI: connection sidebar + dual-pane SFTP file manager.
use crate::localfs;
use crate::model::{AuthMethod, Connection, ConnectionKind, FileEntry};
use crate::remote::{self, Command, Event, WorkerHandle};
use crate::store::Store;
use crate::vault::Vault;
use eframe::egui;
use egui::RichText;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;
use uuid::Uuid;

mod chrome;
mod console;
mod dialogs;
mod editor;
mod fkeys;
mod live_edit;
mod pane_actions;
mod panes;
mod session;
mod sidebar;
mod ssh_panel;
mod transfers;

use chrome::Toast;
use dialogs::{Prompt, PromptKind};
use editor::ConnEditor;
use live_edit::EditWatch;
use ssh_panel::SshPanel;
use transfers::{NewTransfer, StageJob, TransferState};

/// Which pane a drag/action came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    /// The opposite pane.
    pub fn other(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        }
    }
}

/// Payload carried during a drag-and-drop between panes.
#[derive(Clone)]
pub struct DragPayload {
    pub side: Side,
    pub remote: bool, // is the source pane currently browsing a remote?
    pub paths: Vec<String>,
}

/// Payload carried while a host is dragged from the sidebar onto a pane.
#[derive(Clone, Debug)]
pub struct HostPayload {
    pub id: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq)]
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
    /// Text of the path field while the user is typing a path; `None` shows breadcrumbs.
    path_editing: Option<String>,
    /// Home directory used to collapse the breadcrumb prefix to "~".
    home: Option<String>,
    sort_col: SortCol,
    sort_asc: bool,
}

impl PaneState {
    fn new(remote: bool, cwd: String) -> Self {
        PaneState {
            remote,
            cwd,
            entries: Vec::new(),
            selected: HashSet::new(),
            loading: false,
            error: None,
            path_editing: None,
            home: (!remote).then(local_home),
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
    /// A host was dropped onto the pane: browse it there.
    OpenHost(Uuid),
}

pub struct FerropipeApp {
    store: Store,
    vault: Vault,
    store_path: PathBuf,
    worker: WorkerHandle,
    worker_join: Option<std::thread::JoinHandle<()>>,
    events: mpsc::Receiver<Event>,

    connected: Option<Uuid>,
    /// When the right pane's session came up; drives the status bar's uptime.
    connected_at: Option<f64>,
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
    /// Most recent transfer error per worker `(time, message)`, indexed by `Side`;
    /// attached to the `TransferDone { ok: false }` that follows it.
    last_transfer_error: [Option<(f64, String)>; 2],

    local: PaneState,
    remote: PaneState,
    /// The pane the function keys act on; a click inside a pane moves it.
    focused: Side,

    transfers: Vec<TransferState>,
    next_id: u64,

    editor: Option<ConnEditor>,
    prompt: Option<Prompt>,
    confirm_delete: Option<(Side, Vec<String>)>,

    toasts: Vec<Toast>,
    search: String,
    selected_conn: Option<Uuid>,
    /// Last connect failure per host; drawn as a red dot in the sidebar until the next attempt.
    host_errors: HashMap<Uuid, String>,

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

    // SSH-transfer panel for a Windows host (enable SSH → SFTP → disable SSH).
    ssh_panel: Option<SshPanel>,
}

/// A background worker and the channel ends the app keeps for it.
struct Worker {
    handle: WorkerHandle,
    join: std::thread::JoinHandle<()>,
    events: mpsc::Receiver<Event>,
}

/// Spawn a remote worker that repaints the UI whenever it reports an event.
fn spawn_worker(ctx: &egui::Context) -> Worker {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
    let (ev_tx, ev_rx) = mpsc::channel::<Event>();
    let ctx = ctx.clone();
    let join = remote::spawn(cmd_rx, ev_tx, move || ctx.request_repaint());
    Worker {
        handle: WorkerHandle { tx: cmd_tx },
        join,
        events: ev_rx,
    }
}

/// The local pane, opened at the last-used directory (or home) and listed.
fn local_pane(store: &Store) -> PaneState {
    let start = store
        .settings
        .last_local_dir
        .clone()
        .unwrap_or_else(local_home);
    let mut local = PaneState::new(false, start);
    reload_local(&mut local, store.settings.show_hidden);
    local
}

impl FerropipeApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        store: Store,
        vault: Vault,
        store_path: PathBuf,
    ) -> Self {
        crate::theme::install_fonts(&cc.egui_ctx);
        crate::theme::apply(&cc.egui_ctx, store.settings.dark_mode);
        egui_extras::install_image_loaders(&cc.egui_ctx);
        // The right pane's worker, then a second one that backs the left pane
        // when it hosts a remote (remote-to-remote).
        let workers = [spawn_worker(&cc.egui_ctx), spawn_worker(&cc.egui_ctx)];
        Self::assemble(store, vault, store_path, workers)
    }

    /// Every field's starting value: the given parts, everything else idle or empty.
    fn assemble(store: Store, vault: Vault, store_path: PathBuf, workers: [Worker; 2]) -> Self {
        let [right, left] = workers;
        FerropipeApp {
            local: local_pane(&store), // reads `store` before it moves in below
            store,
            vault,
            store_path,
            worker: right.handle,
            worker_join: Some(right.join),
            events: right.events,
            connected: None,
            connected_at: None,
            connecting: false,
            pending_connect: None,
            status: String::new(),
            left_remote: false,
            lworker: left.handle,
            lworker_join: Some(left.join),
            levents: left.events,
            lconnected: None,
            lconnecting: false,
            lpending: None,
            lkind: "",
            left_saved_cwd: String::new(),
            stage_download: HashMap::new(),
            stage_cleanup: HashMap::new(),
            last_transfer_error: [None, None],
            remote: PaneState::new(true, "/".into()),
            focused: Side::Left,
            transfers: Vec::new(),
            next_id: 1,
            editor: None,
            prompt: None,
            confirm_delete: None,
            toasts: Vec::new(),
            search: String::new(),
            selected_conn: None,
            host_errors: HashMap::new(),
            console: Vec::new(),
            console_input: String::new(),
            console_open: false,
            exec_running: false,
            active_kind: "",
            pending_edit: HashMap::new(),
            edits: Vec::new(),
            edit_poll: 0.0,
            native_rdp: crate::native_rdp::NativeRdpManager::new(),
            ssh_panel: None,
        }
    }

    /// Display name of a saved host.
    fn conn_name(&self, id: Uuid) -> String {
        self.store
            .connections
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "Remote".into())
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
        self.toast(
            ctx,
            format!("Imported {added} host(s) from ~/.ssh/config"),
            added == 0,
        );
    }
}

impl eframe::App for FerropipeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_events(&ctx);
        // Function keys go first so a handled press is consumed before any
        // widget of this frame sees it.
        self.handle_fkeys(&ctx);

        // Draw any open native RDP session windows.
        self.native_rdp.show(&ctx);

        self.render_top_bar(&ctx, ui);
        // Bottom panels stack upward in this order: function-key bar (when
        // shown), status bar, drawer, console.
        self.render_fkey_bar(&ctx, ui);
        self.render_status_bar(&ctx, ui);
        self.render_transfer_drawer(&ctx, ui);
        self.render_console(ui);
        // Toasts float just above whatever those bottom panels take.
        let bottom_inset = ctx.content_rect().bottom() - ui.available_rect_before_wrap().bottom();
        self.render_workspace(&ctx, ui);

        self.render_dialogs(&ctx);
        self.render_ssh_panel(&ctx);
        self.render_toasts(&ctx, bottom_inset);
        self.tick(&ctx);
    }
}

impl FerropipeApp {
    /// Hosts sidebar on the left; the two file panes fill the rest.
    fn render_workspace(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        egui::Panel::left("connections")
            .resizable(true)
            .default_size(crate::theme::SIDEBAR_W)
            .size_range(180.0..=420.0)
            .show(ui, |ui| self.render_sidebar(ctx, ui));

        // Panes draw their own headers flush to the edges, so no panel margins here.
        let pane_frame = egui::Frame::new().fill(crate::theme::current(ui).bg1);
        egui::CentralPanel::default()
            .frame(pane_frame)
            .show(ui, |ui| {
                // The gutter sits between the panes; split what is left of it.
                let avail = ui.available_width() - crate::theme::GUTTER_W;
                egui::Panel::left("localpane")
                    .resizable(true)
                    .default_size(avail * 0.5)
                    .size_range(egui::Rangef::new(260.0, (avail - 220.0).max(300.0)))
                    .frame(pane_frame)
                    .show(ui, |ui| self.render_side(ctx, ui, Side::Left));
                self.render_gutter(ctx, ui);
                egui::CentralPanel::default()
                    .frame(pane_frame)
                    .show(ui, |ui| self.render_side(ctx, ui, Side::Right));
            });
    }

    /// Per-frame housekeeping: poll live edits and keep animating while busy.
    fn tick(&mut self, ctx: &egui::Context) {
        // Poll edited files for changes (auto re-upload on save).
        if !self.edits.is_empty() {
            let now = ctx.input(|i| i.time);
            if now - self.edit_poll > 1.0 {
                self.edit_poll = now;
                self.poll_edits(ctx);
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(1000));
        }
        if self.transfers.iter().any(|t| !t.done) || self.connecting {
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        } else if self.transfers.iter().any(|t| !t.error) {
            // Let completed rows leave the drawer on time.
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
        if self.connected_at.is_some() {
            // Keep the status bar's "connected N min" honest.
            ctx.request_repaint_after(std::time::Duration::from_secs(60));
        }
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
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_string()
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

/// The local home directory as a display string.
fn local_home() -> String {
    localfs::home_dir().to_string_lossy().into_owned()
}
