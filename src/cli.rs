//! Command-line interface: parses arguments, merges them over the configuration and prints results.
//! Usage: fs-spacemaker scan --source <dir> --destination <dir> [--mode content|same-path|relocated]
//!        fs-spacemaker apply --plan <plan.json> (--confirm <code> | --dry-run)

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::apply::{ActionStatus, ApplyOutput, ApplyRequest, run_apply};
use crate::config::{Config, load_config, resolve_work_dir};
use crate::format::format_bytes;
use crate::matching::MatchMode;
use crate::paths::display_path;
use crate::progress::print_step;
use crate::scan::{ScanOutput, ScanRequest, run_scan};

// ----------------------------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------------------------

/// Exit code of an apply run where some deletions failed on I/O errors.
const EXIT_PARTIAL_FAILURE: u8 = 2;

// ----------------------------------------------------------------------------------------------
// Arguments
// ----------------------------------------------------------------------------------------------

/// Safely frees disk space by deleting source files verified identical to destination files.
#[derive(Debug, Parser)]
#[command(name = "fs-spacemaker", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Pre-processing: inventory, fingerprint, plan and report. Never deletes anything.
    Scan(ScanArgs),
    /// Processing: verify each planned deletion byte by byte, then delete the source file.
    Apply(ApplyArgs),
}

#[derive(Debug, Args)]
struct ScanArgs {
    /// Folder or drive to free (files proven duplicated in the destination will be deleted).
    #[arg(long)]
    source: PathBuf,
    /// Folder or drive to keep (never modified).
    #[arg(long)]
    destination: PathBuf,
    /// How a source file must correspond to a destination file.
    #[arg(long, value_enum, default_value_t = MatchMode::Content)]
    mode: MatchMode,
    /// Configuration file (default: fs-spacemaker.config.json in the installation folder, if present).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Folder for the index, plans and reports (default: fs-spacemaker-work in the installation folder).
    #[arg(long)]
    work_dir: Option<PathBuf>,
    /// Parallel file reads.
    #[arg(long)]
    threads: Option<usize>,
    /// Also delete empty source files (content mode; they free no space).
    #[arg(long)]
    include_empty_files: bool,
    /// Also delete read-only source files.
    #[arg(long)]
    delete_read_only: bool,
    /// NTFS stream name holding no user data, ignored when comparing (repeatable; '*' at the end
    /// matches a prefix, e.g. --ignore-stream "com.dropbox.*").
    #[arg(long = "ignore-stream", value_name = "NAME")]
    ignored_streams: Vec<String>,
}

#[derive(Debug, Args)]
struct ApplyArgs {
    /// Plan file printed by the scan.
    #[arg(long)]
    plan: PathBuf,
    /// Confirmation code printed by the scan (first characters of the plan identifier).
    #[arg(long, required_unless_present = "dry_run")]
    confirm: Option<String>,
    /// Run every verification but delete nothing.
    #[arg(long)]
    dry_run: bool,
    /// Configuration file.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Parallel verifications.
    #[arg(long)]
    threads: Option<usize>,
}

// ----------------------------------------------------------------------------------------------
// Commands
// ----------------------------------------------------------------------------------------------

/// Parses the command line and runs the chosen command.
pub fn run() -> Result<ExitCode> {
    match Cli::parse().command {
        Command::Scan(args) => run_scan_command(args),
        Command::Apply(args) => run_apply_command(args),
    }
}

/// Runs a scan and prints its summary.
fn run_scan_command(args: ScanArgs) -> Result<ExitCode> {
    let mut config = load_config(args.config.as_deref())?;
    if let Some(threads) = args.threads {
        config.scan_threads = threads;
    }
    if let Some(dir) = args.work_dir {
        config.work_dir = Some(dir);
    }
    config.include_empty_files |= args.include_empty_files;
    config.delete_read_only |= args.delete_read_only;
    config.ignored_stream_names.extend(args.ignored_streams);
    config.validate()?;
    let work_dir = resolve_work_dir(&config)?;
    let request =
        ScanRequest { source: args.source, destination: args.destination, mode: args.mode, config, work_dir };
    let output = run_scan(&request)?;
    print_scan_summary(&output);
    Ok(ExitCode::SUCCESS)
}

/// Prints what the scan found and how to apply it.
fn print_scan_summary(output: &ScanOutput) {
    let summary = &output.summary;
    print_step("Result (nothing deleted)");
    println!("  will be freed:     {} files, {}", summary.delete_files, format_bytes(summary.delete_bytes));
    println!(
        "  remains on source: {} files, {}",
        summary.residual_files,
        format_bytes(summary.residual_bytes)
    );
    if let (Some(now), Some(after)) = (summary.source_space.available, summary.predicted_source_free()) {
        println!("  source free space: {} now -> about {} after", format_bytes(now), format_bytes(after));
    }
    println!("  skipped entries:   {}, read errors: {}", summary.skipped_entries, summary.error_count);
    println!(
        "  files read:        {} source, {} destination",
        output.source_stats.files_read, output.destination_stats.files_read
    );
    println!("  report: {}", display_path(&output.html_path));
    println!("  csv:    {}", display_path(&output.csv_path));
    println!("  plan:   {}", display_path(&output.plan_path));
    println!();
    println!("Review the report, then verify without deleting:");
    println!("  {} --dry-run", output.apply_command.split(" --confirm").next().unwrap_or_default());
    println!("Apply the plan:");
    println!("  {}", output.apply_command);
}

/// Runs an apply (or dry run) and prints its summary.
fn run_apply_command(args: ApplyArgs) -> Result<ExitCode> {
    let mut config: Config = load_config(args.config.as_deref())?;
    if let Some(threads) = args.threads {
        config.apply_threads = threads;
    }
    let request =
        ApplyRequest { plan_path: args.plan, confirmation: args.confirm, is_dry_run: args.dry_run, config };
    let output = run_apply(&request)?;
    print_apply_summary(&output);
    if output.has_failures() {
        return Ok(ExitCode::from(EXIT_PARTIAL_FAILURE));
    }
    Ok(ExitCode::SUCCESS)
}

/// Prints what the apply did.
fn print_apply_summary(output: &ApplyOutput) {
    let (files, bytes) = output.removed_totals();
    print_step(if output.is_dry_run { "Result (dry run: nothing deleted)" } else { "Result" });
    let verb = if output.is_dry_run { "would delete" } else { "deleted" };
    println!("  {verb}: {files} files, {}", format_bytes(bytes));
    println!("  skipped by safety checks: {}", output.skipped_count());
    println!("  failed: {}", output.count(ActionStatus::Failed));
    if !output.is_dry_run {
        println!("  emptied folders removed: {}", output.removed_dirs.len());
        if let Some(free) = output.space_after.available {
            println!("  source free space now: {}", format_bytes(free));
        }
    }
    println!("  journal: {}", display_path(&output.journal_path));
    println!("  report:  {}", display_path(&output.html_path));
}
