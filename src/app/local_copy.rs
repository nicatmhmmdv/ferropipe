//! Local → local copies: when both panes show this computer, the destination
//! pane's (idle) worker copies the files off the UI thread and reports
//! progress like an upload.

use super::*;
use transfers::Verb;

impl FerropipeApp {
    /// local → local: the destination side's worker copies into its pane's directory.
    pub(super) fn start_local_copy(
        &mut self,
        ctx: &egui::Context,
        payload: DragPayload,
        dest: Side,
        on_conflict: OnConflict,
    ) {
        let dest_dir = PathBuf::from(&self.side_pane(dest).cwd);
        let spec = self.transfer_spec(&payload, dest, dest, Verb::Copy);
        let id = self.push_transfer(ctx, spec);
        let paths = payload.paths.iter().map(PathBuf::from).collect();
        self.side_send(
            dest,
            Command::CopyLocal {
                id,
                paths,
                dest_dir,
                on_conflict,
            },
        );
    }

    /// A transfer into `dest` succeeded: relist it when it is local, and the
    /// other pane too when that is local and shows the same directory.
    pub(super) fn relist_local_dest(&mut self, dest: Side) {
        if self.side_is_remote(dest) {
            return;
        }
        let show_hidden = self.store.settings.show_hidden;
        reload_local(self.side_pane_mut(dest), show_hidden);
        let src = dest.other();
        let same_dir = self.side_pane(src).cwd == self.side_pane(dest).cwd;
        if !self.side_is_remote(src) && same_dir {
            reload_local(self.side_pane_mut(src), show_hidden);
        }
    }
}
