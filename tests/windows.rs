//! Windows-specific tests: files in use, NTFS alternate data streams, hidden/system attributes,
//! names only reachable through \\?\ paths, file symbolic links, and the same storage seen
//! through another path (SUBST drive, \\localhost\C$ administrative share, junction).

#![cfg(windows)]

mod common;

use std::fs::{self, File};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{Sandbox, apply_request, snapshot, test_config};
use fs_spacemaker::apply::{ActionStatus, run_apply};
use fs_spacemaker::matching::MatchMode;
use fs_spacemaker::paths::display_path;
use fs_spacemaker::scan::{ScanOutput, ScanRequest, run_scan};

/// Windows share modes (see CreateFileW).
const SHARE_NONE: u32 = 0;
const SHARE_READ: u32 = 1;
const SHARE_WRITE: u32 = 2;
const SHARE_DELETE: u32 = 4;
/// Candidate letters for temporary SUBST drives, tried from the end of the alphabet.
const SUBST_LETTERS: &str = "ZYXWVUTSRQP";

// ----------------------------------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------------------------------

/// Returns a sandbox with one duplicated file, already scanned.
fn scanned_duplicate() -> (Sandbox, ScanOutput) {
    let sandbox = Sandbox::new();
    sandbox.write_source("dup.txt", b"duplicate content");
    sandbox.write_destination("dup.txt", b"duplicate content");
    let scan = sandbox.scan(MatchMode::Content);
    assert_eq!(scan.plan.body.actions.len(), 1);
    (sandbox, scan)
}

/// Opens a file the way another program could, with the given access and sharing.
fn hold(path: &Path, is_writer: bool, share_mode: u32) -> File {
    File::options().read(true).write(is_writer).share_mode(share_mode).open(path).unwrap()
}

/// Runs a Windows command and returns whether it succeeded.
fn run_command(program: &str, arguments: &[&str]) -> bool {
    Command::new(program).args(arguments).output().is_ok_and(|output| output.status.success())
}

/// Scans arbitrary roots with the test configuration.
fn scan_roots(source: &Path, destination: &Path, work: &Path) -> anyhow::Result<ScanOutput> {
    run_scan(&ScanRequest {
        source: source.to_path_buf(),
        destination: destination.to_path_buf(),
        mode: MatchMode::Content,
        config: test_config(),
        work_dir: work.to_path_buf(),
    })
}

/// A temporary SUBST drive, removed when dropped.
struct SubstDrive {
    root: PathBuf,
    letter: String,
}

impl SubstDrive {
    /// Maps a free drive letter to `target`; None if no letter is free or SUBST fails.
    /// Tests run in parallel, so a letter seen free may be taken first: try every free letter.
    fn create(target: &Path) -> Option<Self> {
        let target_text = display_path(target);
        SUBST_LETTERS
            .chars()
            .map(|c| format!("{c}:"))
            .filter(|letter| !Path::new(&format!("{letter}\\")).exists())
            .find(|letter| run_command("subst", &[letter, &target_text]))
            .map(|letter| Self { root: PathBuf::from(format!("{letter}\\")), letter })
    }
}

impl Drop for SubstDrive {
    fn drop(&mut self) {
        run_command("subst", &[&self.letter, "/D"]);
    }
}

/// Returns the \\localhost\X$ form of a local path, if the administrative share is reachable.
fn admin_share_path(path: &Path) -> Option<PathBuf> {
    let text = display_path(path);
    let (drive, rest) = text.split_once(":\\")?;
    let unc = PathBuf::from(format!(r"\\localhost\{drive}$\{rest}"));
    unc.exists().then_some(unc)
}

// ----------------------------------------------------------------------------------------------
// Files in use
// ----------------------------------------------------------------------------------------------

#[test]
fn exclusively_locked_files_are_reported_unreadable_by_scan() {
    let sandbox = Sandbox::new();
    let path = sandbox.write_source("locked.txt", b"locked content");
    sandbox.write_destination("copy.txt", b"locked content");
    let _lock = hold(&path, false, SHARE_NONE);
    let scan = sandbox.scan(MatchMode::Content);
    assert!(scan.plan.body.actions.is_empty());
    let csv = fs::read_to_string(&scan.csv_path).unwrap();
    assert!(csv.lines().any(|line| line.contains("locked.txt") && line.contains("unreadable")));
}

