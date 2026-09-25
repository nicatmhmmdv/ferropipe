//! Transfers: the gutter between the panes, the bottom drawer, and the
//! bookkeeping that feeds them.
//!
//! Pure helpers (labels, phase, rate, status text, pruning, toast copy) sit at
//! the top and are unit-tested; the `FerropipeApp` methods below them wire the
//! worker events, the drawer actions and the two gutter arrows.

use super::*;
use crate::localfs::human_size;
use crate::remote::TransferProgress;
use crate::theme::{self, GUTTER_W};
use crate::widgets::{self, TransferAction, TransferPhase, TransferView};
use egui::{
    Align, FontFamily, FontId, Frame, Label, Layout, Margin, Rangef, ScrollArea, Stroke, Ui,
};

/// Seconds a completed transfer lingers in the drawer before it is pruned.
const DONE_LINGER_SECS: f64 = 3.0;
/// No rate is shown until a transfer has run this long.
const RATE_MIN_ELAPSED_SECS: f64 = 0.5;
/// A worker error older than this is not attached to a failed transfer.
const ERROR_ATTACH_WINDOW_SECS: f64 = 2.0;
/// Space above the gutter arrows: pane header plus path bar.
const GUTTER_TOP_PAD: f32 = 72.0;
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
    pub upload: bool,
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

/// Everything needed to replay a transfer, plus the hosts it was aimed at.
/// A retry is only valid while both sides still talk to the same hosts:
/// the paths in `payload` are absolute paths on a specific machine.
#[derive(Clone)]
pub(super) struct RetryRequest {
    pub payload: DragPayload,
    pub dest: Side,
    /// Connected host per side (left, right) when the transfer started.
    pub hosts: [Option<Uuid>; 2],
}

/// What a caller knows about a transfer when it queues it.
pub(super) struct NewTransfer {
    pub worker: Side,
    pub label: String,
    pub dest: String,
    pub upload: bool,
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

/// A remote→remote staged transfer awaiting its upload leg.
pub(super) struct StageJob {
    pub temp_dir: PathBuf,
    pub names: Vec<String>,
    pub dest_dir: String,
    pub dest: Side,
    pub label: String,
    pub dest_label: String,
}

/// How many transfers sit in each phase, plus how many are still unfinished.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct PhaseCounts {
    pub running: usize,
    pub queued: usize,
    pub failed: usize,
    pub active: usize,
}

/// The direction a finished transfer is described with in toasts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verb {
    Upload,
    Download,
    /// Remote → remote, staged through a temp dir.
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

    fn noun(self) -> &'static str {
        match self {
            Verb::Upload => "Upload",
            Verb::Download => "Download",
            Verb::Copy => "Copy",
        }
    }
}

/// Snapshot of a finished transfer for its toast.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Finished {
    pub label: String,
    pub upload: bool,
    pub bytes_total: u64,
    pub elapsed: f64,
    /// `None` on success.
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

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
    matches!(context, "download" | "upload")
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

