//! The add / edit connection window and its form.

use super::*;
use crate::widgets;
use egui::Ui;

/// Add/edit connection form.
pub(super) struct ConnEditor {
    editing: Option<Uuid>,
    kind: ConnectionKind,
    name: String,
    host: String,
    port: String,
    username: String,
    domain: String,
    insecure_tls: bool,
    group: String,
    auth_kind: usize, // 0 password, 1 key, 2 agent
    password: String,
    key_path: String,
    passphrase: String,
    color: Option<[u8; 3]>,
    notes: String,
}

impl ConnEditor {
    pub(super) fn blank() -> Self {
        ConnEditor {
            editing: None,
            kind: ConnectionKind::Ssh,
            name: String::new(),
            host: String::new(),
            port: "22".into(),
            username: String::new(),
            domain: String::new(),
            insecure_tls: false,
            group: String::new(),
            auth_kind: 0,
            password: String::new(),
            key_path: String::new(),
            passphrase: String::new(),
            color: None,
            notes: String::new(),
        }
    }
}

/// Editor pre-filled from a saved connection, with its secrets decrypted from the vault.
pub(super) fn editor_from(c: &Connection, vault: &Vault) -> ConnEditor {
    let (auth_kind, password, key_path, passphrase) = match &c.auth {
        AuthMethod::Password { password_enc } => (
            0,
            vault.decrypt(password_enc).unwrap_or_default(),
            String::new(),
            String::new(),
        ),
        AuthMethod::Key {
            private_key,
            passphrase_enc,
        } => (
            1,
            String::new(),
            private_key.clone(),
            passphrase_enc
                .as_ref()
                .and_then(|b| vault.decrypt(b).ok())
                .unwrap_or_default(),
        ),
        AuthMethod::Agent => (2, String::new(), String::new(), String::new()),
    };
    ConnEditor {
        editing: Some(c.id),
        kind: c.kind,
        name: c.name.clone(),
        host: c.host.clone(),
        port: c.port.to_string(),
        username: c.username.clone(),
        domain: c.domain.clone(),
        insecure_tls: c.insecure_tls,
        group: c.group.clone(),
        auth_kind,
        password,
        key_path,
        passphrase,
        color: c.color,
        notes: c.notes.clone(),
    }
}

/// What the editor's buttons asked for this frame.
enum EditorAction {
    None,
    Save,
    Cancel,
}

impl FerropipeApp {
    /// The connection editor window, while one is open.
    pub(super) fn render_editor(&mut self, ctx: &egui::Context) {
        let groups = self.store.groups();
        let mut action = EditorAction::None;
        if let Some(ed) = &mut self.editor {
            let title = if ed.editing.is_some() {
                "Edit connection"
            } else {
                "New connection"
            };
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    egui::Grid::new("editor_grid")
                        .num_columns(2)
                        .spacing([8.0, 6.0])
                        .show(ui, |ui| editor_form(ui, ed, &groups));
                    ui.separator();
                    ui.horizontal(|ui| {
                        if widgets::primary_button(ui, "Save").clicked() {
                            action = EditorAction::Save;
                        }
                        if ui.button("Cancel").clicked() {
                            action = EditorAction::Cancel;
                        }
                    });
                });
        }
        match action {
            EditorAction::Save => self.commit_editor(),
            EditorAction::Cancel => self.editor = None,
            EditorAction::None => {}
        }
    }

    /// Persist the editor's contents as a new or updated connection and close it.
    fn commit_editor(&mut self) {
        let Some(ed) = self.editor.take() else { return };
        let auth = self.editor_auth(&ed);
        if let Some(id) = ed.editing {
            if let Some(c) = self.store.connections.iter_mut().find(|c| c.id == id) {
                apply_editor(c, ed, auth);
            }
        } else {
            let mut c = Connection::new(String::new(), String::new(), String::new());
            apply_editor(&mut c, ed, auth);
            self.store.connections.push(c);
        }
        self.save_store();
    }

    /// The auth method described by the editor, with secrets vault-encrypted.
    fn editor_auth(&self, ed: &ConnEditor) -> AuthMethod {
        match ed.auth_kind {
            0 => AuthMethod::Password {
                password_enc: self.vault.encrypt(&ed.password).unwrap_or_default(),
            },
            1 => AuthMethod::Key {
                private_key: ed.key_path.clone(),
                passphrase_enc: if ed.passphrase.is_empty() {
                    None
                } else {
                    Some(self.vault.encrypt(&ed.passphrase).unwrap_or_default())
                },
            },
            _ => AuthMethod::Agent,
        }
    }
}

