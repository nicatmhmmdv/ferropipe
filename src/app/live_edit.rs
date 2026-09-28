//! Live editing of remote files: download to a temp dir, open the editor and
//! re-upload whenever the temp file changes on disk.

use super::*;
use std::time::SystemTime;

/// A remote file being edited locally; polled for changes to auto re-upload.
pub(super) struct EditWatch {
    side: Side,
    pub temp_file: PathBuf,
    remote_dir: String,
    name: String,
    last_mtime: Option<SystemTime>,
}

impl FerropipeApp {
    pub(super) fn start_edit(&mut self, ctx: &egui::Context, side: Side, name: &str) {
        let remote_path = self.side_pane(side).full_path(name);
        let remote_dir = self.side_pane(side).cwd.clone();
        let dir = std::env::temp_dir().join(format!(
            "ferropipe-edit-{}-{}",
            std::process::id(),
            self.next_id
        ));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.toast(ctx, format!("edit: {e:#}"), true);
            return;
        }
        let temp_file = dir.join(name);
        let id = self.next_id;
        self.next_id += 1;
        self.pending_edit.insert(
            id,
            EditWatch {
                side,
                temp_file,
                remote_dir,
                name: name.to_string(),
                last_mtime: None,
            },
        );
        self.side_send(
            side,
            // The temp dir is private to this edit, so nothing can be in the way.
            Command::Download {
                id,
                remote: vec![remote_path],
                local_dir: dir,
                on_conflict: OnConflict::Overwrite,
            },
        );
        self.toast(ctx, format!("Opening {name} for editing…"), false);
    }

    /// Poll edited temp files; re-upload any that changed since last check.
    pub(super) fn poll_edits(&mut self, ctx: &egui::Context) {
        let mut uploads: Vec<(Side, PathBuf, String, String)> = Vec::new();
        for w in &mut self.edits {
            let m = std::fs::metadata(&w.temp_file)
                .and_then(|md| md.modified())
                .ok();
            match (w.last_mtime, m) {
                (Some(last), Some(now)) if now > last => {
                    w.last_mtime = Some(now);
                    uploads.push((
                        w.side,
                        w.temp_file.clone(),
                        w.remote_dir.clone(),
                        w.name.clone(),
                    ));
                }
                (None, Some(now)) => w.last_mtime = Some(now),
                _ => {}
            }
        }
        for (side, temp, dir, name) in uploads {
            // Upload back to the SAME session the file was opened from.
            if !self.side_connected(side) {
                self.toast(
                    ctx,
                    format!("{name}: host disconnected, save not synced"),
                    true,
                );
                continue;
            }
            let spec = NewTransfer {
                worker: side,
                label: name.clone(),
                dest: self.side_dest_label(side, &dir),
                upload: true,
                verb: transfers::Verb::Upload,
                retry: None,
            };
            let id = self.push_transfer(ctx, spec);
            self.side_send(
                side,
                // Replacing the edited file is the point of the sync.
                Command::Upload {
                    id,
                    local: vec![temp],
                    remote_dir: dir,
                    on_conflict: OnConflict::Overwrite,
                },
            );
            self.toast(ctx, format!("Syncing {name} → remote"), false);
        }
    }

    /// A live-edit download finished: open the editor and watch the file for saves.
    pub(super) fn finish_edit_download(&mut self, ctx: &egui::Context, id: u64, ok: bool) {
        let Some(watch) = self.pending_edit.remove(&id) else {
            return;
        };
        if !ok {
            self.toast(ctx, "edit: download failed", true);
            return;
        }
        let name = watch.name.clone();
        match crate::external::open_editor(&watch.temp_file) {
            Ok(_) => {
                let mtime = std::fs::metadata(&watch.temp_file)
                    .and_then(|m| m.modified())
                    .ok();
                self.edits.push(EditWatch {
                    last_mtime: mtime,
                    ..watch
                });
                self.toast(ctx, format!("Editing {name} — saves auto-upload"), false);
            }
            Err(e) => self.toast(ctx, format!("editor: {e:#}"), true),
        }
    }
}
