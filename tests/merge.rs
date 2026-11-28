//! Merge mode: same name and size anywhere in the destination means deleted, everything else is
//! moved into the destination without replacing anything, and the source ends completely empty.
//! The cross-volume test runs only when FS_SPACEMAKER_OTHER_VOLUME_DIR names a folder on another
//! drive than the system temporary folder (e.g. S:\dev\Legal\FS SpaceMaker\target).

mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use assert_cmd::Command;
use common::{Sandbox, apply_request, folders, overwrite_keeping_mtime, snapshot, test_config, write_file};
use fs_spacemaker::apply::{ActionStatus, ApplyOutput, run_apply};
use fs_spacemaker::matching::MatchMode;
use fs_spacemaker::scan::{ScanOutput, ScanRequest, run_scan};

/// Folder on another volume for the cross-volume test (unset: that test is skipped).
const OTHER_VOLUME_ENV: &str = "FS_SPACEMAKER_OTHER_VOLUME_DIR";
/// A modification time far in the past, easy to recognise after a move.
const OLD_MTIME_SECS: u64 = 1_000_000_000;

/// Returns (source, destination) of every planned move.
fn planned_moves(scan: &ScanOutput) -> Vec<(String, String)> {
    scan.plan
        .body
        .moves
        .iter()
        .map(|planned| (planned.source_rel.clone(), planned.destination_rel.clone()))
        .collect()
}

/// Returns the status tags of every move, in plan order.
fn move_statuses(output: &ApplyOutput) -> Vec<ActionStatus> {
    output.move_results.iter().map(|result| result.status).collect()
}

/// Returns true when the source root holds nothing at all.
fn is_empty_dir(path: &Path) -> bool {
    fs::read_dir(path).unwrap().next().is_none()
}

/// Sets a file's modification time.
fn set_mtime(path: &Path, modified: SystemTime) {
    fs::File::options().write(true).open(path).unwrap().set_modified(modified).unwrap();
}

#[test]
fn source_ends_empty_duplicates_deleted_and_the_rest_moved() {
    let sandbox = Sandbox::new();
    sandbox.write_source("photos/dup.jpg", b"same size");
    sandbox.write_source("photos/new.jpg", b"only in the source");
    sandbox.write_source("deep/a/b/c.txt", b"deep file");
    fs::create_dir_all(sandbox.source_path("already/empty")).unwrap();
    sandbox.write_destination("elsewhere/DUP.jpg", b"SAME SIZE");
    sandbox.write_destination("kept.txt", b"destination data");
    let destination_before = snapshot(&sandbox.destination);

    let scan = sandbox.scan(MatchMode::Merge);
    assert_eq!(scan.plan.body.actions.len(), 1);
    assert_eq!(scan.plan.body.actions[0].source_rel, "photos/dup.jpg");
    assert!(scan.plan.body.actions[0].blake3.is_empty(), "nothing is fingerprinted in merge mode");
    assert_eq!(
        planned_moves(&scan),
        vec![
            ("deep/a/b/c.txt".into(), "deep/a/b/c.txt".into()),
            ("photos/new.jpg".into(), "photos/new.jpg".into())
        ]
    );
    assert_eq!(scan.summary.residual_files, 0);
    assert_eq!(scan.source_stats.files_read + scan.destination_stats.files_read, 0, "no file is read");

    let output = sandbox.apply(&scan);
    assert_eq!(output.results[0].status, ActionStatus::Deleted);
    assert_eq!(move_statuses(&output), vec![ActionStatus::Moved, ActionStatus::Moved]);
    assert!(!output.has_failures());
    assert!(is_empty_dir(&sandbox.source), "source must be completely empty: {:?}", folders(&sandbox.source));

    let mut expected = destination_before;
    expected.insert("deep/a/b/c.txt".into(), b"deep file".to_vec());
    expected.insert("photos/new.jpg".into(), b"only in the source".to_vec());
    assert_eq!(snapshot(&sandbox.destination), expected, "destination only gains the moved files");
}