#[test]
fn source_open_for_writing_is_not_deleted() {
    let (sandbox, scan) = scanned_duplicate();
    let writer = hold(&sandbox.source_path("dup.txt"), true, SHARE_READ | SHARE_WRITE | SHARE_DELETE);
    let output = sandbox.apply(&scan);
    assert_eq!(output.results[0].status, ActionStatus::InUse, "{:?}", output.results[0]);
    assert!(!output.has_failures(), "in use is a safe skip, not a failure");
    drop(writer);
    assert!(sandbox.source_path("dup.txt").exists());
}

#[test]
fn source_held_without_delete_sharing_is_not_deleted() {
    let (sandbox, scan) = scanned_duplicate();
    let reader = hold(&sandbox.source_path("dup.txt"), false, SHARE_READ);
    let output = sandbox.apply(&scan);
    assert_eq!(output.results[0].status, ActionStatus::InUse, "{:?}", output.results[0]);
    drop(reader);
    assert!(sandbox.source_path("dup.txt").exists());
}

#[test]
fn locked_destination_keeps_the_source() {
    let (sandbox, scan) = scanned_duplicate();
    let lock = hold(&sandbox.destination_path("dup.txt"), false, SHARE_NONE);
    let output = sandbox.apply(&scan);
    assert_eq!(output.results[0].status, ActionStatus::InUse, "{:?}", output.results[0]);
    drop(lock);
    assert!(sandbox.source_path("dup.txt").exists());
}

// ----------------------------------------------------------------------------------------------
// NTFS alternate data streams
// ----------------------------------------------------------------------------------------------

/// Writes an alternate data stream on a file.
fn write_stream(path: &Path, stream: &str, content: &[u8]) {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(":{stream}"));
    fs::write(PathBuf::from(name), content).unwrap();
}

#[test]
fn hidden_stream_data_protects_the_source_at_scan_time() {
    let sandbox = Sandbox::new();
    let path = sandbox.write_source("doc.txt", b"visible");
    sandbox.write_destination("doc.txt", b"visible");
    write_stream(&path, "notes", b"data that exists only here");
    let scan = sandbox.scan(MatchMode::Content);
    assert!(scan.plan.body.actions.is_empty());
    let csv = fs::read_to_string(&scan.csv_path).unwrap();
    assert!(csv.lines().any(|line| line.contains("doc.txt") && line.contains("alternate-streams")));
}

#[test]
fn hidden_stream_added_after_scan_protects_the_source() {
    let (sandbox, scan) = scanned_duplicate();
    let path = sandbox.source_path("dup.txt");
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    write_stream(&path, "notes", b"late hidden data");
    File::options().write(true).open(&path).unwrap().set_modified(modified).unwrap();
    let output = sandbox.apply(&scan);
    assert_eq!(output.results[0].status, ActionStatus::AlternateStreams);
    assert!(path.exists());
}

#[test]
fn hidden_stream_copied_identically_allows_deletion() {
    let sandbox = Sandbox::new();
    let source = sandbox.write_source("photo.jpg", b"jpeg bytes");
    let destination = sandbox.write_destination("photo.jpg", b"jpeg bytes");
    write_stream(&source, "com.dropbox.attributes", b"sync metadata");
    write_stream(&destination, "com.dropbox.attributes", b"sync metadata");
    let scan = sandbox.scan(MatchMode::Content);
    assert_eq!(sandbox.apply(&scan).count(ActionStatus::Deleted), 1);
    assert!(!source.exists());
}

#[test]
fn hidden_stream_with_different_content_protects_the_source() {
    let sandbox = Sandbox::new();
    let source = sandbox.write_source("photo.jpg", b"jpeg bytes");
    let destination = sandbox.write_destination("photo.jpg", b"jpeg bytes");
    write_stream(&source, "notes", b"version A");
    write_stream(&destination, "notes", b"version B");
    assert!(sandbox.scan(MatchMode::Content).plan.body.actions.is_empty());
    assert!(source.exists());
}

#[test]
fn hidden_stream_changed_on_destination_after_scan_protects_the_source() {
    let sandbox = Sandbox::new();
    let source = sandbox.write_source("photo.jpg", b"jpeg bytes");
    let destination = sandbox.write_destination("photo.jpg", b"jpeg bytes");
    write_stream(&source, "notes", b"same notes");
    write_stream(&destination, "notes", b"same notes");
    let scan = sandbox.scan(MatchMode::Content);
    assert_eq!(scan.plan.body.actions.len(), 1);
    write_stream(&destination, "notes", b"edited notes");
    let output = sandbox.apply(&scan);
    assert_eq!(output.results[0].status, ActionStatus::AlternateStreams);
    assert!(source.exists());
}

