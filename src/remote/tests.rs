//! Unit tests for the worker's pure helpers and the local → local copy.

use super::conflicts::{Existing, find_conflicts, refuse_mismatches, renamed_name};
use super::transfer::{TransferPlan, copy_target, part_path, plan_local_copy};
use super::*;

#[test]
fn parent_of_works() {
    assert_eq!(parent_of("/a/b/c"), Some("/a/b".into()));
    assert_eq!(parent_of("/a"), Some("/".into()));
    assert_eq!(parent_of("x"), None);
}

#[test]
fn safe_component_rejects_traversal() {
    assert!(safe_component("ok.txt"));
    assert!(!safe_component(".."));
    assert!(!safe_component("a/b"));
}

#[test]
fn basename_sanitizes() {
    assert_eq!(basename("/a/b/file.txt"), Some("file.txt".into()));
    assert_eq!(basename("/a/b/"), Some("b".into()));
}

// -- copy_target --------------------------------------------------------

#[test]
fn copy_target_lands_a_source_under_its_name_in_the_destination() {
    // Arrange
    let src = Path::new("/srv/app/report.pdf");
    let dest = Path::new("/home/u/out");
    // Act
    let target = copy_target(src, dest).unwrap();
    // Assert
    assert_eq!(target, PathBuf::from("/home/u/out/report.pdf"));
}

#[test]
fn copy_target_refuses_a_directory_into_itself_or_a_descendant() {
    assert!(copy_target(Path::new("/srv/app"), Path::new("/srv/app")).is_err());
    assert!(copy_target(Path::new("/srv/app"), Path::new("/srv/app/sub")).is_err());
}

#[test]
fn copy_target_refuses_a_copy_onto_its_own_path() {
    let err = copy_target(Path::new("/srv/app/report.pdf"), Path::new("/srv/app")).unwrap_err();
    assert!(err.to_string().contains("already there"), "{err}");
}

#[test]
fn copy_target_allows_a_sibling_sharing_the_name_prefix() {
    let target = copy_target(Path::new("/srv/app"), Path::new("/srv/app2")).unwrap();
    assert_eq!(target, PathBuf::from("/srv/app2/app"));
}

#[test]
fn copy_target_refuses_a_source_without_a_usable_name() {
    assert!(copy_target(Path::new("/"), Path::new("/tmp")).is_err());
}

// -- part_path ----------------------------------------------------------

#[test]
fn part_path_appends_the_marker_without_replacing_the_extension() {
    let part = part_path(Path::new("/out/archive.tar.gz"));
    assert_eq!(part, PathBuf::from("/out/archive.tar.gz.ferropipe-part"));
}

// -- plan_local_copy ----------------------------------------------------

fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

/// `src/f.txt`, `src/d/e/g.txt`, an empty `src/d/empty/` and an empty `dest/`.
fn copy_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    write(&src.join("f.txt"), b"hello");
    write(&src.join("d/e/g.txt"), b"nested");
    std::fs::create_dir_all(src.join("d/empty")).unwrap();
    let dest = tmp.path().join("dest");
    std::fs::create_dir(&dest).unwrap();
    (tmp, src, dest)
}

#[test]
fn plan_local_copy_lists_every_directory_and_file_under_the_destination() {
    // Arrange
    let (_tmp, src, dest) = copy_fixture();
    // Act
    let plan = plan_local_copy(&[src.join("f.txt"), src.join("d")], &dest).unwrap();
    // Assert
    let mut dirs = plan.dirs;
    dirs.sort();
    assert_eq!(
        dirs,
        vec![dest.join("d"), dest.join("d/e"), dest.join("d/empty")]
    );
    let mut files = plan.files;
    files.sort();
    assert_eq!(
        files,
        vec![
            (src.join("d/e/g.txt"), dest.join("d/e/g.txt"), 6),
            (src.join("f.txt"), dest.join("f.txt"), 5),
        ]
    );
}

#[test]
fn plan_local_copy_refuses_a_destination_inside_a_source() {
    let (_tmp, src, _dest) = copy_fixture();
    let err = plan_local_copy(std::slice::from_ref(&src), &src.join("d")).unwrap_err();
    assert!(err.to_string().contains("into itself"), "{err}");
}

#[test]
fn plan_local_copy_resolves_dot_dot_before_checking_containment() {
    let (_tmp, src, _dest) = copy_fixture();
    let inside = src.join("d/../d/e");
    assert!(plan_local_copy(std::slice::from_ref(&src), &inside).is_err());
}

