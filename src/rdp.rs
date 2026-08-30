//! RDP support via an external Remmina client.
//!
//! Ferropipe generates a `.remmina` profile with sensible, fast defaults
//! (GFX AVC444 codec + network autodetect — the settings that make FreeRDP feel
//! responsive) and launches `remmina -c <profile>`. The decrypted password is
//! written base64-encoded (Remmina's file-backend format) into a 0600 profile
//! inside Ferropipe's config dir.

use crate::model::Connection;
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use std::path::PathBuf;
use std::process::Command;

/// Directory where generated Remmina profiles are stored.
fn profiles_dir(config_dir: &std::path::Path) -> PathBuf {
    config_dir.join("rdp-profiles")
}

/// Build the `.remmina` profile text for a connection.
fn profile_text(conn: &Connection, password: &str) -> String {
    let server = if conn.port == 3389 {
        conn.host.clone()
    } else {
        format!("{}:{}", conn.host, conn.port)
    };
    let pw_b64 = if password.is_empty() {
        // "." tells Remmina to prompt / use its own secret store.
        ".".to_string()
    } else {
        B64.encode(password)
    };
    // colordepth=66 → "GFX AVC444"; network=autodetect adapts encoding to the link.
    format!(
        "[remmina]\n\
         name=Ferropipe - {name}\n\
         protocol=RDP\n\
         server={server}\n\
         username={user}\n\
         domain={domain}\n\
         password={pw}\n\
         colordepth=66\n\
         network=autodetect\n\
         quality=0\n\
         sound=off\n\
         microphone=0\n\
         glyph-cache=1\n\
         disableclipboard=0\n\
         disable-smooth-scrolling=0\n\
         cert_ignore=1\n\
         ignore-tls-errors=1\n\
         window_maximize=1\n\
         viewmode=1\n\
         scale=2\n",
        name = conn.name,
        server = server,
        user = conn.username,
        domain = conn.domain,
        pw = pw_b64,
    )
}

/// Write the profile (0600) and launch Remmina against it. Returns the profile path.
pub fn launch(conn: &Connection, password: &str, config_dir: &std::path::Path) -> Result<PathBuf> {
    let remmina = which("remmina").ok_or_else(|| {
        anyhow!("remmina is not installed. Install it with: sudo pacman -S remmina freerdp")
    })?;
    let dir = profiles_dir(config_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let file = dir.join(format!("ferropipe-{}.remmina", conn.id));
    std::fs::write(&file, profile_text(conn, password)).context("write remmina profile")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
    }
    // Use the absolute path (robust against a minimal PATH when launched from a
    // desktop launcher) and fully detach stdio so Remmina outlives Ferropipe.
    Command::new(&remmina)
        .arg("-c")
        .arg(&file)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("failed to launch {}", remmina.display()))?;
    Ok(file)
}

/// Minimal `which`: is `bin` on PATH?
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|p| p.join(bin))
        .find(|p| p.is_file())
}
