//! File panes: the host header with its popup, the path bar, the empty /
//! connecting placeholders and the listing table.

use super::*;
use crate::theme::{self, HOST_ROW_H, Palette, ROW_H};
use crate::widgets::{self, HostStatus, PathAction};
use egui::{
    Align, Align2, CornerRadius, DragAndDrop, FontFamily, FontId, Frame, Id, InnerResponse, Label,
    Layout, Margin, Popup, Rect, Response, Sense, Stroke, Ui, pos2,
};
use egui_extras::{Column, TableBuilder, TableRow};
use std::any::Any;
use std::sync::Arc;
use transfers::{Verb, transfer_verb};

/// Width of the host popup under a pane header.
const HOST_POPUP_W: f32 = 260.0;
/// Column-header font size (matches `TextStyle::Small`).
const HEADER_FONT_SIZE: f32 = 11.5;
/// Column-header row height.
const HEADER_H: f32 = 22.0;
/// Where the "Open … here" hint sits below the top of a pane body.
const HOST_DROP_HINT_Y: f32 = 10.0;
/// Padding of the hint's backdrop around its text.
const HOST_DROP_HINT_PAD: f32 = 6.0;

/// What a pane body shows this frame.
enum PaneView {
    Listing,
    Connecting(String),
    Empty,
}

/// What the user picked in a pane's host popup.
enum HostChoice {
    Local,
    Host(Uuid),
    Disconnect,
}

/// Deferred results collected while the table body holds the pane immutably.
#[derive(Default)]
struct RowEvents {
    /// `(name, ctrl held)` of the row clicked this frame.
    click: Option<(String, bool)>,
    out: Vec<PaneOutcome>,
}

/// Id of the host popup under `side`'s header. The empty-state button opens
/// the same popup programmatically, so both agree on an explicit id.
fn host_popup_id(side: Side) -> Id {
    Id::new(("ferropipe.host_popup", side as u8))
}

/// The row menu's transfer entry: the verb with an arrow toward the other pane.
fn transfer_menu_label(side: Side, verb: Verb) -> String {
    match side {
        Side::Left => format!("{} ▶", verb.noun()),
        Side::Right => format!("◀ {}", verb.noun()),
    }
}

/// The line under a pane header: a 2 px accent for the focused pane, the
/// 1 px hairline for the other.
fn header_rule(is_focused: bool, p: &Palette) -> Stroke {
    if is_focused {
        Stroke::new(2.0, p.accent)
    } else {
        Stroke::new(1.0, p.line)
    }
}

/// The primary button went down inside `ui`'s rect this frame, with no popup
/// or window covering that spot.
fn pane_pressed(ctx: &egui::Context, ui: &Ui) -> bool {
    let pressed_at = ctx.input(|i| {
        i.pointer
            .primary_pressed()
            .then(|| i.pointer.press_origin())
            .flatten()
    });
    let Some(pos) = pressed_at else {
        return false;
    };
    ui.max_rect().contains(pos)
        && ctx
            .layer_id_at(pos)
            .is_none_or(|layer| layer == ui.layer_id())
}

impl FerropipeApp {
    /// Draw one pane (header, path bar, body) and apply the actions it emitted.
    pub(super) fn render_side(&mut self, ctx: &egui::Context, ui: &mut Ui, side: Side) {
        // Checked before drawing so this frame's header already shows the focus.
        if pane_pressed(ctx, ui) {
            self.focused = side;
        }
        let out = self.render_pane_header(ctx, ui, side);
        let out2 = self.render_pane_body(ctx, ui, side);
        self.handle_pane(ctx, side, out);
        self.handle_pane(ctx, side, out2);
    }

