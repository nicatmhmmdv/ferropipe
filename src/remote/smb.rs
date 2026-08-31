//! SMB/CIFS backend via `pavao` (libsmbclient). Lets Ferropipe browse and transfer
//! files on Windows/Samba shares with no SSH server on the target.
//!
//! The connection points at a server (`smb://host`) with an empty share, so the
//! root listing shows the server's shares (C$, ADMIN$, custom shares); navigating
//! into one browses its contents. libsmbclient is single-threaded — this backend
//! lives entirely on the worker thread.

use super::{copy_with_progress, RStat, RemoteFs};
use crate::model::{Connection, FileEntry};
use anyhow::{anyhow, Result};
use pavao::{
    SmbClient, SmbCredentials, SmbDirentType, SmbMode, SmbOpenOptions, SmbOptions,
};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct SmbFs {
    client: SmbClient,
    home: String,
}

fn to_secs(t: SystemTime) -> Option<u64> {
    t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()).filter(|&s| s > 0)
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
        let workgroup = if conn.domain.is_empty() { "WORKGROUP" } else { &conn.domain };
        let creds = SmbCredentials::default()
            .server(server)
            .share("")
            .username(&conn.username)
            .password(secret.unwrap_or(""))
            .workgroup(workgroup);
        let opts = SmbOptions::default().case_sensitive(false).one_share_per_server(false);
        let client = SmbClient::new(creds, opts).map_err(|e| anyhow!("SMB connect failed: {e:?}"))?;
        let fs = SmbFs { client, home: "/".into() };
        // Probe connectivity so auth errors surface at connect time.
        fs.list("/").map_err(|e| anyhow!("SMB: cannot list shares ({e:#})"))?;
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
        Ok(RStat { is_dir: s.mode.is_dir(), size: s.size })
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
        if !is_root {
            if let Ok(infos) = self.client.list_dirplus(&p) {
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
            out.push(FileEntry { name, is_dir, size: 0, mtime: None, perm: None, symlink: false });
        }
        Ok(out)
    }

    fn read_file(&self, path: &str, out: &mut dyn std::io::Write, progress: &mut dyn FnMut(u64)) -> Result<()> {
        let mut f = self
            .client
            .open_with(norm(path), SmbOpenOptions::default().read(true))
            .map_err(|e| anyhow!("open {path}: {e:?}"))?;
        copy_with_progress(&mut f, out, progress)
    }

    fn write_file(&self, path: &str, input: &mut dyn std::io::Read, progress: &mut dyn FnMut(u64)) -> Result<()> {
        let tmp = format!("{path}.ferropipe-part");
        {
            let mut f = self
                .client
                .open_with(&tmp, SmbOpenOptions::default().write(true).create(true).truncate(true).mode(0o644))
                .map_err(|e| anyhow!("create {tmp}: {e:?}"))?;
            copy_with_progress(input, &mut f, progress)?;
        }
        let _ = self.client.unlink(norm(path));
        self.client.rename(tmp, norm(path)).map_err(|e| anyhow!("finalize: {e:?}"))?;
        Ok(())
    }

    fn mkdir(&self, path: &str) -> Result<()> {
        self.client.mkdir(norm(path), SmbMode::from(0o755)).map_err(|e| anyhow!("{e:?}"))?;
        Ok(())
    }

    fn create_empty(&self, path: &str) -> Result<()> {
        let _f = self
            .client
            .open_with(norm(path), SmbOpenOptions::default().write(true).create(true).exclusive(true).mode(0o644))
            .map_err(|e| anyhow!("{e:?}"))?;
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.client.rename(norm(from), norm(to)).map_err(|e| anyhow!("{e:?}"))?;
        Ok(())
    }

    fn remove_file(&self, path: &str) -> Result<()> {
        self.client.unlink(norm(path)).map_err(|e| anyhow!("{e:?}"))?;
        Ok(())
    }

    fn remove_dir(&self, path: &str) -> Result<()> {
        self.client.rmdir(norm(path)).map_err(|e| anyhow!("{e:?}"))?;
        Ok(())
    }
}
