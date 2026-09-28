//! Session memory: the directory each host and each local pane was last in,
//! which host each pane showed at exit, and restoring both on the next
//! start. Directory changes reach the store through a debounce so browsing
//! never hammers the disk; the `Drop` impl writes whatever is still pending.

use super::*;
use crate::diag::diag_info;
use crate::store::Settings;
use std::time::Duration;

/// Shortest gap between two directory-driven store writes.
const STORE_SAVE_DEBOUNCE_SECS: f64 = 2.0;

/// The hosts to reopen on start, right pane first so the primary session
/// comes up before the secondary one. A pane's remembered host is kept only
/// while it still exists and browses files; nothing when reopening is off.
pub(super) fn restore_plan(settings: &Settings, connections: &[Connection]) -> Vec<(Side, Uuid)> {
    if !settings.restore_session {
        return Vec::new();
    }
    [Side::Right, Side::Left]
        .into_iter()
        .filter_map(|side| {
            let id = settings.pane_hosts[side as usize]?;
            let conn = connections.iter().find(|c| c.id == id)?;
            conn.kind.browses_files().then_some((side, id))
        })
        .collect()
}

/// Whether the debounce window since `last_save` has passed at `now`.
pub(super) fn store_save_due(last_save: f64, now: f64) -> bool {
    now - last_save >= STORE_SAVE_DEBOUNCE_SECS
}

/// `dir` while it still names a directory on this computer.
pub(super) fn existing_dir(dir: Option<String>) -> Option<String> {
    dir.filter(|d| std::path::Path::new(d).is_dir())
}

/// The local directory `side` returns to when it leaves a host: the
/// remembered one if it still exists, else "" (meaning home).
pub(super) fn saved_local_dir(settings: &Settings, side: Side) -> String {
    existing_dir(settings.remembered_dir(None, side)).unwrap_or_default()
}

/// The host each pane shows, as `pane_hosts` records it: a pane on this
/// computer shows none, whatever its worker's stale session says.
pub(super) fn pane_hosts(
    left_remote: bool,
    lconnected: Option<Uuid>,
    right_local: bool,
    connected: Option<Uuid>,
) -> [Option<Uuid>; 2] {
    [
        lconnected.filter(|_| left_remote),
        connected.filter(|_| !right_local),
    ]
}

/// Where a fresh session lists first: a remembered directory other than
/// `home`, flagged `true` so a failure can fall back to home — else home.
pub(super) fn first_listing(remembered: Option<String>, home: &str) -> (String, bool) {
    match remembered {
        Some(dir) if dir != home => (dir, true),
        _ => (home.to_string(), false),
    }
}

impl FerropipeApp {
    /// Note a settings change for the next due `flush_store_if_due`.
    fn mark_store_dirty(&mut self) {
        self.store_dirty = true;
    }

    /// Write a dirty store once the debounce window has passed, and make
    /// sure a frame comes around to do so even while the UI sits idle.
    pub(super) fn flush_store_if_due(&mut self, ctx: &egui::Context) {
        if !self.store_dirty {
            return;
        }
        let now = ctx.input(|i| i.time);
        if store_save_due(self.last_store_save, now) {
            self.last_store_save = now;
            self.save_store();
        } else {
            let wait = STORE_SAVE_DEBOUNCE_SECS - (now - self.last_store_save);
            ctx.request_repaint_after(Duration::from_secs_f64(wait.max(0.0)));
        }
    }

    /// Record which host each pane shows, for the next start.
    pub(super) fn sync_pane_hosts(&mut self) {
        let hosts = pane_hosts(
            self.left_remote,
            self.lconnected,
            self.right_local,
            self.connected,
        );
        if self.store.settings.pane_hosts != hosts {
            self.store.settings.pane_hosts = hosts;
            self.mark_store_dirty();
        }
    }

    /// Put the local pane `side` in `dir`, remember it and list it. Every
    /// local directory change goes through here.
    pub(super) fn set_local_cwd(&mut self, side: Side, dir: String) {
        self.store.settings.remember_dir(None, side, &dir);
        self.mark_store_dirty();
        let show_hidden = self.store.settings.show_hidden;
        let pane = self.side_pane_mut(side);
        pane.cwd = dir;
        reload_local(pane, show_hidden);
    }

    /// A listing arrived on `side`: remember `path` for its host and end any
    /// directory restore in progress there.
    pub(super) fn remember_remote_dir(&mut self, side: Side, path: &str) {
        self.restoring[side as usize] = false;
        if let Some(id) = self.side_host(side) {
            self.store.settings.remember_dir(Some(id), side, path);
            self.mark_store_dirty();
        }
    }

    /// The first listing of a fresh session on `side`: the directory its
    /// host was last in when one is remembered, else `home`.
    pub(super) fn list_first_dir(&mut self, side: Side, home: String) {
        let remembered = self
            .side_host(side)
            .and_then(|id| self.store.settings.remembered_dir(Some(id), side));
        let (path, restoring) = first_listing(remembered, &home);
        if restoring {
            let name = self.side_name(side);
            diag_info!("session", "{name}: restoring last directory {path}");
        }
        self.restoring[side as usize] = restoring;
        self.side_send(side, Command::ListRemote { path });
    }

