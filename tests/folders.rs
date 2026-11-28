//! Two folders of the same drive (S:\Fin into S:\Finance), version-control metadata (.svn, .git),
//! trash folders and fingerprint reuse between a folder scan and a scan of its parent.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{Sandbox, folders, snapshot, test_config, write_file};
use fs_spacemaker::apply::{MetadataStatus, run_apply};
use fs_spacemaker::config::Config;
use fs_spacemaker::matching::MatchMode;
use fs_spacemaker::scan::{ScanOutput, ScanRequest, run_scan};
use rusqlite::Connection;

/// Minimal Subversion 1.7+ working copy database: the tables the report reads.
const WC_DB_SQL: &str = "
    CREATE TABLE REPOSITORY (id INTEGER PRIMARY KEY, root TEXT, uuid TEXT);
    CREATE TABLE NODES (local_relpath TEXT, op_depth INTEGER);
    INSERT INTO REPOSITORY VALUES (1, 'https://server:8443/svn/Finance', 'uuid');
    INSERT INTO NODES VALUES ('', 0), ('a.txt', 1);";
const REPOSITORY_URL: &str = "https://server:8443/svn/Finance";

// ----------------------------------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------------------------------

/// Two sibling folders whose names share a prefix, like S:\Fin and S:\Finance.
struct SameDrive {
    sandbox: Sandbox,
    fin: PathBuf,
    finance: PathBuf,
}

impl SameDrive {
    /// Creates empty Fin and Finance folders side by side.
    fn new() -> Self {
        let sandbox = Sandbox::new();
        let base = sandbox.source.parent().unwrap().to_path_buf();
        let (fin, finance) = (base.join("Fin"), base.join("Finance"));
        fs::create_dir_all(&fin).unwrap();
        fs::create_dir_all(&finance).unwrap();
        Self { sandbox, fin, finance }
    }

    /// Writes the same file in both folders.
    fn write_both(&self, rel: &str, content: &[u8]) {
        write_file(&self.fin, rel, content);
        write_file(&self.finance, rel, content);
    }

    /// Scans Fin (source) against Finance (destination).
    fn scan(&self, adjust: impl FnOnce(&mut Config)) -> ScanOutput {
        let mut config = test_config();
        adjust(&mut config);
        run_scan(&ScanRequest {
            source: self.fin.clone(),
            destination: self.finance.clone(),
            mode: MatchMode::Content,
            config,
            work_dir: self.sandbox.work.clone(),
            reports_dir: self.sandbox.reports.clone(),
        })
        .unwrap()
    }
}

/// Turns a folder into a Subversion working copy: .svn with wc.db and a read-only pristine copy.
fn make_svn_working_copy(dir: &Path, pristine_content: &[u8]) -> PathBuf {
    let metadata = dir.join(".svn");
    let pristine = write_file(&metadata, "pristine/ab/abcdef.svn-base", pristine_content);
    Connection::open(metadata.join("wc.db")).unwrap().execute_batch(WC_DB_SQL).unwrap();
    let mut permissions = fs::metadata(&pristine).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&pristine, permissions).unwrap();
    metadata
}

/// Returns every entry (files and folders) left under a folder.
fn entries(root: &Path) -> Vec<String> {
    let mut all: Vec<String> = folders(root);
    all.extend(snapshot(root).into_keys());
    all.sort();
    all
}

/// Applies a scanned plan with its confirmation code.
fn apply(scan: &ScanOutput, is_dry_run: bool) -> fs_spacemaker::apply::ApplyOutput {
    run_apply(&common::apply_request(scan, is_dry_run)).unwrap()
}

// ----------------------------------------------------------------------------------------------
// Same drive
// ----------------------------------------------------------------------------------------------

