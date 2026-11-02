//! Transfers: the gutter between the panes, the bottom drawer, and the
//! bookkeeping that feeds them.
//!
//! Pure helpers (labels, phase, rate, status text, pruning, toast copy) sit at
//! the top and are unit-tested; the `FerropipeApp` methods below them wire the
//! worker events and the drawer actions. The gutter arrows live in `gutter`,
//! local → local copies in `super::local_copy`, remote → remote staging in
//! `staged`.

use super::*;
use crate::localfs::human_size;
use crate::remote::TransferProgress;
use crate::theme;
use crate::widgets::{self, Direction, TransferAction, TransferPhase, TransferView};
use egui::{Align, FontFamily, FontId, Frame, Label, Layout, Margin, ScrollArea, Ui};

mod gutter;
mod staged;

pub(super) use staged::StageJob;

/// Seconds a completed transfer lingers in the drawer before it is pruned.
const DONE_LINGER_SECS: f64 = 3.0;
/// No rate is shown until a transfer has run this long.
const RATE_MIN_ELAPSED_SECS: f64 = 0.5;
/// A worker error older than this is not attached to a failed transfer.
const ERROR_ATTACH_WINDOW_SECS: f64 = 2.0;
/// Rows visible in the drawer before it scrolls.
const DRAWER_VISIBLE_ROWS: f32 = 3.0;
/// Pitch of one drawer row.
const DRAWER_ROW_H: f32 = 26.0;
/// Gap between rows: `transfer_row` is 24px tall, so this makes the pitch.
const DRAWER_ROW_GAP: f32 = 2.0;
/// Drawer frame margin (x, y).
const DRAWER_MARGIN: (i8, i8) = (10, 6);

/// One transfer known to the UI: a worker job plus what the drawer shows for it.
pub(super) struct TransferState {
    pub id: u64,
    /// The worker running it: the source side for downloads, the destination for uploads.
    pub worker: Side,
    pub label: String,
    /// Display target: "host:dir" or a local directory with `~`.
    pub dest: String,
    /// The worker is the destination side (uploads and the staged upload leg).
    pub upload: bool,
    /// How the finished transfer is described.
    pub verb: Verb,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub file_index: usize,
    pub file_count: usize,
    pub done: bool,
    pub error: bool,
    pub error_msg: Option<String>,
    /// The request to replay on Retry; `None` for the staged upload leg.
    pub retry: Option<RetryRequest>,
    pub started_at: f64,
    pub finished_at: Option<f64>,
}

/// Everything needed to replay a transfer, plus where it was aimed. A retry
/// is only valid while both sides still talk to the same hosts: the paths in
/// `payload` are absolute paths on a specific machine. The destination
/// directory is pinned when the transfer is first issued, so a replay (Retry,
/// or a conflict answer) writes where the user was shown the conflicts even
/// if the pane has moved on since.
#[derive(Clone)]
pub(super) struct RetryRequest {
    pub payload: DragPayload,
    pub dest: Side,
    /// `dest`'s current directory when the transfer was first issued.
    pub dest_dir: String,
    /// Connected host per side (left, right) when the transfer started.
    pub hosts: [Option<Uuid>; 2],
}

/// What a caller knows about a transfer when it queues it.
pub(super) struct NewTransfer {
    pub worker: Side,
    pub label: String,
    pub dest: String,
    pub upload: bool,
    pub verb: Verb,
    pub retry: Option<RetryRequest>,
}

impl TransferState {
    fn new(id: u64, spec: NewTransfer, started_at: f64) -> Self {
        TransferState {
            id,
            worker: spec.worker,
            label: spec.label,
            dest: spec.dest,
            upload: spec.upload,
            verb: spec.verb,
            bytes_done: 0,
            bytes_total: 0,
            file_index: 0,
            file_count: 0,
            done: false,
            error: false,
            error_msg: None,
            retry: spec.retry,
            started_at,
            finished_at: None,
        }
    }
}

/// How many transfers sit in each phase, plus how many are still unfinished.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct PhaseCounts {
    pub running: usize,
    pub queued: usize,
    pub failed: usize,
    pub active: usize,
}

