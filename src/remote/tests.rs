//! Unit tests for the worker's pure helpers and the local → local copy.

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
