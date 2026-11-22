//! The plan: the exact, frozen list of deletions proposed by a scan. Its identifier is the BLAKE3
//! digest of its body, so any edit or corruption is detected, and apply requires the first
//! characters of that identifier as a confirmation code.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::matching::MatchMode;

// ----------------------------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------------------------

/// Version of the plan file layout; apply refuses other versions.
pub const PLAN_FORMAT_VERSION: u32 = 1;
/// Number of identifier characters the user must type to confirm.
pub const CONFIRMATION_LENGTH: usize = 8;

// ----------------------------------------------------------------------------------------------
// Model
// ----------------------------------------------------------------------------------------------

/// One source file to delete, with the facts that must still hold at deletion time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedDeletion {
    pub source_rel: String,
    pub destination_rel: String,
    pub size: u64,
    pub source_mtime_ns: i64,
    pub blake3: String,
}

/// Everything the identifier covers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanBody {
    pub created_at: String,
    pub tool_version: String,
    pub mode: MatchMode,
    pub source_root: String,
    pub destination_root: String,
    pub delete_read_only: bool,
    pub remove_emptied_dirs: bool,
    /// Alternate data stream patterns treated as holding no user data. Omitted when empty, so
    /// plans written before this field existed keep their identifier.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored_stream_names: Vec<String>,
    pub actions: Vec<PlannedDeletion>,
}

/// A plan file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub format_version: u32,
    pub plan_id: String,
    pub body: PlanBody,
}

impl Plan {
    /// Wraps a body with its identifier.
    pub fn new(body: PlanBody) -> Result<Self> {
        Ok(Self { format_version: PLAN_FORMAT_VERSION, plan_id: compute_plan_id(&body)?, body })
    }

    /// Returns the code the user must pass to apply this plan.
    pub fn confirmation_code(&self) -> &str {
        &self.plan_id[..CONFIRMATION_LENGTH]
    }

    /// Returns the total bytes the plan would free.
    pub fn total_bytes(&self) -> u64 {
        self.body.actions.iter().map(|action| action.size).sum()
    }
}

// ----------------------------------------------------------------------------------------------
// Identifier and confirmation
// ----------------------------------------------------------------------------------------------

/// Returns the hexadecimal BLAKE3 digest of the canonical JSON form of a body.
pub fn compute_plan_id(body: &PlanBody) -> Result<String> {
    let bytes = serde_json::to_vec(body).context("Cannot serialize the plan")?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// Fails unless the code matches the start of the plan identifier.
pub fn verify_confirmation(plan: &Plan, code: Option<&str>) -> Result<()> {
    let expected = plan.confirmation_code();
    match code.map(str::trim) {
        None => bail!(
            "Nothing deleted: add --confirm {expected} to apply this plan (or --dry-run to only verify)."
        ),
        Some(given) if given.eq_ignore_ascii_case(expected) => Ok(()),
        Some(given) => bail!(
            "Nothing deleted: confirmation code {given:?} does not match this plan (expected {expected}). \
             Check that you are applying the plan you reviewed."
        ),
    }
}

// ----------------------------------------------------------------------------------------------
// Files
// ----------------------------------------------------------------------------------------------

/// Writes a plan as pretty JSON.
pub fn write_plan(plan: &Plan, path: &Path) -> Result<()> {
    let text = serde_json::to_string_pretty(plan).context("Cannot serialize the plan")?;
    fs::write(path, text).with_context(|| format!("Cannot write plan {}", path.display()))
}

/// Reads a plan and verifies its version and identifier.
pub fn read_plan(path: &Path) -> Result<Plan> {
    let text = fs::read_to_string(path).with_context(|| {
        format!("Cannot read plan {}: check the path printed by the scan.", path.display())
    })?;
    let plan: Plan = serde_json::from_str(&text)
        .with_context(|| format!("Plan {} is not valid: run a new scan.", path.display()))?;
    if plan.format_version != PLAN_FORMAT_VERSION {
        bail!(
            "Plan format {} is not supported (this version expects {PLAN_FORMAT_VERSION}): \
             run a new scan.",
            plan.format_version
        );
    }
    if compute_plan_id(&plan.body)? != plan.plan_id {
        bail!(
            "Plan {} was modified or corrupted after the scan: nothing deleted. Run a new scan.",
            path.display()
        );
    }
    Ok(plan)
}

// ----------------------------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PLAN_FILE_NAME: &str = "plan.json";

    fn sample_plan() -> Plan {
        Plan::new(PlanBody {
            created_at: "2026-01-01T00:00:00Z".to_string(),
            tool_version: "test".to_string(),
            mode: MatchMode::Content,
            source_root: "S".to_string(),
            destination_root: "D".to_string(),
            delete_read_only: false,
            remove_emptied_dirs: true,
            ignored_stream_names: Vec::new(),
            actions: vec![PlannedDeletion {
                source_rel: "a/b.txt".to_string(),
                destination_rel: "x/b.txt".to_string(),
                size: 3,
                source_mtime_ns: 42,
                blake3: "00".repeat(32),
            }],
        })
        .unwrap()
    }

    #[test]
    fn round_trips_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TEST_PLAN_FILE_NAME);
        let plan = sample_plan();
        write_plan(&plan, &path).unwrap();
        assert_eq!(read_plan(&path).unwrap(), plan);
        assert_eq!(plan.total_bytes(), 3);
    }

    #[test]
    fn detects_any_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TEST_PLAN_FILE_NAME);
        write_plan(&sample_plan(), &path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, text.replace("x/b.txt", "y/b.txt")).unwrap();
        assert!(read_plan(&path).is_err());
    }

    #[test]
    fn rejects_other_format_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TEST_PLAN_FILE_NAME);
        let mut plan = sample_plan();
        plan.format_version = PLAN_FORMAT_VERSION + 1;
        write_plan(&plan, &path).unwrap();
        assert!(read_plan(&path).is_err());
    }

    #[test]
    fn confirmation_must_match() {
        let plan = sample_plan();
        assert!(verify_confirmation(&plan, None).is_err());
        assert!(verify_confirmation(&plan, Some("wrong")).is_err());
        assert!(verify_confirmation(&plan, Some(plan.confirmation_code())).is_ok());
        let upper = plan.confirmation_code().to_uppercase();
        assert!(verify_confirmation(&plan, Some(&upper)).is_ok());
    }

    #[test]
    fn plans_without_ignored_streams_keep_their_identifier() {
        let plan = sample_plan();
        let text = serde_json::to_string(&plan).unwrap();
        assert!(!text.contains("ignored_stream_names"), "empty list is not written");
        let mut with_streams = plan.body.clone();
        with_streams.ignored_stream_names = vec!["com.dropbox.*".to_string()];
        assert_ne!(compute_plan_id(&with_streams).unwrap(), plan.plan_id);
    }

    #[test]
    fn identifier_depends_on_every_field() {
        let plan = sample_plan();
        let mut changed = plan.body.clone();
        changed.actions[0].size = 4;
        assert_ne!(compute_plan_id(&changed).unwrap(), plan.plan_id);
    }
}