    /// Header bar: host pill (with its popup) on the left, "+" menu and item
    /// count on the right, hairline below.
    fn render_pane_header(
        &mut self,
        ctx: &egui::Context,
        ui: &mut Ui,
        side: Side,
    ) -> Vec<PaneOutcome> {
        let p = theme::current(ui);
        let mut out = Vec::new();
        let rect = Frame::new()
            .fill(p.bg2)
            .inner_margin(Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    self.pane_host_pill(ctx, ui, side);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        new_item_menu(ui, &mut out);
                        item_count_label(ui, self.side_pane(side));
                    });
                });
            })
            .response
            .rect;
        let rule = header_rule(self.focused == side, p);
        ui.painter()
            .hline(rect.x_range(), rect.bottom() - rule.width / 2.0, rule);
        out
    }

    /// The host pill and its popup; applies whatever was chosen in the popup.
    fn pane_host_pill(&mut self, ctx: &egui::Context, ui: &mut Ui, side: Side) {
        let (name, sub, status) = self.pane_host(side);
        let resp =
            widgets::host_selector(ui, ("hostsel", side as u8), &name, sub.as_deref(), status);
        match self.host_popup(&resp, side) {
            Some(HostChoice::Local) => self.side_to_local(side),
            Some(HostChoice::Host(id)) => self.connect_side(ctx, side, id),
            Some(HostChoice::Disconnect) => self.disconnect(),
            None => {}
        }
    }

    /// Name, subtitle and status for a pane's host pill.
    fn pane_host(&self, side: Side) -> (String, Option<String>, HostStatus) {
        if !self.side_is_remote(side) {
            return ("This computer".into(), None, HostStatus::Idle);
        }
        match side {
            Side::Left => {
                let status = if self.lconnected.is_some() {
                    HostStatus::Connected
                } else if self.lconnecting {
                    HostStatus::Connecting
                } else {
                    HostStatus::Idle
                };
                let (name, sub) = self.host_label(self.lconnected.or(self.lpending));
                (name, sub, status)
            }
            Side::Right if self.connected.is_some() => {
                let (name, sub) = self.host_label(self.connected);
                (name, sub, HostStatus::Connected)
            }
            Side::Right if self.connecting => {
                let (name, _) = self.host_label(self.pending_connect);
                (name, Some("connecting…".into()), HostStatus::Connecting)
            }
            Side::Right => ("Choose a host".into(), None, HostStatus::Idle),
        }
    }

    /// Display name and "user@host:port" of a saved host, if `id` names one.
    fn host_label(&self, id: Option<Uuid>) -> (String, Option<String>) {
        match id.and_then(|id| self.store.connections.iter().find(|c| c.id == id)) {
            Some(c) => (c.name.clone(), Some(c.target())),
            None => ("Remote".into(), None),
        }
    }

    /// Popup under the host pill: "This computer", every file-capable host
    /// and, on a connected right pane, "Disconnect". Returns what was clicked.
    fn host_popup(&self, resp: &Response, side: Side) -> Option<HostChoice> {
        let mut choice = None;
        Popup::menu(resp)
            .id(host_popup_id(side))
            .width(HOST_POPUP_W)
            .show(|ui| {
                if ui.button("⌂ This computer").clicked() {
                    choice = Some(HostChoice::Local);
                    ui.close();
                }
                ui.separator();
                for c in self
                    .store
                    .connections
                    .iter()
                    .filter(|c| c.kind.browses_files())
                {
                    if popup_host_row(ui, c) {
                        choice = Some(HostChoice::Host(c.id));
                        ui.close();
                    }
                }
                if side == Side::Right && self.connected.is_some() {
                    ui.separator();
                    if ui.button("Disconnect").clicked() {
                        choice = Some(HostChoice::Disconnect);
                        ui.close();
                    }
                }
            });
        choice
    }

    /// Path bar plus listing, or the empty / connecting placeholder for a
    /// remote pane. The whole body accepts a host dragged from the sidebar.
    fn render_pane_body(
        &mut self,
        ctx: &egui::Context,
        ui: &mut Ui,
        side: Side,
    ) -> Vec<PaneOutcome> {
        let mut out = Vec::new();
        let (zone, dropped) = drop_zone::<HostPayload, _>(ui, Frame::NONE, |ui| {
            // Fill the body so a host can be dropped anywhere in the pane.
            ui.set_min_size(ui.available_size());
            self.render_pane_view(ctx, ui, side, &mut out);
        });
        if let Some(host) = dropped {
            out.push(PaneOutcome::OpenHost(host.id));
        }
        if let Some(host) = zone.response.dnd_hover_payload::<HostPayload>() {
            host_drop_hint(ui, zone.response.rect, &self.conn_name(host.id));
        }
        out
    }

    /// The body's contents this frame: a placeholder, or the path bar and listing.
    fn render_pane_view(
        &mut self,
        ctx: &egui::Context,
        ui: &mut Ui,
        side: Side,
        out: &mut Vec<PaneOutcome>,
    ) {
        match self.pane_view(side) {
            PaneView::Empty => {
                let hint = "or pick one from the list on the left";
                if widgets::empty_pane(ui, "No host in this pane", "Choose a host", hint) {
                    Popup::open_id(ctx, host_popup_id(side));
                }
            }
            PaneView::Connecting(name) => connecting_note(ui, &name),
            PaneView::Listing => {
                let verb =
                    transfer_verb(self.side_is_remote(side), self.side_is_remote(side.other()));
                let menu_label = transfer_menu_label(side, verb);
                let pane = self.side_pane_mut(side);
                if let Some(action) = path_bar_row(ui, pane, side) {
                    out.push(path_outcome(action));
                }
                out.extend(render_listing(ui, pane, side, &menu_label));
            }
        }
    }

    /// Which body a pane shows: the listing when local or connected, otherwise a placeholder.
    fn pane_view(&self, side: Side) -> PaneView {
        let (remote, connected, connecting, pending) = match side {
            Side::Left => (
                self.left_remote,
                self.lconnected.is_some(),
                self.lconnecting,
                self.lpending,
            ),
            Side::Right => (
                !self.right_local,
                self.connected.is_some(),
                self.connecting,
                self.pending_connect,
            ),
        };
        if !remote || connected {
            PaneView::Listing
        } else if connecting {
            PaneView::Connecting(self.host_label(pending).0)
        } else {
            PaneView::Empty
        }
    }
}