#[test]
fn same_name_and_size_is_deleted_even_when_the_content_differs() {
    let sandbox = Sandbox::new();
    sandbox.write_source("notes.txt", b"version A");
    sandbox.write_destination("archive/notes.txt", b"version B");
    let scan = sandbox.scan(MatchMode::Merge);
    assert_eq!(scan.plan.body.actions.len(), 1, "files are compared by name and size only");
    assert!(scan.plan.body.moves.is_empty());
    let output = sandbox.apply(&scan);
    assert_eq!(output.results[0].status, ActionStatus::Deleted);
    assert!(is_empty_dir(&sandbox.source));
    assert_eq!(fs::read(sandbox.destination_path("archive/notes.txt")).unwrap(), b"version B");
}

#[test]
fn same_name_other_size_is_moved_under_a_numbered_name() {
    let sandbox = Sandbox::new();
    sandbox.write_source("report.pdf", b"longer source");
    sandbox.write_destination("report.pdf", b"short");
    sandbox.write_destination("report (2).pdf", b"also taken");
    let scan = sandbox.scan(MatchMode::Merge);
    assert!(scan.plan.body.actions.is_empty());
    assert_eq!(planned_moves(&scan), vec![("report.pdf".into(), "report (3).pdf".into())]);
    sandbox.apply(&scan);
    assert!(is_empty_dir(&sandbox.source));
    assert_eq!(fs::read(sandbox.destination_path("report.pdf")).unwrap(), b"short", "never replaced");
    assert_eq!(fs::read(sandbox.destination_path("report (3).pdf")).unwrap(), b"longer source");
}

#[test]
fn same_size_other_name_is_moved_not_deleted() {
    let sandbox = Sandbox::new();
    sandbox.write_source("a.txt", b"identical");
    sandbox.write_destination("b.txt", b"identical");
    let scan = sandbox.scan(MatchMode::Merge);
    assert!(scan.plan.body.actions.is_empty(), "a different name is never a match");
    assert_eq!(planned_moves(&scan), vec![("a.txt".into(), "a.txt".into())]);
}

#[test]
fn a_source_folder_blocked_by_a_destination_file_is_renamed_whole() {
    let sandbox = Sandbox::new();
    sandbox.write_source("docs/one.txt", b"1");
    sandbox.write_source("docs/sub/two.txt", b"22");
    sandbox.write_destination("docs", b"a file named docs");
    let scan = sandbox.scan(MatchMode::Merge);
    let targets: Vec<String> = planned_moves(&scan).into_iter().map(|(_, target)| target).collect();
    assert_eq!(targets, vec!["docs (2)/one.txt", "docs (2)/sub/two.txt"]);
    sandbox.apply(&scan);
    assert!(is_empty_dir(&sandbox.source));
    assert_eq!(fs::read(sandbox.destination_path("docs")).unwrap(), b"a file named docs");
    assert_eq!(fs::read(sandbox.destination_path("docs (2)/sub/two.txt")).unwrap(), b"22");
}

#[test]
fn moved_files_keep_their_modification_time() {
    let sandbox = Sandbox::new();
    let path = sandbox.write_source("old.txt", b"old content");
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(OLD_MTIME_SECS);
    set_mtime(&path, old);
    let scan = sandbox.scan(MatchMode::Merge);
    sandbox.apply(&scan);
    let moved = fs::metadata(sandbox.destination_path("old.txt")).unwrap();
    assert_eq!(moved.modified().unwrap(), old);
}

#[test]
fn read_only_files_are_moved_rather_than_deleted() {
    let sandbox = Sandbox::new();
    let path = sandbox.write_source("locked.txt", b"read only");
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&path, permissions).unwrap();
    sandbox.write_destination("locked.txt", b"READ ONLY");
    let scan = sandbox.scan(MatchMode::Merge);
    assert!(scan.plan.body.actions.is_empty(), "read-only files are never deleted by default");
    assert_eq!(planned_moves(&scan), vec![("locked.txt".into(), "locked (2).txt".into())]);
    let output = sandbox.apply(&scan);
    assert_eq!(move_statuses(&output), vec![ActionStatus::Moved]);
    assert!(is_empty_dir(&sandbox.source));
    assert_eq!(fs::read(sandbox.destination_path("locked (2).txt")).unwrap(), b"read only");
}

