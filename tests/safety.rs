//! Safety tests: every way the data could change or be misdesignated between the scan and the
//! deletion must leave the source file in place and the destination untouched.

mod common;

use std::fs;
use std::path::Path;

use common::{Sandbox, apply_request, overwrite_keeping_mtime, snapshot, test_config};
use fs_spacemaker::apply::{ActionStatus, run_apply};
use fs_spacemaker::matching::MatchMode;
use fs_spacemaker::scan::{ScanRequest, run_scan};

/// Builds a scan request between two arbitrary folders.
fn request(source: &Path, destination: &Path, work: &Path) -> ScanRequest {
    ScanRequest {
        source: source.to_path_buf(),
        destination: destination.to_path_buf(),
        mode: MatchMode::Content,
        config: test_config(),
        work_dir: work.to_path_buf(),
        reports_dir: work.join("reports"),
    }
}

/// Returns a sandbox with one duplicated file, already scanned.
fn scanned_duplicate() -> (Sandbox, fs_spacemaker::scan::ScanOutput) {
    let sandbox = Sandbox::new();
    sandbox.write_source("dup.txt", b"duplicate content");
    sandbox.write_destination("dup.txt", b"duplicate content");
    let scan = sandbox.scan(MatchMode::Content);
    assert_eq!(scan.plan.body.actions.len(), 1);
    (sandbox, scan)
}

#[test]
fn refuses_identical_or_nested_roots() {
    let sandbox = Sandbox::new();
    let inner = sandbox.source.join("inner");
    fs::create_dir_all(&inner).unwrap();
    assert!(run_scan(&request(&sandbox.source, &sandbox.source, &sandbox.work)).is_err());
    assert!(run_scan(&request(&sandbox.source, &inner, &sandbox.work)).is_err());
    assert!(run_scan(&request(&inner, &sandbox.source, &sandbox.work)).is_err());
}

#[test]
fn refuses_missing_roots() {
    let sandbox = Sandbox::new();
    let missing = sandbox.source.join("missing");
    assert!(run_scan(&request(&missing, &sandbox.destination, &sandbox.work)).is_err());
}

#[test]
fn refuses_a_work_folder_inside_the_source() {
    let sandbox = Sandbox::new();
    let work = sandbox.source.join("work");
    assert!(run_scan(&request(&sandbox.source, &sandbox.destination, &work)).is_err());
}

#[test]
fn refuses_a_reports_folder_inside_the_source() {
    let sandbox = Sandbox::new();
    let mut scan_request = request(&sandbox.source, &sandbox.destination, &sandbox.work);
    scan_request.reports_dir = sandbox.source.join("reports");
    let error = run_scan(&scan_request).unwrap_err();
    assert!(format!("{error:#}").contains("overlaps the source"), "{error:#}");
}

#[test]
fn apply_requires_the_matching_confirmation_code() {
    let (sandbox, scan) = scanned_duplicate();
    let mut without = apply_request(&scan, false);
    without.confirmation = None;
    assert!(run_apply(&without).is_err());
    let mut wrong = apply_request(&scan, false);
    wrong.confirmation = Some("00000000".to_string());
    assert!(run_apply(&wrong).is_err());
    assert!(sandbox.source_path("dup.txt").exists());
}

#[test]
fn edited_plan_is_refused() {
    let (sandbox, scan) = scanned_duplicate();
    let text = fs::read_to_string(&scan.plan_path).unwrap();
    fs::write(&scan.plan_path, text.replace("\"size\": 17", "\"size\": 18")).unwrap();
    assert!(run_apply(&apply_request(&scan, false)).is_err());
    assert!(sandbox.source_path("dup.txt").exists());
}

#[test]
fn source_modified_after_scan_is_kept() {
    let (sandbox, scan) = scanned_duplicate();
    fs::write(sandbox.source_path("dup.txt"), b"changed after scan!").unwrap();
    let output = sandbox.apply(&scan);
    assert_eq!(output.results[0].status, ActionStatus::SourceChanged);
    assert!(sandbox.source_path("dup.txt").exists());
}

#[test]
fn destination_changed_with_same_size_and_date_is_detected_byte_by_byte() {
    let (sandbox, scan) = scanned_duplicate();
    overwrite_keeping_mtime(&sandbox.destination_path("dup.txt"), b"DUPLICATE CONTENT");
    let output = sandbox.apply(&scan);
    assert_eq!(output.results[0].status, ActionStatus::ContentDiffers);
    assert!(sandbox.source_path("dup.txt").exists());
}

#[test]
fn source_changed_with_same_size_and_date_is_kept() {
    let (sandbox, scan) = scanned_duplicate();
    overwrite_keeping_mtime(&sandbox.source_path("dup.txt"), b"DUPLICATE CONTENT");
    let output = sandbox.apply(&scan);
    assert_eq!(output.results[0].status, ActionStatus::ContentDiffers);
    assert!(sandbox.source_path("dup.txt").exists());
}

