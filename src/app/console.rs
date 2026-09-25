//! The command console: an SSH exec prompt with its scrollback, toggled from
//! the top bar.

use super::*;

impl FerropipeApp {
    /// Command console (SSH exec), toggled from the top bar.
    pub(super) fn render_console(&mut self, ui: &mut egui::Ui) {
        if !self.console_open {
            return;
        }
        egui::Panel::bottom("console")
            .resizable(true)
            .default_size(190.0)
            .size_range(egui::Rangef::new(120.0, 480.0))
            .show(ui, |ui| {
                ui.add_space(3.0);
                ui.horizontal(|ui| {
                    let p = crate::theme::current(ui);
                    ui.label(RichText::new("Console").strong().color(p.text));
                    if let Some(id) = self.connected {
                        ui.label(RichText::new(self.conn_name(id)).color(p.text2));
                        ui.label(RichText::new(self.active_kind).monospace().color(p.text3));
                    }
                    if self.exec_running {
                        ui.spinner();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Clear").clicked() {
                            self.console.clear();
                        }
                    });
                });
                let submit = self.console_prompt_row(ui);
                ui.separator();
                self.console_output(ui);
                if submit {
                    self.submit_console();
                }
            });
    }

    /// "$" prompt and the command field; true when Enter submitted it.
    fn console_prompt_row(&mut self, ui: &mut egui::Ui) -> bool {
        let p = crate::theme::current(ui);
        ui.horizontal(|ui| {
            ui.label(RichText::new("$").monospace().color(p.text2));
            let hint = if self.active_kind == "SFTP" {
                "run a command… (Enter)"
            } else {
                "connect via SSH to run commands"
            };
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.console_input)
                    .desired_width(f32::INFINITY)
                    .hint_text(hint)
                    .font(egui::TextStyle::Monospace),
            );
            r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
        })
        .inner
    }

    /// Scrollback of the console, errors in the palette's error color.
    fn console_output(&self, ui: &mut egui::Ui) {
        let p = crate::theme::current(ui);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for (line, is_err) in &self.console {
                    let color = if *is_err { p.err } else { p.text };
                    ui.label(RichText::new(line).monospace().color(color));
                }
            });
    }

    fn submit_console(&mut self) {
        let cmd = self.console_input.trim().to_string();
        if cmd.is_empty() {
            return;
        }
        if self.connected.is_none() {
            self.console.push(("(not connected)".into(), true));
            return;
        }
        self.console.push((format!("$ {cmd}"), false));
        self.exec_running = true;
        self.worker.send(Command::Exec { command: cmd });
        self.console_input.clear();
    }
}
