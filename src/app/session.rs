//! Sessions: connecting and disconnecting the panes' workers, the events they
//! report back, and the `side_*` helpers that address one pane's state.

use super::*;

impl FerropipeApp {
    /// Connect the in-app file browser to `id`. Only SSH/SMB/WinRM browse files;
    /// RDP kinds open a session window instead, so they delegate to open_session.
    pub(super) fn connect(&mut self, ctx: &egui::Context, id: Uuid) {
        let Some(mut conn) = self.store.connections.iter().find(|c| c.id == id).cloned() else {
            return;
        };
        if matches!(conn.kind, ConnectionKind::Rdp | ConnectionKind::RdpNative) {
            self.open_session(ctx, id);
            return;
        }
        // Resolve the effective host (falls back to the name); bail clearly if blank.
        conn.host = conn.effective_host();
        if conn.host.is_empty() {
            self.toast(
                ctx,
                format!("{}: no host set — edit the connection", conn.name),
                true,
            );
            return;
        }
        if self.connecting {
            self.toast(ctx, "Already connecting — please wait…", true);
            return;
        }
        // If already connected, drop the current session first. Commands are processed
        // in order, so the worker handles Disconnect and then the new Connect.
        if self.connected.is_some() {
            self.worker.send(Command::Disconnect);
            self.connected = None;
            self.remote.entries.clear();
            self.remote.selected.clear();
            self.remote.home = None;
        }
        self.host_errors.remove(&id);
        let secret = match &conn.auth {
            AuthMethod::Password { password_enc } => self.vault.decrypt(password_enc).ok(),
            AuthMethod::Key { passphrase_enc, .. } => passphrase_enc
                .as_ref()
                .and_then(|b| self.vault.decrypt(b).ok()),
            AuthMethod::Agent => None,
        };
        self.selected_conn = Some(id);
        self.pending_connect = Some(id);
        self.connecting = true;
        self.status = format!("Connecting to {}…", conn.target());
        self.toast(ctx, format!("Connecting to {}", conn.name), false);
        self.worker.send(Command::Connect {
            conn: Box::new(conn),
            secret,
        });
    }

    /// The primary "activate" gesture for a saved connection: open a fresh
    /// session in its own window. SSH opens an Alacritty terminal; Windows/RDP
    /// opens a maximized native RDP window; SMB/WinRM open the in-app file pane.
    pub(super) fn open_session(&mut self, ctx: &egui::Context, id: Uuid) {
        let Some(mut conn) = self.store.connections.iter().find(|c| c.id == id).cloned() else {
            return;
        };
        self.selected_conn = Some(id);
        // Resolve the effective host (falls back to the name); bail clearly if blank.
        conn.host = conn.effective_host();
        if conn.host.is_empty() {
            self.toast(
                ctx,
                format!("{}: no host set — edit the connection", conn.name),
                true,
            );
            return;
        }
        match conn.kind {
            ConnectionKind::Ssh => {
                let target = format!("{}@{}", conn.username, conn.host);
                let identity = match &conn.auth {
                    AuthMethod::Key { private_key, .. } if !private_key.is_empty() => {
                        Some(private_key.clone())
                    }
                    _ => None,
                };
                match crate::external::open_alacritty_ssh(&target, conn.port, identity.as_deref()) {
                    Ok(()) => self.toast(ctx, format!("Terminal → {}", conn.name), false),
                    Err(e) => self.toast(ctx, format!("terminal: {e:#}"), true),
                }
            }
            // The mature path: launch Remmina (fast, renders GFX/H.264). This is
            // what a plain "RDP" connection is for.
            ConnectionKind::Rdp => self.launch_remmina(ctx, &conn),
            // The experimental in-app client — explicit opt-in via the RdpNative kind.
            ConnectionKind::RdpNative => self.open_native_rdp(ctx, &conn),
            ConnectionKind::Smb | ConnectionKind::WinRm => {
                // File-oriented kinds open the in-app dual-pane browser.
                self.connect(ctx, id);
            }
        }
    }

    /// Launch the external Remmina client for `conn` (the mature RDP path).
    pub(super) fn launch_remmina(&mut self, ctx: &egui::Context, conn: &Connection) {
        let password = match &conn.auth {
            AuthMethod::Password { password_enc } => {
                self.vault.decrypt(password_enc).unwrap_or_default()
            }
            _ => String::new(),
        };
        let config_dir = self
            .store_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        match crate::rdp::launch(conn, &password, &config_dir) {
            Ok(_) => self.toast(ctx, format!("Launched Remmina → {}", conn.name), false),
            Err(e) => self.toast(ctx, format!("RDP: {e:#}"), true),
        }
    }

