//! Host widgets: the protocol chip, status dot, color stripe, the host pill
//! and the generic selectable row (`row_slot` / `row_ui`).

use std::any::Any;
use std::hash::Hash;
use std::time::Duration;

use egui::{
    Align, Area, Color32, CornerRadius, CursorIcon, DragAndDrop, FontId, Frame, Id, Layout, Margin,
    Order, Rect, Response, RichText, Sense, Stroke, Ui, UiBuilder, pos2, vec2,
};

use super::{chip_label, mono, plain_label, pulse_alpha, salt_id};
use crate::model::ConnectionKind;
use crate::theme::{self, RADIUS};

/// How soon to repaint while a pulse is animating.
const PULSE_REPAINT: Duration = Duration::from_millis(60);

/// Connection state shown by `status_dot` and `host_selector`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HostStatus {
    Idle,
    Connecting,
    Connected,
    Failed,
}

/// Small monospace tag naming the connection protocol.
pub fn protocol_chip(ui: &mut Ui, kind: ConnectionKind) -> Response {
    let p = theme::current(ui);
    Frame::new()
        .fill(p.bg2)
        .stroke((1.0, p.line))
        .corner_radius(CornerRadius::same(3))
        .inner_margin(Margin::symmetric(5, 1))
        .show(ui, |ui| {
            plain_label(ui, mono(chip_label(kind), 10.5, p.text2))
        })
        .response
}

/// 11×11 status indicator: grey idle, pulsing amber connecting, green with a
/// halo when connected, red when failed.
pub fn status_dot(ui: &mut Ui, status: HostStatus) -> Response {
    let p = theme::current(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(11.0, 11.0), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let c = rect.center();
    let painter = ui.painter();
    match status {
        HostStatus::Idle => {
            painter.circle_filled(c, 3.5, p.text3);
        }
        HostStatus::Connecting => {
            let alpha = pulse_alpha(ui.ctx().input(|i| i.time)) as f32;
            painter.circle_filled(c, 3.5, p.warn.gamma_multiply(alpha));
            ui.ctx().request_repaint_after(PULSE_REPAINT);
        }
        HostStatus::Connected => {
            painter.circle_filled(c, 5.5, p.ok.gamma_multiply(0.22));
            painter.circle_filled(c, 3.5, p.ok);
        }
        HostStatus::Failed => {
            painter.circle_filled(c, 3.5, p.err);
        }
    }
    resp
}

/// Paint a 3×16 user-chosen color stripe at the left edge of `row`.
pub fn color_stripe(ui: &Ui, row: Rect, color: Option<[u8; 3]>) {
    let Some([r, g, b]) = color else { return };
    let stripe = Rect::from_min_size(pos2(row.left(), row.center().y - 8.0), vec2(3.0, 16.0));
    ui.painter()
        .rect_filled(stripe, CornerRadius::same(2), Color32::from_rgb(r, g, b));
}

/// Clickable host pill: status dot, name, optional monospace subtitle and a
/// "▾" caret. The whole frame is one click target, usable with
/// `egui::Popup::menu(&response)`.
pub fn host_selector(
    ui: &mut Ui,
    id_salt: impl Hash,
    name: &str,
    sub: Option<&str>,
    status: HostStatus,
) -> Response {
    let p = theme::current(ui);
    let mut frame = Frame::new()
        .fill(p.bg1)
        .stroke((1.0, p.line))
        .corner_radius(CornerRadius::same(5))
        .inner_margin(Margin::symmetric(8, 3))
        .begin(ui);
    host_selector_contents(&mut frame.content_ui, name, sub, status);
    let outer = frame.allocate_space(ui);
    let resp = ui.interact(outer.rect, salt_id(ui, id_salt), Sense::click());
    if resp.hovered() {
        frame.frame.stroke = Stroke::new(1.0, p.text3);
    }
    frame.paint(ui);
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Inner row of `host_selector`.
fn host_selector_contents(ui: &mut Ui, name: &str, sub: Option<&str>, status: HostStatus) {
    let p = theme::current(ui);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        if status == HostStatus::Idle && name == "This computer" {
            plain_label(ui, RichText::new("⌂").color(p.text2));
        } else {
            status_dot(ui, status);
        }
        plain_label(
            ui,
            RichText::new(name)
                .font(FontId::new(13.0, theme::medium()))
                .color(p.text),
        );
        if let Some(sub) = sub {
            plain_label(ui, mono(sub, 11.0, p.text3));
        }
        plain_label(ui, RichText::new("▾").color(p.text3));
    });
}

