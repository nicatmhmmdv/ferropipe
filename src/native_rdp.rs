//! Native RDP sessions rendered inside Ferropipe using the `ferropipe-rdp` crate
//! (no external Remmina). Each session runs its protocol loop on a background
//! thread and publishes framebuffer snapshots; the UI draws them in a floating
//! window and forwards mouse/keyboard as fast-path input.
//!
//! Keyboard: every `egui::Event::Key` goes out as a scan code for the *physical*
//! key (`ferropipe_rdp::keymap::egui`), so modifiers, arrows, F-keys, Escape,
//! Tab and friends reach the server and non-US layouts work (the server applies
//! its own layout). `Event::Text` is sent as a Unicode event only when no scan
//! code already carried it (a key `KeyCode` cannot express, or an IME commit).
//! A strip above the desktop carries the host name, a Ctrl+Alt+Del button and
//! Disconnect.
//!
//! Mouse: moves, the three buttons (press and release as distinct fast-path
//! events) and the wheel — one `wheel_event` per notch, both axes — go to the
//! remote while the pointer is over the desktop; the surrounding `ScrollArea`
//! ignores the wheel so the two never fight. The remote pointer shape, when
//! the server sends one, is drawn under the pointer in place of the OS cursor.
//!
//! The session advertises multitransport, so where the server and the transport
//! stack support it the graphics ride UDP (via the sibling `rdpeudp` crate);
//! otherwise it stays on the reliable TCP path.

use eframe::egui;
use egui::containers::scroll_area::ScrollSource;
use egui::{
    Align, Button, Color32, CursorIcon, FontId, Label, Layout, MouseWheelUnit, PointerButton, Pos2,
    Rect, RichText, Sense, Stroke, UiBuilder, Vec2,
};
use ferropipe_rdp::graphics::pointer::Cursor;
use ferropipe_rdp::input::{
    self, PTRFLAGS_BUTTON1, PTRFLAGS_BUTTON2, PTRFLAGS_BUTTON3, PTRFLAGS_DOWN, PTRFLAGS_MOVE,
};
use ferropipe_rdp::keymap::egui::KeyboardBridge;
use ferropipe_rdp::keymap::{self, KeyCode, KeyEvent};
use ferropipe_rdp::session::{RdpSession, SessionParams};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::theme::{self, RADIUS};

/// One encoded fast-path input event (scan code, Unicode or mouse): what the
/// session thread's channel carries, in batches.
type Outgoing = Vec<u8>;
/// A batch of fast-path input event byte-blobs.
type InputBatch = Vec<Outgoing>;

/// Keep Tab, arrows and Escape on the desktop instead of letting egui move its
/// own focus around (otherwise Tab lands on a strip button and Enter clicks it).
const DESKTOP_EVENT_FILTER: egui::EventFilter = egui::EventFilter {
    tab: true,
    horizontal_arrows: true,
    vertical_arrows: true,
    escape: true,
};
/// Height of the strip above the desktop image.
const STRIP_H: f32 = 24.0;
/// Horizontal inset of the strip's contents.
const STRIP_PAD: f32 = 8.0;
/// Horizontal padding inside the strip's buttons.
const STRIP_BUTTON_PAD_X: f32 = 8.0;
/// Size of the host name in the strip (matches the sidebar's host rows).
const STRIP_NAME_SIZE: f32 = 13.0;
/// Mouse buttons forwarded to the remote, with their [MS-RDPBCGR] button flags.
const BUTTONS: [(PointerButton, u16); 3] = [
    (PointerButton::Primary, PTRFLAGS_BUTTON1),
    (PointerButton::Secondary, PTRFLAGS_BUTTON2),
    (PointerButton::Middle, PTRFLAGS_BUTTON3),
];
/// Points of wheel travel per remote wheel notch: what egui-winit reports for
/// one `MouseWheelUnit::Line`, so a real wheel click is exactly one notch.
const POINTS_PER_NOTCH: f32 = 50.0;

/// What the user clicked in the top strip this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StripAction {
    None,
    /// Send Ctrl+Alt+Del.
    SecureAttention,
    /// Close the session.
    Disconnect,
}

