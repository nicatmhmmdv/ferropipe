//! SMB/CIFS backend via `pavao` (libsmbclient). Lets Ferropipe browse and transfer
//! files on Windows/Samba shares with no SSH server on the target.
//!
//! The connection points at a server (`smb://host`) with an empty share, so the
//! root listing shows the server's shares (C$, ADMIN$, custom shares); navigating
//! into one browses its contents. libsmbclient is single-threaded — this backend
//! lives entirely on the worker thread.

use super::{RStat, RemoteFs, copy_with_progress};
use crate::model::{Connection, FileEntry};
use anyhow::{Result, anyhow};
use pavao::{SmbClient, SmbCredentials, SmbDirentType, SmbMode, SmbOpenOptions, SmbOptions};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct SmbFs {
    client: SmbClient,
    home: String,
}

fn to_secs(t: SystemTime) -> Option<u64> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
        .filter(|&s| s > 0)
}

fn norm(path: &str) -> String {
    if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    }
}

impl SmbFs {
    pub fn connect(conn: &Connection, secret: Option<&str>) -> Result<SmbFs> {
        let server = format!("smb://{}", conn.host);
        let workgroup = if conn.domain.is_empty() {
            "WORKGROUP"
        } else {
            &conn.domain
        };
        let creds = SmbCredentials::default()
            .server(server)
            .share("")
            .username(&conn.username)
            .password(secret.unwrap_or(""))
            .workgroup(workgroup);
        let opts = SmbOptions::default()
            .case_sensitive(false)
            .one_share_per_server(false);
        let client =
            SmbClient::new(creds, opts).map_err(|e| anyhow!("SMB connect failed: {e:?}"))?;
        let fs = SmbFs {
            client,
            home: "/".into(),
        };
        // Probe connectivity so auth errors surface at connect time.
        fs.list("/")
            .map_err(|e| anyhow!("SMB: cannot list shares ({e:#})"))?;
        Ok(fs)
    }
}

impl RemoteFs for SmbFs {
    fn home(&self) -> String {
        self.home.clone()
    }

    fn realpath(&self, path: &str) -> Result<String> {
        Ok(norm(path))
    }

    fn stat(&self, path: &str) -> Result<RStat> {
        let s = self.client.stat(norm(path)).map_err(|e| anyhow!("{e:?}"))?;
        Ok(RStat {
            is_dir: s.mode.is_dir(),
            size: s.size,
        })
    }

    fn list(&self, path: &str) -> Result<Vec<FileEntry>> {
        // The server root (the share list) must be the *bare* server URL:
        // libsmbclient enumerates shares from an empty path and rejects a "/"
        // path ("smb://host/") with EBADF. Inside a share, paths are used as-is.
        let is_root = path.is_empty() || path == "/";
        let p = if is_root { String::new() } else { norm(path) };
        // Inside a share, prefer list_dirplus (gives size + mtime). At the server
        // root it returns 0 entries (shares carry no stat info), so skip it there
        // and use list_dir, which enumerates the shares.
        if !is_root && let Ok(infos) = self.client.list_dirplus(&p) {
            let mut out = Vec::with_capacity(infos.len());
            for i in infos {
                let name = i.name().to_string();
                if name == "." || name == ".." {
                    continue;
                }
                let is_dir = i.get_type() == SmbDirentType::Dir;
                out.push(FileEntry {
                    name,
                    is_dir,
                    size: i.size,
                    mtime: to_secs(i.mtime),
                    perm: None,
                    symlink: false,
                });
            }
            return Ok(out);
        }
        let dirents = self.client.list_dir(&p).map_err(|e| anyhow!("{e:?}"))?;
        let mut out = Vec::with_capacity(dirents.len());
        for d in dirents {
            let name = d.name().to_string();
            if name == "." || name == ".." || name.is_empty() {
                continue;
            }
            let is_dir = matches!(
                d.get_type(),
                SmbDirentType::Dir
                    | SmbDirentType::FileShare
                    | SmbDirentType::Server
                    | SmbDirentType::Workgroup
            );
            // Skip non-navigable share types.
            if matches!(
                d.get_type(),
                SmbDirentType::PrinterShare | SmbDirentType::IpcShare | SmbDirentType::CommsShare
            ) {
                continue;
            }
            out.push(FileEntry {
                name,
                is_dir,
                size: 0,
                mtime: None,
                perm: None,
                symlink: false,
            });
        }
        Ok(out)
    }

    fn read_file(
        &self,
        path: &str,
        out: &mut dyn std::io::Write,
        progress: &mut dyn FnMut(u64),
    ) -> Result<()> {
        let mut f = self
            .client
            .open_with(norm(path), SmbOpenOptions::default().read(true))
            .map_err(|e| anyhow!("open {path}: {e:?}"))?;
        copy_with_progress(&mut f, out, progress)
    }

