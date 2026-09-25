//! The breadcrumb path bar and its inline path editor.

use std::hash::Hash;

use egui::{
    Align, CursorIcon, FontFamily, FontId, Frame, Id, Key, Layout, Margin, ScrollArea, Sense,
    TextEdit, Ui,
};

use super::misc::icon_button;
use super::{Crumb, breadcrumb_segments, link_label, mono, plain_label, salt_id};
use crate::theme::{self, RADIUS};

/// What the user asked the path bar to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathAction {
    Navigate(String),
    Up,
    Home,
    Refresh,
}

/// Breadcrumb path bar with Up / Home / Refresh buttons on the right.
///
/// While `edit` is `Some`, a monospace text field replaces the crumbs: Enter
/// yields `Navigate` and clears `edit`; Escape just clears it. Clicking the
/// last crumb or the "✎" icon starts editing the current `path`.
pub fn path_bar(
    ui: &mut Ui,
    id_salt: impl Hash,
    path: &str,
    home: Option<&str>,
    edit: &mut Option<String>,
) -> Option<PathAction> {
    let id = salt_id(ui, id_salt);
    let mut action = None;
    // `horizontal` first: a bare right-to-left layout in a vertical parent is
    // handed the whole remaining pane and centres the row in the middle of it.
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if icon_button(ui, "↻", "Refresh").clicked() {
                action = Some(PathAction::Refresh);
            }
            if icon_button(ui, "⌂", "Home").clicked() {
                action = Some(PathAction::Home);
            }
            if icon_button(ui, "↑", "Up").clicked() {
                action = Some(PathAction::Up);
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                let inner = if edit.is_some() {
                    path_editor(ui, id, edit)
                } else {
                    breadcrumb_bar(ui, id, path, home, edit)
                };
                action = inner.or(action.take());
            });
        });
    });
    action
}

/// Id of the per-bar flag remembering that the editor already took focus.
fn editor_focus_flag(id: Id) -> Id {
    id.with("path_edit_focused")
}

/// The text-field branch of `path_bar`.
fn path_editor(ui: &mut Ui, id: Id, edit: &mut Option<String>) -> Option<PathAction> {
    let p = theme::current(ui);
    let text = edit.as_mut()?;
    let resp = ui.add(
        TextEdit::singleline(text)
            .id(id.with("path_edit"))
            .font(FontId::new(12.0, FontFamily::Monospace))
            .text_color(p.text)
            .desired_width(ui.available_width()),
    );
    let flag = editor_focus_flag(id);
    if !ui.data(|d| d.get_temp::<bool>(flag).unwrap_or(false)) {
        resp.request_focus();
        ui.data_mut(|d| d.insert_temp(flag, true));
    }
    let (enter, escape) = ui.input(|i| (i.key_pressed(Key::Enter), i.key_pressed(Key::Escape)));
    if resp.lost_focus() && enter {
        let value = text.trim().to_string();
        *edit = None;
        return Some(PathAction::Navigate(value));
    }
    if escape {
        *edit = None;
    }
    None
}

/// The breadcrumb branch of `path_bar`.
fn breadcrumb_bar(
    ui: &mut Ui,
    id: Id,
    path: &str,
    home: Option<&str>,
    edit: &mut Option<String>,
) -> Option<PathAction> {
    ui.data_mut(|d| d.remove_temp::<bool>(editor_focus_flag(id)));
    let crumbs = breadcrumb_segments(path, home);
    ScrollArea::horizontal()
        .id_salt(id.with("crumbs"))
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .auto_shrink([false, true])
        .max_width(ui.available_width())
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let action = crumb_trail(ui, &crumbs);
                let edit_hit = crumb_edit_hit(ui, &crumbs);
                if edit_hit && action.is_none() {
                    *edit = Some(path.to_string());
                }
                action
            })
            .inner
        })
        .inner
}

/// Draw the non-last crumbs as links; return a `Navigate` for the one clicked.
fn crumb_trail(ui: &mut Ui, crumbs: &[Crumb]) -> Option<PathAction> {
    let p = theme::current(ui);
    let mut action = None;
    for (i, crumb) in crumbs
        .iter()
        .enumerate()
        .take(crumbs.len().saturating_sub(1))
    {
        if i > 0 && crumbs[i - 1].label != "/" {
            plain_label(ui, mono("/", 12.0, p.text3));
        }
        if link_label(ui, mono(&crumb.label, 12.0, p.text2)).clicked() {
            action = Some(PathAction::Navigate(crumb.path.clone()));
        }
    }
    action
}

/// Draw the last crumb as a pill plus the "✎" icon; true when either is clicked.
fn crumb_edit_hit(ui: &mut Ui, crumbs: &[Crumb]) -> bool {
    let p = theme::current(ui);
    let Some(last) = crumbs.last() else {
        return false;
    };
    if crumbs.len() > 1 && crumbs[crumbs.len() - 2].label != "/" {
        plain_label(ui, mono("/", 12.0, p.text3));
    }
    let pill = Frame::new()
        .fill(p.bg3)
        .corner_radius(RADIUS)
        .inner_margin(Margin::symmetric(6, 1))
        .show(ui, |ui| plain_label(ui, mono(&last.label, 12.0, p.text)))
        .response;
    let pill_hit = ui
        .interact(pill.rect, pill.id.with("crumb_pill"), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand)
        .clicked();
    ui.add_space(4.0);
    let pen_hit = link_label(ui, mono("✎", 12.0, p.text3))
        .on_hover_text("Edit path")
        .clicked();
    pill_hit || pen_hit
}
