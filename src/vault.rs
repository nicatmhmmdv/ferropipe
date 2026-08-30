//! Local secret vault: AES-256-GCM encryption of stored passwords/passphrases.
//!
//! A random 256-bit key is generated on first run and stored in the app config
//! directory with 0600 permissions. Each secret is encrypted with a fresh random
//! 96-bit nonce; the on-disk form is `base64(nonce) : base64(ciphertext)`.
//!
//! This mirrors the "no master password prompt" UX: security rests on filesystem
//! permissions of the key file. A future option can derive the key from an Argon2id
//! master password instead (see `derive_key_from_password`).

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use rand::rngs::OsRng;
use rand::RngCore;
use std::path::{Path, PathBuf};
use zeroize::Zeroize;

const NONCE_LEN: usize = 12; // 96-bit for AES-GCM

const KEY_LEN: usize = 32; // 256-bit

/// The vault holds the symmetric key in memory for the session.
pub struct Vault {
    key: [u8; KEY_LEN],
}

impl Drop for Vault {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

impl Vault {
    /// Load the key from `key_path`, or generate + persist a new one (0600).
    pub fn load_or_create(key_path: &Path) -> Result<Self> {
        if let Some(parent) = key_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating vault dir {}", parent.display()))?;
        }
        let key = if key_path.exists() {
            let raw = std::fs::read(key_path).context("reading vault key")?;
            if raw.len() != KEY_LEN {
                return Err(anyhow!(
                    "vault key file has wrong length ({} bytes)",
                    raw.len()
                ));
            }
            let mut k = [0u8; KEY_LEN];
            k.copy_from_slice(&raw);
            k
        } else {
            let mut arr = [0u8; KEY_LEN];
            OsRng.fill_bytes(&mut arr);
            write_private(key_path, &arr).context("writing new vault key")?;
            arr
        };
        Ok(Vault { key })
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new_from_slice(&self.key).expect("vault key is 32 bytes")
    }

    /// Encrypt a UTF-8 secret into a portable `nonce:ciphertext` base64 string.
    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        if plaintext.is_empty() {
            return Ok(String::new());
        }
        let cipher = self.cipher();
        let mut nonce_bytes = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::try_from(&nonce_bytes[..]).map_err(|_| anyhow!("nonce build"))?;
        let ct = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| anyhow!("encrypt failed: {e}"))?;
        Ok(format!("{}:{}", B64.encode(nonce_bytes), B64.encode(ct)))
    }

    /// Decrypt a `nonce:ciphertext` blob back to plaintext.
    pub fn decrypt(&self, blob: &str) -> Result<String> {
        if blob.is_empty() {
            return Ok(String::new());
        }
        let (n_b64, c_b64) = blob
            .split_once(':')
            .ok_or_else(|| anyhow!("malformed secret blob"))?;
        let nonce_bytes = B64.decode(n_b64).context("bad nonce b64")?;
        let ct = B64.decode(c_b64).context("bad ciphertext b64")?;
        if nonce_bytes.len() != NONCE_LEN {
            return Err(anyhow!("bad nonce length"));
        }
        let nonce = Nonce::try_from(&nonce_bytes[..]).map_err(|_| anyhow!("nonce build"))?;
        let cipher = self.cipher();
        let pt = cipher
            .decrypt(&nonce, ct.as_ref())
            .map_err(|e| anyhow!("decrypt failed (wrong key?): {e}"))?;
        // Consume `pt` directly into the String (no intermediate clone left in memory).
        let s = String::from_utf8(pt).context("secret not utf-8")?;
        Ok(s)
    }
}

/// Write bytes to a file with 0600 permissions on Unix.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Default vault key path inside the app config dir.
pub fn default_key_path(config_dir: &Path) -> PathBuf {
    config_dir.join("vault.key")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault() -> Vault {
        // In-memory vault with a fixed random key (no file needed for the crypto test).
        let mut key = [0u8; KEY_LEN];
        OsRng.fill_bytes(&mut key);
        Vault { key }
    }

    #[test]
    fn roundtrip_recovers_plaintext() {
        let v = temp_vault();
        for s in ["hunter2", "p@ss w0rd!", "unicode-ключ-🔑", ""] {
            let enc = v.encrypt(s).unwrap();
            let dec = v.decrypt(&enc).unwrap();
            assert_eq!(dec, s, "roundtrip mismatch for {s:?}");
        }
    }

    #[test]
    fn empty_secret_encrypts_to_empty() {
        let v = temp_vault();
        assert_eq!(v.encrypt("").unwrap(), "");
        assert_eq!(v.decrypt("").unwrap(), "");
    }

    #[test]
    fn nonce_differs_per_encryption() {
        let v = temp_vault();
        let a = v.encrypt("same").unwrap();
        let b = v.encrypt("same").unwrap();
        assert_ne!(a, b, "nonce reuse: identical ciphertext for same plaintext");
    }

    #[test]
    fn wrong_key_fails_to_decrypt() {
        let a = temp_vault();
        let b = temp_vault();
        let enc = a.encrypt("secret").unwrap();
        assert!(b.decrypt(&enc).is_err(), "decrypt should fail under a different key");
    }
}