    /// Open the experimental in-app native RDP client for `conn` in its own
    /// maximized window. Still work-in-progress: no EGFX/H.264 decode yet, so
    /// GFX-mandatory hosts render black — use Remmina for those.
    pub(super) fn open_native_rdp(&mut self, ctx: &egui::Context, conn: &Connection) {
        // Don't spawn a second window for a connection that's already open.
        let key = conn.id.to_string();
        if self.native_rdp.is_open(&key) {
            self.toast(ctx, format!("{} is already open", conn.name), false);
            return;
        }
        let password = match &conn.auth {
            AuthMethod::Password { password_enc } => {
                self.vault.decrypt(password_enc).unwrap_or_default()
            }
            _ => String::new(),
        };
        // The native client authenticates via NLA and can't prompt, so a missing
        // password will just fail — warn rather than silently retry.
        if password.is_empty() {
            self.toast(
                ctx,
                format!("No saved password for {} — auth will fail", conn.name),
                true,
            );
        }
        let mut params =
            ferropipe_rdp::session::SessionParams::new(&conn.host, &conn.username, &password);
        params.port = conn.port;
        params.domain = conn.domain.clone();
        self.native_rdp
            .open(params, format!("RDP — {}", conn.name), key);
        self.toast(ctx, format!("Opening native RDP → {}", conn.name), false);
    }

    pub(super) fn disconnect(&mut self) {
        self.worker.send(Command::Disconnect);
        self.connected = None;
        self.connected_at = None;
        self.remote.entries.clear();
        self.remote.selected.clear();
        self.remote.home = None;
        self.status = String::new();
    }

    fn decrypt_secret(&self, conn: &Connection) -> Option<String> {
        match &conn.auth {
            AuthMethod::Password { password_enc } => self.vault.decrypt(password_enc).ok(),
            AuthMethod::Key { passphrase_enc, .. } => passphrase_enc
                .as_ref()
                .and_then(|b| self.vault.decrypt(b).ok()),
            AuthMethod::Agent => None,
        }
    }

    pub(super) fn connect_left(&mut self, ctx: &egui::Context, id: Uuid) {
        let Some(mut conn) = self.store.connections.iter().find(|c| c.id == id).cloned() else {
            return;
        };
        if matches!(conn.kind, ConnectionKind::Rdp | ConnectionKind::RdpNative) {
            self.toast(
                ctx,
                "RDP can't be used as a file pane — use SSH/SMB/WinRM",
                true,
            );
            return;
        }
        conn.host = conn.effective_host();
        if conn.host.is_empty() {
            self.toast(
                ctx,
                format!("{}: no host set — edit the connection", conn.name),
                true,
            );
            return;
        }
        if self.lconnecting {
            self.toast(ctx, "Left pane already connecting…", true);
            return;
        }
        if !self.left_remote {
            self.left_saved_cwd = self.local.cwd.clone();
        }
        if self.lconnected.is_some() {
            self.lworker.send(Command::Disconnect);
            self.lconnected = None;
        }
        let name = conn.name.clone();
        let secret = self.decrypt_secret(&conn);
        self.host_errors.remove(&id);
        self.left_remote = true;
        self.local.remote = true;
        self.local.entries.clear();
        self.local.selected.clear();
        self.local.home = None;
        self.lpending = Some(id);
        self.lconnecting = true;
        self.lworker.send(Command::Connect {
            conn: Box::new(conn),
            secret,
        });
        self.toast(ctx, format!("Left pane ← {name}"), false);
    }

    /// Revert the left pane from a remote session back to the local filesystem.
    pub(super) fn revert_left_to_local(&mut self) {
        self.left_remote = false;
        self.lconnected = None;
        self.lconnecting = false;
        self.local.remote = false;
        self.local.loading = false;
        self.local.entries.clear();
        self.local.selected.clear();
        self.local.cwd = if self.left_saved_cwd.is_empty() {
            local_home()
        } else {
            self.left_saved_cwd.clone()
        };
        self.local.home = Some(local_home());
        let show_hidden = self.store.settings.show_hidden;
        reload_local(&mut self.local, show_hidden);
    }

    pub(super) fn drain_events(&mut self, ctx: &egui::Context) {
        // Collect first (try_recv borrows the receiver) then handle with &mut self.
        let mut right = Vec::new();
        while let Ok(ev) = self.events.try_recv() {
            right.push(ev);
        }
        let mut left = Vec::new();
        while let Ok(ev) = self.levents.try_recv() {
            left.push(ev);
        }
        for ev in right {
            self.handle_event(ctx, Side::Right, ev);
        }
        for ev in left {
            self.handle_event(ctx, Side::Left, ev);
        }
    }

    fn handle_event(&mut self, ctx: &egui::Context, side: Side, ev: Event) {
        match ev {
            Event::Connected { home, kind } => self.on_connected(ctx, side, home, kind),
            Event::RemoteListing { path, entries } => self.on_listing(side, path, entries),
            Event::Progress(p) => self.on_progress(p),
            Event::TransferDone { id, ok } => self.on_transfer_done(ctx, side, id, ok),
            Event::OpDone { refresh } => self.on_op_done(side, refresh),
            Event::ExecOutput { line, is_err, .. } => {
                self.console.push((line, is_err));
                if self.console.len() > 5000 {
                    self.console.drain(0..self.console.len() - 5000);
                }
            }
            Event::ExecDone { code, .. } => {
                self.exec_running = false;
                self.console.push((format!("[exit {code}]"), code != 0));
            }
            // A transfer failure arrives as Error then TransferDone { ok: false }:
            // keep the message for that row instead of toasting it twice.
            Event::Error { context, message } if transfers::is_transfer_context(&context) => {
                self.status = format!("{context}: {message}");
                let now = ctx.input(|i| i.time);
                self.note_transfer_error(side, now, message);
            }
            Event::Error { context, message } => {
                self.on_error(ctx, side, format!("{context}: {message}"));
            }
            Event::Disconnected => self.on_disconnected(side),
        }
    }