#[test]
fn sibling_folders_sharing_a_name_prefix_are_compared() {
    let drive = SameDrive::new();
    drive.write_both("a.txt", b"alpha");
    drive.write_both("sub/b.txt", b"beta");
    write_file(&drive.finance, "only-in-finance.txt", b"extra");
    let finance_before = snapshot(&drive.finance);
    let scan = drive.scan(|_| {});
    assert_eq!(scan.plan.body.actions.len(), 2);
    let output = apply(&scan, false);
    assert_eq!(output.removed_totals().0, 2);
    assert!(entries(&drive.fin).is_empty(), "Fin is emptied: {:?}", entries(&drive.fin));
    assert!(drive.fin.is_dir(), "the source root itself is kept");
    assert_eq!(snapshot(&drive.finance), finance_before, "Finance is never modified");
}

// ----------------------------------------------------------------------------------------------
// Subversion
// ----------------------------------------------------------------------------------------------

#[test]
fn svn_working_copy_is_removed_whole_when_every_file_is_on_the_destination() {
    let drive = SameDrive::new();
    drive.write_both("a.txt", b"alpha");
    drive.write_both("QL 2015/b.pdf", b"beta");
    make_svn_working_copy(&drive.fin, b"alpha");
    let finance_before = snapshot(&drive.finance);
    let scan = drive.scan(|config| config.remove_svn_metadata = true);
    let planned: Vec<&str> = scan.plan.body.actions.iter().map(|a| a.source_rel.as_str()).collect();
    assert_eq!(planned, vec!["QL 2015/b.pdf", "a.txt"], "metadata files are never planned one by one");
    assert_eq!(scan.plan.body.metadata_removals.len(), 1);
    assert_eq!(scan.plan.body.metadata_removals[0].metadata_rel, ".svn");
    let csv = fs::read_to_string(&scan.csv_path).unwrap();
    assert!(csv.contains(REPOSITORY_URL), "the report names the repository");
    assert!(csv.contains("1 uncommitted local changes"));

    let dry_run = apply(&scan, true);
    assert_eq!(dry_run.metadata_results[0].status, MetadataStatus::WouldRemove);
    assert!(drive.fin.join(".svn").join("wc.db").exists(), "a dry run removes nothing");

    let output = apply(&scan, false);
    assert_eq!(output.metadata_results[0].status, MetadataStatus::Removed);
    assert!(!output.has_failures());
    assert!(entries(&drive.fin).is_empty(), "Fin is gone entirely: {:?}", entries(&drive.fin));
    assert_eq!(snapshot(&drive.finance), finance_before, "Finance is never modified");
    let journal = fs::read_to_string(&output.journal_path).unwrap();
    assert!(journal.contains(",deleted-svn-metadata,"));
}

#[test]
fn svn_metadata_is_kept_while_a_file_of_the_working_copy_remains() {
    let drive = SameDrive::new();
    drive.write_both("a.txt", b"alpha");
    write_file(&drive.fin, "only-in-fin.txt", b"unique");
    make_svn_working_copy(&drive.fin, b"alpha");
    let scan = drive.scan(|config| config.remove_svn_metadata = true);
    assert!(scan.plan.body.metadata_removals.is_empty());
    apply(&scan, false);
    assert!(drive.fin.join(".svn").join("wc.db").exists());
    assert!(drive.fin.join("only-in-fin.txt").exists());
    assert!(!drive.fin.join("a.txt").exists());
}

#[test]
fn svn_metadata_is_kept_without_the_option() {
    let drive = SameDrive::new();
    drive.write_both("a.txt", b"alpha");
    make_svn_working_copy(&drive.fin, b"alpha");
    let scan = drive.scan(|_| {});
    assert!(scan.plan.body.metadata_removals.is_empty());
    let html = fs::read_to_string(&scan.html_path).unwrap();
    assert!(html.contains("--remove-svn-metadata"), "the report explains how to remove it");
    apply(&scan, false);
    assert!(drive.fin.join(".svn").join("wc.db").exists());
    assert!(!drive.fin.join("a.txt").exists());
}

