//! Load test (ignored by default, run with: cargo test --release --test stress -- --ignored --nocapture).
//! Builds many files with heavy duplication and size collisions, then checks correctness and timing.

mod common;

use std::time::Instant;

use common::{Sandbox, snapshot};
use fs_spacemaker::apply::ActionStatus;
use fs_spacemaker::matching::MatchMode;

/// Number of source files generated.
const FILE_COUNT: usize = 10_000;
/// One file in this many has no copy in the destination.
const UNIQUE_EVERY: usize = 7;
/// Content sizes cycle through this many values, forcing many same-size candidates.
const SIZE_CYCLE: usize = 50;

#[test]
#[ignore = "load test: run explicitly"]
fn many_files_with_collisions() {
    let sandbox = Sandbox::new();
    let mut expected_deleted = 0;
    for index in 0..FILE_COUNT {
        let size = 100 + index % SIZE_CYCLE;
        let mut content = format!("{index:08}").into_bytes();
        content.resize(size, b'.');
        sandbox.write_source(&format!("d{}/e{}/f{index}.dat", index % 20, index % 7), &content);
        if index % UNIQUE_EVERY == 0 {
            let mut altered = content.clone();
            altered[0] = b'X';
            sandbox.write_destination(&format!("copy/{index}.dat"), &altered);
        } else {
            sandbox.write_destination(&format!("copy/{}/{index}.bak", index % 13), &content);
            expected_deleted += 1;
        }
    }
    let destination_before = snapshot(&sandbox.destination);
    let started = Instant::now();
    let scan = sandbox.scan(MatchMode::Content);
    eprintln!("scan of {FILE_COUNT} files: {:?}", started.elapsed());
    assert_eq!(scan.plan.body.actions.len(), expected_deleted);
    let started = Instant::now();
    let output = sandbox.apply(&scan);
    eprintln!("apply: {:?}", started.elapsed());
    assert_eq!(output.count(ActionStatus::Deleted), expected_deleted);
    assert_eq!(snapshot(&sandbox.destination), destination_before);
}
