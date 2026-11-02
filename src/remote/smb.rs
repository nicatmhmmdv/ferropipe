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
        // Through a `.ferropipe-part` sibling that is removed again on failure.
        let tmp = format!("{path}.ferropipe-part");
        let written: Result<()> = (|| {
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
                .rename(tmp.clone(), norm(path))
                .map_err(|e| anyhow!("finalize: {e:?}"))
        })();
        if written.is_err() {
            let _ = self.client.unlink(&tmp);
        }
        written
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

/// Shared plumbing for the opt-in live probes in `smb`, `sftp` and `winrm`:
/// reading a saved connection and its secret out of the real store and vault,
/// the scratch-file round trip every probe runs, and a guard that removes the
/// scratch file even when an assertion fails halfway. Test-only; it lives here
/// so no new `mod` line is needed in `remote/mod.rs`.
#[cfg(test)]
pub(crate) mod probe_support {
    use super::super::{Event, OnConflict, Outcome, RemoteFs, upload};
    use crate::model::{AuthMethod, Connection, ConnectionKind};
    use crate::vault::Vault;
    use std::cell::RefCell;

    const SCRATCH_BODY: &[u8] = b"ferropipe live probe\n";

    /// Load the saved connection called `name` of `kind` from the real store,
    /// together with its decrypted secret (see [`secret_for`]).
    pub(crate) fn saved_connection(name: &str, kind: ConnectionKind) -> (Connection, String) {
        let dirs = crate::store::project_dirs().expect("config dir");
        let store = crate::store::Store::load(&crate::store::store_path(&dirs)).expect("store");
        let key = crate::vault::default_key_path(dirs.config_dir());
        let vault = Vault::load_or_create(&key).expect("vault");
        let conn = store
            .connections
            .iter()
            .find(|c| c.name == name && c.kind == kind)
            .cloned()
            .unwrap_or_else(|| panic!("no saved {} connection named {name:?}", kind.label()));
        let secret = secret_for(&conn.auth, &vault);
        (conn, secret)
    }

    /// The plaintext secret `auth` carries: the password, the key passphrase,
    /// or "" when there is none (agent auth, unset password, no passphrase).
    pub(crate) fn secret_for(auth: &AuthMethod, vault: &Vault) -> String {
        let blob = match auth {
            AuthMethod::Password { password_enc } => password_enc.as_str(),
            AuthMethod::Key { passphrase_enc, .. } => passphrase_enc.as_deref().unwrap_or(""),
            AuthMethod::Agent => "",
        };
        if blob.is_empty() {
            return String::new();
        }
        vault.decrypt(blob).expect("decrypt saved secret")
    }

