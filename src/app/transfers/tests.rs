//! Unit tests for the pure transfer helpers.

use super::*;
use TransferPhase::{Done, Failed, Queued, Running};

fn transfer(id: u64, worker: Side) -> TransferState {
    let spec = NewTransfer {
        worker,
        label: "report.pdf".into(),
        dest: "web01:/srv".into(),
        upload: true,
        retry: None,
    };
    TransferState::new(id, spec, 10.0)
}

fn finished(t: &mut TransferState, at: f64, error: Option<&str>) {
    t.done = true;
    t.error = error.is_some();
    t.error_msg = error.map(str::to_string);
    t.finished_at = Some(at);
}

fn done_snapshot(bytes_total: u64, elapsed: f64, error: Option<&str>) -> Finished {
    Finished {
        label: "report.pdf".into(),
        upload: true,
        bytes_total,
        elapsed,
        error: error.map(str::to_string),
    }
}

// -- transfer_label ----------------------------------------------------

#[test]
fn transfer_label_single_path_is_its_base_name() {
    let paths = vec!["/srv/app/report.pdf".to_string()];
    assert_eq!(transfer_label(&paths), "report.pdf");
}

#[test]
fn transfer_label_tolerates_a_trailing_slash() {
    let paths = vec!["/srv/app/".to_string()];
    assert_eq!(transfer_label(&paths), "app");
}

#[test]
fn transfer_label_counts_several_paths_as_items() {
    let paths: Vec<String> = ["/a", "/b", "/c"].iter().map(|s| s.to_string()).collect();
    assert_eq!(transfer_label(&paths), "3 items");
    assert_eq!(transfer_label(&[]), "0 items");
}

// -- dest_label --------------------------------------------------------

#[test]
fn dest_label_remote_is_host_colon_dir() {
    assert_eq!(
        dest_label(Some("web01"), "/srv/app", None),
        "web01:/srv/app"
    );
}

#[test]
fn dest_label_local_collapses_home_to_tilde() {
    let label = dest_label(None, "/home/nicat/Downloads", Some("/home/nicat"));
    assert_eq!(label, "~/Downloads");
}

#[test]
fn dest_label_exactly_home_is_a_lone_tilde() {
    assert_eq!(dest_label(None, "/home/nicat/", Some("/home/nicat")), "~");
}

#[test]
fn dest_label_does_not_collapse_a_sibling_sharing_the_home_prefix() {
    let label = dest_label(None, "/home/nicatx", Some("/home/nicat"));
    assert_eq!(label, "/home/nicatx");
}

#[test]
fn dest_label_remote_collapses_its_own_home_too() {
    let label = dest_label(Some("web01"), "/home/deploy/app/", Some("/home/deploy/"));
    assert_eq!(label, "web01:~/app");
}

#[test]
fn dest_label_ignores_a_root_home() {
    assert_eq!(dest_label(None, "/srv", Some("/")), "/srv");
    assert_eq!(dest_label(None, "/", Some("/")), "/");
}

// -- phase_of / phases_of ---------------------------------------------

#[test]
fn phase_of_failed_wins_over_done() {
    let mut t = transfer(1, Side::Right);
    finished(&mut t, 12.0, Some("boom"));
    assert_eq!(phase_of(&t, true), Failed);
}

#[test]
fn phase_of_done_when_finished_cleanly() {
    let mut t = transfer(1, Side::Right);
    finished(&mut t, 12.0, None);
    assert_eq!(phase_of(&t, true), Done);
}

#[test]
fn phase_of_queued_only_before_any_progress_behind_an_older_job() {
    let t = transfer(1, Side::Right);
    assert_eq!(phase_of(&t, true), Queued);
    assert_eq!(phase_of(&t, false), Running);
}

#[test]
fn phase_of_running_once_bytes_or_files_moved_even_if_blocked() {
    let mut t = transfer(1, Side::Right);
    t.bytes_done = 1;
    assert_eq!(phase_of(&t, true), Running);
    let mut t = transfer(2, Side::Right);
    t.file_index = 1;
    assert_eq!(phase_of(&t, true), Running);
}

