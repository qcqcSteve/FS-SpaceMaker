//! Integration tests of the normal behaviour: the three modes, prediction accuracy, emptied
//! folders, dry runs, index reuse, reports, Unicode and long paths.

mod common;

use std::fs;

use common::{Sandbox, folders, planned_sources, snapshot};
use fs_spacemaker::apply::ActionStatus;
use fs_spacemaker::matching::MatchMode;

/// Size large enough to span several read buffers (1 MiB each).
const MULTI_BUFFER_BYTES: usize = 3 * 1024 * 1024 + 123;

#[test]
fn content_mode_deletes_only_identical_files() {
    let sandbox = Sandbox::new();
    sandbox.write_source("same.txt", b"identical");
    sandbox.write_source("differs.txt", b"version-A");
    sandbox.write_source("unique.txt", b"only in source");
    sandbox.write_destination("same.txt", b"identical");
    sandbox.write_destination("differs.txt", b"version-B");
    let destination_before = snapshot(&sandbox.destination);

    let scan = sandbox.scan(MatchMode::Content);
    assert_eq!(planned_sources(&scan), vec!["same.txt"]);
    assert_eq!(scan.summary.delete_bytes, 9);
    assert_eq!(scan.summary.residual_files, 2);

    let output = sandbox.apply(&scan);
    assert_eq!(output.count(ActionStatus::Deleted), 1);
    assert!(!sandbox.source_path("same.txt").exists());
    assert!(sandbox.source_path("differs.txt").exists());
    assert!(sandbox.source_path("unique.txt").exists());
    assert_eq!(snapshot(&sandbox.destination), destination_before, "destination must never change");
}

#[test]
fn content_mode_finds_renamed_files_anywhere() {
    let sandbox = Sandbox::new();
    sandbox.write_source("photos/2019/IMG_001.jpg", b"jpeg-bytes-1");
    sandbox.write_destination("archive/old/renamed.jpg", b"jpeg-bytes-1");
    let scan = sandbox.scan(MatchMode::Content);
    assert_eq!(scan.plan.body.actions.len(), 1);
    assert_eq!(scan.plan.body.actions[0].destination_rel, "archive/old/renamed.jpg");
    sandbox.apply(&scan);
    assert!(!sandbox.source_path("photos/2019/IMG_001.jpg").exists());
}

#[test]
fn content_mode_prefers_the_copy_at_the_same_path() {
    let sandbox = Sandbox::new();
    sandbox.write_source("docs/a.txt", b"payload");
    sandbox.write_destination("aaa/first.txt", b"payload");
    sandbox.write_destination("docs/a.txt", b"payload");
    let scan = sandbox.scan(MatchMode::Content);
    assert_eq!(scan.plan.body.actions[0].destination_rel, "docs/a.txt");
}

#[test]
fn same_path_mode_requires_the_same_relative_path() {
    let sandbox = Sandbox::new();
    sandbox.write_source("a/file.txt", b"payload");
    sandbox.write_source("b/moved.txt", b"other");
    sandbox.write_destination("a/file.txt", b"payload");
    sandbox.write_destination("elsewhere/moved.txt", b"other");
    let scan = sandbox.scan(MatchMode::SamePath);
    assert_eq!(planned_sources(&scan), vec!["a/file.txt"]);
    sandbox.apply(&scan);
    assert!(sandbox.source_path("b/moved.txt").exists());
}

#[test]
fn relocated_mode_deletes_whole_moved_folders() {
    let sandbox = Sandbox::new();
    sandbox.write_source("Projects/Alpha/main.rs", b"fn main() {}");
    sandbox.write_source("Projects/Alpha/docs/readme.md", b"# Alpha");
    sandbox.write_source("Projects/Beta/beta.txt", b"beta");
    sandbox.write_source("Projects/Beta/not-copied.txt", b"only in source");
    sandbox.write_destination("Backup/2021/Alpha-final/main.rs", b"fn main() {}");
    sandbox.write_destination("Backup/2021/Alpha-final/docs/readme.md", b"# Alpha");
    sandbox.write_destination("Backup/2021/Alpha-final/extra.log", b"only in destination");
    sandbox.write_destination("Random/beta.txt", b"beta");

    let scan = sandbox.scan(MatchMode::Relocated);
    assert_eq!(planned_sources(&scan), vec!["Projects/Alpha/docs/readme.md", "Projects/Alpha/main.rs"]);
    let trees = scan.plan.body.actions.iter().map(|a| a.destination_rel.clone()).collect::<Vec<_>>();
    assert!(trees.iter().all(|rel| rel.starts_with("Backup/2021/Alpha-final/")));

    sandbox.apply(&scan);
    assert!(!sandbox.source.join("Projects").join("Alpha").exists(), "emptied tree removed");
    assert!(sandbox.source_path("Projects/Beta/beta.txt").exists(), "isolated file kept in relocated mode");
}

