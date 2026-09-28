//! Window chrome: the top bar, the one-line status bar and the toast stack.

use super::*;
use crate::diag::{self, diag_error, diag_info};
use crate::theme;
use crate::widgets;
use egui::{
    Align, Align2, Area, Color32, FontFamily, FontId, Frame, Label, Layout, Margin, Stroke, Ui,
    vec2,
};
use std::time::Duration;

/// How long a toast stays up.
const TOAST_SECS: f64 = 4.0;
/// Most toasts shown at once, newest first.
const TOAST_MAX_SHOWN: usize = 5;
/// Gap between the toast stack and the window / panel edges.
const TOAST_INSET: f32 = 12.0;
/// Status bar frame margin (x, y).
const STATUS_MARGIN: (i8, i8) = (10, 4);
/// The status bar's copy-diagnostics button (egui's icon font has this one).
const DIAG_GLYPH: &str = "⎘";
const DIAG_TIP: &str = "Copy diagnostics to the clipboard";

/// One notification.
pub(super) struct Toast {
    pub msg: String,
    pub detail: Option<String>,
    pub expires: f64,
    pub err: bool,
}

/// Whole minutes elapsed from `since` to `now`, never negative.
pub(super) fn minutes_since(since: f64, now: f64) -> u64 {
    ((now - since).max(0.0) / 60.0).floor() as u64
}

/// The left half of the status bar as `(accent, muted)` text: the right
/// pane's host in the success color, then protocol / uptime / left-pane host
/// muted. With only the left pane on a host, that host takes the accent;
/// with neither, a muted placeholder.
pub(super) fn session_summary(
    host: Option<&str>,
    kind: &str,
    minutes: u64,
    left: Option<(&str, &str)>,
) -> (String, String) {
    match (host, left) {
        (Some(host), left) => {
            let mut muted = format!(" · {kind} · connected {minutes} min");
            if let Some((name, kind)) = left {
                muted.push_str(&format!(" · left: {name} ({kind})"));
            }
            (format!("● {host}"), muted)
        }
        (None, Some((name, kind))) => (format!("● {name}"), format!(" · {kind} · left pane")),
        (None, None) => (String::new(), "No remote host".to_string()),
    }
}

/// Mirror a toast into the diagnostics log: failures as `Error`, the rest `Info`.
fn log_toast(msg: &str, detail: Option<&str>, err: bool) {
    let line = match detail {
        Some(detail) => format!("{msg} — {detail}"),
        None => msg.to_string(),
    };
    if err {
        diag_error!("ui", "{line}");
    } else {
        diag_info!("ui", "{line}");
    }
}

/// 11px monospace text in `color`.
fn mono(text: &str, color: Color32) -> RichText {
    RichText::new(text)
        .font(FontId::new(11.0, FontFamily::Monospace))
        .color(color)
}

/// Paint a 1px hairline along the top edge of a panel drawn with `margin`.
pub(super) fn top_hairline(ui: &Ui, margin: (i8, i8)) {
    let p = theme::current(ui);
    let outer = ui
        .max_rect()
        .expand2(vec2(f32::from(margin.0), f32::from(margin.1)));
    ui.painter()
        .hline(outer.x_range(), outer.top() + 0.5, Stroke::new(1.0, p.line));
}

impl FerropipeApp {
    pub(super) fn toast(&mut self, ctx: &egui::Context, msg: impl Into<String>, err: bool) {
        self.toast_detail(ctx, msg, None, err);
    }

    /// A toast with a second, monospace line under the message.
    pub(super) fn toast_detail(
        &mut self,
        ctx: &egui::Context,
        msg: impl Into<String>,
        detail: Option<String>,
        err: bool,
    ) {
        let now = ctx.input(|i| i.time);
        let msg = msg.into();
        log_toast(&msg, detail.as_deref(), err);
        self.toasts.push(Toast {
            msg,
            detail,
            expires: now + TOAST_SECS,
            err,
        });
    }

    /// Put the diagnostics snapshot on the clipboard and say where the log file is.
    fn copy_diagnostics(&mut self, ctx: &egui::Context) {
        ctx.copy_text(diag::snapshot());
        let path = diag::log_path().map(|p| p.display().to_string());
        self.toast_detail(ctx, "Diagnostics copied", path, false);
    }

