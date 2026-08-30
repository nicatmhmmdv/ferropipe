//! The remote worker thread: builds the right backend for a connection and runs the
//! generic command loop against it.

use super::sftp::{self, SftpFs};
use super::{serve, smb, winrm, Command, Event};
use crate::model::ConnectionKind;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;

/// Handle held by the UI to talk to the worker.
pub struct WorkerHandle {
    pub tx: Sender<Command>,
}

impl WorkerHandle {
    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }
}

pub fn spawn(
    rx: Receiver<Command>,
    tx: Sender<Event>,
    repaint: impl Fn() + Send + 'static,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("ferropipe-remote".into())
        .spawn(move || worker_main(rx, tx, repaint))
        .expect("spawn remote worker")
}

fn worker_main(rx: Receiver<Command>, tx: Sender<Event>, repaint: impl Fn()) {
    let emit = move |ev: Event| {
        let _ = tx.send(ev);
        repaint();
    };
    loop {
        match rx.recv() {
            Ok(Command::Connect { conn, secret }) => {
                let shutdown = match conn.kind {
                    ConnectionKind::Ssh => run_ssh(&rx, &emit, &conn, secret.as_deref()),
                    ConnectionKind::Smb => run_smb(&rx, &emit, &conn, secret.as_deref()),
                    ConnectionKind::WinRm => run_winrm(&rx, &emit, &conn, secret.as_deref()),
                    ConnectionKind::Rdp | ConnectionKind::RdpNative => {
                        emit(Event::Error {
                            context: "connect".into(),
                            message: "RDP connections are handled in-app, not by the worker".into(),
                        });
                        false
                    }
                };
                if shutdown {
                    break;
                }
            }
            Ok(Command::Shutdown) | Err(_) => break,
            Ok(_) => { /* ignore ops while disconnected */ }
        }
    }
}

fn run_ssh(rx: &Receiver<Command>, emit: &dyn Fn(Event), conn: &crate::model::Connection, secret: Option<&str>) -> bool {
    let session = match sftp::connect(conn, secret) {
        Ok(s) => s,
        Err(e) => {
            emit(Event::Error { context: "connect".into(), message: format!("{e:#}") });
            return false;
        }
    };
    let sftp = match session.sftp() {
        Ok(s) => s,
        Err(e) => {
            emit(Event::Error { context: "open sftp".into(), message: e.to_string() });
            return false;
        }
    };
    let home = sftp::resolve_home(&sftp);
    emit(Event::Connected { home: home.clone(), kind: "SFTP" });
    let fs = SftpFs { session: &session, sftp: &sftp, home };
    let shutdown = serve(rx, emit, &fs);
    emit(Event::Disconnected);
    shutdown
}

fn run_smb(rx: &Receiver<Command>, emit: &dyn Fn(Event), conn: &crate::model::Connection, secret: Option<&str>) -> bool {
    match smb::SmbFs::connect(conn, secret) {
        Ok(fs) => {
            emit(Event::Connected { home: fs.home(), kind: "SMB" });
            let shutdown = serve(rx, emit, &fs);
            emit(Event::Disconnected);
            shutdown
        }
        Err(e) => {
            emit(Event::Error { context: "smb connect".into(), message: format!("{e:#}") });
            false
        }
    }
}

fn run_winrm(rx: &Receiver<Command>, emit: &dyn Fn(Event), conn: &crate::model::Connection, secret: Option<&str>) -> bool {
    match winrm::WinRmFs::connect(conn, secret) {
        Ok(fs) => {
            emit(Event::Connected { home: fs.home(), kind: "WinRM" });
            let shutdown = serve(rx, emit, &fs);
            emit(Event::Disconnected);
            shutdown
        }
        Err(e) => {
            emit(Event::Error { context: "winrm connect".into(), message: format!("{e:#}") });
            false
        }
    }
}

use super::RemoteFs; // for fs.home()
