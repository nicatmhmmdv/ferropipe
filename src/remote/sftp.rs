//! SFTP backend (ssh2 / libssh2), plus SSH connect + host-key verification + exec.

use super::{Event, RStat, RemoteFs, copy_with_progress};
use crate::model::{AuthMethod, Connection};
use anyhow::{Context, Result, anyhow};
use ssh2::{
    CheckResult, KnownHostFileKind, KnownHostKeyFormat, OpenFlags, OpenType, Session, Sftp,
};
use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// SFTP filesystem view. Borrows the session (for exec) and its sftp channel.
pub struct SftpFs<'a> {
    pub session: &'a Session,
    pub sftp: &'a Sftp,
    pub home: String,
}

impl RemoteFs for SftpFs<'_> {
    fn home(&self) -> String {
        self.home.clone()
    }

    fn realpath(&self, path: &str) -> Result<String> {
        Ok(self
            .sftp
            .realpath(Path::new(path))?
            .to_string_lossy()
            .into_owned())
    }

    fn stat(&self, path: &str) -> Result<RStat> {
        let s = self.sftp.stat(Path::new(path))?;
        Ok(RStat {
            is_dir: s.is_dir(),
            size: s.size.unwrap_or(0),
        })
    }

    fn list(&self, path: &str) -> Result<Vec<crate::model::FileEntry>> {
        let mut out = Vec::new();
        for (p, stat) in self
            .sftp
            .readdir(Path::new(path))
            .with_context(|| format!("readdir {path}"))?
        {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string_lossy().into_owned());
            if name == "." || name == ".." {
                continue;
            }
            out.push(crate::model::FileEntry {
                name,
                is_dir: stat.is_dir(),
                size: stat.size.unwrap_or(0),
                mtime: stat.mtime,
                perm: stat.perm,
                symlink: stat.file_type() == ssh2::FileType::Symlink,
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
            .sftp
            .open(Path::new(path))
            .with_context(|| format!("open {path}"))?;
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
                    .sftp
                    .open_mode(
                        Path::new(&tmp),
                        OpenFlags::WRITE | OpenFlags::TRUNCATE | OpenFlags::CREATE,
                        0o644,
                        OpenType::File,
                    )
                    .with_context(|| format!("create remote {tmp}"))?;
                copy_with_progress(input, &mut f, progress)?;
            }
            let _ = self.sftp.unlink(Path::new(path));
            self.sftp
                .rename(Path::new(&tmp), Path::new(path), None)
                .with_context(|| format!("finalize {path}"))
        })();
        if written.is_err() {
            let _ = self.sftp.unlink(Path::new(&tmp));
        }
        written
    }

    fn mkdir(&self, path: &str) -> Result<()> {
        self.sftp.mkdir(Path::new(path), 0o755)?;
        Ok(())
    }

    fn create_empty(&self, path: &str) -> Result<()> {
        let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUSIVE;
        self.sftp
            .open_mode(Path::new(path), flags, 0o644, OpenType::File)?;
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.sftp.rename(Path::new(from), Path::new(to), None)?;
        Ok(())
    }

    fn remove_file(&self, path: &str) -> Result<()> {
        self.sftp.unlink(Path::new(path))?;
        Ok(())
    }

    fn remove_dir(&self, path: &str) -> Result<()> {
        self.sftp.rmdir(Path::new(path))?;
        Ok(())
    }

    fn exec(&self, command: &str, emit: &dyn Fn(Event)) -> Result<i32> {
        let mut ch = self.session.channel_session()?;
        ch.exec(command)?;
        let mut out = String::new();
        ch.read_to_string(&mut out).ok();
        for line in out.lines() {
            emit(Event::ExecOutput {
                line: line.to_string(),
                is_err: false,
            });
        }
        let mut err = String::new();
        ch.stderr().read_to_string(&mut err).ok();
        for line in err.lines() {
            emit(Event::ExecOutput {
                line: line.to_string(),
                is_err: true,
            });
        }
        ch.wait_close().ok();
        Ok(ch.exit_status().unwrap_or(-1))
    }
}

pub fn connect(conn: &Connection, secret: Option<&str>) -> Result<Session> {
    let addr = format!("{}:{}", conn.host, conn.port);
    let sockaddr = addr
        .to_socket_addrs()
        .with_context(|| format!("resolve {addr}"))?
        .next()
        .ok_or_else(|| anyhow!("could not resolve {addr}"))?;
    let tcp = TcpStream::connect_timeout(&sockaddr, CONNECT_TIMEOUT).with_context(|| {
        format!(
            "tcp connect {addr} (timeout {}s)",
            CONNECT_TIMEOUT.as_secs()
        )
    })?;
    tcp.set_read_timeout(Some(IO_TIMEOUT))?;
    tcp.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut session = Session::new()?;
    session.set_tcp_stream(tcp);
    session.set_blocking(true);
    session.handshake().context("ssh handshake")?;

    verify_host_key(&session, &conn.host, conn.port).context("host key verification")?;

    match &conn.auth {
        AuthMethod::Password { .. } => {
            let pw = secret
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("no password stored (edit the connection and set one)"))?;
            session
                .userauth_password(&conn.username, pw)
                .context("password auth failed")?;
        }
        AuthMethod::Key { private_key, .. } => {
            let key = Path::new(private_key);
            let pass = secret.filter(|s| !s.is_empty());
            session
                .userauth_pubkey_file(&conn.username, None, key, pass)
                .context("public-key auth failed")?;
        }
        AuthMethod::Agent => {
            let mut agent = session.agent()?;
            agent.connect()?;
            agent.list_identities()?;
            let mut ok = false;
            for id in agent.identities()? {
                if agent.userauth(&conn.username, &id).is_ok() {
                    ok = true;
                    break;
                }
            }
            if !ok {
                return Err(anyhow!("ssh-agent had no accepted identity"));
            }
        }
    }
    if !session.authenticated() {
        return Err(anyhow!("authentication failed"));
    }
    Ok(session)
}