    /// One-line status bar: the session on the left, the latest status
    /// message and the copy-diagnostics button on the right.
    pub(super) fn render_status_bar(&mut self, ctx: &egui::Context, ui: &mut Ui) {
        let p = theme::current(ui);
        let (accent, muted) = self.session_line(ctx.input(|i| i.time));
        let margin = Margin::symmetric(STATUS_MARGIN.0, STATUS_MARGIN.1);
        let mut copy_clicked = false;
        egui::Panel::bottom("status")
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(p.bg2).inner_margin(margin))
            .show(ui, |ui| {
                top_hairline(ui, STATUS_MARGIN);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    if !accent.is_empty() {
                        ui.add(Label::new(mono(&accent, p.ok)).selectable(false));
                    }
                    ui.add(Label::new(mono(&muted, p.text3)).selectable(false));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        copy_clicked = widgets::icon_button(ui, DIAG_GLYPH, DIAG_TIP).clicked();
                        ui.add_space(6.0);
                        let status = Label::new(mono(&self.status, p.text3));
                        ui.add(status.truncate().selectable(false));
                    });
                });
            });
        if copy_clicked {
            self.copy_diagnostics(ctx);
        }
    }

    /// The session summary for the status bar.
    fn session_line(&self, now: f64) -> (String, String) {
        let host = self.connected.map(|id| self.conn_name(id));
        let minutes = self.connected_at.map_or(0, |t| minutes_since(t, now));
        let left_name = self
            .lconnected
            .filter(|_| self.left_remote)
            .map(|id| self.conn_name(id));
        let left = left_name.as_deref().map(|name| (name, self.lkind));
        session_summary(host.as_deref(), self.active_kind, minutes, left)
    }

    /// Toast stack in the bottom-right corner, lifted above the bottom panels.
    pub(super) fn render_toasts(&mut self, ctx: &egui::Context, bottom_inset: f32) {
        let now = ctx.input(|i| i.time);
        self.toasts.retain(|t| t.expires > now);
        if self.toasts.is_empty() {
            return;
        }
        Area::new(egui::Id::new("toasts"))
            .anchor(
                Align2::RIGHT_BOTTOM,
                [-TOAST_INSET, -(bottom_inset + TOAST_INSET)],
            )
            .show(ctx, |ui| {
                for t in self.toasts.iter().rev().take(TOAST_MAX_SHOWN) {
                    widgets::toast(ui, &t.msg, t.detail.as_deref(), t.err);
                    ui.add_space(4.0);
                }
            });
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

impl FerropipeApp {
    /// Logo and wordmark on the left; theme, console, hidden-files,
    /// function-key-bar and reopen-on-start toggles on the right.
    pub(super) fn render_top_bar(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let p = crate::theme::current(ui);
                let logo = egui::Image::new(egui::include_image!("../../assets/ferropipe-256.png"))
                    .fit_to_exact_size(egui::vec2(18.0, 18.0));
                ui.add(logo);
                ui.label(
                    RichText::new("Ferropipe")
                        .text_style(egui::TextStyle::Heading)
                        .color(p.text),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    self.top_bar_toggles(ctx, ui);
                });
            });
            ui.add_space(2.0);
        });
    }

    /// Right-aligned top-bar toggles (laid out right to left).
    fn top_bar_toggles(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let dark = self.store.settings.dark_mode;
        let glyph = if dark { "☾" } else { "☀" };
        let tip = if dark {
            "Switch to light theme"
        } else {
            "Switch to dark theme"
        };
        if crate::widgets::icon_button(ui, glyph, tip).clicked() {
            self.store.settings.dark_mode = !dark;
            crate::theme::apply(ctx, !dark);
            self.save_store();
        }
        if ui
            .selectable_label(self.console_open, "Console")
            .on_hover_text("Run commands on the SSH host")
            .clicked()
        {
            self.console_open = !self.console_open;
        }
        let hidden = self.store.settings.show_hidden;
        if ui.selectable_label(hidden, "Hidden files").clicked() {
            self.set_show_hidden(!hidden);
        }
        let keys = self.store.settings.show_fkeys;
        if ui
            .selectable_label(keys, "Keys")
            .on_hover_text("Show the function-key bar")
            .clicked()
        {
            self.store.settings.show_fkeys = !keys;
            self.save_store();
        }
        let reopen = self.store.settings.restore_session;
        if ui
            .selectable_label(reopen, "Reopen")
            .on_hover_text("Reopen the last hosts in their panes on start")
            .clicked()
        {
            self.store.settings.restore_session = !reopen;
            self.save_store();
        }
    }

    /// Toggle dotfile visibility and relist both panes; local listings filter
    /// as they are read, remote ones as they arrive (`on_listing`).
    fn set_show_hidden(&mut self, hidden: bool) {
        self.store.settings.show_hidden = hidden;
        self.refresh_pane(Side::Left);
        self.refresh_pane(Side::Right);
        self.save_store();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minutes_since_floors_to_whole_minutes() {
        assert_eq!(minutes_since(100.0, 100.0), 0);
        assert_eq!(minutes_since(0.0, 59.9), 0);
        assert_eq!(minutes_since(0.0, 60.0), 1);
        assert_eq!(minutes_since(0.0, 3599.0), 59);
    }

    #[test]
    fn minutes_since_is_zero_when_now_precedes_since() {
        assert_eq!(minutes_since(50.0, 10.0), 0);
    }

    #[test]
    fn session_summary_with_a_host_shows_dot_name_kind_and_uptime() {
        let (accent, muted) = session_summary(Some("web01"), "SFTP", 3, None);
        assert_eq!(accent, "● web01");
        assert_eq!(muted, " · SFTP · connected 3 min");
    }

    #[test]
    fn session_summary_without_a_host_is_a_muted_placeholder() {
        let (accent, muted) = session_summary(None, "", 0, None);
        assert_eq!(accent, "");
        assert_eq!(muted, "No remote host");
    }

    #[test]
    fn session_summary_appends_the_left_pane_host_after_the_right_one() {
        let (_, muted) = session_summary(Some("web01"), "SFTP", 0, Some(("nas", "SMB")));
        assert_eq!(muted, " · SFTP · connected 0 min · left: nas (SMB)");
    }

    #[test]
    fn session_summary_names_the_left_pane_host_when_it_is_the_only_one() {
        // Arrange: the right pane shows this computer, the left a host.
        let left = Some(("nas", "SMB"));
        // Act
        let (accent, muted) = session_summary(None, "", 0, left);
        // Assert
        assert_eq!(accent, "● nas");
        assert_eq!(muted, " · SMB · left pane");
    }
}