/// "+" menu with the New folder / New file actions.
fn new_item_menu(ui: &mut Ui, out: &mut Vec<PaneOutcome>) {
    ui.menu_button("+", |ui| {
        if ui.button("New folder").clicked() {
            out.push(PaneOutcome::NewFolder);
            ui.close();
        }
        if ui.button("New file").clicked() {
            out.push(PaneOutcome::NewFile);
            ui.close();
        }
    })
    .response
    .on_hover_text("New folder or file");
}

/// "N items · K selected" in the small monospace caption style.
fn item_count_label(ui: &mut Ui, pane: &PaneState) {
    let p = theme::current(ui);
    let mut text = format!("{} items", pane.entries.len());
    if !pane.selected.is_empty() {
        text.push_str(&format!(" · {} selected", pane.selected.len()));
    }
    let text = RichText::new(text)
        .font(FontId::new(11.0, FontFamily::Monospace))
        .color(p.text3);
    ui.add(Label::new(text).selectable(false));
}

/// One full-width host row (name + protocol chip) in the host popup; true when clicked.
fn popup_host_row(ui: &mut Ui, c: &Connection) -> bool {
    let p = theme::current(ui);
    let (rect, resp) = widgets::row_slot(ui, HOST_ROW_H, false, p.bg3);
    let mut row = widgets::row_ui(ui, rect, 8.0, 8.0);
    row.with_layout(Layout::right_to_left(Align::Center), |ui| {
        widgets::protocol_chip(ui, c.kind).on_hover_text(c.kind.label());
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            let name = RichText::new(&c.name).color(p.text);
            ui.add(Label::new(name).truncate().selectable(false));
        });
    });
    resp.on_hover_text(c.target()).clicked()
}

/// Spinner and "Connecting to …" note for a pane whose session is still being set up.
fn connecting_note(ui: &mut Ui, name: &str) {
    let p = theme::current(ui);
    ui.vertical_centered(|ui| {
        ui.add_space(32.0);
        ui.spinner();
        ui.add_space(6.0);
        let text = RichText::new(format!("Connecting to {name}…")).color(p.text2);
        ui.add(Label::new(text).selectable(false));
    });
}