#[test]
fn ignored_stream_names_allow_deletion_without_a_copied_stream() {
    let sandbox = Sandbox::new();
    let source = sandbox.write_source("photo.jpg", b"jpeg bytes");
    sandbox.write_destination("photo.jpg", b"jpeg bytes");
    write_stream(&source, "com.dropbox.attrs", b"sync metadata");
    assert!(sandbox.scan(MatchMode::Content).plan.body.actions.is_empty(), "protected by default");
    let scan = sandbox.scan_with(MatchMode::Content, |config| {
        config.ignored_stream_names = vec!["com.dropbox.*".to_string()];
    });
    assert_eq!(scan.plan.body.ignored_stream_names, vec!["com.dropbox.*"]);
    assert_eq!(sandbox.apply(&scan).count(ActionStatus::Deleted), 1);
    assert!(!source.exists());
}

#[test]
fn ignored_stream_names_do_not_cover_other_streams() {
    let sandbox = Sandbox::new();
    let source = sandbox.write_source("photo.jpg", b"jpeg bytes");
    sandbox.write_destination("photo.jpg", b"jpeg bytes");
    write_stream(&source, "com.dropbox.attrs", b"sync metadata");
    write_stream(&source, "notes", b"user notes");
    let scan = sandbox.scan_with(MatchMode::Content, |config| {
        config.ignored_stream_names = vec!["com.dropbox.*".to_string()];
    });
    assert!(scan.plan.body.actions.is_empty());
}

#[test]
fn download_origin_mark_does_not_block_deletion() {
    let sandbox = Sandbox::new();
    let path = sandbox.write_source("setup.exe", b"installer bytes");
    sandbox.write_destination("setup.exe", b"installer bytes");
    write_stream(&path, "Zone.Identifier", b"[ZoneTransfer]\r\nZoneId=3\r\n");
    let scan = sandbox.scan(MatchMode::Content);
    assert_eq!(sandbox.apply(&scan).count(ActionStatus::Deleted), 1);
}

// ----------------------------------------------------------------------------------------------
// Attributes and names
// ----------------------------------------------------------------------------------------------

#[test]
fn hidden_and_system_duplicates_are_deleted() {
    let sandbox = Sandbox::new();
    let path = sandbox.write_source("Thumbs.db", b"thumbnail cache");
    sandbox.write_destination("Thumbs.db", b"thumbnail cache");
    assert!(run_command("attrib", &["+h", "+s", &display_path(&path)]));
    let scan = sandbox.scan(MatchMode::Content);
    assert_eq!(sandbox.apply(&scan).count(ActionStatus::Deleted), 1);
    assert!(!path.exists());
}

#[test]
fn read_only_hidden_file_is_deleted_only_when_allowed() {
    let sandbox = Sandbox::new();
    let path = sandbox.write_source("desktop.ini", b"[.ShellClassInfo]");
    sandbox.write_destination("desktop.ini", b"[.ShellClassInfo]");
    assert!(run_command("attrib", &["+r", "+h", "+s", &display_path(&path)]));
    assert!(sandbox.scan(MatchMode::Content).plan.body.actions.is_empty());
    let scan = sandbox.scan_with(MatchMode::Content, |config| config.delete_read_only = true);
    assert_eq!(sandbox.apply(&scan).count(ActionStatus::Deleted), 1);
}

#[test]
fn names_reachable_only_through_verbatim_paths_are_handled() {
    let sandbox = Sandbox::new();
    let names = ["trailing-dot.", "trailing-space ", "CON", "nul.txt", "aux"];
    for (index, name) in names.iter().enumerate() {
        let content = format!("special name {index}");
        fs::write(sandbox.source.join(name), &content).unwrap();
        fs::write(sandbox.destination.join(format!("copy-{index}.txt")), &content).unwrap();
    }
    let scan = sandbox.scan(MatchMode::Content);
    assert_eq!(scan.plan.body.actions.len(), names.len());
    assert_eq!(sandbox.apply(&scan).count(ActionStatus::Deleted), names.len());
    for name in names {
        assert!(fs::symlink_metadata(sandbox.source.join(name)).is_err(), "{name:?} should be deleted");
    }
}