/// Toast text for a finished transfer: `(message, detail)`.
pub(super) fn result_toast(verb: Verb, done: &Finished) -> (String, Option<String>) {
    if let Some(error) = &done.error {
        let msg = format!("{} failed: {}", verb.noun(), widgets::short_error(error));
        return (msg, None);
    }
    let time = format!("{:.1} s", done.elapsed);
    let detail = if done.bytes_total > 0 {
        format!("{} · {time}", human_size(done.bytes_total))
    } else {
        time
    };
    (format!("{} {}", verb.past(), done.label), Some(detail))
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
            let arrow = if t.upload { "↑" } else { "↓" };
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

    /// Move `payload`'s paths into `dest`'s current directory.
    pub(super) fn start_transfer(&mut self, ctx: &egui::Context, payload: DragPayload, dest: Side) {
        if payload.side == dest || payload.paths.is_empty() {
            return;
        }
        if payload.remote != self.side_is_remote(payload.side) {
            self.toast(
                ctx,
                "The source pane changed — select the files again",
                true,
            );
            return;
        }
        if !self.side_usable(payload.side) || !self.side_usable(dest) {
            self.toast(ctx, "Both sides must be connected to transfer", true);
            return;
        }
        match (payload.remote, self.side_is_remote(dest)) {
            (true, false) => self.start_download(ctx, payload, dest),
            (false, true) => self.start_upload(ctx, payload, dest),
            (true, true) => self.start_staged(ctx, payload, dest),
            (false, false) => { /* local → local: not supported here */ }
        }
    }

    /// Row description for a request into `dest`'s current directory.
    fn transfer_spec(
        &self,
        payload: &DragPayload,
        dest: Side,
        worker: Side,
        upload: bool,
    ) -> NewTransfer {
        NewTransfer {
            worker,
            label: transfer_label(&payload.paths),
            dest: self.side_dest_label(dest, &self.side_pane(dest).cwd),
            upload,
            retry: Some(RetryRequest {
                payload: payload.clone(),
                dest,
                hosts: self.session_hosts(),
            }),
        }
    }

    /// The host connected on each side (left, right); `None` for a local pane.
    fn session_hosts(&self) -> [Option<Uuid>; 2] {
        [self.side_host(Side::Left), self.side_host(Side::Right)]
    }

    /// remote → local: the source worker downloads into the destination pane's directory.
    fn start_download(&mut self, ctx: &egui::Context, payload: DragPayload, dest: Side) {
        let local_dir = PathBuf::from(&self.side_pane(dest).cwd);
        let spec = self.transfer_spec(&payload, dest, payload.side, false);
        let id = self.push_transfer(ctx, spec);
        let cmd = Command::Download {
            id,
            remote: payload.paths,
            local_dir,
        };
        self.side_send(payload.side, cmd);
    }

    /// local → remote: the destination worker uploads into its pane's directory.
    fn start_upload(&mut self, ctx: &egui::Context, payload: DragPayload, dest: Side) {
        let remote_dir = self.side_pane(dest).cwd.clone();
        let spec = self.transfer_spec(&payload, dest, dest, true);
        let id = self.push_transfer(ctx, spec);
        let local = payload.paths.iter().map(PathBuf::from).collect();
        self.side_send(
            dest,
            Command::Upload {
                id,
                local,
                remote_dir,
            },
        );
    }

    /// remote → remote: download into a temp dir now, upload from it when that finishes.
    fn start_staged(&mut self, ctx: &egui::Context, payload: DragPayload, dest: Side) {
        let temp_dir = std::env::temp_dir().join(format!(
            "ferropipe-r2r-{}-{}",
            std::process::id(),
            self.next_id
        ));
        if let Err(e) = std::fs::create_dir_all(&temp_dir) {
            self.toast(ctx, format!("stage: {e:#}"), true);
            return;
        }
        let spec = self.transfer_spec(&payload, dest, payload.side, false);
        let job = StageJob {
            temp_dir: temp_dir.clone(),
            names: payload.paths.iter().map(|p| base_name(p)).collect(),
            dest_dir: self.side_pane(dest).cwd.clone(),
            dest,
            label: spec.label.clone(),
            dest_label: spec.dest.clone(),
        };
        let id = self.push_transfer(ctx, spec);
        let cmd = Command::Download {
            id,
            remote: payload.paths,
            local_dir: temp_dir,
        };
        self.side_send(payload.side, cmd);
        self.stage_download.insert(id, job);
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
        } else if let Some(temp) = self.stage_cleanup.remove(&id) {
            let _ = std::fs::remove_dir_all(&temp);
            self.toast_result(ctx, Verb::Copy, &done);
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
        Some(Finished {
            label: t.label.clone(),
            upload: t.upload,
            bytes_total: t.bytes_total,
            elapsed: now - t.started_at,
            error,
        })
    }

    /// The staged download leg ended: start the upload leg, or give up.
    fn finish_stage_download(&mut self, ctx: &egui::Context, job: StageJob, done: Finished) {
        if done.error.is_some() {
            let _ = std::fs::remove_dir_all(&job.temp_dir);
            self.toast_result(ctx, Verb::Copy, &done);
            return;
        }
        let spec = NewTransfer {
            worker: job.dest,
            label: job.label,
            dest: job.dest_label,
            upload: true,
            retry: None,
        };
        let id = self.push_transfer(ctx, spec);
        let local = job.names.iter().map(|n| job.temp_dir.join(n)).collect();
        let cmd = Command::Upload {
            id,
            local,
            remote_dir: job.dest_dir,
        };
        self.side_send(job.dest, cmd);
        self.stage_cleanup.insert(id, job.temp_dir);
    }

    /// A plain upload or download ended: relist a local destination and report.
    fn finish_plain(&mut self, ctx: &egui::Context, done: Finished) {
        if done.error.is_none() && !done.upload && !self.left_remote {
            reload_local(&mut self.local, self.store.settings.show_hidden);
        }
        let verb = if done.upload {
            Verb::Upload
        } else {
            Verb::Download
        };
        self.toast_result(ctx, verb, &done);
    }

    /// Toast the outcome and leave it in the status bar once the toast is gone.
    fn toast_result(&mut self, ctx: &egui::Context, verb: Verb, done: &Finished) {
        let (msg, detail) = result_toast(verb, done);
        self.status = msg.clone();
        self.toast_detail(ctx, msg, detail, done.error.is_some());
    }
}