/// Allocate a full-width, `height`-tall click target and paint its background:
/// `sel` when selected, `hover` while hovered, nothing otherwise. Draw the
/// row's contents inside the returned rect with [`row_ui`].
pub fn row_slot(ui: &mut Ui, height: f32, selected: bool, hover: Color32) -> (Rect, Response) {
    row_slot_with(ui, height, selected, hover, Sense::click())
}

/// A selectable row that is also a drag source publishing `payload`.
///
/// Wrapping a click-only row in `Ui::dnd_drag_source` does not work: that lays
/// a separate drag-only widget exactly over the row, and egui's hit test then
/// refuses to hand the click to the widget underneath (two overlapping small
/// widgets). This row senses click *and* drag itself, so select, double-click
/// and the context menu keep working while a drag past the threshold publishes
/// the payload for drop zones and shows `preview` at the pointer.
pub fn drag_row_slot<P>(
    ui: &mut Ui,
    height: f32,
    selected: bool,
    hover: Color32,
    payload: P,
    preview: &str,
) -> (Rect, Response)
where
    P: Any + Send + Sync,
{
    let (rect, resp) = row_slot_with(ui, height, selected, hover, Sense::click_and_drag());
    if resp.dragged() {
        // Re-published every frame: egui drops the payload once the pointer is
        // released, and the drop zone reads it on that release frame.
        DragAndDrop::set_payload(ui.ctx(), payload);
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        drag_preview(ui, preview);
    }
    (rect, resp)
}

/// A small floating label following the pointer while a row is dragged.
fn drag_preview(ui: &Ui, text: &str) {
    let Some(pos) = ui.ctx().pointer_interact_pos() else {
        return;
    };
    let p = theme::current(ui);
    Area::new(Id::new("widgets.drag-preview"))
        .order(Order::Tooltip)
        .interactable(false)
        .fixed_pos(pos + vec2(14.0, 10.0))
        .show(ui.ctx(), |ui| {
            Frame::new()
                .fill(p.bg2)
                .stroke(Stroke::new(1.0, p.line))
                .corner_radius(RADIUS)
                .inner_margin(Margin::symmetric(8, 4))
                .show(ui, |ui| {
                    let text = RichText::new(text)
                        .font(FontId::new(13.0, theme::medium()))
                        .color(p.text);
                    plain_label(ui, text);
                });
        });
}

/// `row_slot` with an explicit sense.
fn row_slot_with(
    ui: &mut Ui,
    height: f32,
    selected: bool,
    hover: Color32,
    sense: Sense,
) -> (Rect, Response) {
    let p = theme::current(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), height), sense);
    let fill = if selected {
        Some(p.sel)
    } else if resp.hovered() {
        Some(hover)
    } else {
        None
    };
    if let Some(fill) = fill.filter(|_| ui.is_rect_visible(rect)) {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(RADIUS), fill);
    }
    (rect, resp)
}