    /// A session came up: remember its home, clear any stale failure and list it.
    fn on_connected(&mut self, ctx: &egui::Context, side: Side, home: String, kind: &'static str) {
        if side == Side::Left {
            self.lconnecting = false;
            self.lconnected = self.lpending.take();
            self.lkind = kind;
            self.local.remote = true;
            self.local.loading = true;
            self.local.home = Some(home.clone());
        } else {
            self.connecting = false;
            self.connected = self.pending_connect.take().or(self.selected_conn);
            self.connected_at = Some(ctx.input(|i| i.time));
            self.active_kind = kind;
            self.remote.loading = true;
            self.remote.home = Some(home.clone());
            self.status = format!("Connected ({kind}) — {home}");
        }
        if let Some(id) = self.side_host(side) {
            self.host_errors.remove(&id);
        }
        self.side_send(side, Command::ListRemote { path: home });
    }

    fn on_listing(&mut self, side: Side, path: String, mut entries: Vec<FileEntry>) {
        let pane = self.side_pane_mut(side);
        pane.cwd = path;
        std::mem::swap(&mut pane.entries, &mut entries);
        pane.selected.clear();
        pane.loading = false;
        pane.error = None;
        pane.sort();
    }

    /// A mutation finished on `side`: relist the directory it names (or the current one).
    fn on_op_done(&mut self, side: Side, refresh: Option<String>) {
        if !self.side_connected(side) {
            return;
        }
        let path = refresh.unwrap_or_else(|| self.side_pane(side).cwd.clone());
        if side == Side::Left {
            self.local.loading = true;
        }
        self.side_send(side, Command::ListRemote { path });
    }

    /// A worker error: clear the side's busy flags and, when a connect was in
    /// flight, remember the failure for the sidebar's red dot.
    fn on_error(&mut self, ctx: &egui::Context, side: Side, msg: String) {
        let failed = self.pending_host(side);
        if side == Side::Left {
            self.lconnecting = false;
            self.local.loading = false;
            // A failed left-pane connect never emits Disconnected — recover here.
            if self.lconnected.is_none() {
                self.revert_left_to_local();
            }
        } else {
            self.connecting = false;
            self.remote.loading = false;
            self.status = msg.clone();
        }
        self.toast(ctx, msg.clone(), true);
        if let Some(id) = failed {
            self.host_errors.insert(id, msg);
        }
    }

    fn on_disconnected(&mut self, side: Side) {
        if side == Side::Left {
            self.revert_left_to_local();
            return;
        }
        self.connected = None;
        self.connected_at = None;
        self.remote.entries.clear();
        self.remote.selected.clear();
        self.remote.cwd = "/".into();
        self.remote.home = None;
        self.remote.error = None;
        self.status = "Disconnected".into();
    }

    /// The host connected on `side`, if any.
    pub(super) fn side_host(&self, side: Side) -> Option<Uuid> {
        match side {
            Side::Left => self.lconnected,
            Side::Right => self.connected,
        }
    }

    /// The host whose connect is still in flight on `side`, if any.
    fn pending_host(&self, side: Side) -> Option<Uuid> {
        match side {
            Side::Left => self.lpending.filter(|_| self.lconnecting),
            Side::Right => self.pending_connect.filter(|_| self.connecting),
        }
    }

    pub(super) fn side_is_remote(&self, side: Side) -> bool {
        match side {
            Side::Left => self.left_remote,
            Side::Right => true,
        }
    }

    pub(super) fn side_pane(&self, side: Side) -> &PaneState {
        match side {
            Side::Left => &self.local,
            Side::Right => &self.remote,
        }
    }

    pub(super) fn side_pane_mut(&mut self, side: Side) -> &mut PaneState {
        match side {
            Side::Left => &mut self.local,
            Side::Right => &mut self.remote,
        }
    }

    pub(super) fn side_send(&self, side: Side, cmd: Command) {
        match side {
            Side::Left => self.lworker.send(cmd),
            Side::Right => self.worker.send(cmd),
        }
    }

    pub(super) fn side_connected(&self, side: Side) -> bool {
        match side {
            Side::Left => self.lconnected.is_some(),
            Side::Right => self.connected.is_some(),
        }
    }

    pub(super) fn navigate(&mut self, side: Side, path: String) {
        if self.side_is_remote(side) {
            if !self.side_connected(side) {
                return;
            }
            self.side_pane_mut(side).loading = true;
            self.side_send(side, Command::ListRemote { path });
        } else {
            self.side_pane_mut(side).cwd = path;
            let show_hidden = self.store.settings.show_hidden;
            reload_local(self.side_pane_mut(side), show_hidden);
        }
    }
}