/// Turn one frame's egui keyboard events into fast-path input events, in order.
///
/// `modifiers` is the frame's collapsed modifier state (`InputState::modifiers`).
/// The server's Shift/Ctrl/Alt are brought in line with it first, then each
/// event is applied: keys go out as scan codes and the bridge keeps modifier
/// state straight; a `Text` event is sent as Unicode only when the bridge says
/// no scan code already carried it (`KeyboardBridge::text_wanted`). Pointer
/// events are not handled here — the mouse is forwarded from the image widget's
/// response (see [`pointer_event`]).
fn translate_events(
    bridge: &mut KeyboardBridge,
    events: &[egui::Event],
    modifiers: egui::Modifiers,
) -> Vec<Outgoing> {
    let mut out: Vec<Outgoing> = encode(bridge.modifiers_changed(modifiers)).collect();
    for event in events {
        match event {
            egui::Event::Key {
                key,
                physical_key,
                pressed,
                repeat,
                modifiers,
            } => {
                let events = bridge.key(*key, *physical_key, *pressed, *repeat, *modifiers);
                out.extend(encode(events));
            }
            egui::Event::ModifiersChanged(m) => out.extend(encode(bridge.modifiers_changed(*m))),
            egui::Event::Text(text) => {
                if bridge.text_wanted(modifiers) {
                    out.extend(text_events(text));
                }
            }
            // egui-winit turns Ctrl+C/X/V into these instead of key events. Ctrl
            // is already down on the server, so tapping the letter completes the
            // chord there; clipboard redirection is out of scope.
            egui::Event::Copy => out.extend(tap(KeyCode::C)),
            egui::Event::Cut => out.extend(tap(KeyCode::X)),
            egui::Event::Paste(_) => out.extend(tap(KeyCode::V)),
            // Never leave a modifier stuck down on the server when the window
            // goes away.
            egui::Event::WindowFocused(false) => out.extend(encode(bridge.release_all())),
            _ => {}
        }
    }
    out
}

/// A press and release Unicode event per UTF-16 unit of `text`.
fn text_events(text: &str) -> impl Iterator<Item = Outgoing> + '_ {
    text.encode_utf16().flat_map(|unit| {
        [
            input::unicode_event(unit, true),
            input::unicode_event(unit, false),
        ]
    })
}

fn encode(events: Vec<KeyEvent>) -> impl Iterator<Item = Outgoing> {
    events.into_iter().map(KeyEvent::encode)
}

/// Press and release one key.
fn tap(key: KeyCode) -> [Outgoing; 2] {
    [
        keymap::key_event(key, true).encode(),
        keymap::key_event(key, false).encode(),
    ]
}

/// One frame's pointer activity over the desktop image, in remote pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PointerFrame {
    /// Where the pointer is, clamped onto the desktop: while it hovers the image
    /// or holds a button it pressed there. `None` anywhere else (the strip, a
    /// tooltip), so clicks on the chrome never reach the remote.
    pos: Option<(u16, u16)>,
    /// Buttons that went down over the image this frame (indexed like `BUTTONS`).
    pressed: [bool; 3],
    /// Buttons that came up this frame, anywhere (indexed like `BUTTONS`).
    released: [bool; 3],
    /// Wheel travel over the image this frame, in points, egui's way round:
    /// +y = content moves down (wheel rolled away), +x = content moves right.
    scroll: Vec2,
}

/// Pointer state carried between frames.
#[derive(Debug, Default)]
struct PointerTracker {
    /// Buttons the server currently has down (indexed like `BUTTONS`).
    held: [bool; 3],
    /// Last position sent, so a still pointer doesn't send a move every frame.
    last_pos: Option<(u16, u16)>,
    /// Sub-notch wheel remainders (x, y) in points.
    wheel_acc: (f32, f32),
}

impl PointerTracker {
    /// Fast-path mouse events for one frame, in order: move, presses, releases,
    /// then one wheel event per notch. A press is `PTRFLAGS_DOWN` plus the
    /// button; a release carries the button alone ([MS-RDPBCGR] 2.2.8.1.2.2.3).
    /// A release with the pointer gone (drag ended off the image) is sent at the
    /// last position.
    fn events(&mut self, frame: &PointerFrame) -> Vec<Outgoing> {
        let Some((x, y)) = frame.pos.or(self.last_pos) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if frame.pos.is_some() && frame.pos != self.last_pos {
            self.last_pos = frame.pos;
            out.push(input::mouse_event(PTRFLAGS_MOVE, x, y));
        }
        for (i, (_, flag)) in BUTTONS.iter().enumerate() {
            if frame.pressed[i] && !self.held[i] {
                self.held[i] = true;
                out.push(input::mouse_event(PTRFLAGS_DOWN | flag, x, y));
            }
            if frame.released[i] && self.held[i] {
                self.held[i] = false;
                out.push(input::mouse_event(*flag, x, y));
            }
        }
        out.extend(self.wheel_events(frame.scroll, x, y));
        out
    }