fn known_hosts_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    PathBuf::from(home).join(".ssh").join("known_hosts")
}

fn verify_host_key(session: &Session, host: &str, port: u16) -> Result<()> {
    let (key_vec, key_type) = {
        let (k, t) = session
            .host_key()
            .ok_or_else(|| anyhow!("server presented no host key"))?;
        (k.to_vec(), t)
    };
    let mut kh = session.known_hosts()?;
    let path = known_hosts_path();
    let _ = kh.read_file(&path, KnownHostFileKind::OpenSSH);
    match kh.check_port(host, port, &key_vec) {
        CheckResult::Match => Ok(()),
        CheckResult::Mismatch => Err(anyhow!(
            "host key for {host}:{port} does NOT match ~/.ssh/known_hosts — refusing (possible MITM)."
        )),
        CheckResult::NotFound => {
            let fmt = KnownHostKeyFormat::from(key_type);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = kh.add(host, &key_vec, "added by ferropipe (TOFU)", fmt);
            let _ = kh.write_file(&path, KnownHostFileKind::OpenSSH);
            Ok(())
        }
        CheckResult::Failure => Err(anyhow!("host key check failed")),
    }
}

pub fn resolve_home(sftp: &Sftp) -> String {
    sftp.realpath(Path::new("."))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".to_string())
}

/// Live probe against a saved SSH host, opt-in: set `FERROPIPE_SFTP_PROBE` to
/// the saved connection's name and run
/// `FERROPIPE_SFTP_PROBE=<name> cargo test sftp::live_probe -- --ignored --nocapture`.
/// Reads the real store and vault (password, key passphrase or ssh-agent, as
/// saved) and never prints the secret. Like the app, `connect` records an
/// unknown host key in `~/.ssh/known_hosts`. On the host it runs
/// `echo ferropipe-probe` and writes one scratch file `ferropipe-probe-<pid>.txt`
/// into the login user's home directory, which it removes again.
#[cfg(test)]
mod live_probe {
    use super::*;
    use crate::model::ConnectionKind;
    use crate::remote::smb::probe_support::{
        auth_label, exec_stdout, saved_connection, scratch_round_trip,
    };

    /// What `connect` gets as the secret: the password, the key passphrase if
    /// there is one, and nothing for agent auth (the agent holds the keys).
    fn secret_arg<'a>(auth: &AuthMethod, secret: &'a str) -> Option<&'a str> {
        match auth {
            AuthMethod::Agent => None,
            AuthMethod::Key { .. } if secret.is_empty() => None,
            AuthMethod::Password { .. } | AuthMethod::Key { .. } => Some(secret),
        }
    }

    #[test]
    fn secret_arg_passes_passwords_and_passphrases_but_nothing_for_agent() {
        let password = AuthMethod::Password {
            password_enc: "blob".into(),
        };
        let key = AuthMethod::Key {
            private_key: "/home/u/.ssh/id_ed25519".into(),
            passphrase_enc: None,
        };
        assert_eq!(secret_arg(&password, "pw"), Some("pw"));
        assert_eq!(secret_arg(&key, "phrase"), Some("phrase"));
        assert_eq!(secret_arg(&key, ""), None);
        assert_eq!(secret_arg(&AuthMethod::Agent, "ignored"), None);
    }

    #[test]
    #[ignore = "needs a saved SSH host on the network; see the module docs"]
    fn sftp_round_trip_against_saved_host() {
        let Ok(name) = std::env::var("FERROPIPE_SFTP_PROBE") else {
            println!("FERROPIPE_SFTP_PROBE not set; skipping");
            return;
        };
        let (conn, secret) = saved_connection(&name, ConnectionKind::Ssh);
        println!(
            "[1] connecting to {}@{}:{} ({} auth)",
            conn.username,
            conn.host,
            conn.port,
            auth_label(&conn.auth)
        );
        let session = connect(&conn, secret_arg(&conn.auth, &secret)).expect("connect");
        let sftp = session.sftp().expect("open sftp");
        let home = resolve_home(&sftp);
        let fs = SftpFs {
            session: &session,
            sftp: &sftp,
            home: home.clone(),
        };
        let entries = fs.list(&home).expect("list home");
        println!("[2] home {home}: {} entries", entries.len());
        let (code, lines) = exec_stdout(&fs, "echo ferropipe-probe");
        assert_eq!(code, 0, "echo exited {code}");
        assert_eq!(lines, ["ferropipe-probe"]);
        println!("[3] exec echo: {lines:?} (exit {code})");
        scratch_round_trip(&fs, &home, 4);
    }
}
