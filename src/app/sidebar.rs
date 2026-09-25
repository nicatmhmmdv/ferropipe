//! Hosts sidebar: header actions, the filter box, grouped host rows and the
//! per-host context menu.

use super::editor::{ConnEditor, editor_from};
use super::*;
use crate::theme::{self, HOST_ROW_H};
use crate::widgets::{self, HostStatus};
use egui::{
    Align, CollapsingHeader, FontFamily, FontId, Id, Label, Layout, ScrollArea, TextEdit, Ui,
};

/// Left padding of a host row's contents: the 3px color stripe plus 8px.
const ROW_PAD_LEFT: f32 = 11.0;
/// Right padding of a host row's contents.
const ROW_PAD_RIGHT: f32 = 6.0;

/// Id of the host filter box, so F10 can hand it the keyboard.
pub(super) fn host_filter_id() -> Id {
    Id::new("host-filter")
}

impl FerropipeApp {
    /// The whole sidebar: header, filter, host list and the footer hint.
    pub(super) fn render_sidebar(&mut self, ctx: &egui::Context, ui: &mut Ui) {
        let p = theme::current(ui);
        ui.add_space(4.0);
        self.sidebar_header(ctx, ui);
        ui.add(
            TextEdit::singleline(&mut self.search)
                .id(host_filter_id())
                .hint_text("Filter hosts…")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(2.0);
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add_space(2.0);
            let hint = RichText::new("Double-click or drag onto a pane · Right-click: menu")
                .small()
                .color(p.text3);
            ui.add(Label::new(hint).wrap().selectable(false));
            ui.add_space(2.0);
            ui.with_layout(Layout::top_down(Align::Min), |ui| self.host_list(ctx, ui));
        });
    }

    /// "HOSTS" caption with the new-host and import buttons on the right.
    fn sidebar_header(&mut self, ctx: &egui::Context, ui: &mut Ui) {
        ui.horizontal(|ui| {
            widgets::section_label(ui, "Hosts");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::icon_button(ui, "+", "New host").clicked() {
                    self.editor = Some(ConnEditor::blank());
                }
                if widgets::icon_button(ui, "⇩", "Import from ~/.ssh/config").clicked() {
                    self.import_ssh_config(ctx);
                }
            });
        });
    }

    /// Scrollable host list: ungrouped hosts first, then one collapsible section per group.
    fn host_list(&mut self, ctx: &egui::Context, ui: &mut Ui) {
        let search = self.search.to_lowercase();
        let groups = self.store.groups();
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for c in self.filtered_hosts("", &search) {
                    self.host_row(ctx, ui, &c);
                }
                for g in &groups {
                    let conns = self.filtered_hosts(g, &search);
                    if conns.is_empty() {
                        continue;
                    }
                    CollapsingHeader::new(group_label(ui, g, conns.len()))
                        .id_salt(("group", g))
                        .default_open(!search.is_empty())
                        .show(ui, |ui| {
                            for c in conns {
                                self.host_row(ctx, ui, &c);
                            }
                        });
                }
            });
    }

    /// Hosts in exactly `group` whose name or address contains `search`.
    fn filtered_hosts(&self, group: &str, search: &str) -> Vec<Connection> {
        self.store
            .connections
            .iter()
            .filter(|c| c.group == group && host_matches(c, search))
            .cloned()
            .collect()
    }

    /// Connection state of `id` for the sidebar dot, plus the failure message if any.
    fn host_status(&self, id: Uuid) -> (HostStatus, Option<String>) {
        if self.connected == Some(id) || self.lconnected == Some(id) {
            return (HostStatus::Connected, None);
        }
        let right = self.connecting && self.pending_connect == Some(id);
        let left = self.lconnecting && self.lpending == Some(id);
        if right || left {
            return (HostStatus::Connecting, None);
        }
        match self.host_errors.get(&id) {
            Some(err) => (HostStatus::Failed, Some(err.clone())),
            None => (HostStatus::Idle, None),
        }
    }

    /// One host row: stripe, status dot, name and protocol chip. Click selects,
    /// double-click opens a session, right-click shows the host menu, and the
    /// row can be dragged onto a pane to browse the host there.
    fn host_row(&mut self, ctx: &egui::Context, ui: &mut Ui, c: &Connection) {
        let p = theme::current(ui);
        let (status, err) = self.host_status(c.id);
        let selected = self.selected_conn == Some(c.id);
        // The row senses click *and* drag itself: a drag-source wrapper laid
        // over a click-only row makes egui drop the click (see widgets::host).
        let payload = HostPayload { id: c.id };
        let (rect, resp) =
            widgets::drag_row_slot(ui, HOST_ROW_H, selected, p.bg2, payload, &c.name);
        widgets::color_stripe(ui, rect, c.color);
        let mut row = widgets::row_ui(ui, rect, ROW_PAD_LEFT, ROW_PAD_RIGHT);
        host_row_contents(&mut row, c, status, err.as_deref());
        let resp = resp.on_hover_text(c.target());
        if resp.clicked() {
            self.selected_conn = Some(c.id);
        }
        // Double-click activates: opens a new session (terminal for SSH, a
        // maximized RDP window for Windows, the file pane for SMB/WinRM).
        if resp.double_clicked() {
            self.selected_conn = Some(c.id);
            self.open_session(ctx, c.id);
        }
        resp.context_menu(|ui| self.host_menu(ctx, ui, c));
    }

    /// The host context menu: open / browse / RDP clients / edit / duplicate / delete.
    fn host_menu(&mut self, ctx: &egui::Context, ui: &mut Ui, c: &Connection) {
        let p = theme::current(ui);
        if ui.button("Open session").clicked() {
            self.selected_conn = Some(c.id);
            self.open_session(ctx, c.id);
            ui.close();
        }
        // File-capable kinds can also open the in-app dual-pane browser.
        if c.kind.browses_files() && ui.button("Browse files").clicked() {
            self.selected_conn = Some(c.id);
            self.connect(ctx, c.id);
            ui.close();
        }
        // RDP connections can pick either client explicitly, regardless of kind.
        if matches!(c.kind, ConnectionKind::Rdp | ConnectionKind::RdpNative) {
            self.host_menu_rdp(ctx, ui, c);
        }
        if ui.button("Edit").clicked() {
            self.editor = Some(editor_from(c, &self.vault));
            ui.close();
        }
        if ui.button("Duplicate").clicked() {
            self.duplicate_host(c);
            ui.close();
        }
        ui.separator();
        if ui.button(RichText::new("Delete").color(p.err)).clicked() {
            self.delete_host(c.id);
            ui.close();
        }
    }

    /// Remmina / native RDP / remote-access entries of the host menu.
    fn host_menu_rdp(&mut self, ctx: &egui::Context, ui: &mut Ui, c: &Connection) {
        let mut c = c.clone();
        c.host = c.effective_host();
        if ui.button("Open in Remmina").clicked() {
            self.selected_conn = Some(c.id);
            self.launch_remmina(ctx, &c);
            ui.close();
        }
        if ui.button("Open in native RDP (experimental)").clicked() {
            self.selected_conn = Some(c.id);
            self.open_native_rdp(ctx, &c);
            ui.close();
        }
        if ui.button("Remote access (SSH/WinRM)…").clicked() {
            self.selected_conn = Some(c.id);
            self.open_ssh_panel(c.id);
            ui.close();
        }
    }

    /// Save a copy of `c` named "<name> (copy)".
    fn duplicate_host(&mut self, c: &Connection) {
        let mut nc = c.clone();
        nc.id = Uuid::new_v4();
        nc.name = format!("{} (copy)", c.name);
        self.store.connections.push(nc);
        self.save_store();
    }

    /// Remove a saved host, disconnecting any live session that uses it first.
    fn delete_host(&mut self, id: Uuid) {
        if self.connected == Some(id) {
            self.worker.send(Command::Disconnect);
            self.connected = None;
            self.remote.entries.clear();
        }
        if self.lconnected == Some(id) {
            self.lworker.send(Command::Disconnect);
        }
        self.store.connections.retain(|x| x.id != id);
        self.save_store();
    }
}

