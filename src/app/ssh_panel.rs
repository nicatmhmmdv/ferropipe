//! The "Remote access" panel for a Windows host: toggle its OpenSSH / WinRM
//! services over whichever channel is reachable, then push files over SFTP.

use super::*;
use egui::Ui;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// State for the "SSH file transfer" panel driving a Windows host: toggle the
/// OpenSSH server via WinRM, then transfer files over SFTP. Long operations run
/// on a background thread and report through the shared `status`/`busy` handles.
pub(super) struct SshPanel {
    conn: Connection,
    password: String,
    remote_dir: String,
    files: Vec<PathBuf>,
    status: Arc<Mutex<String>>,
    busy: Arc<AtomicBool>,
}

impl SshPanel {
    fn set_status(&self, msg: impl Into<String>) {
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = msg.into();
    }

    /// Run `op` on a background thread with a busy guard, reporting `working`
    /// first and the op's own result string on completion. Ignored if busy.
    fn run_bg<F>(&self, working: &str, op: F)
    where
        F: FnOnce() -> String + Send + 'static,
    {
        if self.busy.swap(true, Ordering::SeqCst) {
            return; // already running something
        }
        self.set_status(working);
        let status = self.status.clone();
        let busy = self.busy.clone();
        std::thread::spawn(move || {
            let msg = op();
            *status.lock().unwrap_or_else(|e| e.into_inner()) = msg;
            busy.store(false, Ordering::SeqCst);
        });
    }
}

/// One enable/disable action on a host service, offered as a panel button.
#[derive(Clone, Copy)]
struct ServiceAction {
    label: &'static str,
    working: &'static str,
    ok_msg: &'static str,
    enable: bool,
    toggle: fn(&Connection, &str, bool) -> anyhow::Result<String>,
}

const SSH_ON: ServiceAction = ServiceAction {
    label: "Enable SSH",
    working: "Enabling SSH on host…",
    ok_msg: "✓ SSH enabled on the host — you can transfer now.",
    enable: true,
    toggle: crate::winssh::set_ssh_enabled,
};

const SSH_OFF: ServiceAction = ServiceAction {
    label: "Disable SSH",
    working: "Disabling SSH on host…",
    ok_msg: "✓ SSH disabled on the host.",
    enable: false,
    toggle: crate::winssh::set_ssh_enabled,
};

const WINRM_ON: ServiceAction = ServiceAction {
    label: "Enable WinRM",
    working: "Enabling WinRM on host…",
    ok_msg: "✓ WinRM enabled on the host.",
    enable: true,
    toggle: crate::winssh::set_winrm_enabled,
};

const WINRM_OFF: ServiceAction = ServiceAction {
    label: "Disable WinRM",
    working: "Disabling WinRM on host…",
    ok_msg: "✓ WinRM disabled on the host.",
    enable: false,
    toggle: crate::winssh::set_winrm_enabled,
};

impl FerropipeApp {
    /// Open the remote-access panel for a Windows host: toggle SSH/WinRM and
    /// transfer files over SFTP, all with the connection's saved credentials.
    pub(super) fn open_ssh_panel(&mut self, id: Uuid) {
        let Some(mut conn) = self.store.connections.iter().find(|c| c.id == id).cloned() else {
            return;
        };
        conn.host = conn.effective_host();
        let password = match &conn.auth {
            AuthMethod::Password { password_enc } => {
                self.vault.decrypt(password_enc).unwrap_or_default()
            }
            _ => String::new(),
        };
        self.ssh_panel = Some(SshPanel {
            conn,
            password,
            remote_dir: "Desktop".to_string(),
            files: Vec::new(),
            status: Arc::new(Mutex::new(String::new())),
            busy: Arc::new(AtomicBool::new(false)),
        });
    }

    /// Draw the remote-access panel (SSH/WinRM toggles + SFTP transfer).
    pub(super) fn render_ssh_panel(&mut self, ctx: &egui::Context) {
        let Some(panel) = &mut self.ssh_panel else {
            return;
        };
        let busy = panel.busy.load(Ordering::SeqCst);
        let status = panel
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut open = true;
        egui::Window::new(format!("Remote access — {}", panel.conn.name))
            .collapsible(false)
            .resizable(true)
            .default_width(460.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "Toggle the host's SSH / WinRM services and transfer files, using this \
                         connection's Windows credentials over whichever channel is reachable.",
                    )
                    .weak(),
                );
                ui.add_space(4.0);
                service_rows(ui, panel, busy);
                ui.separator();
                transfer_section(ui, panel, busy);
                ui.separator();
                status_line(ui, busy, &status);
            });
        if busy {
            ctx.request_repaint();
        }
        if !open {
            self.ssh_panel = None;
        }
    }
}