    /// One `wheel_event` per whole notch of travel; the remainder carries over.
    /// Vertical signs agree between egui and the wire (+ = rolled away). egui's
    /// +x means the content moves right, i.e. the wheel tilted *left*, which is
    /// a negative horizontal rotation on the wire.
    fn wheel_events(&mut self, scroll: Vec2, x: u16, y: u16) -> Vec<Outgoing> {
        let vertical = take_notches(&mut self.wheel_acc.1, scroll.y, POINTS_PER_NOTCH);
        let horizontal = -take_notches(&mut self.wheel_acc.0, scroll.x, POINTS_PER_NOTCH);
        let notches = |axis_vertical: bool, n: i32| {
            (0..n.abs()).map(move |_| input::wheel_event(axis_vertical, n.signum(), x, y))
        };
        notches(true, vertical)
            .chain(notches(false, horizontal))
            .collect()
    }
}

/// Add `delta` points of wheel travel to `acc` and take out the whole notches
/// of `step` points it now holds (negative for the other direction); the
/// remainder stays in `acc` for the next frame.
fn take_notches(acc: &mut f32, delta: f32, step: f32) -> i32 {
    *acc += delta;
    let notches = (*acc / step).trunc();
    *acc -= notches * step;
    notches as i32
}

/// A pointer position in the image widget's `rect` as remote pixel
/// coordinates, clamped onto the desktop (a drag may leave the image).
fn to_remote(pos: Pos2, rect: Rect) -> (u16, u16) {
    let rel = pos - rect.min;
    let clamp = |v: f32, extent: f32| v.clamp(0.0, (extent - 1.0).max(0.0)) as u16;
    (clamp(rel.x, rect.width()), clamp(rel.y, rect.height()))
}

/// Points of wheel travel in this frame's events, each unit converted the way
/// egui's own scroll areas do: a line is `POINTS_PER_NOTCH`, a page is `page`.
fn wheel_points(events: &[egui::Event], page: Vec2) -> Vec2 {
    events
        .iter()
        .filter_map(|event| match event {
            egui::Event::MouseWheel { unit, delta, .. } => Some(match unit {
                MouseWheelUnit::Point => *delta,
                MouseWheelUnit::Line => *delta * POINTS_PER_NOTCH,
                MouseWheelUnit::Page => *delta * page,
            }),
            _ => None,
        })
        .fold(Vec2::ZERO, |sum, delta| sum + delta)
}

/// Read this frame's pointer activity off the desktop image's response.
fn pointer_frame(ui: &egui::Ui, response: &egui::Response) -> PointerFrame {
    let over = response.contains_pointer();
    let pos = response
        .interact_pointer_pos()
        .or_else(|| response.hover_pos())
        .map(|p| to_remote(p, response.rect));
    ui.input(|i| PointerFrame {
        pos,
        pressed: BUTTONS.map(|(button, _)| over && i.pointer.button_pressed(button)),
        released: BUTTONS.map(|(button, _)| i.pointer.button_released(button)),
        scroll: if over {
            wheel_points(&i.events, response.rect.size())
        } else {
            Vec2::ZERO
        },
    })
}

/// Where to draw a cursor image of `size` pixels whose hot-spot sits on
/// `pointer`, with the desktop drawn at `scale` points per remote pixel. The
/// corner is snapped to whole points so nearest-neighbour sampling stays crisp.
fn cursor_rect(pointer: Pos2, hotspot: (u16, u16), size: (u16, u16), scale: f32) -> Rect {
    let hotspot = Vec2::new(f32::from(hotspot.0), f32::from(hotspot.1)) * scale;
    let size = Vec2::new(f32::from(size.0), f32::from(size.1)) * scale;
    Rect::from_min_size((pointer - hotspot).round(), size)
}

/// The remote pointer shape, uploaded as a texture.
struct RemoteCursor {
    texture: egui::TextureHandle,
    hotspot: (u16, u16),
    size: (u16, u16),
}

/// Upload a pointer shape as a nearest-sampled texture (cursors are tiny pixel
/// art). Empty or malformed shapes count as hidden.
fn upload_cursor(ctx: &egui::Context, cursor: &Cursor) -> Option<RemoteCursor> {
    let size = (
        u16::try_from(cursor.width).ok()?,
        u16::try_from(cursor.height).ok()?,
    );
    if size.0 == 0 || size.1 == 0 || cursor.rgba.len() != cursor.width * cursor.height * 4 {
        return None;
    }
    let image =
        egui::ColorImage::from_rgba_unmultiplied([cursor.width, cursor.height], &cursor.rgba);
    Some(RemoteCursor {
        texture: ctx.load_texture("rdp-cursor", image, egui::TextureOptions::NEAREST),
        hotspot: (cursor.hotspot_x, cursor.hotspot_y),
        size,
    })
}

#[derive(Default)]
struct FrameState {
    rgba: Vec<u8>,
    width: usize,
    height: usize,
    generation: u64,
    /// The remote pointer shape (`None`: show the OS cursor) and how many
    /// shape changes the session has seen, so the UI re-uploads only on change.
    cursor: Option<Cursor>,
    cursor_generation: u64,
    status: Option<String>,
    finished: bool,
}

