//! Persistent storage of connections and app settings (JSON in the config dir).
use crate::model::{Connection, Side};
use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const QUALIFIER: &str = "io";
pub const ORG: &str = "ferropipe";
pub const APP: &str = "Ferropipe";

/// Resolve config dir; falls back to ~/.config/ferropipe.
pub fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from(QUALIFIER, ORG, APP).context("could not resolve config dir")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_theme")]
    pub dark_mode: bool,
    #[serde(default)]
    pub show_hidden: bool,
    #[serde(default = "default_true")]
    pub confirm_delete: bool,
    /// Superseded by `local_dirs[0]`. Still written for one release so an
    /// older build keeps its left pane's directory, and read once to seed
    /// `local_dirs` from a file that predates it (`migrate`).
    #[serde(default)]
    pub last_local_dir: Option<String>,
    /// Show the Commander function-key bar; the keys themselves always work.
    #[serde(default)]
    pub show_fkeys: bool,
    /// The directory last listed on each host, by connection id.
    #[serde(default)]
    pub last_dirs: HashMap<Uuid, String>,
    /// The local directory each pane last showed, indexed by `Side as usize`.
    #[serde(default)]
    pub local_dirs: [Option<String>; 2],
    /// The host each pane showed at exit, indexed by `Side as usize`;
    /// `None` when the pane was on this computer or empty.
    #[serde(default)]
    pub pane_hosts: [Option<Uuid>; 2],
    /// Reopen `pane_hosts` in their panes on start. When off, only the
    /// directories are remembered and the panes start as they always did.
    #[serde(default = "default_true")]
    pub restore_session: bool,
}

fn default_theme() -> bool {
    true
}
fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            dark_mode: true,
            show_hidden: false,
            confirm_delete: true,
            last_local_dir: None,
            show_fkeys: false,
            last_dirs: HashMap::new(),
            local_dirs: [None, None],
            pane_hosts: [None, None],
            restore_session: true,
        }
    }
}

impl Settings {
    /// Seed the left pane's local directory from the field that preceded
    /// `local_dirs`; a value already there wins.
    fn migrate(&mut self) {
        if self.local_dirs[0].is_none() {
            self.local_dirs[0] = self.last_local_dir.clone();
        }
    }

    /// The directory last shown for `host`, or — with `None`, this computer —
    /// the one pane `side` last showed locally. `None` when never remembered.
    pub fn remembered_dir(&self, host: Option<Uuid>, side: Side) -> Option<String> {
        match host {
            Some(id) => self.last_dirs.get(&id).cloned(),
            None => self.local_dirs[side as usize].clone(),
        }
    }

    /// Remember `dir` as the last one shown for `host`, or — with `None` —
    /// locally in pane `side`. An empty `dir` is ignored.
    pub fn remember_dir(&mut self, host: Option<Uuid>, side: Side, dir: &str) {
        if dir.is_empty() {
            return;
        }
        match host {
            Some(id) => {
                self.last_dirs.insert(id, dir.to_string());
            }
            None => {
                self.local_dirs[side as usize] = Some(dir.to_string());
                if side == Side::Left {
                    self.last_local_dir = Some(dir.to_string());
                }
            }
        }
    }
}

/// The persisted document: connections + settings.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub connections: Vec<Connection>,
    #[serde(default)]
    pub settings: Settings,
}

impl Store {
    pub fn load(path: &Path) -> Result<Store> {
        if !path.exists() {
            return Ok(Store::default());
        }
        let raw = std::fs::read_to_string(path).context("reading store")?;
        let mut store: Store = serde_json::from_str(&raw).context("parsing store json")?;
        store.settings.migrate();
        Ok(store)
    }