#[cfg(unix)]
#[test]
fn plan_local_copy_keeps_a_symlinks_own_name() {
    let (tmp, src, dest) = copy_fixture();
    let link = tmp.path().join("link.txt");
    std::os::unix::fs::symlink(src.join("f.txt"), &link).unwrap();
    let plan = plan_local_copy(std::slice::from_ref(&link), &dest).unwrap();
    assert_eq!(plan.files, vec![(link, dest.join("link.txt"), 5)]);
}

#[cfg(unix)]
#[test]
fn plan_local_copy_refuses_a_linked_directory_into_what_it_points_at() {
    let (tmp, src, _dest) = copy_fixture();
    let link = tmp.path().join("srclink");
    std::os::unix::fs::symlink(&src, &link).unwrap();
    let err = plan_local_copy(&[link], &src.join("d")).unwrap_err();
    assert!(err.to_string().contains("into itself"), "{err}");
}

// -- CopyLocal through the worker ---------------------------------------

/// Run one command on a fresh, unconnected worker and collect its events.
fn run_idle_worker(cmd: Command) -> Vec<Event> {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let (ev_tx, ev_rx) = std::sync::mpsc::channel();
    let join = spawn(cmd_rx, ev_tx, || {});
    cmd_tx.send(cmd).unwrap();
    cmd_tx.send(Command::Shutdown).unwrap();
    join.join().unwrap();
    ev_rx.try_iter().collect()
}

fn last_progress(events: &[Event]) -> &TransferProgress {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Progress(p) => Some(p),
            _ => None,
        })
        .expect("a progress event")
}

#[test]
fn copy_local_copies_a_file_and_a_nested_directory_through_an_idle_worker() {
    // Arrange
    let (_tmp, src, dest) = copy_fixture();
    let cmd = Command::CopyLocal {
        id: 7,
        paths: vec![src.join("f.txt"), src.join("d")],
        dest_dir: dest.clone(),
        on_conflict: OnConflict::Ask,
    };
    // Act
    let events = run_idle_worker(cmd);
    // Assert: bytes and structure
    assert_eq!(std::fs::read(dest.join("f.txt")).unwrap(), b"hello");
    assert_eq!(std::fs::read(dest.join("d/e/g.txt")).unwrap(), b"nested");
    assert!(dest.join("d/empty").is_dir());
    assert!(!dest.join("f.txt.ferropipe-part").exists());
    // Assert: events end in success, with the totals reported
    assert!(!events.iter().any(|e| matches!(e, Event::Error { .. })));
    assert!(matches!(
        events.last(),
        Some(Event::TransferDone { id: 7, ok: true })
    ));
    let p = last_progress(&events);
    assert_eq!((p.bytes_done, p.bytes_total, p.file_count), (11, 11, 2));
}

#[test]
fn copy_local_refuses_a_directory_into_itself_and_reports_the_failure() {
    // Arrange
    let (_tmp, src, _dest) = copy_fixture();
    let cmd = Command::CopyLocal {
        id: 3,
        paths: vec![src.join("d")],
        dest_dir: src.join("d"),
        on_conflict: OnConflict::Ask,
    };
    // Act
    let events = run_idle_worker(cmd);
    // Assert: Error then TransferDone { ok: false }, nothing written
    assert!(matches!(
        &events[..],
        [Event::Error { context, .. }, Event::TransferDone { id: 3, ok: false }]
            if context == "copy"
    ));
    assert!(!src.join("d/d").exists());
}