    /// Short auth label for progress output (never the secret or the key path).
    pub(crate) fn auth_label(auth: &AuthMethod) -> &'static str {
        match auth {
            AuthMethod::Password { .. } => "password",
            AuthMethod::Key { .. } => "key",
            AuthMethod::Agent => "agent",
        }
    }

    /// Whether `dir` on `fs` currently lists an entry called `name`.
    pub(crate) fn lists(fs: &dyn RemoteFs, dir: &str, name: &str) -> bool {
        let entries = fs.list(dir).unwrap_or_else(|e| panic!("list {dir}: {e:#}"));
        entries.iter().any(|e| e.name == name)
    }

    /// Run `command` on `fs`; returns its exit code and stdout lines.
    pub(crate) fn exec_stdout(fs: &dyn RemoteFs, command: &str) -> (i32, Vec<String>) {
        let lines = RefCell::new(Vec::new());
        let code = fs
            .exec(command, &|ev| {
                if let Event::ExecOutput {
                    line,
                    is_err: false,
                } = ev
                {
                    lines.borrow_mut().push(line);
                }
            })
            .unwrap_or_else(|e| panic!("exec {command}: {e:#}"));
        (code, lines.into_inner())
    }

    /// A remote scratch file that is removed when dropped, so an assertion
    /// failing after the upload still cleans up on the host. `remove_now`
    /// removes it eagerly instead and reports the outcome.
    pub(crate) struct ScratchFile<'a> {
        fs: &'a dyn RemoteFs,
        path: String,
        armed: bool,
    }

    impl<'a> ScratchFile<'a> {
        pub(crate) fn new(fs: &'a dyn RemoteFs, path: String) -> Self {
            ScratchFile {
                fs,
                path,
                armed: true,
            }
        }

        pub(crate) fn remove_now(&mut self) -> anyhow::Result<()> {
            self.armed = false;
            self.fs.remove_file(&self.path)
        }
    }

    impl Drop for ScratchFile<'_> {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            if let Err(e) = self.fs.remove_file(&self.path) {
                eprintln!("cleanup: could not remove {}: {e:#}", self.path);
            }
        }
    }

    /// Upload `ferropipe-probe-<pid>.txt` into `dir`, check it is listed, read
    /// it back, remove it and check it is gone, printing steps `first_step` and
    /// `first_step + 1`. The file is removed even if an assertion fails.
    pub(crate) fn scratch_round_trip(fs: &dyn RemoteFs, dir: &str, first_step: u32) {
        let file = format!("ferropipe-probe-{}.txt", std::process::id());
        let tmp = tempfile::tempdir().expect("tempdir");
        let local = tmp.path().join(&file);
        std::fs::write(&local, SCRATCH_BODY).expect("write local scratch");
        let outcome =
            upload(fs, 1, &[local], dir, OnConflict::Overwrite, &|_ev| {}).expect("upload scratch");
        assert_eq!(outcome, Outcome::Done, "upload was parked on a conflict");
        let remote_path = fs.join(dir, &file);
        let mut scratch = ScratchFile::new(fs, remote_path.clone());
        assert!(lists(fs, dir, &file), "uploaded {file} not listed in {dir}");
        let mut back = Vec::new();
        fs.read_file(&remote_path, &mut back, &mut |_| {})
            .expect("read back");
        assert_eq!(back, SCRATCH_BODY, "read back differs from the upload");
        println!("[{first_step}] uploaded, listed and read back {file}");
        scratch.remove_now().expect("remove scratch");
        assert!(!lists(fs, dir, &file), "{file} still listed after removal");
        println!("[{}] removed {file}", first_step + 1);
    }

    #[cfg(test)]
    mod tests {
        use super::super::super::RStat;
        use super::*;
        use crate::model::FileEntry;
        use anyhow::Result;
        use std::panic::{AssertUnwindSafe, catch_unwind};

        fn temp_vault() -> (tempfile::TempDir, Vault) {
            let dir = tempfile::tempdir().expect("tempdir");
            let vault = Vault::load_or_create(&dir.path().join("vault.key")).expect("vault");
            (dir, vault)
        }

        #[test]
        fn secret_for_decrypts_a_saved_password() {
            let (_dir, vault) = temp_vault();
            let auth = AuthMethod::Password {
                password_enc: vault.encrypt("hunter2").expect("encrypt"),
            };
            assert_eq!(secret_for(&auth, &vault), "hunter2");
        }

        #[test]
        fn secret_for_decrypts_a_key_passphrase() {
            let (_dir, vault) = temp_vault();
            let auth = AuthMethod::Key {
                private_key: "/home/u/.ssh/id_ed25519".into(),
                passphrase_enc: Some(vault.encrypt("open sesame").expect("encrypt")),
            };
            assert_eq!(secret_for(&auth, &vault), "open sesame");
        }

        #[test]
        fn secret_for_is_empty_without_a_saved_secret() {
            let (_dir, vault) = temp_vault();
            let unset_password = AuthMethod::Password {
                password_enc: String::new(),
            };
            let bare_key = AuthMethod::Key {
                private_key: "/home/u/.ssh/id_ed25519".into(),
                passphrase_enc: None,
            };
            assert_eq!(secret_for(&unset_password, &vault), "");
            assert_eq!(secret_for(&bare_key, &vault), "");
            assert_eq!(secret_for(&AuthMethod::Agent, &vault), "");
        }

        /// Records `remove_file` calls; everything else is out of scope.
        #[derive(Default)]
        struct FakeFs {
            removed: RefCell<Vec<String>>,
        }

        impl RemoteFs for FakeFs {
            fn home(&self) -> String {
                "/".into()
            }
            fn realpath(&self, path: &str) -> Result<String> {
                Ok(path.into())
            }
            fn stat(&self, _: &str) -> Result<RStat> {
                unimplemented!()
            }
            fn list(&self, _: &str) -> Result<Vec<FileEntry>> {
                Ok(Vec::new())
            }
            fn read_file(
                &self,
                _: &str,
                _: &mut dyn std::io::Write,
                _: &mut dyn FnMut(u64),
            ) -> Result<()> {
                unimplemented!()
            }
            fn write_file(
                &self,
                _: &str,
                _: &mut dyn std::io::Read,
                _: &mut dyn FnMut(u64),
            ) -> Result<()> {
                unimplemented!()
            }
            fn mkdir(&self, _: &str) -> Result<()> {
                unimplemented!()
            }
            fn create_empty(&self, _: &str) -> Result<()> {
                unimplemented!()
            }
            fn rename(&self, _: &str, _: &str) -> Result<()> {
                unimplemented!()
            }
            fn remove_file(&self, path: &str) -> Result<()> {
                self.removed.borrow_mut().push(path.into());
                Ok(())
            }
            fn remove_dir(&self, _: &str) -> Result<()> {
                unimplemented!()
            }
        }

        #[test]
        fn scratch_file_is_removed_when_a_panic_unwinds_past_it() {
            let fs = FakeFs::default();
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                let _scratch = ScratchFile::new(&fs, "/tmp/ferropipe-probe-1.txt".into());
                panic!("assertion failed halfway");
            }));
            assert!(outcome.is_err());
            assert_eq!(*fs.removed.borrow(), ["/tmp/ferropipe-probe-1.txt"]);
        }

        #[test]
        fn scratch_file_removed_now_is_not_removed_again_on_drop() {
            let fs = FakeFs::default();
            {
                let mut scratch = ScratchFile::new(&fs, "/tmp/ferropipe-probe-2.txt".into());
                scratch.remove_now().expect("remove");
            }
            assert_eq!(*fs.removed.borrow(), ["/tmp/ferropipe-probe-2.txt"]);
        }
    }
}