/// Case-insensitive match of the (already lowercased) `search` against a host's name or address.
fn host_matches(c: &Connection, search: &str) -> bool {
    search.is_empty()
        || c.name.to_lowercase().contains(search)
        || c.host.to_lowercase().contains(search)
}

/// "GROUP  3": uppercased group name and host count in the small monospace caption style.
fn group_label(ui: &Ui, group: &str, count: usize) -> RichText {
    let p = theme::current(ui);
    RichText::new(format!("{}  {count}", group.to_uppercase()))
        .font(FontId::new(10.5, FontFamily::Monospace))
        .color(p.text3)
}

/// Status dot, truncating name and right-aligned protocol chip inside a host row.
fn host_row_contents(ui: &mut Ui, c: &Connection, status: HostStatus, err: Option<&str>) {
    let p = theme::current(ui);
    ui.spacing_mut().item_spacing.x = 6.0;
    let dot = widgets::status_dot(ui, status);
    if let Some(err) = err {
        dot.on_hover_text(err);
    }
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        widgets::protocol_chip(ui, c.kind).on_hover_text(c.kind.label());
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            let name = RichText::new(&c.name)
                .font(FontId::new(13.0, theme::medium()))
                .color(p.text);
            ui.add(Label::new(name).truncate().selectable(false));
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(name: &str, host: &str) -> Connection {
        Connection::new(name, host, "root")
    }

    #[test]
    fn host_matches_everything_when_search_is_empty() {
        // Arrange
        let c = conn("web01", "10.0.0.1");
        // Act
        let matched = host_matches(&c, "");
        // Assert
        assert!(matched);
    }

    #[test]
    fn host_matches_name_case_insensitively() {
        let c = conn("Web01-PROD", "10.0.0.1");
        assert!(host_matches(&c, "web01-prod"));
        assert!(host_matches(&c, "prod"));
    }

    #[test]
    fn host_matches_on_host_address() {
        let c = conn("web01", "10.0.0.1");
        assert!(host_matches(&c, "0.0.1"));
    }

    #[test]
    fn host_matches_nothing_when_neither_name_nor_host_contains_search() {
        let c = conn("web01", "10.0.0.1");
        assert!(!host_matches(&c, "db"));
    }
}
