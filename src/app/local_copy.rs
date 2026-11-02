//! Local → local copies: when both panes show this computer, the destination
//! pane's (idle) worker copies the files off the UI thread and reports
//! progress like an upload. The command is issued with every other transfer
//! (`transfers::transfer_command`); what lands here is the relisting once it
//! has finished.

use super::*;

impl FerropipeApp {
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