#[test]
fn svn_metadata_is_kept_if_a_file_appears_after_the_scan() {
    let drive = SameDrive::new();
    drive.write_both("a.txt", b"alpha");
    make_svn_working_copy(&drive.fin, b"alpha");
    let scan = drive.scan(|config| config.remove_svn_metadata = true);
    assert_eq!(scan.plan.body.metadata_removals.len(), 1);
    write_file(&drive.fin, "new/late.txt", b"written after the scan");
    let output = apply(&scan, false);
    assert_eq!(output.metadata_results[0].status, MetadataStatus::Kept);
    assert!(drive.fin.join(".svn").join("wc.db").exists());
    assert!(drive.fin.join("new").join("late.txt").exists());
}

#[test]
fn old_format_working_copies_with_metadata_in_every_folder_are_removed_together() {
    let drive = SameDrive::new();
    drive.write_both("a.txt", b"alpha");
    drive.write_both("sub/b.txt", b"beta");
    make_svn_working_copy(&drive.fin, b"alpha");
    write_file(&drive.fin, "sub/.svn/entries", b"old format");
    let scan = drive.scan(|config| config.remove_svn_metadata = true);
    assert_eq!(scan.plan.body.metadata_removals.len(), 2);
    let output = apply(&scan, false);
    assert!(output.metadata_results.iter().all(|result| result.status == MetadataStatus::Removed));
    assert!(entries(&drive.fin).is_empty(), "{:?}", entries(&drive.fin));
}

#[test]
fn destination_metadata_is_never_used_as_a_copy() {
    let drive = SameDrive::new();
    write_file(&drive.fin, "doc.txt", b"only a pristine copy exists on the destination");
    make_svn_working_copy(&drive.finance, b"only a pristine copy exists on the destination");
    let scan = drive.scan(|_| {});
    assert!(scan.plan.body.actions.is_empty(), "a pristine copy may vanish with svn cleanup");
}

// ----------------------------------------------------------------------------------------------
// Git and trash folders
// ----------------------------------------------------------------------------------------------

#[test]
fn git_repositories_are_never_deleted_even_when_fully_copied() {
    let drive = SameDrive::new();
    drive.write_both("code.rs", b"fn main() {}");
    drive.write_both(".git/objects/pack/pack-1.pack", b"history");
    let scan = drive.scan(|config| config.remove_svn_metadata = true);
    let planned: Vec<&str> = scan.plan.body.actions.iter().map(|a| a.source_rel.as_str()).collect();
    assert_eq!(planned, vec!["code.rs"]);
    assert!(scan.plan.body.metadata_removals.is_empty());
    apply(&scan, false);
    assert!(drive.fin.join(".git").join("objects").join("pack").join("pack-1.pack").exists());
}

#[test]
fn files_whose_only_copy_is_in_a_trash_are_kept() {
    let sandbox = Sandbox::new();
    sandbox.write_source("a.txt", b"only in a Linux trash");
    sandbox.write_source("b.txt", b"only in a Windows recycle bin");
    sandbox.write_destination(".Trash-1000/files/a.txt", b"only in a Linux trash");
    sandbox.write_destination("$RECYCLE.BIN/S-1-5-21/$R1.txt", b"only in a Windows recycle bin");
    let scan = sandbox.scan(MatchMode::Content);
    assert!(scan.plan.body.actions.is_empty());
}

// ----------------------------------------------------------------------------------------------
// Fingerprint index
// ----------------------------------------------------------------------------------------------

#[test]
fn a_folder_scan_reuses_the_fingerprints_of_a_parent_scan() {
    let sandbox = Sandbox::new();
    sandbox.write_source("Fin/a.bin", &vec![1u8; 200_000]);
    sandbox.write_destination("Finance/a.bin", &vec![1u8; 200_000]);
    let parent = sandbox.scan(MatchMode::Content);
    assert!(parent.source_stats.files_read > 0);
    let folder = run_scan(&ScanRequest {
        source: sandbox.source.join("Fin"),
        destination: sandbox.destination.join("Finance"),
        mode: MatchMode::Content,
        config: test_config(),
        work_dir: sandbox.work.clone(),
        reports_dir: sandbox.reports.clone(),
    })
    .unwrap();
    assert_eq!(folder.plan.body.actions.len(), 1);
    assert_eq!(folder.source_stats.files_read + folder.destination_stats.files_read, 0);
}