#[test]
fn relocated_mode_keeps_incomplete_folders() {
    let sandbox = Sandbox::new();
    sandbox.write_source("Album/1.mp3", b"one");
    sandbox.write_source("Album/2.mp3", b"two");
    sandbox.write_destination("Music/Album copy/1.mp3", b"one");
    let scan = sandbox.scan(MatchMode::Relocated);
    assert!(scan.plan.body.actions.is_empty());
    assert_eq!(scan.summary.residual_files, 2);
}

#[test]
fn prediction_matches_the_measured_result() {
    let sandbox = Sandbox::new();
    for index in 0..20 {
        let content = format!("file number {index}").repeat(index + 1);
        sandbox.write_source(&format!("dir{}/f{index}.txt", index % 3), content.as_bytes());
        if index % 2 == 0 {
            sandbox.write_destination(&format!("copy/f{index}.bak"), content.as_bytes());
        }
    }
    let scan = sandbox.scan(MatchMode::Content);
    let output = sandbox.apply(&scan);
    let (files, bytes) = output.removed_totals();
    assert_eq!(files, scan.summary.delete_files);
    assert_eq!(bytes, scan.summary.delete_bytes);
    let remaining: u64 = snapshot(&sandbox.source).values().map(|content| content.len() as u64).sum();
    assert_eq!(remaining, scan.summary.residual_bytes);
}

#[test]
fn large_files_differing_only_at_the_end_are_kept() {
    let sandbox = Sandbox::new();
    let mut content = vec![42u8; MULTI_BUFFER_BYTES];
    sandbox.write_source("big.bin", &content);
    *content.last_mut().unwrap() = 43;
    sandbox.write_destination("big.bin", &content);
    let scan = sandbox.scan(MatchMode::Content);
    assert!(scan.plan.body.actions.is_empty());
}

#[test]
fn large_identical_files_are_deleted() {
    let sandbox = Sandbox::new();
    let content: Vec<u8> = (0..MULTI_BUFFER_BYTES).map(|i| u8::try_from(i % 251).unwrap()).collect();
    sandbox.write_source("big.bin", &content);
    sandbox.write_destination("big-copy.bin", &content);
    let scan = sandbox.scan(MatchMode::Content);
    assert_eq!(sandbox.apply(&scan).count(ActionStatus::Deleted), 1);
}

#[test]
fn empty_files_are_excluded_unless_requested() {
    let sandbox = Sandbox::new();
    sandbox.write_source("marker", b"");
    sandbox.write_destination("other-empty", b"");
    assert!(sandbox.scan(MatchMode::Content).plan.body.actions.is_empty());
    let scan = sandbox.scan_with(MatchMode::Content, |config| config.include_empty_files = true);
    assert_eq!(planned_sources(&scan), vec!["marker"]);
}

#[test]
fn same_path_mode_matches_empty_files_at_the_same_path() {
    let sandbox = Sandbox::new();
    sandbox.write_source("d/.keep", b"");
    sandbox.write_destination("d/.keep", b"");
    assert_eq!(planned_sources(&sandbox.scan(MatchMode::SamePath)), vec!["d/.keep"]);
}

#[test]
fn only_folders_emptied_by_deletions_are_removed() {
    let sandbox = Sandbox::new();
    sandbox.write_source("a/b/dup.txt", b"dup");
    sandbox.write_source("c/dup2.txt", b"dup2");
    sandbox.write_source("c/keep.txt", b"keep");
    fs::create_dir_all(sandbox.source.join("already-empty")).unwrap();
    sandbox.write_destination("x/dup.txt", b"dup");
    sandbox.write_destination("x/dup2.txt", b"dup2");
    let scan = sandbox.scan(MatchMode::Content);
    let output = sandbox.apply(&scan);
    let mut removed = output.removed_dirs.clone();
    removed.sort();
    assert_eq!(removed, vec!["a", "a/b"]);
    let mut remaining = folders(&sandbox.source);
    remaining.sort();
    assert_eq!(remaining, vec!["already-empty", "c"]);
    assert!(sandbox.source.exists(), "the source root is never removed");
}

