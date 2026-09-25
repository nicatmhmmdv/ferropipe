//! Dispatch of the actions a pane emits (navigation, transfers, prompts,
//! deletion and live editing) onto the app state.

use super::*;

/// Why an RDP host can't be dropped onto a file pane.
const RDP_DROP_HINT: &str = "RDP hosts open in their own window — double-click the host instead";

/// What dropping a host of `kind` onto a file pane does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostDrop {
    /// Browse the host's files in that pane.
    Browse,
    /// The kind has no file browser; the message says what to do instead.
    Reject(&'static str),
}

/// Only file-capable kinds can back a pane; RDP kinds open their own window.
fn host_drop_action(kind: ConnectionKind) -> HostDrop {
    if kind.browses_files() {
        HostDrop::Browse
    } else {
        HostDrop::Reject(RDP_DROP_HINT)
    }
}

impl FerropipeApp {
    /// Apply the actions a pane emitted this frame.
    pub(super) fn handle_pane(
        &mut self,
        ctx: &egui::Context,
        side: Side,
        outcomes: Vec<PaneOutcome>,
    ) {
        for o in outcomes {
            match o {
                PaneOutcome::Enter(path) | PaneOutcome::GoTo(path) => self.navigate(side, path),
                PaneOutcome::Up => self.go_up(side),
                PaneOutcome::Home => self.go_home(side),
                PaneOutcome::Refresh => self.refresh_pane(side),
                PaneOutcome::Drop(payload) => self.start_transfer(ctx, payload, side),
                PaneOutcome::Transfer => self.transfer_selection(ctx, side),
                PaneOutcome::NewFolder => self.prompt_new_folder(side),
                PaneOutcome::NewFile => self.prompt_new_file(side),
                PaneOutcome::DeleteSelected => self.delete_selection(ctx, side),
                PaneOutcome::Rename(name) => self.prompt_rename(side, name),
                PaneOutcome::Edit(name) => self.edit_entry(ctx, side, &name),
                PaneOutcome::OpenHost(id) => self.open_host(ctx, side, id),
            }
        }
    }

    /// Navigate to the parent of the pane's current directory.
    fn go_up(&mut self, side: Side) {
        let remote = self.side_is_remote(side);
        let parent = parent_dir(&self.side_pane(side).cwd, remote);
        self.navigate(side, parent);
    }

    /// Jump to the session's home (remote) or the local home directory.
    fn go_home(&mut self, side: Side) {
        if !self.side_is_remote(side) {
            self.navigate(side, local_home());
        } else if self.side_connected(side) {
            self.side_pane_mut(side).loading = true;
            self.side_send(side, Command::ListRemote { path: ".".into() });
        }
    }

    /// Relist the pane's current directory.
    fn refresh_pane(&mut self, side: Side) {
        if !self.side_is_remote(side) {
            let show_hidden = self.store.settings.show_hidden;
            reload_local(self.side_pane_mut(side), show_hidden);
        } else if self.side_connected(side) {
            let cwd = self.side_pane(side).cwd.clone();
            self.side_pane_mut(side).loading = true;
            self.side_send(side, Command::ListRemote { path: cwd });
        }
    }

    /// Send the pane's selection to the other pane.
    fn transfer_selection(&mut self, ctx: &egui::Context, side: Side) {
        let payload = DragPayload {
            side,
            remote: self.side_is_remote(side),
            paths: self.side_pane(side).selected_paths(),
        };
        self.start_transfer(ctx, payload, side.other());
    }

    fn prompt_new_folder(&mut self, side: Side) {
        self.prompt = Some(Prompt {
            title: "New folder".into(),
            label: "Folder name".into(),
            value: String::new(),
            kind: PromptKind::NewFolder(side),
        });
    }

    fn prompt_new_file(&mut self, side: Side) {
        self.prompt = Some(Prompt {
            title: "New file".into(),
            label: "File name".into(),
            value: String::new(),
            kind: PromptKind::NewFile(side),
        });
    }

    fn prompt_rename(&mut self, side: Side, name: String) {
        self.prompt = Some(Prompt {
            title: "Rename".into(),
            label: "New name".into(),
            value: name.clone(),
            kind: PromptKind::Rename(side, name),
        });
    }

    /// Delete the selection, asking first when the setting requires it.
    fn delete_selection(&mut self, ctx: &egui::Context, side: Side) {
        let paths = self.side_pane(side).selected_paths();
        if paths.is_empty() {
            return;
        }
        if self.store.settings.confirm_delete {
            self.confirm_delete = Some((side, paths));
        } else {
            self.do_delete(ctx, side, paths);
        }
    }

    /// Open a remote entry in the local editor; local panes have no live edit.
    fn edit_entry(&mut self, ctx: &egui::Context, side: Side, name: &str) {
        if self.side_is_remote(side) && self.side_connected(side) {
            self.start_edit(ctx, side, name);
        }
    }

    /// A host dropped onto `side`: browse it in that pane, or say why it can't be.
    fn open_host(&mut self, ctx: &egui::Context, side: Side, id: Uuid) {
        let Some(kind) = self
            .store
            .connections
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.kind)
        else {
            return;
        };
        self.selected_conn = Some(id);
        // Dropping a host onto the pane that already shows it is a no-op; a
        // reconnect would only clear the listing and selection.
        let already_open = match side {
            Side::Left => self.lconnected == Some(id),
            Side::Right => self.connected == Some(id),
        };
        if already_open {
            return;
        }
        match (host_drop_action(kind), side) {
            (HostDrop::Reject(msg), _) => self.toast(ctx, msg, true),
            (HostDrop::Browse, Side::Left) => self.connect_left(ctx, id),
            (HostDrop::Browse, Side::Right) => self.connect(ctx, id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RDP_HINT: &str = "RDP hosts open in their own window — double-click the host instead";

    #[test]
    fn host_drop_action_browses_an_ssh_host() {
        // Arrange
        let kind = ConnectionKind::Ssh;
        // Act
        let action = host_drop_action(kind);
        // Assert
        assert_eq!(action, HostDrop::Browse);
    }

    #[test]
    fn host_drop_action_browses_an_smb_host() {
        assert_eq!(host_drop_action(ConnectionKind::Smb), HostDrop::Browse);
    }

    #[test]
    fn host_drop_action_browses_a_winrm_host() {
        assert_eq!(host_drop_action(ConnectionKind::WinRm), HostDrop::Browse);
    }

    #[test]
    fn host_drop_action_rejects_an_rdp_host_with_the_double_click_hint() {
        assert_eq!(
            host_drop_action(ConnectionKind::Rdp),
            HostDrop::Reject(RDP_HINT)
        );
    }

    #[test]
    fn host_drop_action_rejects_a_native_rdp_host_with_the_double_click_hint() {
        assert_eq!(
            host_drop_action(ConnectionKind::RdpNative),
            HostDrop::Reject(RDP_HINT)
        );
    }
}