/// Copy every editable field of `ed` onto `c`.
fn apply_editor(c: &mut Connection, ed: ConnEditor, auth: AuthMethod) {
    c.name = ed.name;
    c.host = ed.host;
    c.port = ed.port.trim().parse().unwrap_or(ed.kind.default_port());
    c.username = ed.username;
    c.kind = ed.kind;
    c.domain = ed.domain;
    c.insecure_tls = ed.insecure_tls;
    c.group = ed.group;
    c.color = ed.color;
    c.notes = ed.notes;
    c.auth = auth;
}

/// The two-column form body (inside `editor_grid`).
fn editor_form(ui: &mut Ui, ed: &mut ConnEditor, groups: &[String]) {
    kind_row(ui, ed);
    text_row(ui, "Name", &mut ed.name);
    text_row(ui, "Host", &mut ed.host);
    text_row(ui, "Port", &mut ed.port);
    text_row(ui, "Username", &mut ed.username);
    windows_rows(ui, ed);
    group_row(ui, ed, groups);
    auth_rows(ui, ed);
    color_row(ui, ed);
    text_row(ui, "Notes", &mut ed.notes);
}

/// A labelled single-line text field on its own grid row.
fn text_row(ui: &mut Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.text_edit_singleline(value);
    ui.end_row();
}

/// A labelled password field on its own grid row.
fn secret_row(ui: &mut Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.add(egui::TextEdit::singleline(value).password(true));
    ui.end_row();
}

/// Connection type; retargets the port when it was still the previous type's default.
fn kind_row(ui: &mut Ui, ed: &mut ConnEditor) {
    ui.label("Type");
    let prev_kind = ed.kind;
    let kinds = [
        ConnectionKind::Ssh,
        ConnectionKind::Smb,
        ConnectionKind::WinRm,
        ConnectionKind::Rdp,
        ConnectionKind::RdpNative,
    ];
    egui::ComboBox::from_id_salt("connkind")
        .selected_text(kind_name(ed.kind))
        .show_ui(ui, |ui| {
            for kind in kinds {
                ui.selectable_value(&mut ed.kind, kind, kind_name(kind));
            }
        });
    if ed.kind != prev_kind && ed.port == prev_kind.default_port().to_string() {
        ed.port = ed.kind.default_port().to_string();
    }
    ui.end_row();
}

/// Editor-facing name of a connection kind.
fn kind_name(kind: ConnectionKind) -> &'static str {
    match kind {
        ConnectionKind::Ssh => "SSH / SFTP",
        ConnectionKind::Rdp => "RDP (Remmina)",
        ConnectionKind::RdpNative => "RDP (Native)",
        ConnectionKind::Smb => "SMB (Windows share)",
        ConnectionKind::WinRm => "WinRM (Windows)",
    }
}

/// Domain / workgroup and, for WinRM, the insecure-TLS toggle. Nothing for SSH.
fn windows_rows(ui: &mut Ui, ed: &mut ConnEditor) {
    if ed.kind == ConnectionKind::Ssh {
        return;
    }
    ui.label(if ed.kind == ConnectionKind::Smb {
        "Workgroup/Domain"
    } else {
        "Domain"
    });
    ui.text_edit_singleline(&mut ed.domain)
        .on_hover_text("Windows domain / workgroup (optional)");
    ui.end_row();
    if ed.kind == ConnectionKind::WinRm {
        ui.label("TLS");
        ui.checkbox(&mut ed.insecure_tls, "Accept invalid cert (insecure)")
            .on_hover_text("Only for self-signed WinRM/HTTPS on :5986. Leave off for strict TLS.");
        ui.end_row();
    }
}

/// Group picker over the existing groups plus a free-text field for new ones.
fn group_row(ui: &mut Ui, ed: &mut ConnEditor, groups: &[String]) {
    ui.label("Group");
    ui.horizontal(|ui| {
        let current = if ed.group.is_empty() {
            "(top level)".to_string()
        } else {
            ed.group.clone()
        };
        egui::ComboBox::from_id_salt("groedit")
            .selected_text(current)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(ed.group.is_empty(), "(top level)")
                    .clicked()
                {
                    ed.group.clear();
                }
                for g in groups {
                    if ui.selectable_label(&ed.group == g, g).clicked() {
                        ed.group = g.clone();
                    }
                }
            });
        ui.text_edit_singleline(&mut ed.group)
            .on_hover_text("Pick a group above, or type a new one (use / to nest, e.g. WIM/Tovuz)");
    });
    ui.end_row();
}

