//! Connection data model.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// How to authenticate to a host. Secrets are stored vault-encrypted (base64 blob),
/// never in plaintext on disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AuthMethod {
    /// Password auth. `password_enc` is a vault-encrypted blob (empty = prompt).
    Password { password_enc: String },
    /// Public-key auth from a private key file, with an optional encrypted passphrase.
    Key {
        private_key: String,
        passphrase_enc: Option<String>,
    },
    /// Use the running ssh-agent.
    Agent,
}

impl Default for AuthMethod {
    fn default() -> Self {
        AuthMethod::Password {
            password_enc: String::new(),
        }
    }
}

/// What kind of connection this is — determines what "Connect" does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ConnectionKind {
    /// SSH shell + SFTP file browsing (handled natively in-app).
    #[default]
    Ssh,
    /// Remote Desktop — launched via an external Remmina client.
    Rdp,
    /// Windows/Samba file share (SMB/CIFS) — native file browsing, no SSH needed.
    Smb,
    /// Windows Remote Management — command exec + file transfer over WinRM.
    WinRm,
}

impl ConnectionKind {
    pub fn label(&self) -> &'static str {
        match self {
            ConnectionKind::Ssh => "SSH/SFTP",
            ConnectionKind::Rdp => "RDP",
            ConnectionKind::Smb => "SMB",
            ConnectionKind::WinRm => "WinRM",
        }
    }
    pub fn default_port(&self) -> u16 {
        match self {
            ConnectionKind::Ssh => 22,
            ConnectionKind::Rdp => 3389,
            ConnectionKind::Smb => 445,
            ConnectionKind::WinRm => 5985,
        }
    }
    /// Whether this kind provides an in-app dual-pane file browser.
    pub fn browses_files(&self) -> bool {
        matches!(self, ConnectionKind::Ssh | ConnectionKind::Smb | ConnectionKind::WinRm)
    }
}

/// A single connection definition (SSH/SFTP or RDP).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Connection {
    pub id: Uuid,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
    #[serde(default)]
    pub kind: ConnectionKind,
    /// Windows domain (RDP only; empty = none).
    #[serde(default)]
    pub domain: String,
    /// WinRM over HTTPS: accept an invalid/self-signed TLS certificate (insecure).
    #[serde(default)]
    pub insecure_tls: bool,
    /// Group path, e.g. "WIM/Tovuz". Empty = top level.
    #[serde(default)]
    pub group: String,
    /// Optional accent color for the sidebar row.
    #[serde(default)]
    pub color: Option<[u8; 3]>,
    /// Optional free-text notes.
    #[serde(default)]
    pub notes: String,
}

impl Connection {
    pub fn new(name: impl Into<String>, host: impl Into<String>, username: impl Into<String>) -> Self {
        Connection {
            id: Uuid::new_v4(),
            name: name.into(),
            host: host.into(),
            port: 22,
            username: username.into(),
            auth: AuthMethod::default(),
            kind: ConnectionKind::Ssh,
            domain: String::new(),
            insecure_tls: false,
            group: String::new(),
            color: None,
            notes: String::new(),
        }
    }

    /// "user@host:port" label for display.
    pub fn target(&self) -> String {
        if self.port == 22 {
            format!("{}@{}", self.username, self.host)
        } else {
            format!("{}@{}:{}", self.username, self.host, self.port)
        }
    }
}

/// A file entry shown in a pane (local or remote).
#[derive(Clone, Debug)]
#[allow(dead_code)] // perm/symlink retained for future columns/tooltips
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Unix mtime (seconds), if known.
    pub mtime: Option<u64>,
    /// Unix permission bits, if known.
    pub perm: Option<u32>,
    /// Symlink target, if this is a symlink.
    pub symlink: bool,
}

impl FileEntry {
    pub fn kind_order(&self) -> u8 {
        // Directories sort before files by default.
        if self.is_dir {
            0
        } else {
            1
        }
    }
}