#[test]
fn dry_run_changes_nothing() {
    let sandbox = Sandbox::new();
    sandbox.write_source("dup.txt", b"12345");
    sandbox.write_source("dir/new.txt", b"new");
    sandbox.write_destination("dup.txt", b"abcde");
    let (source_before, destination_before) = (snapshot(&sandbox.source), snapshot(&sandbox.destination));
    let scan = sandbox.scan(MatchMode::Merge);
    let output = sandbox.dry_run(&scan);
    assert_eq!(output.results[0].status, ActionStatus::WouldDelete);
    assert_eq!(move_statuses(&output), vec![ActionStatus::WouldMove]);
    assert_eq!(output.moved_totals(), (1, 3));
    assert_eq!(snapshot(&sandbox.source), source_before);
    assert_eq!(snapshot(&sandbox.destination), destination_before);
    assert_eq!(folders(&sandbox.source), vec!["dir"]);
}

#[test]
fn a_destination_path_taken_after_the_scan_is_never_replaced() {
    let sandbox = Sandbox::new();
    sandbox.write_source("file.txt", b"source data");
    let scan = sandbox.scan(MatchMode::Merge);
    sandbox.write_destination("file.txt", b"arrived later");
    let output = sandbox.apply(&scan);
    assert_eq!(move_statuses(&output), vec![ActionStatus::TargetOccupied]);
    assert_eq!(fs::read(sandbox.destination_path("file.txt")).unwrap(), b"arrived later");
    assert_eq!(fs::read(sandbox.source_path("file.txt")).unwrap(), b"source data", "source kept");
}

#[test]
fn a_source_changed_after_the_scan_is_neither_moved_nor_deleted() {
    let sandbox = Sandbox::new();
    sandbox.write_source("grown.txt", b"before");
    sandbox.write_source("same-size.txt", b"before");
    sandbox.write_destination("same-size.txt", b"BEFORE");
    let scan = sandbox.scan(MatchMode::Merge);
    fs::write(sandbox.source_path("grown.txt"), b"after the scan").unwrap();
    let path = sandbox.source_path("same-size.txt");
    overwrite_keeping_mtime(&path, b"AFTER!");
    set_mtime(&path, SystemTime::now() + Duration::from_secs(60));
    let output = sandbox.apply(&scan);
    assert_eq!(output.results[0].status, ActionStatus::SourceChanged);
    assert_eq!(move_statuses(&output), vec![ActionStatus::SourceChanged]);
    assert!(sandbox.source_path("grown.txt").exists());
    assert!(sandbox.source_path("same-size.txt").exists());
    assert!(!sandbox.destination_path("grown.txt").exists());
}

#[test]
fn empty_files_are_merged_too() {
    let sandbox = Sandbox::new();
    sandbox.write_source("empty-dup.txt", b"");
    sandbox.write_source("empty-new.txt", b"");
    sandbox.write_destination("x/empty-dup.txt", b"");
    let scan = sandbox.scan(MatchMode::Merge);
    assert_eq!(scan.plan.body.actions.len(), 1);
    assert_eq!(planned_moves(&scan), vec![("empty-new.txt".into(), "empty-new.txt".into())]);
    sandbox.apply(&scan);
    assert!(is_empty_dir(&sandbox.source));
}

#[test]
fn applying_twice_does_nothing_more() {
    let sandbox = Sandbox::new();
    sandbox.write_source("a.txt", b"a");
    let scan = sandbox.scan(MatchMode::Merge);
    sandbox.apply(&scan);
    let second = sandbox.apply(&scan);
    assert_eq!(move_statuses(&second), vec![ActionStatus::SourceMissing]);
    assert_eq!(fs::read(sandbox.destination_path("a.txt")).unwrap(), b"a");
}

#[test]
fn merge_through_the_executable() {
    let sandbox = Sandbox::new();
    sandbox.write_source("dup.bin", b"1234");
    sandbox.write_source("folder/new.bin", b"new");
    sandbox.write_destination("dup.bin", b"abcd");
    let program = env!("CARGO_BIN_EXE_fs-spacemaker");
    let scan = Command::new(program)
        .args(["scan", "--mode", "merge", "--threads", "2"])
        .arg("--source")
        .arg(&sandbox.source)
        .arg("--destination")
        .arg(&sandbox.destination)
        .arg("--work-dir")
        .arg(&sandbox.work)
        .arg("--reports-dir")
        .arg(&sandbox.reports)
        .assert()
        .success();
    let text = String::from_utf8(scan.get_output().stdout.clone()).unwrap();
    assert!(text.contains("will be moved:     1 files"), "{text}");
    let command_line = text.lines().rev().find(|line| line.contains("--confirm")).unwrap();
    let code = command_line.rsplit(' ').next().unwrap();
    Command::new(program)
        .args(["apply", "--confirm", code, "--plan"])
        .arg(sandbox.reports.join("plan.json"))
        .assert()
        .success();
    assert!(is_empty_dir(&sandbox.source));
    assert_eq!(fs::read(sandbox.destination_path("folder/new.bin")).unwrap(), b"new");
    let report = fs::read_to_string(sandbox.reports.join("apply-report.html")).unwrap();
    assert!(report.contains("Planned moves"));
}