#[test]
fn both_sides_changed_identically_is_still_kept() {
    let (sandbox, scan) = scanned_duplicate();
    overwrite_keeping_mtime(&sandbox.source_path("dup.txt"), b"DUPLICATE CONTENT");
    overwrite_keeping_mtime(&sandbox.destination_path("dup.txt"), b"DUPLICATE CONTENT");
    let output = sandbox.apply(&scan);
    assert_eq!(output.results[0].status, ActionStatus::SourceChanged);
    assert!(sandbox.source_path("dup.txt").exists());
}

#[test]
fn destination_deleted_after_scan_keeps_the_source() {
    let (sandbox, scan) = scanned_duplicate();
    fs::remove_file(sandbox.destination_path("dup.txt")).unwrap();
    let output = sandbox.apply(&scan);
    assert_eq!(output.results[0].status, ActionStatus::DestinationMissing);
    assert!(sandbox.source_path("dup.txt").exists());
}

#[test]
fn destination_resized_after_scan_keeps_the_source() {
    let (sandbox, scan) = scanned_duplicate();
    fs::write(sandbox.destination_path("dup.txt"), b"short").unwrap();
    let output = sandbox.apply(&scan);
    assert_eq!(output.results[0].status, ActionStatus::DestinationChanged);
}

#[test]
fn source_already_gone_is_reported_not_failed() {
    let (sandbox, scan) = scanned_duplicate();
    fs::remove_file(sandbox.source_path("dup.txt")).unwrap();
    let output = sandbox.apply(&scan);
    assert_eq!(output.results[0].status, ActionStatus::SourceMissing);
    assert!(!output.has_failures());
}

#[test]
fn applying_twice_deletes_nothing_more() {
    let (sandbox, scan) = scanned_duplicate();
    assert_eq!(sandbox.apply(&scan).count(ActionStatus::Deleted), 1);
    assert_eq!(sandbox.apply(&scan).count(ActionStatus::SourceMissing), 1);
}

#[test]
fn hard_link_between_source_and_destination_is_never_deleted() {
    let sandbox = Sandbox::new();
    let destination_file = sandbox.write_destination("shared.txt", b"one physical file");
    fs::hard_link(&destination_file, sandbox.source_path("shared.txt")).unwrap();
    let scan = sandbox.scan(MatchMode::Content);
    assert!(scan.plan.body.actions.is_empty(), "same physical file must not be planned");
    assert!(sandbox.source_path("shared.txt").exists());
}

#[test]
fn read_only_files_are_protected_by_default() {
    let sandbox = Sandbox::new();
    let path = sandbox.write_source("ro.txt", b"read only");
    sandbox.write_destination("ro.txt", b"read only");
    set_read_only(&path, true);
    assert!(sandbox.scan(MatchMode::Content).plan.body.actions.is_empty());
    let scan = sandbox.scan_with(MatchMode::Content, |config| config.delete_read_only = true);
    let output = sandbox.apply(&scan);
    assert_eq!(output.count(ActionStatus::Deleted), 1);
    assert!(!path.exists());
}

#[test]
fn links_and_junctions_are_never_followed_nor_deleted() {
    let sandbox = Sandbox::new();
    sandbox.write_destination("inside/precious.txt", b"precious");
    let link = sandbox.source.join("link-to-destination");
    if !create_dir_link(&sandbox.destination.join("inside"), &link) {
        eprintln!("skipped: this system does not allow creating directory links");
        return;
    }
    let destination_before = snapshot(&sandbox.destination);
    let scan = sandbox.scan(MatchMode::Content);
    assert!(scan.plan.body.actions.is_empty());
    sandbox.apply(&scan);
    assert_eq!(snapshot(&sandbox.destination), destination_before);
    assert!(fs::symlink_metadata(&link).is_ok(), "the link itself is left in place");
}

#[test]
fn link_inside_destination_is_not_used_as_a_copy() {
    let sandbox = Sandbox::new();
    sandbox.write_source("data/file.txt", b"only real copy");
    if !create_dir_link(&sandbox.source.join("data"), &sandbox.destination.join("mirror")) {
        eprintln!("skipped: this system does not allow creating directory links");
        return;
    }
    let scan = sandbox.scan(MatchMode::Content);
    assert!(scan.plan.body.actions.is_empty(), "a link to the source is not a copy");
}

#[test]
fn unavailable_source_at_apply_time_is_an_error() {
    let (sandbox, scan) = scanned_duplicate();
    let moved = sandbox.source.with_file_name("source-moved");
    fs::rename(&sandbox.source, &moved).unwrap();
    assert!(run_apply(&apply_request(&scan, false)).is_err());
    assert!(moved.join("dup.txt").exists());
}

/// Sets or clears the read-only flag.
#[allow(clippy::permissions_set_readonly_false)]
fn set_read_only(path: &Path, is_read_only: bool) {
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_readonly(is_read_only);
    fs::set_permissions(path, permissions).unwrap();
}

/// Creates a directory junction (Windows) or symbolic link (Unix); false if not permitted.
fn create_dir_link(target: &Path, link: &Path) -> bool {
    #[cfg(windows)]
    {
        // mklink does not accept the verbatim (\\?\) prefix of canonical paths.
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(fs_spacemaker::paths::display_path(link))
            .arg(fs_spacemaker::paths::display_path(target))
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
}