#[test]
fn emptied_folders_are_kept_when_disabled() {
    let sandbox = Sandbox::new();
    sandbox.write_source("a/dup.txt", b"dup");
    sandbox.write_destination("dup.txt", b"dup");
    let scan = sandbox.scan_with(MatchMode::Content, |config| config.remove_emptied_dirs = false);
    sandbox.apply(&scan);
    assert!(sandbox.source.join("a").is_dir());
}

#[test]
fn dry_run_verifies_but_deletes_nothing() {
    let sandbox = Sandbox::new();
    sandbox.write_source("dup.txt", b"dup");
    sandbox.write_destination("dup.txt", b"dup");
    let source_before = snapshot(&sandbox.source);
    let scan = sandbox.scan(MatchMode::Content);
    let output = sandbox.dry_run(&scan);
    assert_eq!(output.count(ActionStatus::WouldDelete), 1);
    assert_eq!(snapshot(&sandbox.source), source_before);
    assert!(output.html_path.is_file());
    assert!(output.journal_path.is_file());
}

#[test]
fn second_scan_reuses_the_index() {
    let sandbox = Sandbox::new();
    sandbox.write_source("a.bin", &vec![1u8; 200_000]);
    sandbox.write_destination("b.bin", &vec![1u8; 200_000]);
    let first = sandbox.scan(MatchMode::Content);
    assert!(first.source_stats.files_read > 0);
    let second = sandbox.scan(MatchMode::Content);
    assert_eq!(second.source_stats.files_read + second.destination_stats.files_read, 0);
    assert!(second.source_stats.index_hits > 0);
    assert_eq!(second.plan.body.actions, first.plan.body.actions);
}

#[test]
fn files_without_size_twin_are_never_read() {
    let sandbox = Sandbox::new();
    sandbox.write_source("a.txt", b"12345");
    sandbox.write_destination("b.txt", b"123456");
    let scan = sandbox.scan(MatchMode::Content);
    assert_eq!(scan.source_stats.files_read + scan.destination_stats.files_read, 0);
}

#[test]
fn reports_describe_the_plan() {
    let sandbox = Sandbox::new();
    sandbox.write_source("dup.txt", b"dup");
    sandbox.write_source("kept.txt", b"kept");
    sandbox.write_destination("dup.txt", b"dup");
    let scan = sandbox.scan(MatchMode::Content);
    let html = fs::read_to_string(&scan.html_path).unwrap();
    assert!(html.contains(&scan.plan.plan_id));
    assert!(html.contains("dup.txt") && html.contains("kept.txt"));
    let csv = fs::read_to_string(&scan.csv_path).unwrap();
    assert!(csv.lines().any(|line| line.starts_with("deletions,") && line.contains("dup.txt")));
    assert!(csv.lines().any(|line| line.starts_with("residual,") && line.contains("kept.txt")));
    assert!(scan.apply_command.contains(scan.plan.confirmation_code()));
}

#[test]
fn unicode_names_are_handled() {
    let sandbox = Sandbox::new();
    sandbox.write_source("Été 2020/Café résumé ✓.txt", b"unicode");
    sandbox.write_destination("Copy/Café résumé ✓.txt", b"unicode");
    let scan = sandbox.scan(MatchMode::Content);
    assert_eq!(sandbox.apply(&scan).count(ActionStatus::Deleted), 1);
    assert!(!sandbox.source.join("Été 2020").exists());
}

#[test]
fn paths_longer_than_260_characters_are_handled() {
    let sandbox = Sandbox::new();
    let deep = (0..12).map(|i| format!("very-long-folder-name-{i:02}")).collect::<Vec<_>>().join("/");
    let rel = format!("{deep}/file-with-a-long-name.txt");
    assert!(sandbox.source.join(&deep).to_string_lossy().len() > 260);
    sandbox.write_source(&rel, b"deep");
    sandbox.write_destination("deep.txt", b"deep");
    let scan = sandbox.scan(MatchMode::Content);
    assert_eq!(sandbox.apply(&scan).count(ActionStatus::Deleted), 1);
    assert!(!sandbox.source_path(&rel).exists());
}

#[test]
fn case_differences_follow_platform_rules_in_same_path_mode() {
    let sandbox = Sandbox::new();
    sandbox.write_source("Docs/Report.TXT", b"r");
    sandbox.write_destination("docs/report.txt", b"r");
    let scan = sandbox.scan(MatchMode::SamePath);
    assert_eq!(scan.plan.body.actions.len() == 1, fs_spacemaker::paths::IS_CASE_INSENSITIVE);
}
