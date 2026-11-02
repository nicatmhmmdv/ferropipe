//! Mouse handling for a native RDP desktop: what the pointer did over the
//! image this frame ([`PointerFrame`]), the state carried between frames
//! ([`PointerTracker`]) that turns it into fast-path mouse events — one move
//! per position, presses and releases as distinct events, one wheel event per
//! notch on either axis — plus the wheel-unit conversion and where the remote
//! cursor shape is drawn ([`cursor_rect`]).

use super::Outgoing;
use egui::{MouseWheelUnit, PointerButton, Pos2, Rect, Vec2};
use ferropipe_rdp::input::{
    self, PTRFLAGS_BUTTON1, PTRFLAGS_BUTTON2, PTRFLAGS_BUTTON3, PTRFLAGS_DOWN, PTRFLAGS_MOVE,
};

/// Mouse buttons forwarded to the remote, with their [MS-RDPBCGR] button flags.
const BUTTONS: [(PointerButton, u16); 3] = [
    (PointerButton::Primary, PTRFLAGS_BUTTON1),
    (PointerButton::Secondary, PTRFLAGS_BUTTON2),
    (PointerButton::Middle, PTRFLAGS_BUTTON3),
];
/// Points of wheel travel per remote wheel notch: what egui-winit reports for
/// one `MouseWheelUnit::Line`, so a real wheel click is exactly one notch.
const POINTS_PER_NOTCH: f32 = 50.0;

/// One frame's pointer activity over the desktop image, in remote pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct PointerFrame {
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
pub(super) struct PointerTracker {
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
    pub(super) fn events(&mut self, frame: &PointerFrame) -> Vec<Outgoing> {
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
pub(super) fn pointer_frame(ui: &egui::Ui, response: &egui::Response) -> PointerFrame {
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
pub(super) fn cursor_rect(
    pointer: Pos2,
    hotspot: (u16, u16),
    size: (u16, u16),
    scale: f32,
) -> Rect {
    let hotspot = Vec2::new(f32::from(hotspot.0), f32::from(hotspot.1)) * scale;
    let size = Vec2::new(f32::from(size.0), f32::from(size.1)) * scale;
    Rect::from_min_size((pointer - hotspot).round(), size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, Modifiers, TouchPhase};

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
