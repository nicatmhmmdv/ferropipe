//! Remote access management + file transfer for a Windows host, using the same
//! credentials as its RDP connection.
//!
//! The OpenSSH and WinRM services are normally toggled by hand; Ferropipe drives
//! them here by running PowerShell over whichever remote channel is already
//! reachable (WinRM or SSH) — that's the bootstrap: WinRM turns SSH on, SSH turns
//! WinRM on. Files are then transferred over SFTP. Enabling/disabling stays an
//! explicit, user-initiated action so nothing is left listening silently.

use crate::model::Connection;
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ssh2::Session;
use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Windows OpenSSH listens on 22 regardless of the connection's RDP port.
pub const SSH_PORT: u16 = 22;
/// WinRM HTTP port (NTLM-sealed), the usual pre-existing management channel.
pub const WINRM_PORT: u16 = 5985;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
const IO_TIMEOUT: Duration = Duration::from_secs(60);

// Each script ends with an "OK: …" or "FAIL: …" line so we can report the real
// outcome regardless of exit-code/stderr quirks across the WinRM and SSH
// channels — the earlier version printed "enabled" unconditionally and lied when
// the FoD install silently failed.

/// PowerShell (Administrator): install (if needed) + start the OpenSSH server,
/// open the firewall, and verify it's actually Running. Reports FAIL with the
/// reason (e.g. no Windows Update/FoD source) rather than claiming success.
pub const ENABLE_SSH_PS: &str = "$ErrorActionPreference='SilentlyContinue'; if(-not(Get-Service sshd)){ Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0 | Out-Null }; if(-not(Get-Service sshd)){ Write-Output 'FAIL: OpenSSH is not installed and could not be installed via Windows Update/Features-on-Demand (this host has no WU/FoD source). Use SMB or WinRM to transfer files instead.'; exit 0 }; Set-Service -Name sshd -StartupType Automatic; Start-Service sshd; New-NetFirewallRule -Name ferropipe-sshd -DisplayName 'Ferropipe OpenSSH' -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 -ErrorAction SilentlyContinue | Out-Null; Start-Sleep -Milliseconds 700; $s=(Get-Service sshd).Status; if($s -eq 'Running'){ Write-Output 'OK: sshd is Running on port 22' } else { Write-Output \"FAIL: sshd status is $s\" }";

/// PowerShell (Administrator): stop the OpenSSH server, block startup, close the
/// firewall, and verify it stopped.
pub const DISABLE_SSH_PS: &str = "$ErrorActionPreference='SilentlyContinue'; Stop-Service sshd; Set-Service -Name sshd -StartupType Disabled; Remove-NetFirewallRule -Name ferropipe-sshd; $s=(Get-Service sshd); if($s -and $s.Status -eq 'Running'){ Write-Output 'FAIL: sshd is still Running' } else { Write-Output 'OK: sshd stopped' }";

/// PowerShell (Administrator): enable WinRM/PSRemoting (Public profiles too) and
/// verify the service is Running.
pub const ENABLE_WINRM_PS: &str = "$ErrorActionPreference='SilentlyContinue'; Enable-PSRemoting -Force -SkipNetworkProfileCheck | Out-Null; if((Get-Service WinRM).Status -eq 'Running'){ Write-Output 'OK: WinRM is Running' } else { Write-Output 'FAIL: WinRM did not start' }";

/// PowerShell (Administrator): disable WinRM. Run over SSH when possible — doing
/// it over WinRM severs the channel mid-command (the OK line may not return).
pub const DISABLE_WINRM_PS: &str = "$ErrorActionPreference='SilentlyContinue'; Disable-PSRemoting -Force; Get-NetFirewallRule -DisplayGroup 'Windows Remote Management' | Disable-NetFirewallRule; Set-Service -Name WinRM -StartupType Disabled; Write-Output 'OK: WinRM disabled'; Stop-Service WinRM -Force";

/// Enable/disable the OpenSSH server on the host. Prefers WinRM (SSH may be off,
/// and disabling SSH over SSH would sever the channel).
pub fn set_ssh_enabled(conn: &Connection, password: &str, enable: bool) -> Result<String> {
    let script = if enable { ENABLE_SSH_PS } else { DISABLE_SSH_PS };
    interpret(run_ps_any(conn, password, script, false)?)
}

/// Enable/disable WinRM on the host. Prefers SSH (WinRM is off when enabling, and
/// disabling WinRM over WinRM severs the channel).
pub fn set_winrm_enabled(conn: &Connection, password: &str, enable: bool) -> Result<String> {
    let script = if enable { ENABLE_WINRM_PS } else { DISABLE_WINRM_PS };
    interpret(run_ps_any(conn, password, script, true)?)
}

