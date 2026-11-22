//! Runtime settings: built-in defaults, optional JSON file (installation folder or --config),
//! then command-line overrides applied by the caller. Unknown keys are rejected to catch typos.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

// ----------------------------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------------------------

/// Name of the optional configuration file looked up in the installation folder.
pub const CONFIG_FILE_NAME: &str = "fs-spacemaker.config.json";
/// Name of the default work folder (index, plans, reports) created in the installation folder.
pub const DEFAULT_WORK_DIR_NAME: &str = "fs-spacemaker-work";
/// Cargo output layout: an executable in target\release or target\debug belongs to the project
/// folder above `target`, which is where its work folder and configuration file live.
const CARGO_TARGET_DIR_NAME: &str = "target";
const CARGO_PROFILE_DIR_NAMES: [&str; 2] = ["release", "debug"];
/// Bytes read from the start of a file for the cheap pre-filter fingerprint.
const DEFAULT_PARTIAL_HASH_BYTES: u64 = 64 * 1024;
/// Smallest accepted partial fingerprint length.
const MIN_PARTIAL_HASH_BYTES: u64 = 4 * 1024;
/// Maximum rows per table embedded in the HTML report (the CSV always holds every row).
const DEFAULT_REPORT_MAX_ROWS: usize = 50_000;
/// Upper bound for the default number of scan threads (more rarely helps on disks).
const MAX_DEFAULT_SCAN_THREADS: usize = 8;
/// Default number of parallel verifications during apply.
const DEFAULT_APPLY_THREADS: usize = 4;
/// Characters not allowed in an ignored stream pattern ('*' is only allowed as the last character).
const FORBIDDEN_STREAM_PATTERN_CHARS: [char; 4] = ['*', ':', '\\', '/'];
/// Names never inventoried, on either side (compared case-insensitively).
const DEFAULT_EXCLUDED_NAMES: [&str; 6] = [
    "System Volume Information",
    "$RECYCLE.BIN",
    "pagefile.sys",
    "hiberfil.sys",
    "swapfile.sys",
    "DumpStack.log.tmp",
];

// ----------------------------------------------------------------------------------------------
// Settings
// ----------------------------------------------------------------------------------------------

/// All user-adjustable settings.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Free-form documentation block allowed in the file; ignored by the program.
    #[serde(rename = "_help")]
    pub help_text: Option<serde_json::Value>,
    pub scan_threads: usize,
    pub apply_threads: usize,
    pub partial_hash_bytes: u64,
    pub excluded_names: Vec<String>,
    pub report_max_rows: usize,
    pub include_empty_files: bool,
    pub delete_read_only: bool,
    pub remove_emptied_dirs: bool,
    pub ignored_stream_names: Vec<String>,
    pub work_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            help_text: None,
            scan_threads: default_scan_threads(),
            apply_threads: DEFAULT_APPLY_THREADS,
            partial_hash_bytes: DEFAULT_PARTIAL_HASH_BYTES,
            excluded_names: DEFAULT_EXCLUDED_NAMES.iter().map(ToString::to_string).collect(),
            report_max_rows: DEFAULT_REPORT_MAX_ROWS,
            include_empty_files: false,
            delete_read_only: false,
            remove_emptied_dirs: true,
            ignored_stream_names: Vec::new(),
            work_dir: None,
        }
    }
}

impl Config {
    /// Fails with an actionable message if a value is out of range.
    pub fn validate(&self) -> Result<()> {
        if self.scan_threads == 0 || self.apply_threads == 0 {
            bail!("scan_threads and apply_threads must be at least 1: fix the configuration.");
        }
        if self.partial_hash_bytes < MIN_PARTIAL_HASH_BYTES {
            bail!("partial_hash_bytes must be at least {MIN_PARTIAL_HASH_BYTES}: fix the configuration.");
        }
        if self.report_max_rows == 0 {
            bail!("report_max_rows must be at least 1: fix the configuration.");
        }
        if let Some(pattern) =
            self.ignored_stream_names.iter().find(|pattern| !is_valid_stream_pattern(pattern))
        {
            bail!(
                "Invalid ignored stream name {pattern:?}: use a stream name, optionally ending with '*' \
                 (e.g. com.dropbox.*)."
            );
        }
        Ok(())
    }
}

/// Returns true if a stream pattern is a non-empty name with at most one '*', at its end.
fn is_valid_stream_pattern(pattern: &str) -> bool {
    let name = pattern.strip_suffix('*').unwrap_or(pattern);
    !name.is_empty() && !name.contains(FORBIDDEN_STREAM_PATTERN_CHARS)
}