#[cfg(unix)]
#[test]
fn copy_local_keeps_the_sources_permission_bits() {
    use std::os::unix::fs::PermissionsExt;
    let (_tmp, src, dest) = copy_fixture();
    let script = src.join("run.sh");
    write(&script, b"#!/bin/sh\n");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let events = run_idle_worker(Command::CopyLocal {
        id: 1,
        paths: vec![script],
        dest_dir: dest.clone(),
        on_conflict: OnConflict::Ask,
    });
    assert!(matches!(
        events.last(),
        Some(Event::TransferDone { id: 1, ok: true })
    ));
    let mode = std::fs::metadata(dest.join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
}

// -- renamed_name -------------------------------------------------------

#[test]
fn renamed_name_inserts_a_counter_before_the_extension() {
    // Arrange
    let taken = |_: &str| false;
    // Act
    let name = renamed_name("report.pdf", taken);
    // Assert
    assert_eq!(name, "report (1).pdf");
}

#[test]
fn renamed_name_skips_taken_candidates() {
    let taken = |n: &str| n == "report (1).pdf" || n == "report (2).pdf";
    assert_eq!(renamed_name("report.pdf", taken), "report (3).pdf");
}

#[test]
fn renamed_name_splits_at_the_last_dot_only() {
    assert_eq!(
        renamed_name("archive.tar.gz", |_| false),
        "archive.tar (1).gz"
    );
}

#[test]
fn renamed_name_without_an_extension_appends_the_counter() {
    assert_eq!(renamed_name("Makefile", |_| false), "Makefile (1)");
    // A leading dot is not an extension separator.
    assert_eq!(renamed_name(".bashrc", |_| false), ".bashrc (1)");
}

#[test]
fn renamed_name_increments_an_existing_counter() {
    assert_eq!(renamed_name("report (1).pdf", |_| false), "report (2).pdf");
    assert_eq!(renamed_name("notes (7)", |_| false), "notes (8)");
    // Only a " (N)" suffix counts as a counter.
    assert_eq!(renamed_name("a (b).txt", |_| false), "a (b) (1).txt");
}

// -- find_conflicts / refuse_mismatches ----------------------------------

/// A destination tree for the pre-pass: path → what is there.
fn tree(entries: &[(&str, Existing)]) -> impl Fn(&String) -> Existing {
    let map: std::collections::HashMap<String, Existing> =
        entries.iter().map(|(p, e)| (p.to_string(), *e)).collect();
    move |p: &String| map.get(p).copied().unwrap_or(Existing::Missing)
}

// Mirrors the `&dyn Fn(&String) -> String` the pre-pass takes for `D = String`.
#[allow(clippy::ptr_arg)]
fn rel(p: &String) -> String {
    p.trim_start_matches("/dest/").to_string()
}

fn plan(dirs: &[&str], files: &[(&str, u64)]) -> TransferPlan<(), String> {
    TransferPlan {
        dirs: dirs.iter().map(|d| d.to_string()).collect(),
        files: files.iter().map(|(f, s)| ((), f.to_string(), *s)).collect(),
    }
}

#[test]
fn find_conflicts_is_empty_when_nothing_exists_at_the_destinations() {
    // Arrange
    let plan = plan(&["/dest/d"], &[("/dest/f.txt", 5), ("/dest/d/g.txt", 6)]);
    let probe = tree(&[]);
    // Act
    let conflicts = find_conflicts(&plan, &rel, &probe);
    // Assert
    assert!(conflicts.is_empty());
}

#[test]
fn find_conflicts_reports_an_existing_file_with_both_sizes() {
    let plan = plan(&[], &[("/dest/f.txt", 5), ("/dest/d/g.txt", 6)]);
    let probe = tree(&[("/dest/d/g.txt", Existing::File { size: 1200 })]);
    let conflicts = find_conflicts(&plan, &rel, &probe);
    assert_eq!(
        conflicts,
        vec![Conflict {
            name: "d/g.txt".into(),
            existing_size: 1200,
            incoming_size: 6,
            kind_mismatch: false,
        }]
    );
}

#[test]
fn find_conflicts_merges_into_an_existing_directory_without_complaint() {
    let plan = plan(&["/dest/d", "/dest/d/e"], &[]);
    let probe = tree(&[("/dest/d", Existing::Dir), ("/dest/d/e", Existing::Dir)]);
    assert!(find_conflicts(&plan, &rel, &probe).is_empty());
}

#[test]
fn find_conflicts_flags_a_file_where_a_directory_should_go() {
    let plan = plan(&["/dest/d"], &[]);
    let probe = tree(&[("/dest/d", Existing::File { size: 9 })]);
    assert_eq!(
        find_conflicts(&plan, &rel, &probe),
        vec![Conflict {
            name: "d".into(),
            existing_size: 9,
            incoming_size: 0,
            kind_mismatch: true,
        }]
    );
}

#[test]
fn find_conflicts_flags_a_directory_where_a_file_should_go() {
    let plan = plan(&[], &[("/dest/f.txt", 5)]);
    let probe = tree(&[("/dest/f.txt", Existing::Dir)]);
    assert_eq!(
        find_conflicts(&plan, &rel, &probe),
        vec![Conflict {
            name: "f.txt".into(),
            existing_size: 0,
            incoming_size: 5,
            kind_mismatch: true,
        }]
    );
}

#[test]
fn refuse_mismatches_allows_same_kind_collisions() {
    let plan = plan(&["/dest/d"], &[("/dest/f.txt", 5)]);
    let probe = tree(&[
        ("/dest/d", Existing::Dir),
        ("/dest/f.txt", Existing::File { size: 1 }),
    ]);
    assert!(refuse_mismatches(&plan, &rel, &probe).is_ok());
}

#[test]
fn refuse_mismatches_names_what_is_in_the_way() {
    let plan = plan(&["/dest/d"], &[("/dest/f.txt", 5)]);
    let dir_in_the_way = tree(&[("/dest/f.txt", Existing::Dir)]);
    let err = refuse_mismatches(&plan, &rel, &dir_in_the_way).unwrap_err();
    assert_eq!(err.to_string(), "f.txt: a directory is in the way");
    let file_in_the_way = tree(&[("/dest/d", Existing::File { size: 2 })]);
    let err = refuse_mismatches(&plan, &rel, &file_in_the_way).unwrap_err();
    assert_eq!(err.to_string(), "d: a file is in the way");
}

// -- CopyLocal conflict policies through the worker ---------------------

/// The fixture plus an older `dest/f.txt` holding "old".
fn conflict_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let (tmp, src, dest) = copy_fixture();
    write(&dest.join("f.txt"), b"old");
    (tmp, src, dest)
}