/// Turn a script's trailing "OK: …" / "FAIL: …" line into a Result: OK → the
/// message, FAIL → an error carrying the reason.
fn interpret(out: String) -> Result<String> {
    let marker = out
        .lines()
        .rev()
        .find(|l| l.contains("OK:") || l.contains("FAIL:"));
    match marker {
        Some(l) if l.contains("FAIL:") => {
            Err(anyhow!("{}", l.splitn(2, "FAIL:").nth(1).unwrap_or(l).trim()))
        }
        Some(l) => Ok(l.splitn(2, "OK:").nth(1).unwrap_or(l).trim().to_string()),
        None => Ok(out.trim().to_string()),
    }
}

/// Run a PowerShell script over whichever remote channel is reachable. Tries the
/// preferred channel first, then the other; reports both errors if neither works.
fn run_ps_any(conn: &Connection, password: &str, script: &str, prefer_ssh: bool) -> Result<String> {
    let via_ssh = || -> Result<String> {
        let session = ssh_session(&conn.host, &conn.username, password)?;
        ssh_exec_ps(&session, script)
    };
    let via_winrm = || winrm_run_ps(conn, password, script);

    if prefer_ssh {
        match via_ssh() {
            Ok(o) => Ok(o),
            Err(ssh_err) => via_winrm().map_err(|winrm_err| {
                anyhow!("no reachable channel — SSH: {ssh_err:#}; WinRM: {winrm_err:#}")
            }),
        }
    } else {
        match via_winrm() {
            Ok(o) => Ok(o),
            Err(winrm_err) => via_ssh().map_err(|ssh_err| {
                anyhow!("no reachable channel — WinRM: {winrm_err:#}; SSH: {ssh_err:#}")
            }),
        }
    }
}

/// Run a PowerShell script over WinRM (port 5985) with the connection's creds.
fn winrm_run_ps(conn: &Connection, password: &str, script: &str) -> Result<String> {
    let mut winrm_conn = conn.clone();
    winrm_conn.port = WINRM_PORT;
    let fs = crate::remote::winrm::WinRmFs::connect(&winrm_conn, Some(password))
        .map_err(|e| anyhow!("WinRM connect: {e:#}"))?;
    fs.run_powershell(script)
}

/// Run a PowerShell script over SSH via `-EncodedCommand` (base64 of UTF-16LE,
/// which sidesteps all shell-quoting concerns).
fn ssh_exec_ps(session: &Session, script: &str) -> Result<String> {
    let mut ch = session.channel_session()?;
    let utf16: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let encoded = B64.encode(utf16);
    ch.exec(&format!("powershell -NonInteractive -NoProfile -EncodedCommand {encoded}"))?;
    let mut out = String::new();
    ch.read_to_string(&mut out).ok();
    let mut err = String::new();
    ch.stderr().read_to_string(&mut err).ok();
    ch.wait_close().ok();
    let code = ch.exit_status().unwrap_or(-1);
    if code != 0 {
        let err = err.trim();
        return Err(anyhow!(
            "powershell exit {code}{}",
            if err.is_empty() { String::new() } else { format!(": {err}") }
        ));
    }
    Ok(out)
}

/// Outcome of a transfer: how many files landed, and any per-file failures.
pub struct TransferResult {
    pub ok: usize,
    pub failures: Vec<(String, String)>,
}

/// Normalize a Windows-ish remote directory into an SFTP path (forward slashes,
/// no trailing slash). Empty → "." (the SSH login directory, i.e. %USERPROFILE%).
fn normalize_dir(dir: &str) -> String {
    let d = dir.trim().replace('\\', "/");
    let d = d.trim_end_matches('/');
    if d.is_empty() {
        ".".to_string()
    } else {
        d.to_string()
    }
}

/// Open an authenticated SSH session (port 22, password auth, TOFU host key).
fn ssh_session(host: &str, username: &str, password: &str) -> Result<Session> {
    let addr = format!("{host}:{SSH_PORT}");
    let sockaddr = addr
        .to_socket_addrs()
        .with_context(|| format!("resolve {addr}"))?
        .next()
        .ok_or_else(|| anyhow!("could not resolve {addr}"))?;
    let tcp = TcpStream::connect_timeout(&sockaddr, CONNECT_TIMEOUT)
        .with_context(|| format!("connect {addr} — is SSH enabled on the host?"))?;
    tcp.set_read_timeout(Some(IO_TIMEOUT))?;
    tcp.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut session = Session::new()?;
    session.set_tcp_stream(tcp);
    session.set_blocking(true);
    session.handshake().context("ssh handshake")?;
    if password.is_empty() {
        return Err(anyhow!("no password stored for this connection (edit it and set one)"));
    }
    session
        .userauth_password(username, password)
        .context("password auth failed")?;
    if !session.authenticated() {
        return Err(anyhow!("authentication failed"));
    }
    Ok(session)
}

