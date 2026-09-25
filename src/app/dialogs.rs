//! Modal dialogs: the text prompt (new folder / new file / rename), the delete
//! confirmation, and the actions they commit.

use super::*;

/// A modal text prompt (new folder / rename).
pub(super) struct Prompt {
    pub title: String,
    pub label: String,
    pub value: String,
    pub kind: PromptKind,
}

#[derive(Clone)]
pub(super) enum PromptKind {
    NewFolder(Side),
    NewFile(Side),
    Rename(Side, String),
}

impl FerropipeApp {
    pub(super) fn render_dialogs(&mut self, ctx: &egui::Context) {
        self.render_editor(ctx);
        self.render_prompt(ctx);
        self.render_confirm_delete(ctx);
    }

    /// Modal text prompt (new folder / new file / rename).
    fn render_prompt(&mut self, ctx: &egui::Context) {
        let mut prompt_ok = false;
        let mut prompt_cancel = false;
        if let Some(p) = &mut self.prompt {
            egui::Window::new(&p.title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(&p.label);
                    let r = ui.text_edit_singleline(&mut p.value);
                    r.request_focus();
                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked()
                            || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        {
                            prompt_ok = true;
                        }
                        if ui.button("Cancel").clicked() {
                            prompt_cancel = true;
                        }
                    });
                });
        }
        if prompt_ok {
            self.apply_prompt(ctx);
            self.prompt = None;
        }
        if prompt_cancel {
            self.prompt = None;
        }
    }

    /// "Delete N items?" confirmation.
    fn render_confirm_delete(&mut self, ctx: &egui::Context) {
        let mut do_del: Option<(Side, Vec<String>)> = None;
        let mut cancel_del = false;
        if let Some((side, paths)) = &self.confirm_delete {
            egui::Window::new("Confirm delete")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "Delete {} item(s)? This cannot be undone.",
                        paths.len()
                    ));
                    for p in paths.iter().take(8) {
                        ui.label(RichText::new(p).small().weak());
                    }
                    ui.horizontal(|ui| {
                        let err = crate::theme::current(ui).err;
                        if ui.button(RichText::new("Delete").color(err)).clicked() {
                            do_del = Some((*side, paths.clone()));
                        }
                        if ui.button("Cancel").clicked() {
                            cancel_del = true;
                        }
                    });
                });
        }
        if let Some((side, paths)) = do_del {
            self.do_delete(ctx, side, paths);
            self.confirm_delete = None;
        }
        if cancel_del {
            self.confirm_delete = None;
        }
    }

    fn apply_prompt(&mut self, ctx: &egui::Context) {
        let Some(p) = self.prompt.take() else { return };
        let value = p.value.trim().to_string();
        if value.is_empty() {
            return;
        }
        match p.kind {
            PromptKind::NewFolder(side) => self.make_folder(ctx, side, &value),
            PromptKind::NewFile(side) => self.make_file(ctx, side, &value),
            PromptKind::Rename(side, old) => self.rename_entry(ctx, side, &old, &value),
        }
    }

    /// Create a folder called `name` in the pane's current directory.
    fn make_folder(&mut self, ctx: &egui::Context, side: Side, name: &str) {
        if self.side_is_remote(side) {
            let path = self.side_pane(side).full_path(name);
            self.side_send(side, Command::MkdirRemote { path });
        } else {
            let cwd = self.side_pane(side).cwd.clone();
            if let Err(e) = localfs::mkdir(std::path::Path::new(&cwd), name) {
                self.toast(ctx, format!("mkdir failed: {e:#}"), true);
            }
            let show_hidden = self.store.settings.show_hidden;
            reload_local(self.side_pane_mut(side), show_hidden);
        }
    }

    /// Create an empty file called `name` in the pane's current directory.
    fn make_file(&mut self, ctx: &egui::Context, side: Side, name: &str) {
        if self.side_is_remote(side) {
            let path = self.side_pane(side).full_path(name);
            self.side_send(side, Command::CreateFileRemote { path });
        } else {
            let path = PathBuf::from(&self.side_pane(side).cwd).join(name);
            if let Err(e) = localfs::create_file(&path) {
                self.toast(ctx, format!("create file failed: {e:#}"), true);
            }
            let show_hidden = self.store.settings.show_hidden;
            reload_local(self.side_pane_mut(side), show_hidden);
        }
    }

    /// Rename `old` to `new` within the pane's current directory.
    fn rename_entry(&mut self, ctx: &egui::Context, side: Side, old: &str, new: &str) {
        if self.side_is_remote(side) {
            let from = self.side_pane(side).full_path(old);
            let to = self.side_pane(side).full_path(new);
            self.side_send(side, Command::RenameRemote { from, to });
        } else {
            let cwd = self.side_pane(side).cwd.clone();
            let from = PathBuf::from(&cwd).join(old);
            let to = PathBuf::from(&cwd).join(new);
            if let Err(e) = localfs::rename(&from, &to) {
                self.toast(ctx, format!("rename failed: {e:#}"), true);
            }
            let show_hidden = self.store.settings.show_hidden;
            reload_local(self.side_pane_mut(side), show_hidden);
        }
    }

    pub(super) fn do_delete(&mut self, ctx: &egui::Context, side: Side, paths: Vec<String>) {
        if self.side_is_remote(side) {
            self.side_send(side, Command::DeleteRemote { paths });
        } else {
            for p in &paths {
                if let Err(e) = localfs::delete(std::path::Path::new(p)) {
                    self.toast(ctx, format!("delete failed: {e:#}"), true);
                }
            }
            let show_hidden = self.store.settings.show_hidden;
            reload_local(self.side_pane_mut(side), show_hidden);
        }
    }
}