/// The breadcrumb path bar for `pane`, inset from the pane edges.
fn path_bar_row(ui: &mut Ui, pane: &mut PaneState, side: Side) -> Option<PathAction> {
    Frame::new()
        .inner_margin(Margin::symmetric(8, 4))
        .show(ui, |ui| {
            widgets::path_bar(
                ui,
                ("path", side as u8),
                &pane.cwd,
                pane.home.as_deref(),
                &mut pane.path_editing,
            )
        })
        .inner
}

/// Map a path-bar action onto the pane outcome the app already handles.
fn path_outcome(action: PathAction) -> PaneOutcome {
    match action {
        PathAction::Navigate(path) => PaneOutcome::GoTo(path),
        PathAction::Up => PaneOutcome::Up,
        PathAction::Home => PaneOutcome::Home,
        PathAction::Refresh => PaneOutcome::Refresh,
    }
}

/// Loading / error notes, then the table inside a drop zone. A drag from the
/// other pane gets a dashed accent outline while it hovers here.
/// `menu_label` names the row menu's transfer entry.
fn render_listing(
    ui: &mut Ui,
    pane: &mut PaneState,
    side: Side,
    menu_label: &str,
) -> Vec<PaneOutcome> {
    let p = theme::current(ui);
    let mut out = Vec::new();
    Frame::new()
        .inner_margin(Margin {
            left: 8,
            right: 16, // keep the date column clear of the scrollbar
            top: 0,
            bottom: 6,
        })
        .show(ui, |ui| {
            if pane.loading {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Loading…");
                });
            }
            if let Some(err) = &pane.error {
                ui.colored_label(p.err, err);
            }
            let (zone, dropped) = drop_zone::<DragPayload, _>(ui, Frame::NONE.fill(p.bg0), |ui| {
                out.extend(render_table(ui, pane, side, menu_label));
            });
            if let Some(payload) = dropped {
                out.push(PaneOutcome::Drop((*payload).clone()));
            }
            let from_other_pane = zone
                .response
                .dnd_hover_payload::<DragPayload>()
                .is_some_and(|payload| payload.side != side);
            if from_other_pane {
                widgets::dashed_outline(ui, zone.response.rect);
            }
        });
    out
}

/// Like `Ui::dnd_drop_zone`, but painted with the caller's `frame` instead of
/// egui's widget fills, so the listing stays on the deep ground. The accent
/// outline for a hovering drag is drawn by the caller.
///
/// Only a payload of type `P` is taken: egui's `take_payload` discards one of
/// any other type, so without the check the listing's file zone would swallow
/// a host dropped onto it before the pane body's host zone could see it.
fn drop_zone<P: Any + Send + Sync, R>(
    ui: &mut Ui,
    frame: Frame,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> (InnerResponse<R>, Option<Arc<P>>) {
    let mut frame = frame.begin(ui);
    let inner = add_contents(&mut frame.content_ui);
    let response = frame.allocate_space(ui);
    frame.paint(ui);
    let payload = DragAndDrop::has_payload_of_type::<P>(ui.ctx())
        .then(|| response.dnd_release_payload::<P>())
        .flatten();
    (InnerResponse { inner, response }, payload)
}

/// A host is hovering over a pane body: dashed accent outline around `rect`
/// and "Open <name> here" on a small backdrop, centred near its top.
fn host_drop_hint(ui: &Ui, rect: Rect, name: &str) {
    let p = theme::current(ui);
    widgets::dashed_outline(ui, rect);
    let painter = ui.painter();
    let font = FontId::new(13.0, theme::medium());
    let wrap = rect.width() - 2.0 * HOST_DROP_HINT_PAD;
    let galley = painter.layout(format!("Open {name} here"), font, p.accent, wrap);
    let anchor = pos2(rect.center().x, rect.top() + HOST_DROP_HINT_Y);
    let text_rect = Align2::CENTER_TOP.anchor_size(anchor, galley.size());
    painter.rect_filled(
        text_rect.expand(HOST_DROP_HINT_PAD),
        CornerRadius::same(4),
        p.bg2,
    );
    painter.galley(text_rect.min, galley, p.accent);
}

/// Column headers plus one row per entry; only the rows in view are laid out.
fn render_table(
    ui: &mut Ui,
    pane: &mut PaneState,
    side: Side,
    menu_label: &str,
) -> Vec<PaneOutcome> {
    let ctrl = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
    let mut clicked_sort: Option<SortCol> = None;
    let mut ev = RowEvents::default();
    let view: &PaneState = pane;
    // Keep listings dense: the table inserts item_spacing between rows.
    ui.spacing_mut().item_spacing.y = 2.0;
    TableBuilder::new(ui)
        .striped(true)
        .auto_shrink([false, false])
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::remainder().at_least(160.0).clip(true)) // name
        .column(Column::auto().at_least(80.0)) // size
        .column(Column::auto().at_least(130.0)) // modified
        .sense(Sense::click())
        .header(HEADER_H, |header| clicked_sort = table_header(header, view))
        .body(|body| {
            body.rows(ROW_H, view.entries.len(), |row| {
                table_row(row, view, side, ctrl, menu_label, &mut ev);
            });
        });
    // Apply the deferred changes now that the immutable borrow is released.
    if let Some((name, ctrl_held)) = ev.click.take() {
        apply_click(pane, name, ctrl_held);
    }
    if let Some(col) = clicked_sort {
        apply_sort(pane, col);
    }
    ev.out
}

