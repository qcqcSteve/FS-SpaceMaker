//! FS SpaceMaker library: inventory, fingerprinting, matching, planning, reporting and verified deletion.
//! The binary (src/main.rs) is a thin command-line wrapper around `scan::run_scan` and `apply::run_apply`.
//! Dependencies: see Cargo.toml.

pub mod apply;
pub mod cli;
pub mod config;
pub mod fingerprint;
pub mod format;
pub mod hashing;
pub mod index;
pub mod inventory;
pub mod matching;
pub mod outputs;
pub mod paths;
pub mod plan;
pub mod platform;
pub mod progress;
pub mod report;
pub mod scan;
pub mod space;
pub mod tree_match;