/// Child ui laid out left-to-right and vertically centred inside `rect`, inset
/// by `pad_left` / `pad_right`. Allocates nothing in `ui`: allocate the row
/// first (see [`row_slot`]).
pub fn row_ui(ui: &mut Ui, rect: Rect, pad_left: f32, pad_right: f32) -> Ui {
    let inner = Rect::from_min_max(
        pos2(rect.left() + pad_left, rect.top()),
        pos2(rect.right() - pad_right, rect.bottom()),
    );
    ui.new_child(
        UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::left_to_right(Align::Center)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Context, Event, Modifiers, PointerButton, Pos2, RawInput};

    struct Marker;

    const ROW_H: f32 = 26.0;

    /// A headless context with the app's font families registered (the drag
    /// preview uses the medium face).
    fn app_context() -> Context {
        let ctx = Context::default();
        theme::install_fonts(&ctx);
        ctx
    }

    /// One headless frame drawing a `drag_row_slot`: (clicked, dragged, rect).
    fn drag_row_frame(ctx: &Context, events: Vec<Event>) -> (bool, bool, Rect) {
        let mut out = (false, false, Rect::NOTHING);
        let input = RawInput {
            events,
            ..Default::default()
        };
        ctx.run_ui(input, |ui| {
            let hover = theme::current(ui).bg2;
            let (rect, resp) = drag_row_slot(ui, ROW_H, false, hover, Marker, "host");
            out = (resp.clicked(), resp.dragged(), rect);
        })
        .drop_without_applying_deltas();
        out
    }

    /// The pattern the sidebar used before: a click-only row wrapped in
    /// `Ui::dnd_drag_source`. Returns (clicked, rect).
    fn wrapped_row_frame(ctx: &Context, events: Vec<Event>) -> (bool, Rect) {
        let mut out = (false, Rect::NOTHING);
        let input = RawInput {
            events,
            ..Default::default()
        };
        ctx.run_ui(input, |ui| {
            let hover = theme::current(ui).bg2;
            let inner = ui.dnd_drag_source(Id::new("wrapped"), Marker, |ui| {
                row_slot(ui, ROW_H, false, hover)
            });
            let (rect, resp) = inner.inner;
            out = (resp.clicked(), rect);
        })
        .drop_without_applying_deltas();
        out
    }

    fn button(pos: Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    #[test]
    fn drag_row_reports_a_click_on_press_and_release_in_place() {
        // Arrange
        let ctx = app_context();
        let (_, _, rect) = drag_row_frame(&ctx, Vec::new());
        let pos = rect.center();
        drag_row_frame(&ctx, vec![Event::PointerMoved(pos)]);
        // Act
        let (clicked_on_press, _, _) = drag_row_frame(&ctx, vec![button(pos, true)]);
        let (clicked, dragged, _) = drag_row_frame(&ctx, vec![button(pos, false)]);
        // Assert
        assert!(!clicked_on_press, "nothing may fire on the press itself");
        assert!(clicked, "a release in place must count as a click");
        assert!(!dragged);
    }

    #[test]
    fn drag_row_starts_a_drag_and_publishes_its_payload_once_the_pointer_moves() {
        // Arrange
        let ctx = app_context();
        let (_, _, rect) = drag_row_frame(&ctx, Vec::new());
        let pos = rect.center();
        drag_row_frame(&ctx, vec![Event::PointerMoved(pos)]);
        drag_row_frame(&ctx, vec![button(pos, true)]);
        // Act
        let moved = vec![Event::PointerMoved(pos + vec2(40.0, 0.0))];
        let (clicked, dragged, _) = drag_row_frame(&ctx, moved);
        let published = DragAndDrop::has_payload_of_type::<Marker>(&ctx);
        // Assert
        assert!(!clicked);
        assert!(dragged, "moving past the drag threshold must start a drag");
        assert!(published, "drop zones must see the payload while dragging");
    }

    #[test]
    fn dnd_drag_source_over_a_click_row_swallows_the_click_in_egui_0_36() {
        // Documents why `drag_row_slot` exists. If this starts failing, egui's
        // hit test changed and the sidebar could use `dnd_drag_source` again.
        let ctx = Context::default();
        let (_, rect) = wrapped_row_frame(&ctx, Vec::new());
        let pos = rect.center();
        wrapped_row_frame(&ctx, vec![Event::PointerMoved(pos)]);
        wrapped_row_frame(&ctx, vec![button(pos, true)]);
        let (clicked, _) = wrapped_row_frame(&ctx, vec![button(pos, false)]);
        assert!(!clicked, "egui handed the click to the covered row");
    }
}