/// Upload `files` to `remote_dir`, preferring SFTP (fast) and falling back to
/// WinRM when SSH isn't available (e.g. OpenSSH can't be installed on the host).
/// Returns the channel used alongside the per-file outcome.
pub fn transfer(
    conn: &Connection,
    password: &str,
    remote_dir: &str,
    files: &[PathBuf],
) -> Result<(&'static str, TransferResult)> {
    match transfer_sftp(&conn.host, &conn.username, password, remote_dir, files) {
        Ok(r) => Ok(("SFTP", r)),
        Err(ssh_err) => match transfer_winrm(conn, password, remote_dir, files) {
            Ok(r) => Ok(("WinRM", r)),
            Err(winrm_err) => Err(anyhow!(
                "no transfer channel worked — SFTP: {ssh_err:#}; WinRM: {winrm_err:#}"
            )),
        },
    }
}

/// Upload over SFTP (port 22, password auth).
fn transfer_sftp(
    host: &str,
    username: &str,
    password: &str,
    remote_dir: &str,
    files: &[PathBuf],
) -> Result<TransferResult> {
    let session = ssh_session(host, username, password)?;
    let sftp = session.sftp().context("open SFTP (OpenSSH sftp subsystem)")?;

    let dir = normalize_dir(remote_dir);
    let mut ok = 0usize;
    let mut failures = Vec::new();
    for local in files {
        match upload_one(&sftp, local, &dir) {
            Ok(()) => ok += 1,
            Err(e) => failures.push((file_label(local), format!("{e:#}"))),
        }
    }
    Ok(TransferResult { ok, failures })
}

/// Upload over WinRM (chunked, via `winrm-rs`). Relative `remote_dir` is resolved
/// under the login profile (%USERPROFILE%).
fn transfer_winrm(
    conn: &Connection,
    password: &str,
    remote_dir: &str,
    files: &[PathBuf],
) -> Result<TransferResult> {
    use crate::remote::RemoteFs;
    let mut winrm_conn = conn.clone();
    winrm_conn.port = WINRM_PORT;
    let fs = crate::remote::winrm::WinRmFs::connect(&winrm_conn, Some(password))
        .map_err(|e| anyhow!("WinRM connect: {e:#}"))?;

    let dir = normalize_dir(remote_dir);
    let base = if dir == "." {
        fs.home()
    } else if dir.contains(':') {
        dir // absolute Windows path
    } else {
        format!("{}/{}", fs.home().trim_end_matches('/'), dir)
    };

    let mut ok = 0usize;
    let mut failures = Vec::new();
    for local in files {
        let remote = format!("{}/{}", base.trim_end_matches('/'), file_label(local));
        let result = std::fs::File::open(local)
            .map_err(|e| anyhow!("open {}: {e}", local.display()))
            .and_then(|mut f| fs.write_file(&remote, &mut f, &mut |_| {}));
        match result {
            Ok(()) => ok += 1,
            Err(e) => failures.push((file_label(local), format!("{e:#}"))),
        }
    }
    Ok(TransferResult { ok, failures })
}

fn file_label(local: &Path) -> String {
    local
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| local.display().to_string())
}

fn upload_one(sftp: &ssh2::Sftp, local: &Path, remote_dir: &str) -> Result<()> {
    let name = local
        .file_name()
        .ok_or_else(|| anyhow!("invalid file name"))?
        .to_string_lossy()
        .into_owned();
    let remote_path = if remote_dir == "." {
        name
    } else {
        format!("{remote_dir}/{name}")
    };
    let mut src = std::fs::File::open(local).with_context(|| format!("open {}", local.display()))?;
    let mut dst = sftp
        .create(Path::new(&remote_path))
        .with_context(|| format!("create remote {remote_path} (does the folder exist?)"))?;
    std::io::copy(&mut src, &mut dst).with_context(|| format!("upload {}", local.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_dir_handles_windows_paths() {
        assert_eq!(normalize_dir("  C:\\temp\\  "), "C:/temp");
        assert_eq!(normalize_dir("Desktop/"), "Desktop");
        assert_eq!(normalize_dir(""), ".");
        assert_eq!(normalize_dir("   "), ".");
    }
}
