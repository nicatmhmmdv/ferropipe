//! Reusable, state-free egui widgets for the Foundry UI.
//!
//! Every color comes from [`crate::theme::current`]; nothing here constructs a
//! color literal. Pure helpers (breadcrumbs, rate/progress text, error
//! shortening, hexagon geometry) live in this file and are unit-tested; the
//! painting functions are split by area into `host`, `path`, `transfer` and
//! `misc`, re-exported below, and each stays under fifty lines.

use std::hash::Hash;

use egui::{
    Color32, CursorIcon, FontFamily, FontId, Id, Label, Pos2, Response, RichText, Sense, Ui, pos2,
};

use crate::localfs::human_size;
use crate::model::ConnectionKind;

mod fkeys;
mod host;
mod misc;
mod path;
mod transfer;

pub use fkeys::{FKEY_BAR_H, FKeyAction, FKeyContext, fkey_action, fkey_bar, fkey_enabled};
pub use host::{
    HostStatus, color_stripe, host_selector, protocol_chip, row_slot, row_ui, status_dot,
};
pub use misc::{dashed_outline, empty_pane, icon_button, primary_button, section_label, toast};
pub use path::{PathAction, path_bar};
pub use transfer::{TransferAction, TransferPhase, TransferView, gutter_arrow, transfer_row};

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Longest `short_error` output, in chars, including the ellipsis.
const SHORT_ERROR_MAX_CHARS: usize = 48;
/// Minimum alpha of the "connecting" pulse.
const PULSE_MIN_ALPHA: f64 = 0.35;
/// Angular speed of the "connecting" pulse, in radians per second.
const PULSE_SPEED: f64 = 4.0;

/// Short protocol tag for a connection kind.
pub fn chip_label(kind: ConnectionKind) -> &'static str {
    match kind {
        ConnectionKind::Ssh => "SSH",
        ConnectionKind::Smb => "SMB",
        ConnectionKind::WinRm => "WinRM",
        ConnectionKind::Rdp => "RDP",
        ConnectionKind::RdpNative => "RDP·N",
    }
}

/// One clickable segment of a breadcrumb path bar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Crumb {
    /// What to draw ("/", "~", or a directory name).
    pub label: String,
    /// The full path to navigate to when clicked.
    pub path: String,
}

/// Split `path` into breadcrumbs with cumulative paths.
///
/// A leading `home` prefix collapses to a single `~` crumb. Absolute paths
/// start with a `/` crumb; relative ones (SMB share paths) do not. Trailing
/// slashes are ignored and an empty path is treated as the root.
pub fn breadcrumb_segments(path: &str, home: Option<&str>) -> Vec<Crumb> {
    let path = trim_trailing_slashes(path);
    let home = home.map(trim_trailing_slashes).filter(|h| !h.is_empty());
    let (mut crumbs, rest) = match home_remainder(path, home) {
        Some((home, rest)) => (
            vec![Crumb {
                label: "~".into(),
                path: home.to_string(),
            }],
            rest,
        ),
        None if path.starts_with('/') => (
            vec![Crumb {
                label: "/".into(),
                path: "/".into(),
            }],
            path,
        ),
        None => (Vec::new(), path),
    };
    for name in rest.split('/').filter(|s| !s.is_empty()) {
        let parent = crumbs.last().map(|c| c.path.as_str()).unwrap_or("");
        crumbs.push(Crumb {
            label: name.to_string(),
            path: join_segment(parent, name),
        });
    }
    if crumbs.is_empty() {
        crumbs.push(Crumb {
            label: "/".into(),
            path: "/".into(),
        });
    }
    crumbs
}

/// Strip trailing slashes, but keep a lone "/" intact.
fn trim_trailing_slashes(path: &str) -> &str {
    if path == "/" {
        path
    } else {
        path.trim_end_matches('/')
    }
}

/// If `path` is `home` or lives under it, return `(home, remainder)`.
fn home_remainder<'a>(path: &'a str, home: Option<&'a str>) -> Option<(&'a str, &'a str)> {
    let home = home?;
    let rest = path.strip_prefix(home)?;
    (rest.is_empty() || rest.starts_with('/')).then_some((home, rest))
}

