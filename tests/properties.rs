//! Property-based tests: random source/destination trees with deliberate collisions (same names,
//! same sizes, same contents) in every mode. Invariants checked after a real apply:
//! 1. the destination is byte-for-byte unchanged;
//! 2. every deleted source file had identical content at its planned destination twin;
//! 3. every source file not deleted is unchanged;
//! 4. the freed bytes equal the prediction;
//! 5. in content mode, nothing deletable was missed (completeness).

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::{Sandbox, snapshot};
use fs_spacemaker::matching::MatchMode;
use fs_spacemaker::paths::name_key;
use proptest::prelude::*;

/// Folder, file name and content pools, small on purpose to force collisions.
const FOLDERS: [&str; 5] = ["", "a", "a/b", "c", "c/a"];
const NAMES: [&str; 4] = ["x.txt", "y.bin", "z", "w.dat"];
const CONTENTS: [&[u8]; 6] = [b"", b"1", b"22", b"hello", b"hellO", b"another-longer-content"];
const MAX_FILES_PER_SIDE: usize = 12;
/// Default number of random cases; override with the FS_SPACEMAKER_PROPTEST_CASES variable.
const CASES: u32 = 48;
const CASES_VARIABLE: &str = "FS_SPACEMAKER_PROPTEST_CASES";

/// One generated file: indices into the pools.
type FileSpec = (usize, usize, usize);

/// Returns '/'-joined relative path of a spec.
fn rel_of((folder, name, _): FileSpec) -> String {
    if FOLDERS[folder].is_empty() {
        NAMES[name].to_string()
    } else {
        format!("{}/{}", FOLDERS[folder], NAMES[name])
    }
}

/// Generates a list of files with unique paths.
fn tree_strategy() -> impl Strategy<Value = Vec<FileSpec>> {
    prop::collection::vec((0..FOLDERS.len(), 0..NAMES.len(), 0..CONTENTS.len()), 0..MAX_FILES_PER_SIDE)
        .prop_map(|specs| {
            let mut seen = BTreeSet::new();
            specs.into_iter().filter(|spec| seen.insert(rel_of(*spec))).collect()
        })
}

/// Generates a matching mode.
fn mode_strategy() -> impl Strategy<Value = MatchMode> {
    prop_oneof![Just(MatchMode::Content), Just(MatchMode::SamePath), Just(MatchMode::Relocated)]
}

/// Writes a tree into one side of the sandbox (file names never equal folder names).
fn write_tree(sandbox: &Sandbox, specs: &[FileSpec], is_source: bool) {
    for spec in specs {
        let rel = rel_of(*spec);
        if is_source {
            sandbox.write_source(&rel, CONTENTS[spec.2])
        } else {
            sandbox.write_destination(&rel, CONTENTS[spec.2])
        };
    }
}

/// Checks invariants 1 to 4 and returns the deleted source paths.
fn check_soundness(
    before_source: &BTreeMap<String, Vec<u8>>,
    before_destination: &BTreeMap<String, Vec<u8>>,
    sandbox: &Sandbox,
    scan: &fs_spacemaker::scan::ScanOutput,
) -> BTreeSet<String> {
    assert_eq!(&snapshot(&sandbox.destination), before_destination, "destination changed");
    let after_source = snapshot(&sandbox.source);
    let deleted: BTreeSet<String> =
        before_source.keys().filter(|rel| !after_source.contains_key(*rel)).cloned().collect();
    for (rel, content) in &after_source {
        assert_eq!(Some(content), before_source.get(rel), "kept file {rel} changed");
    }
    for action in &scan.plan.body.actions {
        if deleted.contains(&action.source_rel) {
            assert_eq!(
                before_source.get(&action.source_rel),
                before_destination.get(&action.destination_rel)
            );
        }
    }
    let planned: BTreeSet<String> =
        scan.plan.body.actions.iter().map(|action| action.source_rel.clone()).collect();
    assert_eq!(deleted, planned, "deleted files differ from the plan");
    let freed: u64 = deleted.iter().map(|rel| before_source[rel].len() as u64).sum();
    assert_eq!(freed, scan.summary.delete_bytes, "freed bytes differ from the prediction");
    deleted
}

/// Checks the mode-specific pairing rule of each action.
fn check_pairing(mode: MatchMode, scan: &fs_spacemaker::scan::ScanOutput) {
    for action in &scan.plan.body.actions {
        let source_name = action.source_rel.rsplit('/').next().unwrap();
        let destination_name = action.destination_rel.rsplit('/').next().unwrap();
        match mode {
            MatchMode::Content => {}
            MatchMode::SamePath => {
                assert_eq!(name_key(&action.source_rel), name_key(&action.destination_rel))
            }
            MatchMode::Relocated | MatchMode::Merge => {
                assert_eq!(name_key(source_name), name_key(destination_name))
            }
        }
    }
}

/// Checks that content mode deletes every non-empty source file whose content exists in the destination.
fn check_content_completeness(
    before_source: &BTreeMap<String, Vec<u8>>,
    before_destination: &BTreeMap<String, Vec<u8>>,
    deleted: &BTreeSet<String>,
) {
    let destination_contents: BTreeSet<&Vec<u8>> = before_destination.values().collect();
    for (rel, content) in before_source {
        let is_deletable = !content.is_empty() && destination_contents.contains(content);
        assert_eq!(deleted.contains(rel), is_deletable, "completeness failed for {rel}");
    }
}

/// Returns the number of random cases to run.
fn configured_cases() -> u32 {
    std::env::var(CASES_VARIABLE).ok().and_then(|value| value.parse().ok()).unwrap_or(CASES)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(configured_cases()))]

    #[test]
    fn apply_never_loses_unique_data(
        source in tree_strategy(),
        destination in tree_strategy(),
        mode in mode_strategy(),
    ) {
        let sandbox = Sandbox::new();
        write_tree(&sandbox, &source, true);
        write_tree(&sandbox, &destination, false);
        let before_source = snapshot(&sandbox.source);
        let before_destination = snapshot(&sandbox.destination);

        let scan = sandbox.scan(mode);
        check_pairing(mode, &scan);
        sandbox.apply(&scan);
        let deleted = check_soundness(&before_source, &before_destination, &sandbox, &scan);
        if mode == MatchMode::Content {
            check_content_completeness(&before_source, &before_destination, &deleted);
        }
    }
}
