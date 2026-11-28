//! Fixed names of the files written into the reports folder. There is a single set of outputs: each
//! scan replaces the previous plan and reports, each dry run replaces the previous dry-run files.
//! The journal of real deletions is the one exception: it is never removed, only appended to, so
//! the trace of every file ever deleted survives later scans. Only these exact names are ever
//! removed, never anything else in the reports folder.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

// ----------------------------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------------------------

/// Plan written by a scan and read by apply.
pub const PLAN_FILE_NAME: &str = "plan.json";
/// Reports written by a scan.
pub const SCAN_HTML_FILE_NAME: &str = "report.html";
pub const SCAN_CSV_FILE_NAME: &str = "report.csv";
/// Files written by a dry run.
const DRY_RUN_FILE_NAMES: PhaseFileNames =
    PhaseFileNames { html: "dry-run-report.html", csv: "dry-run-report.csv", journal: "dry-run-journal.csv" };
/// Files written by a real apply.
const APPLY_FILE_NAMES: PhaseFileNames =
    PhaseFileNames { html: "apply-report.html", csv: "apply-report.csv", journal: "apply-journal.csv" };

// ----------------------------------------------------------------------------------------------
// Model
// ----------------------------------------------------------------------------------------------

/// File names of one apply phase.
#[derive(Clone, Copy, Debug)]
struct PhaseFileNames {
    html: &'static str,
    csv: &'static str,
    journal: &'static str,
}

/// Full paths of the files written by one apply run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyPaths {
    pub html: PathBuf,
    pub csv: PathBuf,
    pub journal: PathBuf,
}

// ----------------------------------------------------------------------------------------------
// Paths and cleanup
// ----------------------------------------------------------------------------------------------

/// Returns the output paths of an apply run, in the folder holding the plan.
pub fn apply_paths(output_dir: &Path, is_dry_run: bool) -> ApplyPaths {
    let names = if is_dry_run { DRY_RUN_FILE_NAMES } else { APPLY_FILE_NAMES };
    ApplyPaths {
        html: output_dir.join(names.html),
        csv: output_dir.join(names.csv),
        journal: output_dir.join(names.journal),
    }
}

/// Removes the outputs of the previous scan and of its dry runs and applies, except the journal
/// of real deletions.
pub fn clear_previous_outputs(reports_dir: &Path) -> Result<()> {
    let names = [
        PLAN_FILE_NAME,
        SCAN_HTML_FILE_NAME,
        SCAN_CSV_FILE_NAME,
        DRY_RUN_FILE_NAMES.html,
        DRY_RUN_FILE_NAMES.csv,
        DRY_RUN_FILE_NAMES.journal,
        APPLY_FILE_NAMES.html,
        APPLY_FILE_NAMES.csv,
    ];
    for name in names {
        remove_if_present(&reports_dir.join(name))?;
    }
    Ok(())
}

/// Removes a file, ignoring a missing one.
fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("Cannot replace {}: close it and retry.", path.display()))
        }
    }
}

// ----------------------------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clears_only_known_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let known = [PLAN_FILE_NAME, SCAN_HTML_FILE_NAME, "dry-run-journal.csv", "apply-report.html"];
        for name in known.iter().chain(&["index.sqlite", "notes.txt", "apply-journal.csv"]) {
            fs::write(dir.path().join(name), b"x").unwrap();
        }
        clear_previous_outputs(dir.path()).unwrap();
        for name in known {
            assert!(!dir.path().join(name).exists(), "{name} should be removed");
        }
        assert!(dir.path().join("index.sqlite").exists(), "the index is kept");
        assert!(dir.path().join("apply-journal.csv").exists(), "the deletion journal is never removed");
        assert!(dir.path().join("notes.txt").exists(), "unknown files are never touched");
    }

    #[test]
    fn dry_run_and_apply_use_different_files() {
        let dir = Path::new("work");
        assert_ne!(apply_paths(dir, true), apply_paths(dir, false));
        assert_eq!(apply_paths(dir, false).journal, dir.join("apply-journal.csv"));
    }
}
