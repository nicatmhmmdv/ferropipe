//! Native RDP sessions rendered inside Ferropipe using the `ferropipe-rdp` crate
//! (no external Remmina). Each session runs its protocol loop on a background
//! thread and publishes framebuffer snapshots; the UI draws them in a floating
//! window and forwards mouse/keyboard as fast-path input.
//!
//! The session advertises multitransport, so where the server and the transport
//! stack support it the graphics ride UDP (via the sibling `rdpeudp` crate);
//! otherwise it stays on the reliable TCP path.

use eframe::egui;
use ferropipe_rdp::input::{mouse_event, unicode_event, PTRFLAGS_BUTTON1, PTRFLAGS_BUTTON2, PTRFLAGS_DOWN, PTRFLAGS_MOVE};
use ferropipe_rdp::session::{RdpSession, SessionParams};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

/// A batch of fast-path input event byte-blobs.
type InputBatch = Vec<Vec<u8>>;

#[derive(Default)]
struct FrameState {
    rgba: Vec<u8>,
    width: usize,
    height: usize,
    generation: u64,
    status: Option<String>,
    finished: bool,
}

struct NativeSession {
    title: String,
    /// Identity of the saved connection (dedup key), so re-activating a
    /// connection that's already open doesn't spawn a second window.
    key: String,
    /// Stable, unique key for this session's OS window (viewport).
    vp: u64,
    frame: Arc<Mutex<FrameState>>,
    input_tx: Sender<InputBatch>,
    texture: Option<egui::TextureHandle>,
    last_generation: u64,
    open: bool,
}

/// Manages all open native RDP sessions.
#[derive(Default)]
pub struct NativeRdpManager {
    sessions: Vec<NativeSession>,
    /// Monotonic counter handing each session a stable viewport key.
    next_vp: u64,
}

impl NativeRdpManager {
    pub fn new() -> NativeRdpManager {
        NativeRdpManager::default()
    }

    /// Whether a session for the connection identified by `key` is already open.
    pub fn is_open(&self, key: &str) -> bool {
        self.sessions.iter().any(|s| s.key == key)
    }

    /// Open a new native RDP session for `params`, titled `title`, identified by
    /// `key` (the saved connection id). Each session gets its own maximized OS
    /// window.
    pub fn open(&mut self, params: SessionParams, title: String, key: String) {
        let frame = Arc::new(Mutex::new(FrameState {
            status: Some(format!("Connecting to {}…", params.host)),
            ..Default::default()
        }));
        let (input_tx, input_rx): (Sender<InputBatch>, Receiver<InputBatch>) = channel();

        let frame_bg = frame.clone();
        thread::spawn(move || run_session(params, frame_bg, input_rx));

        let vp = self.next_vp;
        self.next_vp += 1;
        self.sessions.push(NativeSession {
            title,
            key,
            vp,
            frame,
            input_tx,
            texture: None,
            last_generation: 0,
            open: true,
        });
    }

    /// Draw every open session. Each rides its own maximized OS window (an egui
    /// immediate viewport), like a real RDP client. Call once per frame.
    pub fn show(&mut self, ctx: &egui::Context) {
        for session in &mut self.sessions {
            if !session.open {
                continue;
            }
            let vid = egui::ViewportId::from_hash_of(("ferropipe-rdp-session", session.vp));
            let builder = egui::ViewportBuilder::default()
                .with_title(session.title.clone())
                .with_inner_size([1280.0, 800.0])
                .with_maximized(true);
            let mut keep_open = true;
            ctx.show_viewport_immediate(vid, builder, |ui, _class| {
                session.render(ui);
                if ui.ctx().input(|i| i.viewport().close_requested()) {
                    keep_open = false;
                }
            });
            session.open = keep_open;
        }
        self.sessions.retain(|s| s.open);
        if !self.sessions.is_empty() {
            // Cap the redraw at ~60fps instead of spinning the whole app at the
            // display's max rate while an RDP window is open.
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }
}

impl NativeSession {
    fn render(&mut self, ui: &mut egui::Ui) {
        // Snapshot the newest framebuffer under the lock, then release it before
        // the (potentially slow) GPU texture upload so the protocol thread isn't
        // blocked writing the next frame.
        let (new_image, status, finished) = {
            let f = self.frame.lock().unwrap();
            let new_image = if f.generation != self.last_generation && f.width > 0 {
                let img = egui::ColorImage::from_rgba_unmultiplied([f.width, f.height], &f.rgba);
                self.last_generation = f.generation;
                Some(img)
            } else {
                None
            };
            (new_image, f.status.clone(), f.finished)
        };
        if let Some(image) = new_image {
            self.texture = Some(ui.ctx().load_texture("rdp-desktop", image, egui::TextureOptions::LINEAR));
        }

        if let Some(msg) = status {
            ui.horizontal(|ui| {
                if !finished {
                    ui.spinner();
                }
                ui.label(msg);
            });
        }

        let Some(tex) = &self.texture else { return };
        let size = tex.size_vec2();
        egui::ScrollArea::both().show(ui, |ui| {
            let response = ui.add(
                egui::Image::new(egui::load::SizedTexture::new(tex.id(), size))
                    .sense(egui::Sense::click_and_drag()),
            );
            self.forward_input(ui, &response);
        });
    }