/// The direction a transfer is described with in menus, tooltips and toasts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verb {
    Upload,
    Download,
    /// Between two remotes (staged through a temp dir) or two local panes.
    Copy,
}

impl Verb {
    fn past(self) -> &'static str {
        match self {
            Verb::Upload => "Uploaded",
            Verb::Download => "Downloaded",
            Verb::Copy => "Copied",
        }
    }

    pub(super) fn noun(self) -> &'static str {
        match self {
            Verb::Upload => "Upload",
            Verb::Download => "Download",
            Verb::Copy => "Copy",
        }
    }
}

/// The drawer direction (and glyph) a verb is drawn with.
pub(super) fn direction_of(verb: Verb) -> Direction {
    match verb {
        Verb::Upload => Direction::Upload,
        Verb::Download => Direction::Download,
        Verb::Copy => Direction::Copy,
    }
}

/// Snapshot of a finished transfer for its toast.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Finished {
    pub label: String,
    pub verb: Verb,
    /// The pane the files went into.
    pub dest: Side,
    pub bytes_total: u64,
    pub elapsed: f64,
    /// `None` on success.
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// The verb for moving files from a pane in `src_remote` mode to one in `dest_remote` mode.
pub(super) fn transfer_verb(src_remote: bool, dest_remote: bool) -> Verb {
    match (src_remote, dest_remote) {
        (true, false) => Verb::Download,
        (false, true) => Verb::Upload,
        _ => Verb::Copy,
    }
}

/// Row label for a request: the base name of a single path, else "N items".
pub(super) fn transfer_label(paths: &[String]) -> String {
    match paths {
        [one] => base_name(one),
        _ => format!("{} items", paths.len()),
    }
}

/// Display target: "host:dir" for a remote, else the directory alone; a
/// leading `home` collapses to "~" either way.
pub(super) fn dest_label(host: Option<&str>, dir: &str, home: Option<&str>) -> String {
    let dir = collapse_home(dir, home);
    match host {
        Some(host) => format!("{host}:{dir}"),
        None => dir,
    }
}

/// `dir` with a leading `home` replaced by "~"; trailing slashes are ignored.
fn collapse_home(dir: &str, home: Option<&str>) -> String {
    let dir = trim_dir(dir);
    let home = home.map(trim_dir).filter(|h| !h.is_empty() && *h != "/");
    let Some(home) = home else {
        return dir.to_string();
    };
    match dir.strip_prefix(home) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => dir.to_string(),
    }
}

/// Strip trailing slashes, keeping a lone "/" intact.
fn trim_dir(path: &str) -> &str {
    if path == "/" {
        path
    } else {
        path.trim_end_matches('/')
    }
}

/// Phase of one transfer given whether an older unfinished job shares its worker.
pub(super) fn phase_of(t: &TransferState, has_older_active_on_same_worker: bool) -> TransferPhase {
    if t.error {
        TransferPhase::Failed
    } else if t.done {
        TransferPhase::Done
    } else if t.bytes_done == 0 && t.file_index == 0 && has_older_active_on_same_worker {
        TransferPhase::Queued
    } else {
        TransferPhase::Running
    }
}

/// Phase of every transfer, in order: a job is queued behind any older
/// unfinished one on the same worker until it reports progress.
pub(super) fn phases_of(transfers: &[TransferState]) -> Vec<TransferPhase> {
    transfers
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let blocked = transfers[..i]
                .iter()
                .any(|older| older.worker == t.worker && !older.done);
            phase_of(t, blocked)
        })
        .collect()
}

/// Bytes per second, once a transfer has run long enough to mean anything.
pub(super) fn rate_of(bytes_done: u64, started_at: f64, now: f64) -> Option<f64> {
    let elapsed = now - started_at;
    (elapsed >= RATE_MIN_ELAPSED_SECS && bytes_done > 0).then(|| bytes_done as f64 / elapsed)
}