#[test]
fn phases_of_queues_behind_an_unfinished_job_on_the_same_worker_only() {
    // Arrange: two jobs on the right worker, one on the left.
    let transfers = vec![
        transfer(1, Side::Right),
        transfer(2, Side::Right),
        transfer(3, Side::Left),
    ];
    // Act
    let phases = phases_of(&transfers);
    // Assert
    assert_eq!(phases, vec![Running, Queued, Running]);
}

#[test]
fn phases_of_does_not_queue_behind_a_finished_job() {
    let mut first = transfer(1, Side::Right);
    finished(&mut first, 11.0, None);
    let mut failed = transfer(2, Side::Right);
    finished(&mut failed, 11.5, Some("x"));
    let transfers = vec![first, failed, transfer(3, Side::Right)];
    assert_eq!(phases_of(&transfers), vec![Done, Failed, Running]);
}

// -- rate_of -----------------------------------------------------------

#[test]
fn rate_of_is_none_before_half_a_second() {
    assert_eq!(rate_of(4096, 10.0, 10.4), None);
}

#[test]
fn rate_of_is_none_with_no_bytes() {
    assert_eq!(rate_of(0, 10.0, 20.0), None);
}

#[test]
fn rate_of_divides_bytes_by_elapsed_seconds() {
    assert_eq!(rate_of(4096, 10.0, 12.0), Some(2048.0));
}

// -- status_text -------------------------------------------------------

#[test]
fn status_text_running_uses_progress_text_with_a_one_based_index() {
    // Arrange: no progress event yet (index 0) and too early for a rate.
    let mut t = transfer(1, Side::Right);
    t.file_count = 1;
    t.bytes_done = 1024;
    t.bytes_total = 2048;
    // Act / Assert
    assert_eq!(status_text(&t, Running, 10.2), "1.0 KB / 2.0 KB");
}

#[test]
fn status_text_running_includes_the_rate_once_measurable() {
    let mut t = transfer(1, Side::Right);
    t.file_index = 2;
    t.file_count = 5;
    t.bytes_done = 2048;
    assert_eq!(
        status_text(&t, Running, 12.0),
        "2 / 5 files · 2.0 KB · 1.0 KB/s"
    );
}

#[test]
fn status_text_queued_says_so() {
    assert_eq!(
        status_text(&transfer(1, Side::Right), Queued, 10.0),
        "queued"
    );
}

#[test]
fn status_text_done_reports_elapsed_seconds() {
    let mut t = transfer(1, Side::Right);
    finished(&mut t, 12.34, None);
    assert_eq!(status_text(&t, Done, 99.0), "done · 2.3 s");
}

#[test]
fn status_text_failed_shortens_the_error_or_falls_back() {
    let mut t = transfer(1, Side::Right);
    finished(&mut t, 12.0, Some("Error: boom\nsecond line"));
    assert_eq!(status_text(&t, Failed, 12.0), "boom");
    let mut t = transfer(2, Side::Right);
    finished(&mut t, 12.0, None);
    t.error = true;
    assert_eq!(status_text(&t, Failed, 12.0), "failed");
}

// -- frac_of -----------------------------------------------------------

#[test]
fn frac_of_is_full_when_done_or_failed() {
    let mut t = transfer(1, Side::Right);
    finished(&mut t, 12.0, None);
    assert_eq!(frac_of(&t), 1.0);
    let mut t = transfer(2, Side::Right);
    finished(&mut t, 12.0, Some("x"));
    assert_eq!(frac_of(&t), 1.0);
}

#[test]
fn frac_of_is_the_byte_ratio_when_the_total_is_known_else_zero() {
    let mut t = transfer(1, Side::Right);
    t.bytes_done = 25;
    t.bytes_total = 100;
    assert_eq!(frac_of(&t), 0.25);
    t.bytes_total = 0;
    assert_eq!(frac_of(&t), 0.0);
}

// -- should_prune ------------------------------------------------------

#[test]
fn should_prune_done_rows_once_they_have_lingered() {
    let mut t = transfer(1, Side::Right);
    finished(&mut t, 10.0, None);
    assert!(!should_prune(&t, 12.9));
    assert!(should_prune(&t, 13.0));
}