struct NativeSession {
    title: String,
    /// The remote host, shown in the strip above the desktop.
    host: String,
    /// Identity of the saved connection (dedup key), so re-activating a
    /// connection that's already open doesn't spawn a second window.
    key: String,
    /// Stable, unique key for this session's OS window (viewport).
    vp: u64,
    frame: Arc<Mutex<FrameState>>,
    input_tx: Sender<InputBatch>,
    texture: Option<egui::TextureHandle>,
    last_generation: u64,
    /// The remote pointer shape as a texture, and the generation it came from.
    cursor: Option<RemoteCursor>,
    cursor_generation: u64,
    /// Which modifier keys the server currently has down.
    keyboard: KeyboardBridge,
    /// Which mouse buttons the server has down, plus wheel remainders.
    pointer: PointerTracker,
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
        let (input_tx, input_rx): (Sender<InputBatch>, Receiver<InputBatch>) = mpsc::channel();

        let host = params.host.clone();
        let frame_bg = frame.clone();
        thread::spawn(move || run_session(params, frame_bg, input_rx));

        let vp = self.next_vp;
        self.next_vp += 1;
        self.sessions.push(NativeSession {
            title,
            host,
            key,
            vp,
            frame,
            input_tx,
            texture: None,
            last_generation: 0,
            cursor: None,
            cursor_generation: 0,
            keyboard: KeyboardBridge::new(),
            pointer: PointerTracker::default(),
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
            ctx.show_viewport_immediate(vid, builder, |ui, _class| {
                session.render(ui);
                if ui.ctx().input(|i| i.viewport().close_requested()) {
                    session.open = false;
                }
            });
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
        match self.render_strip(ui) {
            StripAction::Disconnect => {
                // Dropping the session (and its sender) makes the protocol
                // thread tear the connection down, same as closing the window.
                self.open = false;
                return;
            }
            StripAction::SecureAttention => {
                // Release whatever the bridge believes is held first, so the
                // server's modifier state matches ours after the sequence's
                // own releases (the user may be holding Ctrl while clicking).
                let mut batch: Vec<Outgoing> = encode(self.keyboard.release_all()).collect();
                batch.extend(encode(keymap::secure_attention_sequence()));
                self.send(batch);
            }
            StripAction::None => {}
        }
        let (status, finished) = self.upload_frame(ui.ctx());
        if let Some(msg) = status {
            ui.horizontal(|ui| {
                if !finished {
                    ui.spinner();
                }
                ui.label(msg);
            });
        }

        let Some((tex_id, size)) = self.texture.as_ref().map(|t| (t.id(), t.size_vec2())) else {
            return;
        };
        egui::ScrollArea::both()
            // The wheel belongs to the remote desktop; the bars stay draggable.
            .scroll_source(ScrollSource {
                mouse_wheel: false,
                ..ScrollSource::default()
            })
            .show(ui, |ui| {
                let response = ui.add(
                    egui::Image::new(egui::load::SizedTexture::new(tex_id, size))
                        .sense(Sense::click_and_drag()),
                );
                self.forward_input(ui, &response);
                self.draw_cursor(ui, &response, response.rect.width() / size.x);
            });
    }

    /// Draw the remote pointer shape under the pointer while it hovers the
    /// desktop, hiding the OS cursor so the two don't overlap. With no remote
    /// shape (hidden, or the server wants the default arrow) the OS cursor stays.
    fn draw_cursor(&self, ui: &egui::Ui, response: &egui::Response, scale: f32) {
        let (Some(cursor), Some(pos)) = (&self.cursor, response.hover_pos()) else {
            return;
        };
        ui.ctx().set_cursor_icon(CursorIcon::None);
        let rect = cursor_rect(pos, cursor.hotspot, cursor.size, scale);
        // WHITE is the identity tint: the shape's own colours, unchanged.
        let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        ui.painter()
            .image(cursor.texture.id(), rect, uv, Color32::WHITE);
    }

    /// Upload the newest framebuffer to the texture when it changed. Returns the
    /// session's status line and whether the session has ended.
    fn upload_frame(&mut self, ctx: &egui::Context) -> (Option<String>, bool) {
        // Snapshot the newest framebuffer under the lock, then release it before
        // the (potentially slow) GPU texture upload so the protocol thread isn't
        // blocked writing the next frame.
        let (new_image, new_cursor, status, finished) = {
            let f = self.frame.lock().unwrap();
            let new_image = if f.generation != self.last_generation && f.width > 0 {
                let img = egui::ColorImage::from_rgba_unmultiplied([f.width, f.height], &f.rgba);
                self.last_generation = f.generation;
                Some(img)
            } else {
                None
            };
            let new_cursor = if f.cursor_generation != self.cursor_generation {
                self.cursor_generation = f.cursor_generation;
                Some(f.cursor.clone())
            } else {
                None
            };
            (new_image, new_cursor, f.status.clone(), f.finished)
        };
        if let Some(image) = new_image {
            self.texture =
                Some(ctx.load_texture("rdp-desktop", image, egui::TextureOptions::LINEAR));
        }
        if let Some(cursor) = new_cursor {
            self.cursor = cursor.as_ref().and_then(|c| upload_cursor(ctx, c));
        }
        (status, finished)
    }

    /// The 24 px bar above the desktop: host name on the left, session actions
    /// on the right.
    fn render_strip(&self, ui: &mut egui::Ui) -> StripAction {
        let p = theme::current(ui);
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), STRIP_H), Sense::hover());
        ui.painter().rect_filled(rect, 0, p.bg2);
        ui.painter().hline(
            rect.x_range(),
            rect.bottom() - 0.5,
            Stroke::new(1.0, p.line),
        );
        let builder = UiBuilder::new()
            .max_rect(rect.shrink2(egui::vec2(STRIP_PAD, 0.0)))
            .layout(Layout::left_to_right(Align::Center));
        ui.scope_builder(builder, |ui| {
            let name = RichText::new(&self.host)
                .font(FontId::new(STRIP_NAME_SIZE, theme::medium()))
                .color(p.text);
            ui.add(Label::new(name).selectable(false).truncate());
            ui.with_layout(Layout::right_to_left(Align::Center), strip_buttons)
                .inner
        })
        .inner
    }

    /// Forward this frame's pointer and keyboard input to the session thread.
    ///
    /// The desktop image always owns keyboard focus, so every key is the
    /// server's; the pointer is forwarded only while it is over the image (or
    /// finishing a drag that started there).
    fn forward_input(&mut self, ui: &egui::Ui, response: &egui::Response) {
        response.request_focus();
        ui.memory_mut(|m| m.set_focus_lock_filter(response.id, DESKTOP_EVENT_FILTER));
        let mut batch: InputBatch = self.pointer.events(&pointer_frame(ui, response));
        ui.ctx()
            .input(|i| batch.extend(translate_events(&mut self.keyboard, &i.events, i.modifiers)));
        self.send(batch);
    }

    /// Hand a batch to the session thread.
    fn send(&self, batch: InputBatch) {
        if batch.is_empty() {
            return;
        }
        // A closed receiver means the protocol thread has exited and already
        // reported why in `FrameState::status`; there is nothing to add here.
        let _ = self.input_tx.send(batch);
    }
}