/// The status column of a drawer row.
pub(super) fn status_text(t: &TransferState, phase: TransferPhase, now: f64) -> String {
    match phase {
        TransferPhase::Running => widgets::progress_text(
            t.file_index.max(1),
            t.file_count,
            t.bytes_done,
            t.bytes_total,
            rate_of(t.bytes_done, t.started_at, now),
        ),
        TransferPhase::Queued => "queued".into(),
        TransferPhase::Done => {
            let elapsed = t.finished_at.unwrap_or(now) - t.started_at;
            format!("done · {elapsed:.1} s")
        }
        TransferPhase::Failed => widgets::short_error(t.error_msg.as_deref().unwrap_or("failed")),
    }
}

/// Progress fraction in `0.0..=1.0`; finished and failed rows are full.
pub(super) fn frac_of(t: &TransferState) -> f32 {
    if t.error || t.done {
        1.0
    } else if t.bytes_total > 0 {
        (t.bytes_done as f64 / t.bytes_total as f64) as f32
    } else {
        0.0
    }
}

/// Completed transfers leave the drawer a few seconds after finishing; failures stay.
pub(super) fn should_prune(t: &TransferState, now: f64) -> bool {
    !t.error
        && t.done
        && t.finished_at
            .is_some_and(|finished| now - finished >= DONE_LINGER_SECS)
}

/// Tally the phases; `active` counts everything not yet done or failed.
pub(super) fn count_phases(phases: &[TransferPhase]) -> PhaseCounts {
    phases.iter().fold(PhaseCounts::default(), |mut c, phase| {
        match phase {
            TransferPhase::Running => c.running += 1,
            TransferPhase::Queued => c.queued += 1,
            TransferPhase::Failed => c.failed += 1,
            TransferPhase::Done => {}
        }
        if matches!(phase, TransferPhase::Running | TransferPhase::Queued) {
            c.active += 1;
        }
        c
    })
}

/// "N running", "M queued", "K failed" as separate parts with zeros omitted;
/// the flag marks the failed part so it can be colored.
pub(super) fn summary_parts(counts: PhaseCounts) -> Vec<(String, bool)> {
    [
        (counts.running, "running", false),
        (counts.queued, "queued", false),
        (counts.failed, "failed", true),
    ]
    .into_iter()
    .filter(|(n, _, _)| *n > 0)
    .map(|(n, word, failed)| (format!("{n} {word}"), failed))
    .collect()
}

/// Whether a worker error context belongs to a transfer rather than a listing or file op.
pub(super) fn is_transfer_context(context: &str) -> bool {
    matches!(context, "download" | "upload" | "copy")
}

/// The remembered worker error, if it is recent enough to explain a failure now.
pub(super) fn attach_error(last: Option<(f64, String)>, now: f64) -> Option<String> {
    let (at, msg) = last?;
    (now - at <= ERROR_ATTACH_WINDOW_SECS).then_some(msg)
}

/// Whether a [`RetryRequest`] aimed at `req_hosts` may replay against the `current` hosts.
pub(super) fn retry_allowed(req_hosts: [Option<Uuid>; 2], current: [Option<Uuid>; 2]) -> bool {
    req_hosts == current
}

/// The worker command for transfer `id`: `req.payload`'s paths into the
/// directory pinned in `req.dest_dir`, with `on_conflict`. Every issue of a
/// request — the first and each replay — builds its command here, so a replay
/// never reads the pane again. `verb` is the request's [`transfer_verb`]; a
/// remote → remote copy is `Verb::Copy` too but never comes here (it is
/// staged in two legs, see `staged`), so `Copy` means local → local.
pub(super) fn transfer_command(
    id: u64,
    req: &RetryRequest,
    verb: Verb,
    on_conflict: OnConflict,
) -> Command {
    let local_paths = || req.payload.paths.iter().map(PathBuf::from).collect();
    match verb {
        Verb::Download => Command::Download {
            id,
            remote: req.payload.paths.clone(),
            local_dir: PathBuf::from(&req.dest_dir),
            on_conflict,
        },
        Verb::Upload => Command::Upload {
            id,
            local: local_paths(),
            remote_dir: req.dest_dir.clone(),
            on_conflict,
        },
        Verb::Copy => Command::CopyLocal {
            id,
            paths: local_paths(),
            dest_dir: PathBuf::from(&req.dest_dir),
            on_conflict,
        },
    }
}

