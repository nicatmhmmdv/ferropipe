//! WinRM backend for Windows hosts without SSH: file browsing/transfer (via winrm-rs
//! built-in upload/download + PowerShell) and command execution.
//!
//! Paths are kept forward-slashed for the UI and converted to backslashes when building
//! PowerShell commands. All async winrm-rs calls are driven on a dedicated tokio runtime
//! owned by this backend (which lives on the worker thread).

use super::{Event, RStat, RemoteFs, copy_with_progress};
use crate::model::{Connection, FileEntry};
use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::io::{Read, Write};
use winrm_rs::{AuthMethod, WinrmClient, WinrmConfig, WinrmCredentials};

pub struct WinRmFs {
    rt: tokio::runtime::Runtime,
    client: WinrmClient,
    host: String,
    home: String,
}

#[derive(Deserialize)]
struct WinEntry {
    n: String,
    d: bool,
    #[serde(default)]
    s: i64,
    #[serde(default)]
    m: i64,
}

/// Single-quote and backslash-normalize a path for PowerShell.
fn psq(path: &str) -> String {
    let w = path.replace('/', "\\").replace('\'', "''");
    format!("'{w}'")
}

impl WinRmFs {
    pub fn connect(conn: &Connection, secret: Option<&str>) -> Result<WinRmFs> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| anyhow!("tokio runtime: {e}"))?;
        let cfg = WinrmConfig {
            port: conn.port,
            use_tls: conn.port == 5986,
            // Strict TLS by default; only relaxed when the user opts in per-connection.
            accept_invalid_certs: conn.insecure_tls,
            auth_method: AuthMethod::Ntlm,
            ..WinrmConfig::default()
        };
        let creds = WinrmCredentials::new(
            conn.username.clone(),
            secret.unwrap_or("").to_string(),
            conn.domain.clone(),
        );
        let client = WinrmClient::new(cfg, creds).map_err(|e| anyhow!("WinRM client: {e}"))?;
        let mut fs = WinRmFs {
            rt,
            client,
            host: conn.host.clone(),
            home: "C:/".into(),
        };
        // Probe connectivity + resolve home (surfaces auth errors at connect time).
        let out = fs
            .ps_ok("$env:USERPROFILE")
            .map_err(|e| anyhow!("WinRM connect failed: {e:#}"))?;
        let home = out.trim().replace('\\', "/");
        if !home.is_empty() {
            fs.home = home;
        }
        Ok(fs)
    }

    fn ps(&self, script: &str) -> Result<winrm_rs::CommandOutput> {
        self.rt
            .block_on(self.client.run_powershell(&self.host, script))
            .map_err(|e| anyhow!("{e}"))
    }

    /// Public: run a PowerShell script over WinRM and return stdout (errors on
    /// non-zero exit). Used by the SSH enable/disable helpers.
    pub fn run_powershell(&self, script: &str) -> Result<String> {
        self.ps_ok(script)
    }

    /// Run PowerShell, return stdout, erroring on non-zero exit.
    fn ps_ok(&self, script: &str) -> Result<String> {
        let o = self.ps(script)?;
        if o.exit_code != 0 {
            let err = String::from_utf8_lossy(&o.stderr);
            return Err(anyhow!(
                "{}",
                if err.trim().is_empty() {
                    format!("exit {}", o.exit_code)
                } else {
                    err.trim().to_string()
                }
            ));
        }
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    }
}

impl RemoteFs for WinRmFs {
    fn home(&self) -> String {
        self.home.clone()
    }

    fn realpath(&self, path: &str) -> Result<String> {
        if path == "." || path.is_empty() {
            Ok(self.home.clone())
        } else {
            Ok(path.replace('\\', "/"))
        }
    }

    fn stat(&self, path: &str) -> Result<RStat> {
        let script = format!(
            "$i=Get-Item -LiteralPath {p} -Force; [PSCustomObject]@{{d=[bool]$i.PSIsContainer; s=[int64]($i.Length)}} | ConvertTo-Json -Compress",
            p = psq(path)
        );
        let out = self.ps_ok(&script)?;
        #[derive(Deserialize)]
        struct S {
            d: bool,
            #[serde(default)]
            s: i64,
        }
        let s: S = serde_json::from_str(out.trim()).map_err(|e| anyhow!("stat parse: {e}"))?;
        Ok(RStat {
            is_dir: s.d,
            size: s.s.max(0) as u64,
        })
    }

