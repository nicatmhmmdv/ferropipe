//! The gutter between the panes: two transfer arrows and a queue count.

use super::*;
use crate::theme::GUTTER_W;
use egui::{Rangef, Stroke};

/// Space above the gutter arrows: pane header plus path bar.
const GUTTER_TOP_PAD: f32 = 72.0;

/// Tooltip of the gutter arrow that sends the other pane's selection into `dest`.
fn gutter_tip(verb: Verb, dest: Side) -> String {
    match (verb, dest) {
        (Verb::Copy, Side::Right) => "Copy selected to the right pane".into(),
        (Verb::Copy, Side::Left) => "Copy selected to the left pane".into(),
        (verb, _) => format!("{} selected", verb.noun()),
    }
}

impl FerropipeApp {
    /// The 44px column between the panes: two transfer arrows and a queue count.
    pub(in crate::app) fn render_gutter(&mut self, ctx: &egui::Context, ui: &mut Ui) {
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
        let (left_remote, right_remote) = (
            self.side_is_remote(Side::Left),
            self.side_is_remote(Side::Right),
        );
        let tip_right = gutter_tip(transfer_verb(left_remote, right_remote), Side::Right);
        let tip_left = gutter_tip(transfer_verb(right_remote, left_remote), Side::Left);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gutter_tip_names_the_direction_for_upload_and_download() {
        // Arrange / Act / Assert
        assert_eq!(gutter_tip(Verb::Upload, Side::Right), "Upload selected");
        assert_eq!(gutter_tip(Verb::Download, Side::Left), "Download selected");
    }

    #[test]
    fn gutter_tip_names_the_target_pane_for_a_copy() {
        assert_eq!(
            gutter_tip(Verb::Copy, Side::Right),
            "Copy selected to the right pane"
        );
        assert_eq!(
            gutter_tip(Verb::Copy, Side::Left),
            "Copy selected to the left pane"
        );
    }
}