/// Toast text for a finished transfer: `(message, detail)`.
pub(super) fn result_toast(done: &Finished) -> (String, Option<String>) {
    if let Some(error) = &done.error {
        let msg = format!(
            "{} failed: {}",
            done.verb.noun(),
            widgets::short_error(error)
        );
        return (msg, None);
    }
    let time = format!("{:.1} s", done.elapsed);
    let detail = if done.bytes_total > 0 {
        format!("{} · {time}", human_size(done.bytes_total))
    } else {
        time
    };
    (format!("{} {}", done.verb.past(), done.label), Some(detail))
}

// ---------------------------------------------------------------------------
// Bookkeeping
// ---------------------------------------------------------------------------

impl FerropipeApp {
    /// Queue a transfer row; the caller sends the matching worker command.
    pub(super) fn push_transfer(&mut self, ctx: &egui::Context, spec: NewTransfer) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let now = ctx.input(|i| i.time);
        self.transfers.push(TransferState::new(id, spec, now));
        id
    }

    /// Progress for one row; the file in flight goes to the status bar's message slot.
    pub(super) fn on_progress(&mut self, p: TransferProgress) {
        let Some(t) = self.transfers.iter_mut().find(|t| t.id == p.id) else {
            return;
        };
        t.bytes_done = p.bytes_done;
        t.bytes_total = p.bytes_total;
        t.file_index = p.file_index;
        t.file_count = p.file_count;
        if !p.current_file.is_empty() {
            let arrow = direction_of(t.verb).glyph();
            self.status = format!("{arrow} {}", p.current_file);
        }
    }

    /// Name of the host on `side`, if it is a remote.
    fn side_host_name(&self, side: Side) -> Option<String> {
        if !self.side_is_remote(side) {
            return None;
        }
        Some(
            self.side_host(side)
                .map(|id| self.conn_name(id))
                .unwrap_or_else(|| "remote".into()),
        )
    }

    /// Display target for `dir` on `side`.
    pub(super) fn side_dest_label(&self, side: Side, dir: &str) -> String {
        let host = self.side_host_name(side);
        dest_label(host.as_deref(), dir, self.side_pane(side).home.as_deref())
    }

    /// A side is usable as a transfer endpoint when local or connected.
    pub(super) fn side_usable(&self, side: Side) -> bool {
        !self.side_is_remote(side) || self.side_connected(side)
    }

    /// Remember a worker error so the transfer that fails next can show it.
    pub(super) fn note_transfer_error(&mut self, side: Side, now: f64, message: String) {
        self.last_transfer_error[side as usize] = Some((now, message));
    }

    fn take_transfer_error(&mut self, side: Side, now: f64) -> Option<String> {
        attach_error(self.last_transfer_error[side as usize].take(), now)
    }

    // -- starting ---------------------------------------------------------

    /// Move `payload`'s paths into `dest`'s current directory, asking before
    /// anything already there is written over.
    pub(super) fn start_transfer(&mut self, ctx: &egui::Context, payload: DragPayload, dest: Side) {
        self.start_transfer_with(ctx, payload, dest, OnConflict::Ask);
    }

    /// Move `payload`'s paths into `dest`'s current directory, treating files
    /// already there per `on_conflict`. The directory is pinned now: every
    /// replay of this request goes to the same place.
    pub(super) fn start_transfer_with(
        &mut self,
        ctx: &egui::Context,
        payload: DragPayload,
        dest: Side,
        on_conflict: OnConflict,
    ) {
        let req = RetryRequest {
            payload,
            dest,
            dest_dir: self.side_pane(dest).cwd.clone(),
            hosts: self.session_hosts(),
        };
        self.issue_request(ctx, req, on_conflict);
    }

    /// Re-run a remembered request with `on_conflict`, as long as both panes
    /// still show the hosts it was aimed at. It writes into the directory
    /// pinned in the request, not wherever the pane is now.
    pub(super) fn replay_request(
        &mut self,
        ctx: &egui::Context,
        req: RetryRequest,
        on_conflict: OnConflict,
    ) {
        if retry_allowed(req.hosts, self.session_hosts()) {
            self.issue_request(ctx, req, on_conflict);
        } else {
            self.toast(
                ctx,
                "A pane changed host since this transfer — select the files again",
                true,
            );
        }
    }

    /// Issue `req`, first time or replay: check both sides are still fit for
    /// it, queue the row and send the one command that moves the files into
    /// `req.dest_dir`. A remote → remote copy is staged instead.
    fn issue_request(&mut self, ctx: &egui::Context, req: RetryRequest, on_conflict: OnConflict) {
        let (src, dest) = (req.payload.side, req.dest);
        if src == dest || req.payload.paths.is_empty() {
            return;
        }
        if req.payload.remote != self.side_is_remote(src) {
            self.toast(
                ctx,
                "The source pane changed — select the files again",
                true,
            );
            return;
        }
        if !self.side_usable(src) || !self.side_usable(dest) {
            self.toast(ctx, "Both sides must be connected to transfer", true);
            return;
        }
        let dest_remote = self.side_is_remote(dest);
        if req.payload.remote && dest_remote {
            self.start_staged(ctx, req, on_conflict);
            return;
        }
        let verb = transfer_verb(req.payload.remote, dest_remote);
        // A remote source is read by its own worker; otherwise the destination's writes.
        let worker = if req.payload.remote { src } else { dest };
        let spec = self.transfer_spec(&req, worker, verb);
        let id = self.push_transfer(ctx, spec);
        let cmd = transfer_command(id, &req, verb, on_conflict);
        self.side_send(worker, cmd);
    }

    /// Row description for `req`, run by `worker` (the destination side counts
    /// as an upload); the row keeps `req` so it can be replayed as issued.
    pub(super) fn transfer_spec(
        &self,
        req: &RetryRequest,
        worker: Side,
        verb: Verb,
    ) -> NewTransfer {
        NewTransfer {
            worker,
            label: transfer_label(&req.payload.paths),
            dest: self.side_dest_label(req.dest, &req.dest_dir),
            upload: worker == req.dest,
            verb,
            retry: Some(req.clone()),
        }
    }

    /// The host connected on each side (left, right); `None` for a local pane.
    fn session_hosts(&self) -> [Option<Uuid>; 2] {
        [self.side_host(Side::Left), self.side_host(Side::Right)]
    }

    // -- finishing --------------------------------------------------------

    /// A worker on `side` reported the end of transfer `id`.
    pub(super) fn on_transfer_done(&mut self, ctx: &egui::Context, side: Side, id: u64, ok: bool) {
        if self.pending_edit.contains_key(&id) {
            self.finish_edit_download(ctx, id, ok);
            return;
        }
        let now = ctx.input(|i| i.time);
        let error = (!ok).then(|| {
            self.take_transfer_error(side, now)
                .unwrap_or_else(|| "failed".into())
        });
        let Some(done) = self.mark_finished(id, now, error) else {
            return;
        };
        if let Some(job) = self.stage_download.remove(&id) {
            self.finish_stage_download(ctx, job, done);
        } else if let Some(job) = self.stage_cleanup.remove(&id) {
            let _ = std::fs::remove_dir_all(&job.temp_dir);
            self.toast_result(ctx, &done);
        } else {
            self.finish_plain(ctx, done);
        }
    }

    /// Flag transfer `id` finished and snapshot it for the toast.
    fn mark_finished(&mut self, id: u64, now: f64, error: Option<String>) -> Option<Finished> {
        let t = self.transfers.iter_mut().find(|t| t.id == id)?;
        t.done = true;
        t.error = error.is_some();
        t.error_msg = error.clone();
        t.finished_at = Some(now);
        // The worker is the destination for uploads and the source otherwise.
        let dest = if t.upload { t.worker } else { t.worker.other() };
        Some(Finished {
            label: t.label.clone(),
            verb: t.verb,
            dest,
            bytes_total: t.bytes_total,
            elapsed: now - t.started_at,
            error,
        })
    }

    /// An unstaged transfer ended: relist a local destination and report.
    /// (A remote destination is relisted by its worker's `OpDone`.)
    fn finish_plain(&mut self, ctx: &egui::Context, done: Finished) {
        if done.error.is_none() {
            self.relist_local_dest(done.dest);
        }
        self.toast_result(ctx, &done);
    }

    /// Toast the outcome and leave it in the status bar once the toast is gone.
    fn toast_result(&mut self, ctx: &egui::Context, done: &Finished) {
        let (msg, detail) = result_toast(done);
        self.status = msg.clone();
        self.toast_detail(ctx, msg, detail, done.error.is_some());
    }
}

