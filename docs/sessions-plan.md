# Sessions layout: implementation plan

Status: proposal, not started. Written 2026-09-25 from the code as it stood after the
Foundry redesign. Each open host becomes a tab; a tab is a dual-pane FILES view, a
CONSOLE bound to one host, or the native RDP client rendered in-app.

## 1. Session model

New file `src/app/session.rs`:

```rust
pub struct SessionId(u64);                     // monotonic, never reused
pub enum SessionKind { Files, Console, Rdp }   // drives the mono kind label
pub struct Session { id, title: String, host: Option<Uuid>, body: SessionBody }
pub enum SessionBody { Files(FilesSession), Console(ConsoleSession), Rdp(RdpTab) }
```

**`RemoteLink`** (extract first; it is exactly today's duplicated field group): `worker: WorkerHandle`,
`join: Option<JoinHandle<()>>`, `events: Receiver<Event>`, `connected/connecting/pending`,
`kind: &'static str`, `connected_at: Option<f64>`, `last_transfer_error: Option<(f64, String)>`.
The right-pane fields (`worker/worker_join/events/connected/connecting/pending_connect/connected_at/active_kind`)
become one `RemoteLink`; the left-pane fields (`lworker/levents/lconnected/lconnecting/lpending/lkind`)
become a second one. Nothing else changes in step 1.

**`FilesSession`** = today's dual pane, scoped: `panes: [PaneState; 2]`, `links: [Option<RemoteLink>; 2]`
(`None` = local side), `left_saved_cwd`, `stage_download`, `stage_cleanup`. The `side_*` helpers,
`revert_left_to_local` and `left_to_local` become methods on it.

**`ConsoleSession`** = `RemoteLink` + `lines: Vec<(String, bool)>` + `input: String` + `exec_running: bool`.
The globals `console/console_input/console_open/exec_running/active_kind` move here; `submit_console`
and `render_console` become methods; the top-bar "Console" toggle becomes "open a Console tab for the
selected host".

**`RdpTab`** = the current `NativeSession` minus the viewport bits: `frame: Arc<Mutex<FrameState>>`,
`input_tx`, `texture`, `last_generation`.

**Transfers**: keep one global `Vec<TransferState>` but re-key the endpoint. `TransferState.worker: Side`
and `RetryRequest.hosts` become `Endpoint { session: SessionId, side: Side }` (+ hosts per endpoint);
`DragPayload` gains `session: SessionId` so a drag from tab A onto tab B is representable. `next_id`
stays global (it keys `pending_edit`, `stage_download`, `stage_cleanup` across sessions).

**Stays global**: `store`, `vault`, `store_path`, `toasts`, `status`, `search`, `selected_conn`,
`host_errors`, `editor`/`prompt`/`confirm_delete`, `edits`/`pending_edit`/`edit_poll` (key `EditWatch`
by `Endpoint` instead of `Side`), `ssh_panel`, settings.

## 2. Concurrency

`drain_events` already drains two receivers into Vecs and then dispatches with `&mut self`. With N
sessions: iterate sessions, drain each `RemoteLink.events` into `Vec<(SessionId, Side, Event)>`, then
dispatch. `Event` needs no session field: the receiver identifies the link. Repaint stays per worker.

Limits: `MAX_SESSIONS = 8` (a Files tab can hold 2 threads + 2 sockets). One Console per host and one
RDP tab per host, deduped like `NativeRdpManager::is_open`. Files tabs: unlimited up to the cap.

Cross-session transfer: `start_transfer(payload, dest: Endpoint)`; both endpoints resolve through
`sessions.get(id)?.files()?`, so remote→remote between panes of different tabs works with no protocol
change (`start_staged` already stages through a local temp dir; `StageJob.dest` becomes an `Endpoint`).
`retry_allowed` compares endpoint host ids and additionally requires both sessions to still exist.

## 3. RDP in a tab

`NativeRdpManager::show` opens one maximized OS window per session via `show_viewport_immediate`, but
`NativeSession::render` already takes `&mut egui::Ui` and uploads the framebuffer as a
`TextureHandle`. Embedding is therefore a call-site change: call `render(ui)` from the central panel
when the active tab is `SessionBody::Rdp`; keep the viewport path behind a "Pop out" button.

Input routing is the real work. `forward_input` reads `ui.ctx().input(|i| i.events)` globally; in a tab
that steals keystrokes from the sidebar filter and the console. Fix: allocate the image response with
`Sense::click_and_drag()` + focusable, gate on `response.has_focus()`, consume the events for the focused
frame, and intercept Ctrl+Tab / Ctrl+W before forwarding. Gate mouse forwarding on
`response.contains_pointer()`.

Risks from ferropipe-rdp's state (`PLAN.md`): EGFX PLANAR and H.264 (AVC420/444) are undecoded, so
GFX-mandatory hosts paint black (the 7 s watchdog must survive embedding). No scancode keymap is wired
(only `unicode_event`), so modifiers, arrows and Ctrl+Alt+Del do not work. `SessionParams.width/height`
are fixed at connect and deactivate/reactivate is ignored, so the desktop cannot resize with the tab:
scale-to-fit with aspect preservation, or keep `ScrollArea::both`. `session.cursor()` is never read: the
remote pointer is invisible. The forced 16 ms repaint should only apply while an RDP tab is visible.

## 4. UI

New `src/widgets/tab.rs`: `tab(ui, &TabView) -> Option<TabAction>` with
`TabView { title, kind_label, status: HostStatus, kind: Option<ConnectionKind>, active }`, built from
`status_dot` + `protocol_chip` + a mono kind label. Fill `bg2` inactive / `bg1` active with a 2 px accent
top edge on the active tab; `×` on hover; middle-click closes. The strip lives in `src/app/tabs.rs` as
`Panel::top("tabs")` below the top bar, horizontally scrollable, with an `icon_button("+")` at the end.

Keyboard: Ctrl+Tab / Ctrl+Shift+Tab cycle, Ctrl+W close, Ctrl+1..9 jump, as a pure
`tab_shortcut(mods, key) -> Option<TabCmd>` consumed before RDP input forwarding.

Entry points: sidebar double-click → `open_tab(id)`: SSH/SMB/WinRM → Files tab (host right, local left),
RdpNative → Rdp tab, Rdp → Remmina (unchanged), "Open terminal" stays in the host menu. The pane host
popup is reused verbatim as the "+" picker. The "Choose a host" empty state survives inside a Files tab; a
new global empty state replaces the workspace when zero tabs are open.

Gutter stays per Files tab. Drawer stays global and bottom-docked, rows labelled by session title. The
status bar's session line switches to the active session.

## 5. Migration steps

| # | Step | Files | Days | Tests |
|---|------|-------|------|-------|
| 1 | Extract `RemoteLink`; `FerropipeApp.links: [RemoteLink; 2]`. No UI change. | `app.rs`, new `app/session.rs`, `app/panes.rs`, `app/chrome.rs`, `app/sidebar.rs` | 1.5 | `RemoteLink` state transitions |
| 2 | `Session`/`SessionKind` + `Sessions { list, active }` holding one implicit Files session. | `app/session.rs`, `app.rs` | 2 | add/close/activate/next-index math; `(host, kind)` dedupe |
| 3 | Tab strip widget + shortcuts, still one tab. | new `widgets/tab.rs`, new `app/tabs.rs` | 1.5 | `tab_shortcut`, `kind_label`, title truncation |
| 4 | N Files tabs: per-session links, fan-out `drain_events`, open/close, `Endpoint`-keyed transfers. | `app.rs`, `app/transfers.rs`, `app/panes.rs`, `app/sidebar.rs` | 2.5 | event routing, `retry_allowed` with endpoints |
| 5 | Console as its own tab kind, one per host. | `app/session.rs`, `app/console.rs` | 1.5 | ring-buffer cap, per-host dedupe |
| 6 | RDP in a tab: split `render` from the viewport, focus-gated input, "Pop out". | `native_rdp.rs`, `app/tabs.rs` | 2.5 | event→`InputBatch` mapping, scale-to-fit rect math |
| 7 | Cross-tab staged transfers + drawer grouping. | `app/transfers.rs` | 1 | `StageJob` endpoint bookkeeping |

Not unit-testable (manual checklist): egui painting and hit-testing, real worker threads and sockets,
viewport pop-out, all RDP interop.

## 6. Decisions needed before starting

1. Does a Files tab own one host (right pane, local left, left re-pointable as today) or is every pane
   its own session with tabs as pane pairs? The first is proposed; the second is cleaner but much larger.
2. Session limit: 8 tabs ≈ 16 threads. Acceptable?
3. Live edits and transfers when a tab closes: cancel, block the close, or orphan into the global drawer?
4. Persistence: restore open tabs on restart (needs re-auth; `Settings` only stores `last_local_dir`)?
5. RDP keyboard is unusable without a scancode keymap. Ship the RDP tab as experimental with "Pop out"
   and the Remmina fallback, or wait for the keymap?
6. Fixed RDP desktop size: accept scale-to-fit blur, or wait for deactivate/reactivate handling?
7. Does the top-bar "Console" toggle disappear, or stay as a shortcut that opens a Console tab for the
   selected host?