/// The OpenSSH / WinRM rows, each with an enable and a disable button.
fn service_rows(ui: &mut Ui, panel: &SshPanel, busy: bool) {
    egui::Grid::new("remote_access_grid")
        .num_columns(2)
        .spacing([10.0, 8.0])
        .show(ui, |ui| {
            ui.label("OpenSSH server");
            ui.horizontal(|ui| {
                service_button(ui, panel, busy, SSH_ON);
                service_button(ui, panel, busy, SSH_OFF);
            });
            ui.end_row();

            ui.label("WinRM");
            ui.horizontal(|ui| {
                service_button(ui, panel, busy, WINRM_ON);
                service_button(ui, panel, busy, WINRM_OFF);
            });
            ui.end_row();
        });
}

/// One service button; a click runs its toggle on the background thread.
fn service_button(ui: &mut Ui, panel: &SshPanel, busy: bool, action: ServiceAction) {
    if !ui
        .add_enabled(!busy, egui::Button::new(action.label))
        .clicked()
    {
        return;
    }
    let (c, p) = (panel.conn.clone(), panel.password.clone());
    let ServiceAction {
        ok_msg,
        enable,
        toggle,
        ..
    } = action;
    panel.run_bg(action.working, move || match toggle(&c, &p, enable) {
        Ok(_) => ok_msg.into(),
        Err(e) => format!("✗ {e:#}"),
    });
}

/// The SFTP section: remote folder, the picked files and the transfer button.
fn transfer_section(ui: &mut Ui, panel: &mut SshPanel, busy: bool) {
    ui.label(RichText::new("Transfer files (needs SSH enabled)").strong());
    ui.horizontal(|ui| {
        ui.label("Remote folder:");
        ui.add(egui::TextEdit::singleline(&mut panel.remote_dir).desired_width(160.0))
            .on_hover_text(
                "Relative to the user's home (e.g. Desktop) or an absolute path like C:/temp",
            );
        if ui.button("Add files…").clicked()
            && let Some(paths) = rfd::FileDialog::new().pick_files()
        {
            panel.files.extend(paths);
        }
        if !panel.files.is_empty() && ui.button("Clear").clicked() {
            panel.files.clear();
        }
    });
    if panel.files.is_empty() {
        ui.label(RichText::new("No files selected.").weak());
        return;
    }
    for f in &panel.files {
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        ui.label(format!("• {name}"));
    }
    let can_send = !busy && !panel.files.is_empty();
    let send = egui::Button::new(format!("Transfer {} file(s)", panel.files.len()));
    if ui.add_enabled(can_send, send).clicked() {
        send_files(panel);
    }
}

/// Push the picked files to the remote folder on the background thread.
fn send_files(panel: &SshPanel) {
    let (conn, pw) = (panel.conn.clone(), panel.password.clone());
    let (dir, files) = (panel.remote_dir.clone(), panel.files.clone());
    panel.run_bg(
        "Transferring (SFTP, or WinRM if SSH is off)…",
        move || match crate::winssh::transfer(&conn, &pw, &dir, &files) {
            Ok((chan, r)) if r.failures.is_empty() => {
                format!("✓ Transferred {} file(s) to {dir} over {chan}.", r.ok)
            }
            Ok((chan, r)) => format!(
                "{chan}: transferred {} ok, {} failed: {}",
                r.ok,
                r.failures.len(),
                r.failures
                    .iter()
                    .map(|(n, e)| format!("{n} ({e})"))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            Err(e) => format!("✗ Transfer failed: {e:#}"),
        },
    );
}

/// Spinner while an operation runs, plus the last status message.
fn status_line(ui: &mut Ui, busy: bool, status: &str) {
    ui.horizontal(|ui| {
        if busy {
            ui.spinner();
        }
        if !status.is_empty() {
            ui.label(status);
        }
    });
}