    fn list(&self, path: &str) -> Result<Vec<FileEntry>> {
        let script = format!(
            "@(Get-ChildItem -Force -LiteralPath {p} | ForEach-Object {{ [PSCustomObject]@{{ n=$_.Name; d=[bool]$_.PSIsContainer; s=[int64]($_.Length); m=[int64]([datetimeoffset]$_.LastWriteTimeUtc).ToUnixTimeSeconds() }} }}) | ConvertTo-Json -Compress -Depth 2",
            p = psq(path)
        );
        let out = self.ps_ok(&script)?;
        let trimmed = out.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        // ConvertTo-Json yields an object for a single entry, an array for many.
        let entries: Vec<WinEntry> = match serde_json::from_str::<Vec<WinEntry>>(trimmed) {
            Ok(v) => v,
            Err(_) => match serde_json::from_str::<WinEntry>(trimmed) {
                Ok(one) => vec![one],
                Err(e) => return Err(anyhow!("list parse: {e}")),
            },
        };
        Ok(entries
            .into_iter()
            .map(|e| FileEntry {
                name: e.n,
                is_dir: e.d,
                size: e.s.max(0) as u64,
                mtime: if e.m > 0 { Some(e.m as u64) } else { None },
                perm: None,
                symlink: false,
            })
            .collect())
    }

    fn read_file(
        &self,
        path: &str,
        out: &mut dyn Write,
        progress: &mut dyn FnMut(u64),
    ) -> Result<()> {
        // NamedTempFile: random name, created O_EXCL with 0600, auto-removed on drop.
        let tmp = tempfile::NamedTempFile::new().map_err(|e| anyhow!("temp: {e}"))?;
        let win = path.replace('/', "\\");
        self.rt
            .block_on(self.client.download_file(&self.host, &win, tmp.path()))
            .map_err(|e| anyhow!("download {path}: {e}"))?;
        let mut f = std::fs::File::open(tmp.path())?;
        copy_with_progress(&mut f, out, progress)
    }

    fn write_file(
        &self,
        path: &str,
        input: &mut dyn Read,
        progress: &mut dyn FnMut(u64),
    ) -> Result<()> {
        // Spill to a private temp, upload it with winrm's built-in chunked
        // upload to a `.ferropipe-part` sibling, then move that over the target
        // (temp + rename, as the trait promises) so a failed upload never
        // leaves a half-written file under the real name. The sibling is
        // removed again on failure, best effort.
        let mut tmp = tempfile::NamedTempFile::new().map_err(|e| anyhow!("temp: {e}"))?;
        std::io::copy(input, tmp.as_file_mut())?;
        tmp.as_file_mut().flush()?;
        let size = tmp.as_file().metadata().map(|m| m.len()).unwrap_or(0);
        let part = format!("{path}.ferropipe-part");
        let win = part.replace('/', "\\");
        let written: Result<()> = self
            .rt
            .block_on(self.client.upload_file(&self.host, tmp.path(), &win))
            .map_err(|e| anyhow!("upload {path}: {e}"))
            .and_then(|_| {
                self.rename(&part, path)
                    .with_context(|| format!("finalize {path}"))
            });
        if written.is_err() {
            let _ = self.remove_file(&part);
        }
        written?;
        progress(size); // winrm upload is opaque; report the whole file at completion
        Ok(())
    }

    fn mkdir(&self, path: &str) -> Result<()> {
        self.ps_ok(&format!(
            "New-Item -ItemType Directory -Path {p} | Out-Null",
            p = psq(path)
        ))?;
        Ok(())
    }