fn copy_f(src: &Path, dest: &Path, on_conflict: OnConflict) -> Command {
    Command::CopyLocal {
        id: 9,
        paths: vec![src.join("f.txt")],
        dest_dir: dest.to_path_buf(),
        on_conflict,
    }
}

#[test]
fn copy_local_asks_before_overwriting_and_leaves_the_destination_alone() {
    // Arrange
    let (_tmp, src, dest) = conflict_fixture();
    // Act
    let events = run_idle_worker(copy_f(&src, &dest, OnConflict::Ask));
    // Assert: one Conflicts event, no TransferDone, bytes untouched
    assert!(
        matches!(
            &events[..],
            [Event::Conflicts { id: 9, conflicts }]
                if conflicts.len() == 1
                    && conflicts[0].name == "f.txt"
                    && conflicts[0].existing_size == 3
                    && conflicts[0].incoming_size == 5
                    && !conflicts[0].kind_mismatch
        ),
        "{events:?}"
    );
    assert_eq!(std::fs::read(dest.join("f.txt")).unwrap(), b"old");
    assert!(!dest.join("f.txt.ferropipe-part").exists());
}

#[test]
fn copy_local_skip_leaves_the_existing_file_and_still_finishes() {
    let (_tmp, src, dest) = conflict_fixture();
    let events = run_idle_worker(copy_f(&src, &dest, OnConflict::Skip));
    assert_eq!(std::fs::read(dest.join("f.txt")).unwrap(), b"old");
    assert!(matches!(
        events.last(),
        Some(Event::TransferDone { id: 9, ok: true })
    ));
    let p = last_progress(&events);
    assert_eq!((p.bytes_done, p.bytes_total), (5, 5));
}

#[test]
fn copy_local_overwrite_replaces_the_existing_file() {
    let (_tmp, src, dest) = conflict_fixture();
    let events = run_idle_worker(copy_f(&src, &dest, OnConflict::Overwrite));
    assert_eq!(std::fs::read(dest.join("f.txt")).unwrap(), b"hello");
    assert!(matches!(
        events.last(),
        Some(Event::TransferDone { id: 9, ok: true })
    ));
}

#[test]
fn copy_local_rename_keeps_both_files() {
    let (_tmp, src, dest) = conflict_fixture();
    let events = run_idle_worker(copy_f(&src, &dest, OnConflict::Rename));
    assert_eq!(std::fs::read(dest.join("f.txt")).unwrap(), b"old");
    assert_eq!(std::fs::read(dest.join("f (1).txt")).unwrap(), b"hello");
    assert!(matches!(
        events.last(),
        Some(Event::TransferDone { id: 9, ok: true })
    ));
}

#[test]
fn copy_local_overwrite_refuses_a_directory_in_the_way_before_writing() {
    let (_tmp, src, dest) = copy_fixture();
    std::fs::create_dir(dest.join("f.txt")).unwrap();
    let events = run_idle_worker(copy_f(&src, &dest, OnConflict::Overwrite));
    assert!(
        matches!(
            &events[..],
            [Event::Error { context, message }, Event::TransferDone { id: 9, ok: false }]
                if context == "copy" && message.contains("f.txt: a directory is in the way")
        ),
        "{events:?}"
    );
    assert!(dest.join("f.txt").is_dir());
}
