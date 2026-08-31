//! Launch external programs: a terminal running `ssh`, or an editor on a file.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|p| p.join(bin)).find(|p| p.is_file())
}

/// Common terminal emulators, tried in order after $TERMINAL.
/// Alacritty is the preferred default.
const TERMINALS: &[&str] = &[
    "alacritty", "kitty", "wezterm", "foot", "konsole", "gnome-terminal", "xfce4-terminal", "xterm",
];

/// Build (program, args) to run `cmd` inside a terminal emulator.
fn terminal_invocation(cmd: &[String]) -> Option<(String, Vec<String>)> {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(t) = std::env::var("TERMINAL") {
        if !t.is_empty() {
            candidates.push(t);
        }
    }
    candidates.extend(TERMINALS.iter().map(|s| s.to_string()));

    for t in candidates {
        let base = Path::new(&t).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or(t.clone());
        if which(&base).is_none() && !Path::new(&t).is_file() {
            continue;
        }
        let args = match base.as_str() {
            "wezterm" => {
                let mut a = vec!["start".to_string(), "--".to_string()];
                a.extend(cmd.iter().cloned());
                a
            }
            "gnome-terminal" | "xfce4-terminal" => {
                let mut a = vec!["--".to_string()];
                a.extend(cmd.iter().cloned());
                a
            }
            _ => {
                // kitty, alacritty, konsole, foot, xterm all accept `-e cmd args`.
                let mut a = vec!["-e".to_string()];
                a.extend(cmd.iter().cloned());
                a
            }
        };
        return Some((t, args));
    }
    None
}

/// Open Alacritty (preferred) running `ssh [-i key] [-p port] user@host`, giving
/// each connection its own terminal session. Falls back to the generic terminal
/// chooser if Alacritty isn't installed.
pub fn open_alacritty_ssh(user_host: &str, port: u16, identity: Option<&str>) -> Result<()> {
    let mut ssh = vec!["ssh".to_string()];
    if let Some(key) = identity {
        if !key.is_empty() {
            ssh.push("-i".into());
            ssh.push(key.to_string());
        }
    }
    if port != 22 {
        ssh.push("-p".into());
        ssh.push(port.to_string());
    }
    ssh.push(user_host.to_string());

    // Prefer Alacritty explicitly — it's the terminal the workflow is built around.
    if which("alacritty").is_some() {
        let mut args = vec!["-e".to_string()];
        args.extend(ssh.iter().cloned());
        Command::new("alacritty").args(args).spawn()?;
        return Ok(());
    }
    // Otherwise fall back to whatever terminal emulator is available.
    let (prog, args) = terminal_invocation(&ssh)
        .ok_or_else(|| anyhow!("Alacritty not found and no fallback terminal — install alacritty or set $TERMINAL"))?;
    Command::new(prog).args(args).spawn()?;
    Ok(())
}

/// Open a local file in the user's editor. Prefers $VISUAL (GUI), then $EDITOR
/// (run in a terminal), then xdg-open.
pub fn open_editor(path: &Path) -> Result<()> {
    if let Ok(vis) = std::env::var("VISUAL") {
        if !vis.is_empty() {
            Command::new(vis).arg(path).spawn()?;
            return Ok(());
        }
    }
    if let Ok(ed) = std::env::var("EDITOR") {
        if !ed.is_empty() {
            let cmd = vec![ed, path.to_string_lossy().into_owned()];
            if let Some((prog, args)) = terminal_invocation(&cmd) {
                Command::new(prog).args(args).spawn()?;
                return Ok(());
            }
        }
    }
    if which("xdg-open").is_some() {
        Command::new("xdg-open").arg(path).spawn()?;
        return Ok(());
    }
    Err(anyhow!("no editor found — set $EDITOR or $VISUAL"))
}
