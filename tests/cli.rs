//! End-to-end tests of the real executable: scan, dry run, refused apply, apply, exit codes.

mod common;

use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;
use common::Sandbox;
use predicates::prelude::*;

/// Returns a command running the built executable.
fn program() -> Command {
    Command::new(env!("CARGO_BIN_EXE_fs-spacemaker"))
}

/// Runs a scan through the executable and returns (plan path, confirmation code).
fn scan_through_cli(sandbox: &Sandbox) -> (PathBuf, String) {
    let output = program()
        .args(["scan", "--mode", "content", "--threads", "2"])
        .arg("--source")
        .arg(&sandbox.source)
        .arg("--destination")
        .arg(&sandbox.destination)
        .arg("--work-dir")
        .arg(&sandbox.work)
        .assert()
        .success()
        .stdout(predicate::str::contains("Result (nothing deleted)"))
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    let plan_line = text.lines().find(|line| line.trim_start().starts_with("plan:")).unwrap();
    let plan = PathBuf::from(plan_line.trim_start().trim_start_matches("plan:").trim());
    let command_line = text.lines().rev().find(|line| line.contains("--confirm")).unwrap();
    let code = command_line.rsplit(' ').next().unwrap().to_string();
    (plan, code)
}

#[test]
fn full_workflow_through_the_executable() {
    let sandbox = Sandbox::new();
    sandbox.write_source("dup.txt", b"duplicated");
    sandbox.write_source("kept.txt", b"unique");
    sandbox.write_destination("copy/dup.txt", b"duplicated");
    let (plan, code) = scan_through_cli(&sandbox);
    assert!(plan.is_file());

    program()
        .args(["apply", "--dry-run", "--plan"])
        .arg(&plan)
        .assert()
        .success()
        .stdout(predicate::str::contains("would delete: 1 files"));
    assert!(sandbox.source_path("dup.txt").exists());

    program().args(["apply", "--plan"]).arg(&plan).assert().failure();
    program()
        .args(["apply", "--confirm", "deadbeef", "--plan"])
        .arg(&plan)
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not match"));
    assert!(sandbox.source_path("dup.txt").exists());

    program()
        .args(["apply", "--confirm", &code, "--plan"])
        .arg(&plan)
        .assert()
        .success()
        .stdout(predicate::str::contains("deleted: 1 files"));
    assert!(!sandbox.source_path("dup.txt").exists());
    assert!(sandbox.source_path("kept.txt").exists());
    assert!(sandbox.destination_path("copy/dup.txt").exists());
}

#[test]
fn overlapping_roots_fail_with_a_clear_message() {
    let sandbox = Sandbox::new();
    let inner = sandbox.source.join("inner");
    fs::create_dir_all(&inner).unwrap();
    program()
        .arg("scan")
        .arg("--source")
        .arg(&sandbox.source)
        .arg("--destination")
        .arg(&inner)
        .arg("--work-dir")
        .arg(&sandbox.work)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("inside source"));
}

#[test]
fn invalid_mode_is_rejected() {
    program().args(["scan", "--source", ".", "--destination", ".", "--mode", "fuzzy"]).assert().failure();
}

#[test]
fn help_lists_both_commands() {
    program()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("scan").and(predicate::str::contains("apply")));
}

#[test]
fn invalid_configuration_is_rejected() {
    let sandbox = Sandbox::new();
    let config = sandbox.work.join("bad.json");
    fs::write(&config, r#"{"unknown_key": 1}"#).unwrap();
    program()
        .arg("scan")
        .arg("--config")
        .arg(&config)
        .arg("--source")
        .arg(&sandbox.source)
        .arg("--destination")
        .arg(&sandbox.destination)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("Invalid configuration"));
}