/// Live probe against a saved SMB host, opt-in: set `FERROPIPE_SMB_PROBE` to
/// the saved connection's name and run
/// `FERROPIPE_SMB_PROBE=<name> cargo test smb::live_probe -- --ignored --nocapture`.
/// Reads the real store and vault, never prints the secret, downloads
/// `C$/Windows/win.ini` into a local temp dir, and writes one scratch file
/// `ferropipe-probe-<pid>.txt` under `C$/Windows/Temp`, which it removes again.
#[cfg(test)]
mod live_probe {
    use super::super::{OnConflict, Outcome, download};
    use super::probe_support::{saved_connection, scratch_round_trip};
    use super::*;
    use crate::model::ConnectionKind;

    fn names(entries: &[FileEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// Download `C$/Windows/win.ini` into a temp dir; returns its size.
    fn download_win_ini(fs: &SmbFs) -> usize {
        let tmp = tempfile::tempdir().expect("tempdir");
        let remote = ["/C$/Windows/win.ini".to_string()];
        let outcome = download(fs, 1, &remote, tmp.path(), OnConflict::Overwrite, &|_ev| {})
            .expect("download win.ini");
        assert_eq!(outcome, Outcome::Done, "download was parked on a conflict");
        std::fs::read(tmp.path().join("win.ini"))
            .expect("downloaded file")
            .len()
    }

    #[test]
    #[ignore = "needs a saved SMB host on the network; see the module docs"]
    fn smb_round_trip_against_saved_host() {
        let Ok(name) = std::env::var("FERROPIPE_SMB_PROBE") else {
            println!("FERROPIPE_SMB_PROBE not set; skipping");
            return;
        };
        let (conn, secret) = saved_connection(&name, ConnectionKind::Smb);
        println!("[1] connecting to smb://{} as {}", conn.host, conn.username);
        let fs = SmbFs::connect(&conn, Some(&secret)).expect("connect");
        println!(
            "[2] shares: {:?}",
            names(&fs.list("/").expect("list shares"))
        );
        let win = fs.list("/C$/Windows").expect("list C$/Windows");
        println!("[3] C$/Windows: {} entries", win.len());
        let size = download_win_ini(&fs);
        println!("[4] downloaded win.ini: {size} bytes");
        assert!(size > 0, "win.ini came back empty");
        scratch_round_trip(&fs, "/C$/Windows/Temp", 5);
    }
}