#[test]
fn file_symbolic_links_are_never_deleted() {
    let sandbox = Sandbox::new();
    let target = sandbox.write_destination("real.txt", b"real data");
    let link = sandbox.source.join("link.txt");
    if !run_command("cmd", &["/C", "mklink", &display_path(&link), &display_path(&target)]) {
        eprintln!("skipped: creating file symbolic links needs Developer Mode or administrator rights");
        return;
    }
    let scan = sandbox.scan(MatchMode::Content);
    assert!(scan.plan.body.actions.is_empty());
    assert!(fs::symlink_metadata(&link).is_ok());
    assert_eq!(fs::read(&target).unwrap(), b"real data");
}

// ----------------------------------------------------------------------------------------------
// The same storage seen through another path
// ----------------------------------------------------------------------------------------------

#[test]
fn subst_drive_alias_of_the_destination_is_refused() {
    let sandbox = Sandbox::new();
    sandbox.write_destination("precious.txt", b"precious");
    let Some(drive) = SubstDrive::create(&sandbox.destination) else {
        eprintln!("skipped: no free drive letter for SUBST");
        return;
    };
    let before = snapshot(&sandbox.destination);
    assert!(scan_roots(&drive.root, &sandbox.destination, &sandbox.work).is_err());
    assert!(scan_roots(&sandbox.destination, &drive.root, &sandbox.work).is_err());
    assert_eq!(snapshot(&sandbox.destination), before);
}

#[test]
fn subst_drive_alias_of_a_parent_folder_is_refused() {
    let sandbox = Sandbox::new();
    let parent = sandbox.source.parent().unwrap().to_path_buf();
    let Some(drive) = SubstDrive::create(&parent) else {
        eprintln!("skipped: no free drive letter for SUBST");
        return;
    };
    assert!(
        scan_roots(&drive.root, &sandbox.destination, &sandbox.work).is_err(),
        "source alias contains destination"
    );
}

#[test]
fn subst_drive_root_works_as_a_normal_source() {
    let sandbox = Sandbox::new();
    sandbox.write_source("dup.txt", b"via drive root");
    sandbox.write_destination("dup.txt", b"via drive root");
    let Some(drive) = SubstDrive::create(&sandbox.source) else {
        eprintln!("skipped: no free drive letter for SUBST");
        return;
    };
    let scan = scan_roots(&drive.root, &sandbox.destination, &sandbox.work).unwrap();
    assert_eq!(scan.plan.body.actions.len(), 1);
    let output = run_apply(&apply_request(&scan, false)).unwrap();
    assert_eq!(output.count(ActionStatus::Deleted), 1);
    assert!(!sandbox.source_path("dup.txt").exists());
}

#[test]
fn administrative_share_alias_of_the_destination_is_refused() {
    let sandbox = Sandbox::new();
    sandbox.write_destination("precious.txt", b"precious");
    let Some(unc) = admin_share_path(&sandbox.destination) else {
        eprintln!("skipped: \\\\localhost administrative share not reachable");
        return;
    };
    let before = snapshot(&sandbox.destination);
    assert!(scan_roots(&unc, &sandbox.destination, &sandbox.work).is_err());
    assert_eq!(snapshot(&sandbox.destination), before);
}

#[test]
fn administrative_share_works_as_a_normal_source() {
    let sandbox = Sandbox::new();
    sandbox.write_source("dup.txt", b"via network path");
    sandbox.write_destination("dup.txt", b"via network path");
    let Some(unc) = admin_share_path(&sandbox.source) else {
        eprintln!("skipped: \\\\localhost administrative share not reachable");
        return;
    };
    let scan = scan_roots(&unc, &sandbox.destination, &sandbox.work).unwrap();
    let output = run_apply(&apply_request(&scan, false)).unwrap();
    assert_eq!(output.count(ActionStatus::Deleted), 1);
    assert!(!sandbox.source_path("dup.txt").exists());
}

#[test]
fn junction_root_pointing_into_the_destination_is_refused() {
    let sandbox = Sandbox::new();
    sandbox.write_destination("inner/precious.txt", b"precious");
    let junction = sandbox.work.join("junction-source");
    let target = sandbox.destination.join("inner");
    assert!(run_command("cmd", &["/C", "mklink", "/J", &display_path(&junction), &display_path(&target)]));
    let other_work = sandbox.source.parent().unwrap().join("work2");
    assert!(scan_roots(&junction, &sandbox.destination, &other_work).is_err());
}