    fn create_empty(&self, path: &str) -> Result<()> {
        self.ps_ok(&format!(
            "New-Item -ItemType File -Path {p} | Out-Null",
            p = psq(path)
        ))?;
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.ps_ok(&format!(
            "Move-Item -LiteralPath {f} -Destination {t} -Force",
            f = psq(from),
            t = psq(to)
        ))?;
        Ok(())
    }

    fn remove_file(&self, path: &str) -> Result<()> {
        self.ps_ok(&format!(
            "Remove-Item -LiteralPath {p} -Force",
            p = psq(path)
        ))?;
        Ok(())
    }

    fn remove_dir(&self, path: &str) -> Result<()> {
        self.ps_ok(&format!(
            "Remove-Item -LiteralPath {p} -Force",
            p = psq(path)
        ))?;
        Ok(())
    }

    fn exec(&self, command: &str, emit: &dyn Fn(Event)) -> Result<i32> {
        let out = self
            .rt
            .block_on(self.client.run_command(&self.host, command, &[]))
            .map_err(|e| anyhow!("{e}"))?;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            emit(Event::ExecOutput {
                line: line.to_string(),
                is_err: false,
            });
        }
        for line in String::from_utf8_lossy(&out.stderr).lines() {
            emit(Event::ExecOutput {
                line: line.to_string(),
                is_err: true,
            });
        }
        Ok(out.exit_code)
    }
}

/// Live probe against a saved WinRM host, opt-in: set `FERROPIPE_WINRM_PROBE`
/// to the saved connection's name and run
/// `FERROPIPE_WINRM_PROBE=<name> cargo test winrm::live_probe -- --ignored --nocapture`.
/// Reads the real store and vault and never prints the password. On the host it
/// runs `hostname` and writes one scratch file `ferropipe-probe-<pid>.txt` into
/// the login user's profile directory (`$env:USERPROFILE`, or `C:\Windows\Temp`
/// when the profile could not be resolved), which it removes again.
#[cfg(test)]
mod live_probe {
    use super::*;
    use crate::model::ConnectionKind;
    use crate::remote::smb::probe_support::{exec_stdout, saved_connection, scratch_round_trip};

    /// The backend reports paths forward-slashed and keeps `C:/` as home when
    /// `$env:USERPROFILE` came back empty; only then fall back to Windows\Temp.
    fn scratch_dir(home: &str) -> String {
        let trimmed = home.trim_end_matches('/');
        if trimmed.is_empty() || trimmed == "C:" {
            "C:/Windows/Temp".to_string()
        } else {
            trimmed.to_string()
        }
    }

    #[test]
    fn scratch_dir_prefers_the_profile_and_falls_back_to_windows_temp() {
        assert_eq!(scratch_dir("C:/Users/probe"), "C:/Users/probe");
        assert_eq!(scratch_dir("C:/Users/probe/"), "C:/Users/probe");
        assert_eq!(scratch_dir("C:/"), "C:/Windows/Temp");
        assert_eq!(scratch_dir(""), "C:/Windows/Temp");
    }

    #[test]
    #[ignore = "needs a saved WinRM host on the network; see the module docs"]
    fn winrm_round_trip_against_saved_host() {
        let Ok(name) = std::env::var("FERROPIPE_WINRM_PROBE") else {
            println!("FERROPIPE_WINRM_PROBE not set; skipping");
            return;
        };
        let (conn, secret) = saved_connection(&name, ConnectionKind::WinRm);
        println!(
            "[1] connecting to {}:{} as {} (domain {:?}, tls {})",
            conn.host,
            conn.port,
            conn.username,
            conn.domain,
            conn.port == 5986
        );
        let fs = WinRmFs::connect(&conn, Some(&secret)).expect("connect");
        let dir = scratch_dir(&fs.home());
        let entries = fs.list(&dir).expect("list scratch dir");
        println!("[2] {dir}: {} entries", entries.len());
        let (code, lines) = exec_stdout(&fs, "hostname");
        assert_eq!(code, 0, "hostname exited {code}");
        assert!(
            lines.iter().any(|l| !l.trim().is_empty()),
            "hostname printed nothing"
        );
        println!("[3] exec hostname: {lines:?} (exit {code})");
        scratch_round_trip(&fs, &dir, 4);
    }
}