/// Auth method (SSH only) and the secret fields it needs.
fn auth_rows(ui: &mut Ui, ed: &mut ConnEditor) {
    if ed.kind != ConnectionKind::Ssh {
        ed.auth_kind = 0; // RDP/SMB/WinRM use a password
    } else {
        ui.label("Auth");
        egui::ComboBox::from_id_salt("authkind")
            .selected_text(auth_name(ed.auth_kind))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut ed.auth_kind, 0, "Password");
                ui.selectable_value(&mut ed.auth_kind, 1, "Key file");
                ui.selectable_value(&mut ed.auth_kind, 2, "SSH agent");
            });
        ui.end_row();
    }
    match ed.auth_kind {
        0 => secret_row(ui, "Password", &mut ed.password),
        1 => {
            key_file_row(ui, &mut ed.key_path);
            secret_row(ui, "Passphrase", &mut ed.passphrase);
        }
        _ => {}
    }
}

/// Editor-facing name of an auth method index.
fn auth_name(auth_kind: usize) -> &'static str {
    match auth_kind {
        0 => "Password",
        1 => "Key file",
        _ => "SSH agent",
    }
}

/// Private-key path with a file picker.
fn key_file_row(ui: &mut Ui, key_path: &mut String) {
    ui.label("Key file");
    ui.horizontal(|ui| {
        ui.text_edit_singleline(key_path);
        if ui.button("Browse…").clicked()
            && let Some(p) = rfd::FileDialog::new().pick_file()
        {
            *key_path = p.to_string_lossy().into_owned();
        }
    });
    ui.end_row();
}

/// Stripe color seeded when the user first enables a tag color: the rust
/// accent, read from the palette so it cannot drift from it.
fn default_tag_color() -> [u8; 3] {
    let c = crate::theme::DARK.accent;
    [c.r(), c.g(), c.b()]
}

/// Optional sidebar stripe color: a toggle that seeds the accent, then a color picker.
fn color_row(ui: &mut Ui, ed: &mut ConnEditor) {
    ui.label("Tag color");
    ui.horizontal(|ui| {
        let mut tagged = ed.color.is_some();
        if ui.checkbox(&mut tagged, "Show a color stripe").changed() {
            ed.color = tagged.then(default_tag_color);
        }
        if let Some(rgb) = &mut ed.color {
            ui.color_edit_button_srgb(rgb);
        }
    });
    ui.end_row();
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConnectionKind::{Rdp, RdpNative, Smb, Ssh, WinRm};

    fn editor(kind: ConnectionKind, port: &str) -> ConnEditor {
        ConnEditor {
            kind,
            port: port.into(),
            ..ConnEditor::blank()
        }
    }

    fn applied(ed: ConnEditor) -> Connection {
        let mut c = Connection::new("", "", "");
        apply_editor(&mut c, ed, AuthMethod::Agent);
        c
    }

    // -- apply_editor ------------------------------------------------------

    #[test]
    fn apply_editor_blank_port_falls_back_to_the_kinds_default() {
        // Arrange
        let cases = [
            (Ssh, 22),
            (Rdp, 3389),
            (RdpNative, 3389),
            (Smb, 445),
            (WinRm, 5985),
        ];
        for (kind, expected) in cases {
            // Act
            let c = applied(editor(kind, "   "));
            // Assert
            assert_eq!(c.port, expected, "{kind:?}");
        }
    }

    #[test]
    fn apply_editor_invalid_port_falls_back_to_the_kinds_default() {
        for bad in ["abc", "99999", "-1", "22a"] {
            assert_eq!(applied(editor(Smb, bad)).port, 445, "{bad:?}");
            assert_eq!(applied(editor(WinRm, bad)).port, 5985, "{bad:?}");
        }
    }

    #[test]
    fn apply_editor_keeps_a_valid_port() {
        assert_eq!(applied(editor(Ssh, "2222")).port, 2222);
        assert_eq!(applied(editor(Rdp, " 3390 ")).port, 3390);
    }

    #[test]
    fn apply_editor_round_trips_the_tag_color() {
        let tagged = ConnEditor {
            color: Some([0x12, 0x34, 0x56]),
            ..editor(Ssh, "22")
        };
        assert_eq!(applied(tagged).color, Some([0x12, 0x34, 0x56]));
        assert_eq!(applied(editor(Ssh, "22")).color, None);
    }

    // -- kind_name / auth_name --------------------------------------------

    #[test]
    fn kind_name_maps_every_connection_kind() {
        assert_eq!(kind_name(Ssh), "SSH / SFTP");
        assert_eq!(kind_name(Rdp), "RDP (Remmina)");
        assert_eq!(kind_name(RdpNative), "RDP (Native)");
        assert_eq!(kind_name(Smb), "SMB (Windows share)");
        assert_eq!(kind_name(WinRm), "WinRM (Windows)");
    }

    #[test]
    fn auth_name_maps_the_three_indices_and_defaults_to_agent() {
        assert_eq!(auth_name(0), "Password");
        assert_eq!(auth_name(1), "Key file");
        assert_eq!(auth_name(2), "SSH agent");
        assert_eq!(auth_name(7), "SSH agent");
    }
}
