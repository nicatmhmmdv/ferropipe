//! Unit tests for the pure widget helpers.

use super::*;
use crate::model::ConnectionKind;
use egui::pos2;

// -- chip_label -------------------------------------------------------

#[test]
fn chip_label_maps_every_connection_kind() {
    // Arrange
    let cases = [
        (ConnectionKind::Ssh, "SSH"),
        (ConnectionKind::Smb, "SMB"),
        (ConnectionKind::WinRm, "WinRM"),
        (ConnectionKind::Rdp, "RDP"),
        (ConnectionKind::RdpNative, "RDP·N"),
    ];
    for (kind, expected) in cases {
        // Act
        let label = chip_label(kind);
        // Assert
        assert_eq!(label, expected, "{kind:?}");
    }
}

// -- breadcrumb_segments ----------------------------------------------

fn pairs(crumbs: &[Crumb]) -> Vec<(&str, &str)> {
    crumbs
        .iter()
        .map(|c| (c.label.as_str(), c.path.as_str()))
        .collect()
}

#[test]
fn breadcrumb_root_is_a_single_slash_crumb() {
    // Arrange / Act
    let crumbs = breadcrumb_segments("/", None);
    // Assert
    assert_eq!(pairs(&crumbs), vec![("/", "/")]);
}

#[test]
fn breadcrumb_empty_path_is_treated_as_root() {
    let crumbs = breadcrumb_segments("", None);
    assert_eq!(pairs(&crumbs), vec![("/", "/")]);
}

#[test]
fn breadcrumb_absolute_path_yields_cumulative_paths() {
    let crumbs = breadcrumb_segments("/srv/app", None);
    assert_eq!(
        pairs(&crumbs),
        vec![("/", "/"), ("srv", "/srv"), ("app", "/srv/app")]
    );
}

#[test]
fn breadcrumb_collapses_home_to_tilde() {
    let crumbs = breadcrumb_segments("/home/nicat/Projects", Some("/home/nicat"));
    assert_eq!(
        pairs(&crumbs),
        vec![("~", "/home/nicat"), ("Projects", "/home/nicat/Projects")]
    );
}

#[test]
fn breadcrumb_exactly_home_is_a_single_tilde() {
    let crumbs = breadcrumb_segments("/home/nicat", Some("/home/nicat"));
    assert_eq!(pairs(&crumbs), vec![("~", "/home/nicat")]);
}

#[test]
fn breadcrumb_does_not_collapse_a_sibling_that_merely_shares_the_home_prefix() {
    let crumbs = breadcrumb_segments("/home/nicatx", Some("/home/nicat"));
    assert_eq!(
        pairs(&crumbs),
        vec![("/", "/"), ("home", "/home"), ("nicatx", "/home/nicatx")]
    );
}

#[test]
fn breadcrumb_tolerates_trailing_slash() {
    let with = breadcrumb_segments("/srv/app/", None);
    let without = breadcrumb_segments("/srv/app", None);
    assert_eq!(pairs(&with), pairs(&without));
}

#[test]
fn breadcrumb_tolerates_trailing_slash_on_home() {
    let crumbs = breadcrumb_segments("/home/nicat/Projects", Some("/home/nicat/"));
    assert_eq!(
        pairs(&crumbs),
        vec![("~", "/home/nicat"), ("Projects", "/home/nicat/Projects")]
    );
}

#[test]
fn breadcrumb_relative_share_path_has_no_root_crumb() {
    let crumbs = breadcrumb_segments("share/dir", None);
    assert_eq!(
        pairs(&crumbs),
        vec![("share", "share"), ("dir", "share/dir")]
    );
}

#[test]
fn breadcrumb_ignores_root_home_so_nothing_collapses_to_tilde() {
    let crumbs = breadcrumb_segments("/srv", Some("/"));
    assert_eq!(pairs(&crumbs), vec![("/", "/"), ("srv", "/srv")]);
}

// -- format_rate -------------------------------------------------------

#[test]
fn format_rate_zero_is_zero_bytes_per_second() {
    assert_eq!(format_rate(0.0), "0 B/s");
}

#[test]
fn format_rate_below_a_kilobyte_is_whole_bytes() {
    assert_eq!(format_rate(512.0), "512 B/s");
}

#[test]
fn format_rate_uses_one_decimal_above_a_kilobyte() {
    assert_eq!(format_rate(1_536.0), "1.5 KB/s");
}