#[test]
fn should_prune_never_removes_failures_or_running_rows() {
    let mut failed = transfer(1, Side::Right);
    finished(&mut failed, 10.0, Some("x"));
    assert!(!should_prune(&failed, 100.0));
    assert!(!should_prune(&transfer(2, Side::Right), 100.0));
}

// -- count_phases / summary_parts -------------------------------------

#[test]
fn count_phases_tallies_each_phase_and_the_unfinished_total() {
    let counts = count_phases(&[Running, Queued, Queued, Done, Failed]);
    let expected = PhaseCounts {
        running: 1,
        queued: 2,
        failed: 1,
        active: 3,
    };
    assert_eq!(counts, expected);
}

#[test]
fn summary_parts_omits_zero_counts_and_flags_the_failed_part() {
    let counts = PhaseCounts {
        running: 2,
        queued: 0,
        failed: 1,
        active: 2,
    };
    let parts = summary_parts(counts);
    assert_eq!(
        parts,
        vec![
            ("2 running".to_string(), false),
            ("1 failed".to_string(), true)
        ]
    );
}

#[test]
fn summary_parts_is_empty_when_nothing_is_happening() {
    assert!(summary_parts(PhaseCounts::default()).is_empty());
}

// -- error capture -----------------------------------------------------

#[test]
fn is_transfer_context_matches_only_upload_and_download() {
    assert!(is_transfer_context("download"));
    assert!(is_transfer_context("upload"));
    assert!(!is_transfer_context("list /srv"));
    assert!(!is_transfer_context("connect"));
}

#[test]
fn attach_error_keeps_a_recent_message_and_drops_an_old_one() {
    let recent = Some((10.0, "boom".to_string()));
    assert_eq!(attach_error(recent, 11.9), Some("boom".to_string()));
    let old = Some((10.0, "boom".to_string()));
    assert_eq!(attach_error(old, 12.1), None);
    assert_eq!(attach_error(None, 12.0), None);
}

// -- result_toast ------------------------------------------------------

#[test]
fn result_toast_success_names_the_verb_size_and_time() {
    let done = done_snapshot(2048, 1.26, None);
    assert_eq!(
        result_toast(Verb::Upload, &done),
        (
            "Uploaded report.pdf".to_string(),
            Some("2.0 KB · 1.3 s".to_string())
        )
    );
    assert_eq!(
        result_toast(Verb::Download, &done).0,
        "Downloaded report.pdf"
    );
    assert_eq!(result_toast(Verb::Copy, &done).0, "Copied report.pdf");
}

#[test]
fn result_toast_success_without_a_size_shows_only_the_time() {
    let done = done_snapshot(0, 0.4, None);
    assert_eq!(result_toast(Verb::Copy, &done).1, Some("0.4 s".to_string()));
}

#[test]
fn result_toast_failure_shortens_the_error_and_has_no_detail() {
    let done = done_snapshot(2048, 1.0, Some("Error: permission denied\nmore"));
    assert_eq!(
        result_toast(Verb::Upload, &done),
        ("Upload failed: permission denied".to_string(), None)
    );
    assert_eq!(
        result_toast(Verb::Copy, &done).0,
        "Copy failed: permission denied"
    );
}

// -- retry_allowed -----------------------------------------------------

#[test]
fn retry_allowed_when_both_sides_still_have_the_same_hosts() {
    // Arrange
    let hosts = [Some(Uuid::new_v4()), Some(Uuid::new_v4())];
    // Act / Assert
    assert!(retry_allowed(hosts, hosts));
    assert!(retry_allowed([None, hosts[1]], [None, hosts[1]]));
}

#[test]
fn retry_refused_when_the_right_side_changed_host() {
    let (left, right) = (Some(Uuid::new_v4()), Some(Uuid::new_v4()));
    assert!(!retry_allowed([left, right], [left, Some(Uuid::new_v4())]));
    assert!(!retry_allowed([left, right], [left, None]));
}

#[test]
fn retry_refused_when_the_left_side_changed_host() {
    let right = Some(Uuid::new_v4());
    assert!(!retry_allowed([None, right], [Some(Uuid::new_v4()), right]));
    assert!(!retry_allowed([Some(Uuid::new_v4()), right], [None, right]));
}