// ---------------------------------------------------------------------------
// Drawer
// ---------------------------------------------------------------------------

impl FerropipeApp {
    /// Bottom drawer listing every transfer; absent while there are none.
    pub(super) fn render_transfer_drawer(&mut self, ctx: &egui::Context, ui: &mut Ui) {
        let now = ctx.input(|i| i.time);
        self.transfers.retain(|t| !should_prune(t, now));
        if self.transfers.is_empty() {
            return;
        }
        let p = theme::current(ui);
        let phases = phases_of(&self.transfers);
        let counts = count_phases(&phases);
        let (mut clear, mut actions) = (false, Vec::new());
        let margin = Margin::symmetric(DRAWER_MARGIN.0, DRAWER_MARGIN.1);
        egui::Panel::bottom("transfers")
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(p.bg2).inner_margin(margin))
            .show(ui, |ui| {
                chrome::top_hairline(ui, DRAWER_MARGIN);
                clear = drawer_header(ui, counts);
                actions = self.drawer_rows(ui, now, &phases);
            });
        self.apply_drawer_actions(ctx, actions, clear);
    }

    /// One `transfer_row` per transfer inside a three-row scroll area.
    fn drawer_rows(
        &self,
        ui: &mut Ui,
        now: f64,
        phases: &[TransferPhase],
    ) -> Vec<(u64, TransferAction)> {
        let mut actions = Vec::new();
        ScrollArea::vertical()
            .max_height(DRAWER_VISIBLE_ROWS * DRAWER_ROW_H)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = DRAWER_ROW_GAP;
                for (t, &phase) in self.transfers.iter().zip(phases) {
                    let status = status_text(t, phase, now);
                    let view = TransferView {
                        direction: direction_of(t.verb),
                        label: &t.label,
                        dest: &t.dest,
                        frac: frac_of(t),
                        status: &status,
                        phase,
                    };
                    if let Some(action) = widgets::transfer_row(ui, &view) {
                        actions.push((t.id, action));
                    }
                }
            });
        actions
    }

    /// Apply what the drawer's buttons asked for, now that `transfers` is free again.
    fn apply_drawer_actions(
        &mut self,
        ctx: &egui::Context,
        actions: Vec<(u64, TransferAction)>,
        clear: bool,
    ) {
        if clear {
            self.transfers.retain(|t| !t.done);
        }
        for (id, action) in actions {
            let Some(pos) = self.transfers.iter().position(|t| t.id == id) else {
                continue;
            };
            let removed = self.transfers.remove(pos);
            if action != TransferAction::Retry {
                continue;
            }
            match removed.retry {
                Some(req) => self.replay_request(ctx, req, OnConflict::Ask),
                None => self.toast(ctx, "This step can't be retried", true),
            }
        }
    }
}

/// "TRANSFERS", the phase summary and the Clear finished button; true when clicked.
fn drawer_header(ui: &mut Ui, counts: PhaseCounts) -> bool {
    let p = theme::current(ui);
    ui.horizontal(|ui| {
        widgets::section_label(ui, "Transfers");
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(8.0);
        for (i, (part, failed)) in summary_parts(counts).into_iter().enumerate() {
            let text = if i > 0 { format!(" · {part}") } else { part };
            let color = if failed { p.err } else { p.text3 };
            let text = RichText::new(text)
                .font(FontId::new(11.0, FontFamily::Monospace))
                .color(color);
            ui.add(Label::new(text).selectable(false));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.small_button("Clear finished").clicked()
        })
        .inner
    })
    .inner
}

#[cfg(test)]
mod tests;
