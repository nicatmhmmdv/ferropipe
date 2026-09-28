//! Transfer widgets: the drawer row with its progress bar, and the gutter arrow.

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Button, CornerRadius, FontFamily, FontId, Label, Layout, Rect, Response, RichText,
    Sense, Stroke, Ui, vec2,
};

use super::{mono, plain_label};
use crate::theme;

/// Size of the painted progress bar in `transfer_row`.
const BAR_SIZE: egui::Vec2 = vec2(220.0, 6.0);

/// Lifecycle of one transfer row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransferPhase {
    Running,
    Queued,
    Done,
    Failed,
}

/// Which way a transfer moves, for the drawer's glyphs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Upload,
    Download,
    Copy,
}

impl Direction {
    /// The row's leading glyph: up to a host, down from one, across otherwise.
    pub fn glyph(self) -> &'static str {
        match self {
            Direction::Upload => "↑",
            Direction::Download => "↓",
            Direction::Copy => "⇉",
        }
    }

    /// The arrow between the label and its destination; a download reads
    /// right to left, everything else lands in the destination.
    pub(super) fn joiner(self) -> &'static str {
        match self {
            Direction::Download => "←",
            Direction::Upload | Direction::Copy => "→",
        }
    }
}

/// Everything `transfer_row` needs to draw one transfer.
#[derive(Clone, Copy, Debug)]
pub struct TransferView<'a> {
    pub direction: Direction,
    pub label: &'a str,
    pub dest: &'a str,
    /// Progress in `0.0..=1.0`.
    pub frac: f32,
    pub status: &'a str,
    pub phase: TransferPhase,
}

/// What the user clicked on a failed transfer row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransferAction {
    Retry,
    Dismiss,
}

/// One 24px transfer line: arrow, truncated label/destination, a 220×6
/// progress bar, status text and (when failed) Retry / Dismiss buttons.
pub fn transfer_row(ui: &mut Ui, view: &TransferView) -> Option<TransferAction> {
    let p = theme::current(ui);
    ui.horizontal(|ui| {
        ui.set_min_height(24.0);
        ui.spacing_mut().item_spacing.x = 8.0;
        let arrow = view.direction.glyph();
        ui.add_sized(
            vec2(14.0, 24.0),
            Label::new(mono(arrow, 13.0, p.text2)).selectable(false),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let action = transfer_trailing(ui, view);
            transfer_bar(ui, view.frac, view.phase);
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                transfer_label(ui, view);
            });
            action
        })
        .inner
    })
    .inner
}

/// Status text and, when failed, the Retry / Dismiss buttons (right-aligned).
fn transfer_trailing(ui: &mut Ui, view: &TransferView) -> Option<TransferAction> {
    let p = theme::current(ui);
    let mut action = None;
    let failed = view.phase == TransferPhase::Failed;
    if failed {
        if ui.small_button("Dismiss").clicked() {
            action = Some(TransferAction::Dismiss);
        }
        if ui.small_button("Retry").clicked() {
            action = Some(TransferAction::Retry);
        }
    }
    let color = if failed { p.err } else { p.text2 };
    plain_label(ui, mono(view.status, 11.0, color));
    action
}

/// Painter-drawn 220×6 progress bar, colored by phase.
fn transfer_bar(ui: &mut Ui, frac: f32, phase: TransferPhase) {
    let p = theme::current(ui);
    let (rect, _) = ui.allocate_exact_size(BAR_SIZE, Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let fill = match phase {
        TransferPhase::Running => p.accent,
        TransferPhase::Queued => p.text3,
        TransferPhase::Done => p.ok,
        TransferPhase::Failed => p.err,
    };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(3), p.bg3);
    let width = rect.width() * frac.clamp(0.0, 1.0);
    if width > 0.0 {
        let done = Rect::from_min_size(rect.min, vec2(width, rect.height()));
        painter.rect_filled(done, CornerRadius::same(3), fill);
    }
}

/// "label → dest" (or "←") as one truncating label with two text styles.
fn transfer_label(ui: &mut Ui, view: &TransferView) {
    let p = theme::current(ui);
    let body = egui::TextStyle::Body.resolve(ui.style());
    let mut job = LayoutJob::default();
    job.append(
        view.label,
        0.0,
        TextFormat {
            font_id: body,
            color: p.text,
            ..Default::default()
        },
    );
    let dir = view.direction.joiner();
    job.append(
        &format!(" {dir} {}", view.dest),
        0.0,
        TextFormat {
            font_id: FontId::new(11.0, FontFamily::Monospace),
            color: p.text3,
            ..Default::default()
        },
    );
    ui.add(Label::new(job).truncate().selectable(false));
}

/// Round 28px arrow button for the transfer gutter.
pub fn gutter_arrow(ui: &mut Ui, glyph: &str, enabled: bool, primary: bool) -> Response {
    let p = theme::current(ui);
    let (fill, ink, stroke) = if primary {
        (p.accent, p.accent_ink, Stroke::NONE)
    } else {
        (p.bg2, p.text2, Stroke::new(1.0, p.line))
    };
    ui.add_enabled(
        enabled,
        Button::new(RichText::new(glyph).size(15.0).strong().color(ink))
            .min_size(vec2(28.0, 28.0))
            .corner_radius(14)
            .fill(fill)
            .stroke(stroke),
    )
}
