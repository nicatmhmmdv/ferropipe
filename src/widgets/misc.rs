//! Buttons and chrome: primary / icon buttons, the empty-pane placeholder,
//! toasts, section captions and the drop-target outline.

use egui::{
    Align, Button, CornerRadius, FontId, Frame, Layout, Margin, Rect, Response, RichText, Sense,
    Shape, Stroke, Ui, UiBuilder, pos2, vec2,
};

use super::{hexagon_points, mono, plain_label};
use crate::theme::{self, RADIUS};

/// Height of the centered column in `empty_pane`.
const EMPTY_PANE_COLUMN_H: f32 = 120.0;
/// Radius of the `empty_pane` hexagon.
const EMPTY_PANE_HEX_R: f32 = 19.0;

/// Accent-filled button for the one primary action in a view.
pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    let p = theme::current(ui);
    ui.add(
        Button::new(RichText::new(text).color(p.accent_ink))
            .fill(p.accent)
            .corner_radius(RADIUS),
    )
}

/// Centered placeholder for an empty pane: hexagon, title, primary action
/// and a hint. Returns true when the action button is clicked.
pub fn empty_pane(ui: &mut Ui, title: &str, action: &str, hint: &str) -> bool {
    let p = theme::current(ui);
    let avail = ui.available_rect_before_wrap();
    ui.allocate_rect(avail, Sense::hover());
    let column = Rect::from_center_size(avail.center(), vec2(avail.width(), EMPTY_PANE_COLUMN_H));
    let builder = UiBuilder::new()
        .max_rect(column)
        .layout(Layout::top_down(Align::Center));
    ui.scope_builder(builder, |ui| {
        paint_hexagon(ui, EMPTY_PANE_HEX_R);
        ui.add_space(8.0);
        plain_label(
            ui,
            RichText::new(title)
                .font(FontId::new(13.0, theme::medium()))
                .color(p.text),
        );
        ui.add_space(10.0);
        let clicked = primary_button(ui, action).clicked();
        ui.add_space(6.0);
        plain_label(ui, RichText::new(hint).small().color(p.text3));
        clicked
    })
    .inner
}

/// Allocate a square and outline a hexagon of radius `r` inside it.
fn paint_hexagon(ui: &mut Ui, r: f32) {
    let p = theme::current(ui);
    let side = r * 2.0 + 2.0;
    let (rect, _) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
    let stroke = Stroke::new(2.0, p.text3.gamma_multiply(0.5));
    ui.painter()
        .add(Shape::closed_line(hexagon_points(rect.center(), r), stroke));
}

/// Notification card with a colored stripe on its left edge.
pub fn toast(ui: &mut Ui, msg: &str, detail: Option<&str>, is_err: bool) {
    let p = theme::current(ui);
    let rect = Frame::new()
        .fill(p.bg2)
        .stroke((1.0, p.line))
        .corner_radius(CornerRadius::same(5))
        .inner_margin(Margin::symmetric(12, 7))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                plain_label(ui, RichText::new(msg).color(p.text));
                if let Some(detail) = detail {
                    plain_label(ui, mono(detail, 11.0, p.text3));
                }
            });
        })
        .response
        .rect;
    let stripe = Rect::from_min_max(rect.left_top(), pos2(rect.left() + 3.0, rect.bottom()));
    let corners = CornerRadius {
        nw: 3,
        sw: 3,
        ne: 0,
        se: 0,
    };
    ui.painter()
        .rect_filled(stripe, corners, if is_err { p.err } else { p.ok });
}

/// Uppercase monospace caption above a group of controls.
pub fn section_label(ui: &mut Ui, text: &str) {
    let p = theme::current(ui);
    plain_label(ui, mono(&text.to_uppercase(), 10.5, p.text3));
}

/// 22×22 glyph button with a tooltip.
pub fn icon_button(ui: &mut Ui, glyph: &str, tip: &str) -> Response {
    let p = theme::current(ui);
    ui.add(
        Button::new(glyph)
            .min_size(vec2(22.0, 22.0))
            .corner_radius(RADIUS)
            .fill(p.bg2)
            .stroke((1.0, p.line)),
    )
    .on_hover_text(tip)
}

/// Dashed accent rectangle marking a drop target, inset 2px inside `rect`.
pub fn dashed_outline(ui: &Ui, rect: Rect) {
    let p = theme::current(ui);
    let r = rect.shrink(2.0);
    let stroke = Stroke::new(1.5, p.accent);
    let corners = [
        r.left_top(),
        r.right_top(),
        r.right_bottom(),
        r.left_bottom(),
    ];
    let painter = ui.painter();
    for i in 0..corners.len() {
        let edge = [corners[i], corners[(i + 1) % corners.len()]];
        painter.extend(Shape::dashed_line(&edge, stroke, 4.0, 3.0));
    }
}