/// The three sortable column headers; returns the column clicked, if any.
fn table_header(mut header: TableRow<'_, '_>, pane: &PaneState) -> Option<SortCol> {
    let mut clicked = None;
    let columns = [
        ("Name", SortCol::Name),
        ("Size", SortCol::Size),
        ("Modified", SortCol::Modified),
    ];
    for (title, col) in columns {
        header.col(|ui| {
            // Size and Modified values are right-aligned; their headers follow.
            let layout = if col == SortCol::Name {
                Layout::left_to_right(Align::Center)
            } else {
                Layout::right_to_left(Align::Center)
            };
            ui.with_layout(layout, |ui| {
                if sort_header(ui, title, pane, col) {
                    clicked = Some(col);
                }
            });
        });
    }
    clicked
}

/// One listing row: name (a drag source), size and modified cells, plus the
/// click / double-click / context-menu handling.
fn table_row(
    mut row: TableRow<'_, '_>,
    pane: &PaneState,
    side: Side,
    ctrl: bool,
    menu_label: &str,
    ev: &mut RowEvents,
) {
    let entry = &pane.entries[row.index()];
    let is_sel = pane.selected.contains(&entry.name);
    row.set_selected(is_sel);
    row.col(|ui| name_cell(ui, pane, side, entry, is_sel));
    row.col(|ui| {
        let size = if entry.is_dir {
            "—".to_owned()
        } else {
            localfs::human_size(entry.size)
        };
        mono_cell(ui, size);
    });
    row.col(|ui| mono_cell(ui, localfs::format_mtime(entry.mtime)));

    let resp = row.response();
    if resp.clicked() {
        ev.click = Some((entry.name.clone(), ctrl));
    }
    if resp.double_clicked() && entry.is_dir {
        ev.out.push(PaneOutcome::Enter(pane.full_path(&entry.name)));
    }
    resp.context_menu(|ui| row_menu(ui, pane, entry, is_sel, menu_label, ev));
}

/// Glyph + name, wrapped in a drag source carrying the selection (or just this entry).
fn name_cell(ui: &mut Ui, pane: &PaneState, side: Side, entry: &FileEntry, is_sel: bool) {
    let (glyph, label) = entry_label(entry, theme::current(ui));
    let paths = if is_sel {
        pane.selected_paths()
    } else {
        vec![pane.full_path(&entry.name)]
    };
    let payload = DragPayload {
        side,
        remote: pane.remote,
        paths,
    };
    // Keyed by side as well: both panes can be remote and list the same name.
    let id = Id::new((side as u8, pane.remote, "row", &entry.name));
    ui.dnd_drag_source(id, payload, |ui| {
        ui.horizontal(|ui| {
            ui.label(glyph);
            ui.add(Label::new(label).truncate().selectable(false));
        });
    });
}

