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
use std::path::{Path, PathBuf};
use std::process::Command;

/// Directory where generated Remmina profiles are stored.
fn profiles_dir(config_dir: &std::path::Path) -> PathBuf {
    config_dir.join("rdp-profiles")
}

/// Read Remmina's `secret=` key from `~/.config/remmina/remmina.pref`. This is the
/// base64 of a 32-byte value (24-byte 3DES key + 8-byte IV) Remmina uses to
/// encrypt profile passwords.
fn remmina_secret() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let pref = Path::new(&home).join(".config/remmina/remmina.pref");
    let content = std::fs::read_to_string(pref).ok()?;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("secret=") {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Encrypt a password the way Remmina's `remmina_crypt_encrypt` does, so a
/// generated profile can supply credentials without prompting: 3DES-CBC (EDE3)
/// with key = secret[0..24], IV = secret[24..32], zero-padded to a multiple of 8
/// (always adding at least one byte), then base64. Returns `None` if the secret
/// can't be decoded (caller falls back to the kiosk base64 format).
fn remmina_encrypt(password: &str, secret_b64: &str) -> Option<String> {
    use cbc::cipher::{BlockEncryptMut, KeyIvInit};
    use cbc::cipher::generic_array::GenericArray;
    type Enc = cbc::Encryptor<des::TdesEde3>;

    let secret = B64.decode(secret_b64.trim()).ok()?;
    if secret.len() < 32 {
        return None;
    }
    let key: [u8; 24] = secret[0..24].try_into().ok()?;
    let iv: [u8; 8] = secret[24..32].try_into().ok()?;

    let pw = password.as_bytes();
    // Remmina pads to a multiple of the 8-byte DES block, always adding ≥1 zero.
    let padded_len = pw.len() + (8 - pw.len() % 8);
    let mut buf = vec![0u8; padded_len];
    buf[..pw.len()].copy_from_slice(pw);

    let mut enc = Enc::new(&key.into(), &iv.into());
    for chunk in buf.chunks_mut(8) {
        enc.encrypt_block_mut(GenericArray::from_mut_slice(chunk));
    }
    Some(B64.encode(&buf))
}

/// Encode a password for a Remmina profile's `password=` field. Uses Remmina's
/// own 3DES scheme when its secret is available (so Remmina decrypts it and
/// connects without prompting); otherwise the kiosk base64 fallback, or "." to
/// let Remmina prompt when there's no password at all.
fn encode_password(password: &str) -> String {
    if password.is_empty() {
        return ".".to_string();
    }
    remmina_secret()
        .and_then(|s| remmina_encrypt(password, &s))
        .unwrap_or_else(|| B64.encode(password))
}

/// Build the `.remmina` profile text for a connection.
fn profile_text(conn: &Connection, password: &str) -> String {
    let server = if conn.port == 3389 {
        conn.host.clone()
    } else {
        format!("{}:{}", conn.host, conn.port)
    };
    let pw_b64 = encode_password(password);
    // Redirect the user's home directory into the session so files can be dragged
    // both ways — it shows up in Windows as a drive under \\tsclient (This PC).
    // clipboard is also on, for quick copy/paste of individual files.
    let share = std::env::var("HOME").unwrap_or_default();
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
         sharefolder={share}\n\
         drive={share}\n\
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
        share = share,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remmina_encrypt_matches_reference() {
        // Cross-checked against `openssl enc -des-ede3-cbc -nopad` with a fixed
        // key/iv (secret = bytes 0..=31) and password "secret" (zero-padded to 8).
        // This pins the exact byte layout Remmina's remmina_crypt_encrypt uses.
        let secret_b64 = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
        let got = remmina_encrypt("secret", secret_b64);
        assert_eq!(got.as_deref(), Some("lHQN3eia3Uw="));
    }

    #[test]
    fn remmina_encrypt_pads_exact_block_multiple() {
        // An 8-char password must still get a full extra padding block (Remmina
        // always adds ≥1 byte), so the ciphertext is 16 bytes → 24 base64 chars.
        let secret_b64 = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
        let out = remmina_encrypt("password", secret_b64).unwrap();
        let raw = B64.decode(out).unwrap();
        assert_eq!(raw.len(), 16, "8-byte input pads to two 8-byte blocks");
    }

    #[test]
    fn encode_password_empty_is_dot() {
        assert_eq!(encode_password(""), ".");
    }
}