/// Append one path segment, avoiding a doubled slash after "/" or "".
fn join_segment(parent: &str, name: &str) -> String {
    match parent {
        "" => name.to_string(),
        "/" => format!("/{name}"),
        _ => format!("{parent}/{name}"),
    }
}

/// Human-readable transfer rate using the same binary units as `human_size`.
pub fn format_rate(bytes_per_sec: f64) -> String {
    let bytes = if bytes_per_sec.is_finite() && bytes_per_sec > 0.0 {
        bytes_per_sec.round() as u64
    } else {
        0
    };
    format!("{}/s", human_size(bytes))
}

/// Compact progress summary for the transfer drawer.
///
/// Multi-file batches read "38 / 112 files · 21.4 MB · 9.8 MB/s"; a single
/// file reads "21.4 MB / 50.0 MB · 9.8 MB/s". A zero `bytes_total` means the
/// total is unknown and only `bytes_done` is shown. `file_index` is displayed
/// as given (the caller decides whether it is 1-based).
pub fn progress_text(
    file_index: usize,
    file_count: usize,
    bytes_done: u64,
    bytes_total: u64,
    rate: Option<f64>,
) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(3);
    if file_count > 1 {
        parts.push(format!("{file_index} / {file_count} files"));
        parts.push(human_size(bytes_done));
    } else if bytes_total > 0 {
        parts.push(format!(
            "{} / {}",
            human_size(bytes_done),
            human_size(bytes_total)
        ));
    } else {
        parts.push(human_size(bytes_done));
    }
    if let Some(rate) = rate {
        parts.push(format_rate(rate));
    }
    parts.join(" · ")
}

/// First line of an error, without an "Error:" prefix, capped at 48 chars.
pub fn short_error(msg: &str) -> String {
    let line = msg.lines().next().unwrap_or("").trim();
    let line = line
        .strip_prefix("Error:")
        .or_else(|| line.strip_prefix("error:"))
        .unwrap_or(line)
        .trim();
    if line.is_empty() {
        return "failed".into();
    }
    if line.chars().count() <= SHORT_ERROR_MAX_CHARS {
        return line.to_string();
    }
    let mut out: String = line.chars().take(SHORT_ERROR_MAX_CHARS - 1).collect();
    out.push('…');
    out
}

/// Six points of a regular hexagon of radius `r`, starting straight up.
pub fn hexagon_points(center: Pos2, r: f32) -> Vec<Pos2> {
    (0..6)
        .map(|i| {
            let angle = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::FRAC_PI_3;
            pos2(center.x + r * angle.cos(), center.y + r * angle.sin())
        })
        .collect()
}

/// Alpha for the "connecting" pulse at time `t` seconds, in `0.35..=1.0`.
fn pulse_alpha(t: f64) -> f64 {
    let wave = ((t * PULSE_SPEED).sin() + 1.0) * 0.5;
    PULSE_MIN_ALPHA + (1.0 - PULSE_MIN_ALPHA) * wave
}

// ---------------------------------------------------------------------------
// Small shared drawing helpers
// ---------------------------------------------------------------------------

/// A non-selectable label so clicks fall through to the enclosing widget.
fn plain_label(ui: &mut Ui, text: RichText) -> Response {
    ui.add(Label::new(text).selectable(false))
}

/// Monospace text at the given size and color.
fn mono(text: &str, size: f32, color: Color32) -> RichText {
    RichText::new(text)
        .font(FontId::new(size, FontFamily::Monospace))
        .color(color)
}

/// Derive a stable child id of `ui` from any hashable salt.
///
/// egui's `Id::with` also wants `Debug`; hashing first keeps the public
/// `impl Hash` contract.
fn salt_id(ui: &Ui, salt: impl Hash) -> Id {
    use std::hash::{DefaultHasher, Hasher};
    let mut hasher = DefaultHasher::new();
    salt.hash(&mut hasher);
    ui.id().with(hasher.finish())
}

/// A clickable, non-selectable label with a pointing-hand cursor.
fn link_label(ui: &mut Ui, text: RichText) -> Response {
    ui.add(Label::new(text).selectable(false).sense(Sense::click()))
        .on_hover_cursor(CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests;