#[test]
fn moves_between_volumes_copy_verify_then_delete() {
    let Some(other_volume) = std::env::var_os(OTHER_VOLUME_ENV) else {
        eprintln!("skipped: set {OTHER_VOLUME_ENV} to a folder on another drive to run this test");
        return;
    };
    let source_dir = tempfile::tempdir().unwrap();
    let destination_dir = tempfile::tempdir_in(&other_volume).unwrap();
    let source = source_dir.path().canonicalize().unwrap();
    let destination = destination_dir.path().canonicalize().unwrap();
    let reports = tempfile::tempdir().unwrap();
    let file = write_file(&source, "sub/big.bin", &vec![7u8; 3 * 1024 * 1024]);
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(OLD_MTIME_SECS);
    let mut stream = file.as_os_str().to_owned();
    stream.push(":notes");
    if cfg!(windows) {
        fs::write(&stream, b"hidden notes").unwrap();
    }
    // Last: writing a stream also updates the file's modification time.
    set_mtime(&file, old);
    let request = ScanRequest {
        source: source.clone(),
        destination: destination.clone(),
        mode: MatchMode::Merge,
        config: test_config(),
        work_dir: reports.path().join("work"),
        reports_dir: reports.path().join("reports"),
    };
    let scan = run_scan(&request).unwrap();
    let output = run_apply(&apply_request(&scan, false)).unwrap();
    assert_eq!(move_statuses(&output), vec![ActionStatus::Moved], "{:?}", output.move_results);
    assert!(is_empty_dir(&source), "source must be empty");
    let moved = destination.join("sub").join("big.bin");
    assert_eq!(fs::read(&moved).unwrap(), vec![7u8; 3 * 1024 * 1024]);
    assert_eq!(fs::metadata(&moved).unwrap().modified().unwrap(), old);
    if cfg!(windows) {
        let mut moved_stream = moved.clone().into_os_string();
        moved_stream.push(":notes");
        assert_eq!(fs::read(&moved_stream).unwrap(), b"hidden notes", "streams survive the copy");
    }
    let leftovers: Vec<_> = fs::read_dir(destination.join("sub")).unwrap().collect();
    assert_eq!(leftovers.len(), 1, "no temporary copy is left behind");
}

#[cfg(windows)]
#[test]
fn a_file_in_use_is_skipped_and_kept() {
    use std::os::windows::fs::OpenOptionsExt;
    const SHARE_READ_ONLY: u32 = 0x1;
    let sandbox = Sandbox::new();
    let path = sandbox.write_source("busy.txt", b"in use");
    let scan = sandbox.scan(MatchMode::Merge);
    let holder = fs::File::options().read(true).share_mode(SHARE_READ_ONLY).open(&path).unwrap();
    let output = sandbox.apply(&scan);
    drop(holder);
    assert_eq!(move_statuses(&output), vec![ActionStatus::InUse]);
    assert_eq!(fs::read(&path).unwrap(), b"in use");
    assert!(!sandbox.destination_path("busy.txt").exists());
}

#[cfg(windows)]
#[test]
fn hidden_streams_travel_with_a_moved_file() {
    let sandbox = Sandbox::new();
    let path = sandbox.write_source("tagged.txt", b"main data");
    let mut stream = path.as_os_str().to_owned();
    stream.push(":notes");
    fs::write(&stream, b"hidden notes").unwrap();
    let scan = sandbox.scan(MatchMode::Merge);
    sandbox.apply(&scan);
    let mut moved_stream = sandbox.destination_path("tagged.txt").into_os_string();
    moved_stream.push(":notes");
    assert_eq!(fs::read(&moved_stream).unwrap(), b"hidden notes");
}