#[test]
fn format_rate_mirrors_human_size_binary_units() {
    // 9.8 MiB/s expressed in bytes, so the text matches the size column.
    assert_eq!(format_rate(10_276_045.0), "9.8 MB/s");
    // 9_800_000 raw bytes is 9.3 MiB under the same scheme as human_size.
    assert_eq!(
        format_rate(9_800_000.0),
        crate::localfs::human_size(9_800_000) + "/s"
    );
}

#[test]
fn format_rate_treats_negative_and_nan_as_zero() {
    assert_eq!(format_rate(-5.0), "0 B/s");
    assert_eq!(format_rate(f64::NAN), "0 B/s");
}

// -- progress_text -----------------------------------------------------

const MIB_21_4: u64 = 22_439_526; // human_size -> "21.4 MB"
const MIB_50: u64 = 52_428_800; // human_size -> "50.0 MB"
const RATE_9_8: f64 = 10_276_045.0; // format_rate -> "9.8 MB/s"

#[test]
fn progress_text_multi_file_shows_count_bytes_and_rate() {
    let text = progress_text(38, 112, MIB_21_4, MIB_50, Some(RATE_9_8));
    assert_eq!(text, "38 / 112 files · 21.4 MB · 9.8 MB/s");
}

#[test]
fn progress_text_single_file_with_known_total_shows_done_of_total() {
    let text = progress_text(1, 1, MIB_21_4, MIB_50, Some(RATE_9_8));
    assert_eq!(text, "21.4 MB / 50.0 MB · 9.8 MB/s");
}

#[test]
fn progress_text_single_file_with_unknown_total_shows_only_done() {
    let text = progress_text(1, 1, MIB_21_4, 0, Some(RATE_9_8));
    assert_eq!(text, "21.4 MB · 9.8 MB/s");
}

#[test]
fn progress_text_omits_rate_when_none() {
    assert_eq!(
        progress_text(1, 1, MIB_21_4, MIB_50, None),
        "21.4 MB / 50.0 MB"
    );
    assert_eq!(
        progress_text(3, 9, MIB_21_4, 0, None),
        "3 / 9 files · 21.4 MB"
    );
}

// -- short_error -------------------------------------------------------

#[test]
fn short_error_keeps_only_the_first_line_trimmed() {
    assert_eq!(short_error("  boom  \nsecond line"), "boom");
}

#[test]
fn short_error_strips_leading_error_prefix() {
    assert_eq!(short_error("Error: boom"), "boom");
    assert_eq!(short_error("error: boom"), "boom");
}

#[test]
fn short_error_caps_long_messages_with_ellipsis() {
    // Arrange: 60 ascii chars
    let long = "x".repeat(60);
    // Act
    let out = short_error(&long);
    // Assert
    assert_eq!(out.chars().count(), 48);
    assert!(out.ends_with('…'));
    assert!(out.starts_with(&"x".repeat(47)));
}

#[test]
fn short_error_does_not_cut_a_message_of_exactly_the_cap() {
    let exact = "y".repeat(48);
    assert_eq!(short_error(&exact), exact);
}

#[test]
fn short_error_counts_chars_not_bytes() {
    let long = "é".repeat(60);
    let out = short_error(&long);
    assert_eq!(out.chars().count(), 48);
    assert!(out.ends_with('…'));
}

#[test]
fn short_error_empty_or_blank_becomes_failed() {
    assert_eq!(short_error(""), "failed");
    assert_eq!(short_error("   \n"), "failed");
    assert_eq!(short_error("Error: "), "failed");
}

// -- hexagon_points ----------------------------------------------------

#[test]
fn hexagon_points_has_six_points_on_the_circle() {
    // Arrange
    let center = pos2(10.0, 20.0);
    let r = 19.0;
    // Act
    let pts = hexagon_points(center, r);
    // Assert
    assert_eq!(pts.len(), 6);
    for p in &pts {
        let d = (p.x - center.x).hypot(p.y - center.y);
        assert!((d - r).abs() < 1e-3, "point {p:?} at distance {d}");
    }
}

#[test]
fn hexagon_points_first_point_is_straight_up() {
    let center = pos2(10.0, 20.0);
    let pts = hexagon_points(center, 19.0);
    assert!((pts[0].x - center.x).abs() < 1e-3);
    assert!((pts[0].y - (center.y - 19.0)).abs() < 1e-3);
}

// -- pulse_alpha -------------------------------------------------------

#[test]
fn pulse_alpha_stays_within_the_configured_band() {
    for i in 0..200 {
        let t = i as f64 * 0.037;
        let a = pulse_alpha(t);
        assert!((0.35..=1.0).contains(&a), "t={t} a={a}");
    }
}