// ---------------------------------------------------------------------------
// Gutter
// ---------------------------------------------------------------------------

impl FerropipeApp {
    /// The 44px column between the panes: two transfer arrows and a queue count.
    pub(super) fn render_gutter(&mut self, ctx: &egui::Context, ui: &mut Ui) {
        let p = theme::current(ui);
        egui::Panel::left("gutter")
            .exact_size(GUTTER_W)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(p.bg1))
            .show(ui, |ui| {
                let rect = ui.max_rect();
                let line = Stroke::new(1.0, p.line);
                ui.painter().vline(rect.left() + 0.5, rect.y_range(), line);
                ui.painter().vline(rect.right() - 0.5, rect.y_range(), line);
                ui.vertical_centered(|ui| {
                    ui.add_space(GUTTER_TOP_PAD);
                    self.gutter_arrows(ctx, ui);
                    self.gutter_queue_note(ui);
                });
            });
    }

    /// "→" sends the left pane's selection right; "←" the right pane's left.
    fn gutter_arrows(&mut self, ctx: &egui::Context, ui: &mut Ui) {
        let right_on = self.arrow_enabled(Side::Left);
        let left_on = self.arrow_enabled(Side::Right);
        let (tip_right, tip_left) = if self.left_remote {
            (
                "Copy selected to the right pane",
                "Copy selected to the left pane",
            )
        } else {
            ("Upload selected", "Download selected")
        };
        let right = widgets::gutter_arrow(ui, "→", right_on, right_on).on_hover_text(tip_right);
        if right.clicked() {
            let payload = self.selection_payload(Side::Left);
            self.start_transfer(ctx, payload, Side::Right);
        }
        ui.add_space(6.0);
        let left_primary = left_on && !right_on;
        let left = widgets::gutter_arrow(ui, "←", left_on, left_primary).on_hover_text(tip_left);
        if left.clicked() {
            let payload = self.selection_payload(Side::Right);
            self.start_transfer(ctx, payload, Side::Left);
        }
    }

    /// The arrow out of `src` is usable when it has a selection and both ends are reachable.
    fn arrow_enabled(&self, src: Side) -> bool {
        !self.side_pane(src).selected.is_empty()
            && self.side_usable(src)
            && self.side_usable(src.other())
    }

    /// The current selection of `src` as a drop payload.
    fn selection_payload(&self, src: Side) -> DragPayload {
        DragPayload {
            side: src,
            remote: self.side_is_remote(src),
            paths: self.side_pane(src).selected_paths(),
        }
    }

    /// "running / active" under the arrows plus a hairline down to the bottom.
    fn gutter_queue_note(&self, ui: &mut Ui) {
        let counts = count_phases(&phases_of(&self.transfers));
        if counts.active == 0 {
            return;
        }
        let p = theme::current(ui);
        ui.add_space(10.0);
        let text = RichText::new(format!("{} / {}", counts.running, counts.active))
            .font(FontId::new(10.5, FontFamily::Monospace))
            .color(p.text3);
        ui.add(Label::new(text).selectable(false));
        let rect = ui.max_rect();
        let top = ui.cursor().top() + 4.0;
        let range = Rangef::new(top, rect.bottom());
        ui.painter()
            .vline(rect.center().x, range, Stroke::new(1.0, p.line));
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
                        upload: t.upload,
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
                Some(req) if retry_allowed(req.hosts, self.session_hosts()) => {
                    self.start_transfer(ctx, req.payload, req.dest);
                }
                Some(_) => self.toast(
                    ctx,
                    "A pane changed host since this transfer — select the files again",
                    true,
                ),
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