    /// Forget the directories and pane assignments of connections that no
    /// longer exist, so the settings never outgrow the host list.
    pub fn prune_forgotten_hosts(&mut self) {
        let known: HashSet<Uuid> = self.connections.iter().map(|c| c.id).collect();
        self.settings.last_dirs.retain(|id, _| known.contains(id));
        for slot in &mut self.settings.pane_hosts {
            if slot.is_some_and(|id| !known.contains(&id)) {
                *slot = None;
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        // Write atomically via a temp file + rename, with private (0600) permissions
        // since the file references credential locations.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Sorted, de-duplicated list of group paths used by connections.
    pub fn groups(&self) -> Vec<String> {
        let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for c in &self.connections {
            if c.group.is_empty() {
                continue;
            }
            // Include every ancestor so the tree renders fully.
            let mut acc = String::new();
            for part in c.group.split('/') {
                if acc.is_empty() {
                    acc = part.to_string();
                } else {
                    acc = format!("{acc}/{part}");
                }
                set.insert(acc.clone());
            }
        }
        set.into_iter().collect()
    }
}

pub fn store_path(dirs: &ProjectDirs) -> PathBuf {
    dirs.config_dir().join("connections.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_show_fkeys_is_off_by_default() {
        assert!(!Settings::default().show_fkeys);
    }

    #[test]
    fn settings_written_before_show_fkeys_existed_still_load_with_it_off() {
        // Arrange: a settings object from an older connections.json.
        let raw = r#"{"dark_mode": false, "show_hidden": true, "confirm_delete": true}"#;
        // Act
        let settings: Settings = serde_json::from_str(raw).expect("old settings parse");
        // Assert
        assert!(!settings.show_fkeys);
        assert!(!settings.dark_mode);
        assert!(settings.show_hidden);
    }

    #[test]
    fn settings_show_fkeys_round_trips_through_json() {
        // Arrange
        let settings = Settings {
            show_fkeys: true,
            ..Settings::default()
        };
        // Act
        let json = serde_json::to_string(&settings).expect("serialize");
        let back: Settings = serde_json::from_str(&json).expect("parse");
        // Assert
        assert!(back.show_fkeys);
    }

    // -- session memory ----------------------------------------------------

    fn host(name: &str) -> Connection {
        Connection::new(name, format!("{name}.local"), "admin")
    }

    #[test]
    fn settings_default_to_no_remembered_dirs_no_pane_hosts_and_restoring() {
        let s = Settings::default();
        assert!(s.last_dirs.is_empty());
        assert_eq!(s.local_dirs, [None, None]);
        assert_eq!(s.pane_hosts, [None, None]);
        assert!(s.restore_session);
    }

    #[test]
    fn settings_written_before_session_memory_existed_still_load() {
        // Arrange: a settings object from an older connections.json.
        let raw = r#"{"dark_mode": true, "show_hidden": false, "confirm_delete": true,
                      "last_local_dir": "/srv/old", "show_fkeys": true}"#;
        // Act
        let s: Settings = serde_json::from_str(raw).expect("old settings parse");
        // Assert
        assert!(s.last_dirs.is_empty());
        assert_eq!(s.pane_hosts, [None, None]);
        assert!(s.restore_session);
        assert_eq!(s.last_local_dir.as_deref(), Some("/srv/old"));
    }

    #[test]
    fn store_load_migrates_last_local_dir_into_the_left_pane_dir() {
        // Arrange: an older file that only knows the single local directory.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("connections.json");
        let raw = r#"{"connections": [], "settings": {"last_local_dir": "/srv/old"}}"#;
        std::fs::write(&path, raw).expect("write store");
        // Act
        let store = Store::load(&path).expect("load");
        // Assert
        assert_eq!(store.settings.local_dirs[0].as_deref(), Some("/srv/old"));
        assert_eq!(store.settings.local_dirs[1], None);
    }

    #[test]
    fn migrate_keeps_a_left_pane_dir_that_is_already_set() {
        // Arrange
        let mut s = Settings {
            last_local_dir: Some("/srv/old".into()),
            local_dirs: [Some("/srv/new".into()), None],
            ..Settings::default()
        };
        // Act
        s.migrate();
        // Assert
        assert_eq!(s.local_dirs[0].as_deref(), Some("/srv/new"));
    }

    #[test]
    fn session_memory_round_trips_through_json() {
        // Arrange
        let id = Uuid::new_v4();
        let mut s = Settings {
            restore_session: false,
            pane_hosts: [None, Some(id)],
            ..Settings::default()
        };
        s.remember_dir(Some(id), Side::Right, "/var/log");
        s.remember_dir(None, Side::Left, "/home/me/dl");
        // Act
        let json = serde_json::to_string(&s).expect("serialize");
        let back: Settings = serde_json::from_str(&json).expect("parse");
        // Assert
        assert_eq!(
            back.last_dirs.get(&id).map(String::as_str),
            Some("/var/log")
        );
        assert_eq!(back.local_dirs[0].as_deref(), Some("/home/me/dl"));
        assert_eq!(back.pane_hosts, [None, Some(id)]);
        assert!(!back.restore_session);
    }

    #[test]
    fn remember_dir_keeps_one_directory_per_host_regardless_of_side() {
        // Arrange
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut s = Settings::default();
        // Act
        s.remember_dir(Some(a), Side::Right, "/etc");
        s.remember_dir(Some(b), Side::Left, "/opt");
        s.remember_dir(Some(a), Side::Left, "/etc/ssh");
        // Assert
        assert_eq!(
            s.remembered_dir(Some(a), Side::Right).as_deref(),
            Some("/etc/ssh")
        );
        assert_eq!(
            s.remembered_dir(Some(a), Side::Left).as_deref(),
            Some("/etc/ssh")
        );
        assert_eq!(
            s.remembered_dir(Some(b), Side::Right).as_deref(),
            Some("/opt")
        );
    }

    #[test]
    fn remembered_dir_is_none_for_a_host_never_browsed() {
        let s = Settings::default();
        assert_eq!(s.remembered_dir(Some(Uuid::new_v4()), Side::Right), None);
        assert_eq!(s.remembered_dir(None, Side::Left), None);
    }

    #[test]
    fn remember_dir_keeps_a_local_directory_per_pane() {
        // Arrange
        let mut s = Settings::default();
        // Act
        s.remember_dir(None, Side::Left, "/home/me");
        s.remember_dir(None, Side::Right, "/tmp");
        // Assert
        assert_eq!(
            s.remembered_dir(None, Side::Left).as_deref(),
            Some("/home/me")
        );
        assert_eq!(s.remembered_dir(None, Side::Right).as_deref(), Some("/tmp"));
        assert!(s.last_dirs.is_empty());
    }

    #[test]
    fn remember_dir_mirrors_the_left_local_dir_into_the_legacy_field_only() {
        // Arrange
        let mut s = Settings::default();
        // Act
        s.remember_dir(None, Side::Right, "/tmp");
        assert_eq!(s.last_local_dir, None);
        s.remember_dir(None, Side::Left, "/home/me");
        // Assert
        assert_eq!(s.last_local_dir.as_deref(), Some("/home/me"));
    }

    #[test]
    fn remember_dir_ignores_an_empty_directory() {
        let mut s = Settings::default();
        s.remember_dir(None, Side::Left, "");
        s.remember_dir(Some(Uuid::new_v4()), Side::Left, "");
        assert_eq!(s.local_dirs, [None, None]);
        assert!(s.last_dirs.is_empty());
    }

    #[test]
    fn prune_forgotten_hosts_drops_dirs_and_pane_hosts_of_deleted_connections() {
        // Arrange: one host kept, one deleted since its directory was remembered.
        let kept = host("kept");
        let gone = Uuid::new_v4();
        let mut store = Store {
            connections: vec![kept.clone()],
            settings: Settings::default(),
        };
        store
            .settings
            .remember_dir(Some(kept.id), Side::Right, "/srv");
        store.settings.remember_dir(Some(gone), Side::Left, "/opt");
        store.settings.pane_hosts = [Some(gone), Some(kept.id)];
        // Act
        store.prune_forgotten_hosts();
        // Assert
        assert_eq!(store.settings.last_dirs.len(), 1);
        assert!(store.settings.last_dirs.contains_key(&kept.id));
        assert_eq!(store.settings.pane_hosts, [None, Some(kept.id)]);
    }

    #[test]
    fn prune_forgotten_hosts_leaves_local_dirs_alone() {
        // Arrange
        let mut store = Store::default();
        store.settings.remember_dir(None, Side::Left, "/home/me");
        store.settings.remember_dir(None, Side::Right, "/tmp");
        // Act
        store.prune_forgotten_hosts();
        // Assert
        assert_eq!(store.settings.local_dirs[0].as_deref(), Some("/home/me"));
        assert_eq!(store.settings.local_dirs[1].as_deref(), Some("/tmp"));
    }
}