    fn write_file(
        &self,
        path: &str,
        input: &mut dyn std::io::Read,
        progress: &mut dyn FnMut(u64),
    ) -> Result<()> {
        let tmp = format!("{path}.ferropipe-part");
        {
            let mut f = self
                .client
                .open_with(
                    &tmp,
                    SmbOpenOptions::default()
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .mode(0o644),
                )
                .map_err(|e| anyhow!("create {tmp}: {e:?}"))?;
            copy_with_progress(input, &mut f, progress)?;
        }
        let _ = self.client.unlink(norm(path));
        self.client
            .rename(tmp, norm(path))
            .map_err(|e| anyhow!("finalize: {e:?}"))?;
        Ok(())
    }

    fn mkdir(&self, path: &str) -> Result<()> {
        self.client
            .mkdir(norm(path), SmbMode::from(0o755))
            .map_err(|e| anyhow!("{e:?}"))?;
        Ok(())
    }

    fn create_empty(&self, path: &str) -> Result<()> {
        let _f = self
            .client
            .open_with(
                norm(path),
                SmbOpenOptions::default()
                    .write(true)
                    .create(true)
                    .exclusive(true)
                    .mode(0o644),
            )
            .map_err(|e| anyhow!("{e:?}"))?;
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.client
            .rename(norm(from), norm(to))
            .map_err(|e| anyhow!("{e:?}"))?;
        Ok(())
    }

    fn remove_file(&self, path: &str) -> Result<()> {
        self.client
            .unlink(norm(path))
            .map_err(|e| anyhow!("{e:?}"))?;
        Ok(())
    }

    fn remove_dir(&self, path: &str) -> Result<()> {
        self.client
            .rmdir(norm(path))
            .map_err(|e| anyhow!("{e:?}"))?;
        Ok(())
    }
}

/// Live probe against a saved SMB host, opt-in: set `FERROPIPE_SMB_PROBE` to
/// the saved connection's name and run
/// `FERROPIPE_SMB_PROBE=<name> cargo test smb::live_probe -- --ignored --nocapture`.
/// Reads the real store and vault, never prints the secret, and only writes one
/// scratch file under `C$/Windows/Temp`, which it removes again.
#[cfg(test)]
mod live_probe {
    use super::super::{download, upload};
    use super::*;
    use crate::model::{AuthMethod, ConnectionKind};
    use std::path::PathBuf;

    fn saved_smb_connection(name: &str) -> (Connection, String) {
        let dirs = crate::store::project_dirs().expect("config dir");
        let store = crate::store::Store::load(&crate::store::store_path(&dirs)).expect("store");
        let key = crate::vault::default_key_path(dirs.config_dir());
        let vault = crate::vault::Vault::load_or_create(&key).expect("vault");
        let conn = store
            .connections
            .iter()
            .find(|c| c.name == name && c.kind == ConnectionKind::Smb)
            .cloned()
            .expect("saved SMB connection with that name");
        let secret = match &conn.auth {
            AuthMethod::Password { password_enc } => vault.decrypt(password_enc).expect("decrypt"),
            _ => String::new(),
        };
        (conn, secret)
    }

    fn names(entries: &[FileEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    #[ignore = "needs a saved SMB host on the network; see the module docs"]
    fn smb_round_trip_against_saved_host() {
        let Ok(name) = std::env::var("FERROPIPE_SMB_PROBE") else {
            println!("FERROPIPE_SMB_PROBE not set; skipping");
            return;
        };
        let (conn, secret) = saved_smb_connection(&name);
        println!("[1] connecting to smb://{} as {}", conn.host, conn.username);
        let fs = SmbFs::connect(&conn, Some(&secret)).expect("connect");
        println!(
            "[2] shares: {:?}",
            names(&fs.list("/").expect("list shares"))
        );
        let win = fs.list("/C$/Windows").expect("list C$/Windows");
        println!("[3] C$/Windows: {} entries", win.len());

        let tmp = tempfile::tempdir().expect("tempdir");
        let quiet = |_e: super::super::Event| {};
        download(
            &fs,
            1,
            &["/C$/Windows/win.ini".to_string()],
            tmp.path(),
            &quiet,
        )
        .expect("download win.ini");
        let bytes = std::fs::read(tmp.path().join("win.ini")).expect("downloaded file");
        println!("[4] downloaded win.ini: {} bytes", bytes.len());
        assert!(!bytes.is_empty());

        let file = format!("ferropipe-probe-{}.txt", std::process::id());
        let local: PathBuf = tmp.path().join(&file);
        std::fs::write(&local, b"ferropipe smb probe\n").expect("write scratch");
        upload(&fs, 2, &[local], "/C$/Windows/Temp", &quiet).expect("upload");
        let remote_path = format!("/C$/Windows/Temp/{file}");
        let temp = fs.list("/C$/Windows/Temp").expect("list Temp");
        assert!(
            names(&temp).contains(&file.as_str()),
            "uploaded file not listed"
        );
        let mut back = Vec::new();
        fs.read_file(&remote_path, &mut back, &mut |_| {})
            .expect("read back");
        assert_eq!(back, b"ferropipe smb probe\n");
        println!("[5] uploaded, listed and read back {file}");

        fs.remove_file(&remote_path).expect("remove scratch");
        let temp = fs.list("/C$/Windows/Temp").expect("list Temp again");
        assert!(
            !names(&temp).contains(&file.as_str()),
            "scratch file still there"
        );
        println!("[6] removed {file}");
    }
}
