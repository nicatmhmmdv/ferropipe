//! Import connections from an OpenSSH `~/.ssh/config`.

use crate::model::{AuthMethod, Connection, ConnectionKind};
use std::path::PathBuf;

fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".ssh").join("config")
}

/// Parse `~/.ssh/config` into connections. Wildcard `Host *` blocks are skipped.
pub fn import() -> Vec<Connection> {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    parse(&text)
}

struct Draft {
    alias: String,
    hostname: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identity: Option<String>,
}

fn parse(text: &str) -> Vec<Connection> {
    let mut out = Vec::new();
    let mut cur: Option<Draft> = None;

    let flush = |cur: &mut Option<Draft>, out: &mut Vec<Connection>| {
        if let Some(d) = cur.take() {
            if d.alias.contains('*') || d.alias.contains('?') {
                return; // pattern block, not a concrete host
            }
            let host = d.hostname.clone().unwrap_or_else(|| d.alias.clone());
            let mut c = Connection::new(d.alias.clone(), host, d.user.clone().unwrap_or_default());
            c.kind = ConnectionKind::Ssh;
            c.port = d.port.unwrap_or(22);
            c.group = "ssh-config".into();
            c.auth = if let Some(key) = d.identity {
                AuthMethod::Key { private_key: expand_tilde(&key), passphrase_enc: None }
            } else {
                AuthMethod::Agent
            };
            out.push(c);
        }
    };

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once(|c: char| c == ' ' || c == '\t' || c == '=') {
            Some((k, v)) => (k.trim().to_lowercase(), v.trim().trim_start_matches('=').trim().to_string()),
            None => continue,
        };
        match key.as_str() {
            "host" => {
                flush(&mut cur, &mut out);
                // Only take the first pattern token of a Host line.
                let alias = value.split_whitespace().next().unwrap_or("").to_string();
                cur = Some(Draft { alias, hostname: None, user: None, port: None, identity: None });
            }
            "hostname" => {
                if let Some(d) = cur.as_mut() {
                    d.hostname = Some(value);
                }
            }
            "user" => {
                if let Some(d) = cur.as_mut() {
                    d.user = Some(value);
                }
            }
            "port" => {
                if let Some(d) = cur.as_mut() {
                    d.port = value.parse().ok();
                }
            }
            "identityfile" => {
                if let Some(d) = cur.as_mut() {
                    d.identity = Some(value);
                }
            }
            _ => {}
        }
    }
    flush(&mut cur, &mut out);
    out
}

fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    p.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hosts_and_skips_wildcards() {
        let cfg = "\
Host *
  ForwardAgent yes

Host web1
  HostName 10.0.0.5
  User deploy
  Port 2222
  IdentityFile ~/.ssh/id_ed25519

Host db
  HostName db.internal
";
        let conns = parse(cfg);
        assert_eq!(conns.len(), 2);
        assert_eq!(conns[0].name, "web1");
        assert_eq!(conns[0].host, "10.0.0.5");
        assert_eq!(conns[0].username, "deploy");
        assert_eq!(conns[0].port, 2222);
        assert!(matches!(conns[0].auth, AuthMethod::Key { .. }));
        assert_eq!(conns[1].name, "db");
        assert!(matches!(conns[1].auth, AuthMethod::Agent));
    }
}
