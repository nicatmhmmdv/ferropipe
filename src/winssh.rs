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

/// PowerShell (Administrator) to install + start the OpenSSH server and open the
/// firewall. Idempotent.
pub const ENABLE_SSH_PS: &str = "Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0 | Out-Null; Start-Service sshd; Set-Service -Name sshd -StartupType Manual; New-NetFirewallRule -Name ferropipe-sshd -DisplayName 'Ferropipe OpenSSH' -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 -ErrorAction SilentlyContinue | Out-Null; 'ssh enabled'";

/// PowerShell (Administrator) to stop the OpenSSH server and close the firewall.
/// Leaves the capability installed so re-enabling is instant.
pub const DISABLE_SSH_PS: &str = "Stop-Service sshd -ErrorAction SilentlyContinue; Set-Service -Name sshd -StartupType Disabled -ErrorAction SilentlyContinue; Remove-NetFirewallRule -Name ferropipe-sshd -ErrorAction SilentlyContinue; 'ssh disabled'";

/// PowerShell (Administrator) to enable WinRM/PSRemoting (works on Public network
/// profiles too, for workgroup hosts).
pub const ENABLE_WINRM_PS: &str = "Enable-PSRemoting -Force -SkipNetworkProfileCheck | Out-Null; 'winrm enabled'";

/// PowerShell (Administrator) to disable WinRM: stop the service, block startup,
/// and disable its firewall rules. Run this over SSH when possible — doing it
/// over WinRM itself severs the channel mid-command.
pub const DISABLE_WINRM_PS: &str = "Disable-PSRemoting -Force -ErrorAction SilentlyContinue; Get-NetFirewallRule -DisplayGroup 'Windows Remote Management' -ErrorAction SilentlyContinue | Disable-NetFirewallRule -ErrorAction SilentlyContinue; Set-Service -Name WinRM -StartupType Disabled -ErrorAction SilentlyContinue; Stop-Service WinRM -Force -ErrorAction SilentlyContinue; 'winrm disabled'";

/// Enable/disable the OpenSSH server on the host. Prefers WinRM (SSH may be off,
/// and disabling SSH over SSH would sever the channel).
pub fn set_ssh_enabled(conn: &Connection, password: &str, enable: bool) -> Result<String> {
    let script = if enable { ENABLE_SSH_PS } else { DISABLE_SSH_PS };
    run_ps_any(conn, password, script, false)
}

/// Enable/disable WinRM on the host. Prefers SSH (WinRM is off when enabling, and
/// disabling WinRM over WinRM severs the channel).
pub fn set_winrm_enabled(conn: &Connection, password: &str, enable: bool) -> Result<String> {
    let script = if enable { ENABLE_WINRM_PS } else { DISABLE_WINRM_PS };
    run_ps_any(conn, password, script, true)
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

/// Upload `files` to `remote_dir` on `host` (port 22) using password auth.
pub fn transfer(
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
            Err(e) => failures.push((
                local
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| local.display().to_string()),
                format!("{e:#}"),
            )),
        }
    }
    Ok(TransferResult { ok, failures })
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
