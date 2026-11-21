//! Shared helpers for integration tests: sandbox folders, file trees, scan/apply runners, snapshots.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use fs_spacemaker::apply::{ApplyOutput, ApplyRequest, run_apply};
use fs_spacemaker::config::Config;
use fs_spacemaker::matching::MatchMode;
use fs_spacemaker::scan::{ScanOutput, ScanRequest, run_scan};
use tempfile::TempDir;
use walkdir::WalkDir;

/// Worker threads used by tests (small, but more than one to exercise parallel code).
const TEST_THREADS: usize = 2;

/// A temporary source, destination and work folder.
pub struct Sandbox {
    _dir: TempDir,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub work: PathBuf,
}

impl Sandbox {
    /// Creates empty source, destination and work folders.
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let sandbox = Self {
            source: base.join("source"),
            destination: base.join("destination"),
            work: base.join("work"),
            _dir: dir,
        };
        for path in [&sandbox.source, &sandbox.destination, &sandbox.work] {
            fs::create_dir_all(path).unwrap();
        }
        sandbox
    }

    /// Writes a source file (folders created as needed).
    pub fn write_source(&self, rel: &str, content: &[u8]) -> PathBuf {
        write_file(&self.source, rel, content)
    }

    /// Writes a destination file (folders created as needed).
    pub fn write_destination(&self, rel: &str, content: &[u8]) -> PathBuf {
        write_file(&self.destination, rel, content)
    }

    /// Returns the native path of a source file.
    pub fn source_path(&self, rel: &str) -> PathBuf {
        native(&self.source, rel)
    }

    /// Returns the native path of a destination file.
    pub fn destination_path(&self, rel: &str) -> PathBuf {
        native(&self.destination, rel)
    }

    /// Scans with test defaults.
    pub fn scan(&self, mode: MatchMode) -> ScanOutput {
        self.scan_with(mode, |_| {})
    }

    /// Scans after adjusting the configuration.
    pub fn scan_with(&self, mode: MatchMode, adjust: impl FnOnce(&mut Config)) -> ScanOutput {
        let mut config = test_config();
        adjust(&mut config);
        let request = ScanRequest {
            source: self.source.clone(),
            destination: self.destination.clone(),
            mode,
            config,
            work_dir: self.work.clone(),
        };
        run_scan(&request).unwrap()
    }

    /// Applies a scanned plan with its confirmation code.
    pub fn apply(&self, scan: &ScanOutput) -> ApplyOutput {
        run_apply(&apply_request(scan, false)).unwrap()
    }

    /// Runs a dry run of a scanned plan.
    pub fn dry_run(&self, scan: &ScanOutput) -> ApplyOutput {
        run_apply(&apply_request(scan, true)).unwrap()
    }
}

/// Returns the configuration used by tests.
pub fn test_config() -> Config {
    Config { scan_threads: TEST_THREADS, apply_threads: TEST_THREADS, ..Config::default() }
}

/// Returns an apply request for a scan output.
pub fn apply_request(scan: &ScanOutput, is_dry_run: bool) -> ApplyRequest {
    ApplyRequest {
        plan_path: scan.plan_path.clone(),
        confirmation: Some(scan.plan.confirmation_code().to_string()),
        is_dry_run,
        config: test_config(),
    }
}

/// Writes a file under a root.
pub fn write_file(root: &Path, rel: &str, content: &[u8]) -> PathBuf {
    let path = native(root, rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, content).unwrap();
    path
}

/// Returns the native path of a '/'-separated relative path.
pub fn native(root: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(root.to_path_buf(), |path, part| path.join(part))
}

/// Returns every regular file under a root with its content, keyed by '/'-separated path.
pub fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            let rel = entry.path().strip_prefix(root).unwrap();
            let key = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            (key, fs::read(entry.path()).unwrap())
        })
        .collect()
}

/// Returns every folder under a root, keyed by '/'-separated path (root excluded).
pub fn folders(root: &Path) -> Vec<String> {
    WalkDir::new(root)
        .min_depth(1)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_dir())
        .map(|entry| {
            let rel = entry.path().strip_prefix(root).unwrap();
            rel.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect()
}

/// Rewrites a file with new content of the same length, keeping its modification time.
pub fn overwrite_keeping_mtime(path: &Path, content: &[u8]) {
    let modified: SystemTime = fs::metadata(path).unwrap().modified().unwrap();
    fs::write(path, content).unwrap();
    fs::File::options().write(true).open(path).unwrap().set_modified(modified).unwrap();
}

/// Returns the relative source paths planned for deletion.
pub fn planned_sources(scan: &ScanOutput) -> Vec<String> {
    let mut rels: Vec<String> =
        scan.plan.body.actions.iter().map(|action| action.source_rel.clone()).collect();
    rels.sort();
    rels
}