/// Returns the number of logical CPUs, capped for disk-bound work.
fn default_scan_threads() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get).min(MAX_DEFAULT_SCAN_THREADS)
}

// ----------------------------------------------------------------------------------------------
// Loading
// ----------------------------------------------------------------------------------------------

/// Loads the explicit file, else the file in the installation folder, else the defaults.
pub fn load_config(explicit_path: Option<&Path>) -> Result<Config> {
    let path = match explicit_path {
        Some(path) => path.to_path_buf(),
        None => match installation_dir().map(|dir| dir.join(CONFIG_FILE_NAME)) {
            Some(path) if path.is_file() => path,
            _ => return Ok(Config::default()),
        },
    };
    parse_config_file(&path)
}

/// Reads and validates one configuration file.
fn parse_config_file(path: &Path) -> Result<Config> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("Cannot read configuration {}: check the path.", path.display()))?;
    let config: Config = serde_json::from_str(&text).with_context(|| {
        format!("Invalid configuration {}: fix the JSON or delete the file to use defaults.", path.display())
    })?;
    config.validate()?;
    Ok(config)
}

/// Returns the installation folder: the project folder for a cargo build, else the executable's folder.
pub fn installation_dir() -> Option<PathBuf> {
    let executable_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    Some(installation_dir_for(&executable_dir))
}

/// Maps an executable folder to its installation folder (skips cargo's target\<profile>).
fn installation_dir_for(executable_dir: &Path) -> PathBuf {
    let is_profile_dir = executable_dir
        .file_name()
        .is_some_and(|name| CARGO_PROFILE_DIR_NAMES.iter().any(|profile| name.eq_ignore_ascii_case(profile)));
    let target_dir = executable_dir.parent();
    let is_in_target = target_dir
        .and_then(Path::file_name)
        .is_some_and(|name| name.eq_ignore_ascii_case(CARGO_TARGET_DIR_NAME));
    match (is_profile_dir && is_in_target, target_dir.and_then(Path::parent)) {
        (true, Some(project_dir)) => project_dir.to_path_buf(),
        _ => executable_dir.to_path_buf(),
    }
}

/// Returns the configured work folder, or the default one in the installation folder.
pub fn resolve_work_dir(config: &Config) -> Result<PathBuf> {
    if let Some(dir) = &config.work_dir {
        return Ok(dir.clone());
    }
    installation_dir()
        .map(|dir| dir.join(DEFAULT_WORK_DIR_NAME))
        .context("Cannot locate the executable folder: pass --work-dir.")
}

// ----------------------------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_safe() {
        let config = Config::default();
        config.validate().unwrap();
        assert!(!config.delete_read_only);
        assert!(!config.include_empty_files);
        assert!(config.remove_emptied_dirs);
    }

    #[test]
    fn file_values_override_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE_NAME);
        fs::write(&path, r#"{"_help": "doc", "apply_threads": 2, "delete_read_only": true}"#).unwrap();
        let config = load_config(Some(&path)).unwrap();
        assert_eq!(config.apply_threads, 2);
        assert!(config.delete_read_only);
        assert_eq!(config.partial_hash_bytes, DEFAULT_PARTIAL_HASH_BYTES);
    }

    #[test]
    fn unknown_keys_and_bad_values_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE_NAME);
        fs::write(&path, r#"{"delete_readonly": true}"#).unwrap();
        assert!(load_config(Some(&path)).is_err());
        fs::write(&path, r#"{"scan_threads": 0}"#).unwrap();
        assert!(load_config(Some(&path)).is_err());
    }

    #[test]
    fn cargo_builds_use_the_project_folder() {
        let project = Path::new("projects").join("FS SpaceMaker");
        for profile in ["release", "debug", "Release"] {
            let executable_dir = project.join("target").join(profile);
            assert_eq!(installation_dir_for(&executable_dir), project);
        }
    }

    #[test]
    fn installed_executables_use_their_own_folder() {
        for executable_dir in [Path::new("tools").join("fs"), Path::new("apps").join("release")] {
            assert_eq!(installation_dir_for(&executable_dir), executable_dir);
        }
    }

    #[test]
    fn ignored_stream_patterns_are_validated() {
        let valid = Config {
            ignored_stream_names: vec!["com.dropbox.*".into(), "Exact".into()],
            ..Config::default()
        };
        valid.validate().unwrap();
        for bad in ["", "*", "a*b", "a:b", "*.txt"] {
            let config = Config { ignored_stream_names: vec![bad.to_string()], ..Config::default() };
            assert!(config.validate().is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn missing_explicit_file_is_an_error() {
        assert!(load_config(Some(Path::new("does-not-exist.json"))).is_err());
    }
}