/// Right-aligned strip buttons (laid out right to left).
fn strip_buttons(ui: &mut egui::Ui) -> StripAction {
    ui.spacing_mut().button_padding.x = STRIP_BUTTON_PAD_X;
    if strip_button(ui, "Disconnect", "Close this session").clicked() {
        return StripAction::Disconnect;
    }
    if strip_button(
        ui,
        "Ctrl+Alt+Del",
        "Send Ctrl+Alt+Del to the remote desktop",
    )
    .clicked()
    {
        return StripAction::SecureAttention;
    }
    StripAction::None
}

/// `widgets::icon_button` styling on a small button that fits the strip.
fn strip_button(ui: &mut egui::Ui, text: &str, tip: &str) -> egui::Response {
    let p = theme::current(ui);
    ui.add(
        Button::new(text)
            .small()
            .corner_radius(RADIUS)
            .fill(p.bg2)
            .stroke((1.0, p.line)),
    )
    .on_hover_text(tip)
}

/// Background thread body: connect, then pump frames and forward input.
fn run_session(
    params: SessionParams,
    frame: Arc<Mutex<FrameState>>,
    input_rx: Receiver<InputBatch>,
) {
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
        let peer: Option<std::net::SocketAddr> =
            format!("{}:{}", params.host, params.port).parse().ok();
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

    // Watchdog: if the session connects but never paints (the host sends EGFX /
    // RemoteFX / H.264 graphics the native decoder can't handle yet), the window
    // would just sit black. Surface a clear explanation after a grace period.
    let connected_at = std::time::Instant::now();
    let mut rendered_any = false;
    let mut warned = false;
    let mut cursor_generation = 0u64;
    const NO_GRAPHICS_WARN: std::time::Duration = std::time::Duration::from_secs(7);

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
                publish_cursor(&session, &frame, &mut cursor_generation);
                if changed {
                    rendered_any = true;
                    let fb = session.framebuffer();
                    let mut f = frame.lock().unwrap();
                    f.rgba = fb.pixels().to_vec();
                    f.width = fb.width();
                    f.height = fb.height();
                    f.generation += 1;
                    f.status = None;
                } else if !rendered_any && !warned && connected_at.elapsed() > NO_GRAPHICS_WARN {
                    warned = true;
                    frame.lock().unwrap().status = Some(
                        "Connected, but no desktop image — this host sends RemoteFX/H.264 (EGFX) \
                         graphics the native client can't decode yet. Use Remmina for this host \
                         (double-click the connection)."
                            .to_string(),
                    );
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

/// Copy the remote pointer shape into the shared frame when it changed.
fn publish_cursor(session: &RdpSession, frame: &Mutex<FrameState>, last: &mut u64) {
    let generation = session.cursor_generation();
    if generation == *last {
        return;
    }
    *last = generation;
    let mut f = frame.lock().unwrap();
    f.cursor = session.cursor().cloned();
    f.cursor_generation = generation;
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, Key, Modifiers, TouchPhase};
    use ferropipe_rdp::input::KBDFLAGS_RELEASE;

    /// A native `Event::Key` for a key at its own physical position.
    fn key(k: Key, pressed: bool, modifiers: Modifiers) -> Event {
        Event::Key {
            key: k,
            physical_key: Some(k),
            pressed,
            repeat: false,
            modifiers,
        }
    }

    /// Modifiers the way a Windows/Linux integration reports them (`command == ctrl`).
    fn mods(shift: bool, ctrl: bool, alt: bool) -> Modifiers {
        Modifiers {
            shift,
            ctrl,
            alt,
            command: ctrl,
            mac_cmd: false,
        }
    }

    fn scancode(code: KeyCode, pressed: bool) -> Outgoing {
        keymap::key_event(code, pressed).encode()
    }

    fn unicode(unit: u16) -> [Outgoing; 2] {
        [
            input::unicode_event(unit, true),
            input::unicode_event(unit, false),
        ]
    }

    /// Whether `blob` is a fast-path Unicode keyboard event (event code 4).
    fn is_unicode(blob: &[u8]) -> bool {
        blob[0] >> 5 == 4
    }

    #[test]
    fn mapped_key_press_yields_one_scancode_event() {
        // Arrange
        let mut bridge = KeyboardBridge::new();
        let events = [key(Key::A, true, Modifiers::NONE)];
        // Act
        let out = translate_events(&mut bridge, &events, Modifiers::NONE);
        // Assert
        assert_eq!(out, vec![scancode(KeyCode::A, true)]);
    }

    #[test]
    fn key_release_carries_the_release_flag() {
        // Arrange
        let mut bridge = KeyboardBridge::new();
        let events = [key(Key::A, false, Modifiers::NONE)];
        // Act
        let out = translate_events(&mut bridge, &events, Modifiers::NONE);
        // Assert
        assert_eq!(out, vec![scancode(KeyCode::A, false)]);
        assert_ne!(out[0][0] & KBDFLAGS_RELEASE, 0);
    }

    #[test]
    fn text_right_after_mapped_press_is_swallowed() {
        // Arrange: the scan code already carries the character.
        let mut bridge = KeyboardBridge::new();
        let events = [key(Key::A, true, Modifiers::NONE), Event::Text("a".into())];
        // Act
        let out = translate_events(&mut bridge, &events, Modifiers::NONE);
        // Assert
        assert_eq!(out, vec![scancode(KeyCode::A, true)]);
    }

    #[test]
    fn text_after_unmapped_key_is_forwarded_as_unicode() {
        // Arrange: F13 has no scan code, so the text is the only way through.
        let mut bridge = KeyboardBridge::new();
        let events = [
            key(Key::F13, true, Modifiers::NONE),
            Event::Text("x".into()),
        ];
        // Act
        let out = translate_events(&mut bridge, &events, Modifiers::NONE);
        // Assert
        assert_eq!(out, unicode(u16::from(b'x')).to_vec());
    }

    #[test]
    fn text_is_sent_as_utf16_units() {
        // Arrange: one astral character is two UTF-16 units.
        let mut bridge = KeyboardBridge::new();
        let events = [Event::Text("\u{1F600}".into())];
        let units: Vec<u16> = "\u{1F600}".encode_utf16().collect();
        // Act
        let out = translate_events(&mut bridge, &events, Modifiers::NONE);
        // Assert
        let expected: Vec<Outgoing> = units.iter().flat_map(|&u| unicode(u)).collect();
        assert_eq!(units.len(), 2);
        assert_eq!(out, expected);
    }

    #[test]
    fn ctrl_held_suppresses_text() {
        // Arrange: Ctrl+C reported as text is a shortcut side effect, not typing.
        let mut bridge = KeyboardBridge::new();
        let ctrl = mods(false, true, false);
        let events = [Event::Text("c".into())];
        // Act
        let out = translate_events(&mut bridge, &events, ctrl);
        // Assert: the server gets Ctrl down (modifiers first) and no Unicode.
        assert!(out.iter().all(|blob| !is_unicode(blob)), "{out:?}");
        assert_eq!(out, vec![scancode(KeyCode::ControlLeft, true)]);
    }

    #[test]
    fn focus_loss_releases_held_modifiers() {
        // Arrange: a physical Shift is down on the server.
        let mut bridge = KeyboardBridge::new();
        let shift = mods(true, false, false);
        let held = translate_events(&mut bridge, &[key(Key::ShiftLeft, true, shift)], shift);
        assert_eq!(held, vec![scancode(KeyCode::ShiftLeft, true)]);
        // Act: the window goes away while the OS still reports Shift held.
        let out = translate_events(&mut bridge, &[Event::WindowFocused(false)], shift);
        // Assert
        assert_eq!(out, vec![scancode(KeyCode::ShiftLeft, false)]);
    }

    #[test]
    fn copy_cut_paste_tap_the_letter_under_the_held_ctrl() {
        // Arrange
        let mut bridge = KeyboardBridge::new();
        let events = [Event::Copy, Event::Cut, Event::Paste("ignored".into())];
        // Act
        let out = translate_events(&mut bridge, &events, Modifiers::NONE);
        // Assert
        let expected: Vec<Outgoing> = [KeyCode::C, KeyCode::X, KeyCode::V]
            .into_iter()
            .flat_map(|k| [scancode(k, true), scancode(k, false)])
            .collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn pointer_events_are_not_translated() {
        // Arrange: the mouse is forwarded from the image response, not from here.
        let mut bridge = KeyboardBridge::new();
        let events = [
            Event::PointerMoved(egui::pos2(3.0, 4.0)),
            Event::PointerGone,
        ];
        // Act
        let out = translate_events(&mut bridge, &events, Modifiers::NONE);
        // Assert
        assert!(out.is_empty(), "{out:?}");
    }

    /// A pointer frame with the pointer resting at (x, y) and nothing else.
    fn frame_at(x: u16, y: u16) -> PointerFrame {
        PointerFrame {
            pos: Some((x, y)),
            ..PointerFrame::default()
        }
    }

    fn mouse(flags: u16, x: u16, y: u16) -> Outgoing {
        input::mouse_event(flags, x, y)
    }

    #[test]
    fn take_notches_returns_whole_notches_and_carries_the_remainder() {
        // Arrange
        let mut acc = 0.0;
        // Act
        let first = take_notches(&mut acc, 30.0, 50.0);
        let second = take_notches(&mut acc, 30.0, 50.0);
        // Assert: 60 points = one notch, 10 left over.
        assert_eq!((first, second), (0, 1));
        assert!((acc - 10.0).abs() < 1e-4, "{acc}");
    }

    #[test]
    fn take_notches_counts_negative_travel_the_other_way() {
        // Arrange
        let mut acc = 0.0;
        // Act / Assert
        assert_eq!(take_notches(&mut acc, -120.0, 50.0), -2);
        assert!((acc + 20.0).abs() < 1e-4, "{acc}");
    }

    #[test]
    fn take_notches_reversal_first_cancels_the_remainder() {
        // Arrange: 30 points up are pending.
        let mut acc = 0.0;
        take_notches(&mut acc, 30.0, 50.0);
        // Act: 40 down.
        let notches = take_notches(&mut acc, -40.0, 50.0);
        // Assert: no notch either way; 10 down pending.
        assert_eq!(notches, 0);
        assert!((acc + 10.0).abs() < 1e-4, "{acc}");
    }

    #[test]
    fn pointer_move_is_sent_once_per_position() {
        // Arrange
        let mut tracker = PointerTracker::default();
        // Act
        let first = tracker.events(&frame_at(3, 4));
        let still = tracker.events(&frame_at(3, 4));
        let moved = tracker.events(&frame_at(5, 4));
        // Assert
        assert_eq!(first, vec![mouse(PTRFLAGS_MOVE, 3, 4)]);
        assert!(still.is_empty(), "{still:?}");
        assert_eq!(moved, vec![mouse(PTRFLAGS_MOVE, 5, 4)]);
    }

    #[test]
    fn button_press_and_release_are_distinct_events() {
        // Arrange
        let mut tracker = PointerTracker::default();
        let mut press = frame_at(3, 4);
        press.pressed[0] = true;
        let mut release = frame_at(3, 4);
        release.released[0] = true;
        // Act
        let down = tracker.events(&press);
        let up = tracker.events(&release);
        // Assert: DOWN|BUTTON1 on press, BUTTON1 alone on release.
        assert_eq!(
            down,
            vec![
                mouse(PTRFLAGS_MOVE, 3, 4),
                mouse(PTRFLAGS_DOWN | PTRFLAGS_BUTTON1, 3, 4)
            ]
        );
        assert_eq!(up, vec![mouse(PTRFLAGS_BUTTON1, 3, 4)]);
    }

    #[test]
    fn middle_button_is_forwarded_as_button3() {
        // Arrange
        let mut tracker = PointerTracker::default();
        let mut press = frame_at(3, 4);
        press.pressed[2] = true;
        // Act
        let out = tracker.events(&press);
        // Assert
        assert_eq!(out[1], mouse(PTRFLAGS_DOWN | PTRFLAGS_BUTTON3, 3, 4));
    }

    #[test]
    fn release_without_a_press_is_not_sent() {
        // Arrange: a button that went down somewhere else comes up over the image.
        let mut tracker = PointerTracker::default();
        let mut frame = frame_at(3, 4);
        frame.released = [true; 3];
        // Act
        let out = tracker.events(&frame);
        // Assert: only the move.
        assert_eq!(out, vec![mouse(PTRFLAGS_MOVE, 3, 4)]);
    }

    #[test]
    fn release_off_the_image_uses_the_last_position() {
        // Arrange: a drag that started on the desktop.
        let mut tracker = PointerTracker::default();
        let mut press = frame_at(3, 4);
        press.pressed[0] = true;
        tracker.events(&press);
        let release = PointerFrame {
            released: [true, false, false],
            ..PointerFrame::default()
        };
        // Act
        let out = tracker.events(&release);
        // Assert: the server still gets its release.
        assert_eq!(out, vec![mouse(PTRFLAGS_BUTTON1, 3, 4)]);
    }

    #[test]
    fn wheel_travel_becomes_one_event_per_notch() {
        // Arrange: two notches rolled away from the user (content moves down).
        let mut tracker = PointerTracker::default();
        let mut frame = frame_at(3, 4);
        frame.scroll = Vec2::new(0.0, 100.0);
        // Act
        let out = tracker.events(&frame);
        // Assert
        let notch = input::wheel_event(true, 1, 3, 4);
        assert_eq!(out, vec![mouse(PTRFLAGS_MOVE, 3, 4), notch.clone(), notch]);
    }

    #[test]
    fn horizontal_wheel_sign_is_flipped_for_the_wire() {
        // Arrange: egui +x = content moves right = wheel tilted left.
        let mut tracker = PointerTracker::default();
        let mut frame = frame_at(3, 4);
        frame.scroll = Vec2::new(50.0, 0.0);
        // Act
        let out = tracker.events(&frame);
        // Assert: a negative horizontal rotation.
        assert_eq!(out[1], input::wheel_event(false, -1, 3, 4));
    }

    #[test]
    fn wheel_points_converts_lines_and_pages() {
        // Arrange
        let wheel = |unit, delta| Event::MouseWheel {
            unit,
            delta,
            phase: TouchPhase::Move,
            modifiers: Modifiers::NONE,
        };
        let events = [
            wheel(MouseWheelUnit::Line, Vec2::new(0.0, 1.0)),
            wheel(MouseWheelUnit::Point, Vec2::new(5.0, 0.0)),
            wheel(MouseWheelUnit::Page, Vec2::new(0.0, 1.0)),
            Event::PointerGone,
        ];
        // Act
        let points = wheel_points(&events, Vec2::new(200.0, 100.0));
        // Assert: 1 line = 50 points, 1 page = the view's height.
        assert_eq!(points, Vec2::new(5.0, 150.0));
    }

    #[test]
    fn to_remote_clamps_to_the_desktop() {
        // Arrange
        let rect = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(100.0, 50.0));
        // Act / Assert
        assert_eq!(to_remote(Pos2::new(15.5, 22.0), rect), (5, 2));
        assert_eq!(to_remote(Pos2::new(-3.0, 500.0), rect), (0, 49));
    }

    #[test]
    fn cursor_rect_is_offset_by_the_hotspot() {
        // Act
        let rect = cursor_rect(Pos2::new(100.0, 100.0), (3, 4), (32, 32), 1.0);
        // Assert
        assert_eq!(
            rect,
            Rect::from_min_size(Pos2::new(97.0, 96.0), Vec2::splat(32.0))
        );
    }

    #[test]
    fn cursor_rect_scales_hotspot_and_size() {
        // Act
        let rect = cursor_rect(Pos2::new(100.0, 100.0), (3, 4), (32, 32), 2.0);
        // Assert
        assert_eq!(
            rect,
            Rect::from_min_size(Pos2::new(94.0, 92.0), Vec2::splat(64.0))
        );
    }
}