    /// The first listing of a restored directory failed: list home instead,
    /// once. `false` when no restore was in progress on `side`.
    pub(super) fn fall_back_to_home(&mut self, side: Side, msg: &str) -> bool {
        if !std::mem::take(&mut self.restoring[side as usize]) {
            return false;
        }
        let Some(home) = self.side_pane(side).home.clone() else {
            return false;
        };
        let name = self.side_name(side);
        diag_info!(
            "session",
            "{name}: last directory unavailable ({msg}); listing home {home}"
        );
        self.side_send(side, Command::ListRemote { path: home });
        true
    }

    /// Reopen the hosts each pane showed at exit. Runs once, at start; a
    /// failure surfaces through the usual connect error path.
    pub(super) fn restore_panes(&mut self, ctx: &egui::Context) {
        for (side, id) in restore_plan(&self.store.settings, &self.store.connections) {
            let name = self.conn_name(id);
            diag_info!("session", "restoring {name} in the {side:?} pane");
            self.connect_side(ctx, side, id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_host(name: &str) -> Connection {
        Connection::new(name, format!("{name}.local"), "admin")
    }

    fn rdp_host(name: &str) -> Connection {
        Connection {
            kind: ConnectionKind::Rdp,
            ..file_host(name)
        }
    }

    fn settings(pane_hosts: [Option<Uuid>; 2]) -> Settings {
        Settings {
            pane_hosts,
            ..Settings::default()
        }
    }

    // -- restore_plan --------------------------------------------------------

    #[test]
    fn restore_plan_connects_the_right_pane_before_the_left() {
        // Arrange
        let (nas, web) = (file_host("nas"), file_host("web"));
        let s = settings([Some(nas.id), Some(web.id)]);
        // Act
        let plan = restore_plan(&s, &[nas.clone(), web.clone()]);
        // Assert
        assert_eq!(plan, vec![(Side::Right, web.id), (Side::Left, nas.id)]);
    }

    #[test]
    fn restore_plan_drops_a_host_that_no_longer_exists() {
        let nas = file_host("nas");
        let s = settings([Some(Uuid::new_v4()), Some(nas.id)]);
        let plan = restore_plan(&s, std::slice::from_ref(&nas));
        assert_eq!(plan, vec![(Side::Right, nas.id)]);
    }

    #[test]
    fn restore_plan_skips_rdp_kinds() {
        // Arrange: the left pane's host was turned into an RDP connection since.
        let (desk, nas) = (rdp_host("desk"), file_host("nas"));
        let s = settings([Some(desk.id), Some(nas.id)]);
        // Act
        let plan = restore_plan(&s, &[desk, nas.clone()]);
        // Assert
        assert_eq!(plan, vec![(Side::Right, nas.id)]);
    }

    #[test]
    fn restore_plan_is_empty_when_reopening_is_off() {
        let nas = file_host("nas");
        let s = Settings {
            restore_session: false,
            ..settings([None, Some(nas.id)])
        };
        assert!(restore_plan(&s, &[nas]).is_empty());
    }

    #[test]
    fn restore_plan_is_empty_when_both_panes_were_local() {
        let nas = file_host("nas");
        assert!(restore_plan(&settings([None, None]), &[nas]).is_empty());
    }

    // -- store_save_due ------------------------------------------------------

    #[test]
    fn store_save_due_only_once_the_debounce_window_has_passed() {
        assert!(!store_save_due(10.0, 10.5));
        assert!(!store_save_due(10.0, 11.9));
        assert!(store_save_due(10.0, 12.0));
        assert!(store_save_due(0.0, 100.0));
    }

    // -- existing_dir --------------------------------------------------------

    #[test]
    fn existing_dir_keeps_a_directory_that_is_still_there() {
        let tmp = std::env::temp_dir().to_string_lossy().into_owned();
        assert_eq!(existing_dir(Some(tmp.clone())), Some(tmp));
    }

    #[test]
    fn existing_dir_drops_a_missing_or_absent_directory() {
        assert_eq!(existing_dir(Some("/definitely/not/here-7f3a".into())), None);
        assert_eq!(existing_dir(None), None);
    }

    // -- pane_hosts ----------------------------------------------------------

    #[test]
    fn pane_hosts_reports_the_connected_host_of_each_remote_pane() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        assert_eq!(
            pane_hosts(true, Some(a), false, Some(b)),
            [Some(a), Some(b)]
        );
    }

    #[test]
    fn pane_hosts_is_none_for_a_pane_on_this_computer_even_with_a_stale_session() {
        let a = Uuid::new_v4();
        assert_eq!(pane_hosts(false, Some(a), true, Some(a)), [None, None]);
        assert_eq!(pane_hosts(true, None, false, None), [None, None]);
    }

    // -- first_listing -------------------------------------------------------

    #[test]
    fn first_listing_restores_a_remembered_directory_other_than_home() {
        let (path, restoring) = first_listing(Some("/var/log".into()), "/root");
        assert_eq!(path, "/var/log");
        assert!(restoring);
    }

    #[test]
    fn first_listing_lists_home_without_restoring_when_nothing_else_is_remembered() {
        assert_eq!(first_listing(None, "/root"), ("/root".to_string(), false));
        assert_eq!(
            first_listing(Some("/root".into()), "/root"),
            ("/root".to_string(), false)
        );
    }
}
