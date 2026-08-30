//! Persistent storage of connections and app settings (JSON in the config dir).
use crate::model::Connection;
use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
    #[serde(default)]
    pub last_local_dir: Option<String>,
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
        let store: Store = serde_json::from_str(&raw).context("parsing store json")?;
        Ok(store)
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