/// Row context menu: Open, transfer (`menu_label`), live edit, Rename, Delete.
fn row_menu(
    ui: &mut Ui,
    pane: &PaneState,
    entry: &FileEntry,
    is_sel: bool,
    menu_label: &str,
    ev: &mut RowEvents,
) {
    let p = theme::current(ui);
    let name = &entry.name;
    if entry.is_dir && ui.button("Open").clicked() {
        ev.out.push(PaneOutcome::Enter(pane.full_path(name)));
        ui.close();
    }
    if ui.button(menu_label).clicked() {
        if !is_sel {
            ev.click = Some((name.clone(), false));
        }
        ev.out.push(PaneOutcome::Transfer);
        ui.close();
    }
    if !entry.is_dir && pane.remote && ui.button("Edit (live)").clicked() {
        ev.out.push(PaneOutcome::Edit(name.clone()));
        ui.close();
    }
    if ui.button("Rename").clicked() {
        ev.out.push(PaneOutcome::Rename(name.clone()));
        ui.close();
    }
    if ui.button(RichText::new("Delete").color(p.err)).clicked() {
        if !is_sel {
            ev.click = Some((name.clone(), false));
        }
        ev.out.push(PaneOutcome::DeleteSelected);
        ui.close();
    }
}

/// A plain click selects only `name`; a ctrl-click toggles it within the selection.
fn apply_click(pane: &mut PaneState, name: String, ctrl_held: bool) {
    if !ctrl_held {
        pane.selected.clear();
        pane.selected.insert(name);
    } else if !pane.selected.remove(&name) {
        pane.selected.insert(name);
    }
}

/// Clicking the active column flips its direction; another column sorts ascending.
fn apply_sort(pane: &mut PaneState, col: SortCol) {
    if pane.sort_col == col {
        pane.sort_asc = !pane.sort_asc;
    } else {
        pane.sort_col = col;
        pane.sort_asc = true;
    }
    pane.sort();
}

/// Uppercase column-header text with the sort arrow on the active column.
fn sort_label(base: &str, pane: &PaneState, col: SortCol, p: &theme::Palette) -> RichText {
    let arrow = match (pane.sort_col == col, pane.sort_asc) {
        (false, _) => "",
        (true, true) => " ▲",
        (true, false) => " ▼",
    };
    RichText::new(format!("{}{arrow}", base.to_uppercase()))
        .monospace()
        .size(HEADER_FONT_SIZE)
        .color(p.text3)
}

/// Compact clickable column header that fits inside `HEADER_H`. Returns true when clicked.
fn sort_header(ui: &mut Ui, base: &str, pane: &PaneState, col: SortCol) -> bool {
    let label = sort_label(base, pane, col, theme::current(ui));
    ui.spacing_mut().button_padding.y = 2.0;
    ui.spacing_mut().interact_size.y = HEADER_H - 4.0;
    ui.button(label).clicked()
}

/// Right-aligned secondary monospace cell (sizes, timestamps).
fn mono_cell(ui: &mut Ui, text: String) {
    let color = theme::current(ui).text2;
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.label(RichText::new(text).monospace().color(color));
    });
}

/// Glyph and styled name for a listing row: directories in the medium face,
/// symlinks italic and secondary, dotfiles tertiary.
fn entry_label(entry: &FileEntry, p: &theme::Palette) -> (RichText, RichText) {
    let name = RichText::new(entry.name.clone());
    let (glyph, text) = if entry.is_dir {
        ("📁", name.font(FontId::new(13.0, theme::medium())))
    } else if entry.symlink {
        ("🔗", name.italics().color(p.text2))
    } else {
        ("📄", name)
    };
    let text = if entry.name.starts_with('.') {
        text.color(p.text3)
    } else {
        text
    };
    (RichText::new(glyph).color(p.text2), text)
}

#[cfg(test)]
mod tests;