    /// Translate egui pointer + keyboard into fast-path input events.
    fn forward_input(&self, ui: &egui::Ui, response: &egui::Response) {
        let mut events: InputBatch = Vec::new();

        if let Some(pos) = response.hover_pos() {
            let rel = pos - response.rect.min;
            let (x, y) = (rel.x.max(0.0) as u16, rel.y.max(0.0) as u16);
            let mut flags = PTRFLAGS_MOVE;
            if response.dragged_by(egui::PointerButton::Primary) || response.is_pointer_button_down_on() {
                flags |= PTRFLAGS_DOWN | PTRFLAGS_BUTTON1;
            } else if response.dragged_by(egui::PointerButton::Secondary) {
                flags |= PTRFLAGS_DOWN | PTRFLAGS_BUTTON2;
            }
            events.push(mouse_event(flags, x, y));
        }

        ui.ctx().input(|i| {
            for ev in &i.events {
                if let egui::Event::Text(text) = ev {
                    for ch in text.chars() {
                        events.push(unicode_event(ch as u16, true));
                        events.push(unicode_event(ch as u16, false));
                    }
                }
            }
        });

        if !events.is_empty() {
            let _ = self.input_tx.send(events);
        }
    }
}

/// Background thread body: connect, then pump frames and forward input.
fn run_session(params: SessionParams, frame: Arc<Mutex<FrameState>>, input_rx: Receiver<InputBatch>) {
    let mut session = match RdpSession::connect(&params) {
        Ok(s) => {
            frame.lock().unwrap().status = Some("Connected".to_string());
            s
        }
        Err(e) => {
            let mut f = frame.lock().unwrap();
            f.status = Some(format!("Connect failed: {e}"));
            f.finished = true;
            return;
        }
    };

    // Best-effort UDP multitransport upgrade (RDP over UDP). If the server offers
    // it and the sideband comes up, graphics ride rdpeudp; otherwise the session
    // stays on the reliable TCP path — same as a real RDP client's fallback.
    if let Some(req) = session.multitransport_request() {
        frame.lock().unwrap().status = Some("Negotiating UDP transport…".to_string());
        let peer: Option<std::net::SocketAddr> = format!("{}:{}", params.host, params.port).parse().ok();
        let local: Option<std::net::SocketAddr> = "0.0.0.0:0".parse().ok();
        let isn = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0x1234_5678);
        let msg = match (peer, local) {
            (Some(peer), Some(local)) => match session.enable_udp(&req, local, peer, isn) {
                Ok(()) => "Connected — RDP over UDP".to_string(),
                Err(_) => "Connected (TCP; UDP unavailable)".to_string(),
            },
            _ => "Connected (TCP)".to_string(),
        };
        frame.lock().unwrap().status = Some(msg);
    }

    loop {
        // Drain pending input. If the UI side has gone (its window closed and
        // the session was dropped), the sender is disconnected — tear down the
        // RDP connection instead of looping forever holding the socket.
        loop {
            match input_rx.try_recv() {
                Ok(events) => {
                    let _ = session.send_input(&events);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        }
        match session.pump() {
            Ok(changed) => {
                if changed {
                    let fb = session.framebuffer();
                    let mut f = frame.lock().unwrap();
                    f.rgba = fb.pixels().to_vec();
                    f.width = fb.width();
                    f.height = fb.height();
                    f.generation += 1;
                    f.status = None;
                }
            }
            Err(e) => {
                let mut f = frame.lock().unwrap();
                f.status = Some(format!("Session ended: {e}"));
                f.finished = true;
                return;
            }
        }
    }
}
