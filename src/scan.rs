//! Pre-processing ("scan"): inventories both roots, fingerprints only files that can match,
//! builds the frozen plan and writes plan.json, report.html and report.csv into a new run folder.
//! Nothing is modified on disk outside the work folder.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rayon::ThreadPool;
use rayon::prelude::*;
use serde::Serialize;

use crate::config::Config;
use crate::fingerprint::{Fingerprints, HashStats, HashingRuntime};
use crate::format::{format_bytes, timestamp_for_path, timestamp_iso};
use crate::hashing::to_hex;
use crate::index::{HashIndex, INDEX_FILE_NAME};
use crate::inventory::{Inventory, WalkOptions, scan_tree};
use crate::matching::{FileMatch, MatchContext, MatchMode, MatchOutcome, ResidualReason, match_files};
use crate::paths::{canonical_root, display_joined, display_path, ensure_disjoint_roots, is_same_or_inside};
use crate::plan::{PLAN_FILE_NAME, Plan, PlanBody, PlannedDeletion, write_plan};
use crate::platform::has_meaningful_streams;
use crate::progress::{ProgressMeter, print_step};
use crate::report::{
    CSV_FILE_NAME, Card, FolderFile, HTML_FILE_NAME, Phase, ReportData, ReportRow, Table, Tone, VolumeBar,
    aggregate_folders, write_report,
};
use crate::space::{VolumeSpace, volume_space};

// ----------------------------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------------------------

/// Sub-folder of the work folder holding one folder per scan.
pub const RUNS_DIR_NAME: &str = "runs";
/// Highest numeric suffix tried when two scans start in the same second.
const MAX_RUN_DIR_SUFFIX: u32 = 100;
/// Program version recorded in plans and reports.
pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Hex characters of the digest shown in the deletion table.
const DIGEST_PREVIEW_CHARS: usize = 16;

// ----------------------------------------------------------------------------------------------
// Model
// ----------------------------------------------------------------------------------------------

/// What to scan.
#[derive(Clone, Debug)]
pub struct ScanRequest {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub mode: MatchMode,
    pub config: Config,
    pub work_dir: PathBuf,
}

/// Headline figures of a scan.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ScanSummary {
    pub source_files: u64,
    pub source_bytes: u64,
    pub destination_files: u64,
    pub destination_bytes: u64,
    pub delete_files: u64,
    pub delete_bytes: u64,
    pub residual_files: u64,
    pub residual_bytes: u64,
    pub skipped_entries: u64,
    pub error_count: u64,
    pub source_space: VolumeSpace,
    pub destination_space: VolumeSpace,
}

impl ScanSummary {
    /// Returns the predicted free space of the source volume after applying the plan.
    pub fn predicted_source_free(&self) -> Option<u64> {
        let after = self.source_space.available? + self.delete_bytes;
        Some(self.source_space.total.map_or(after, |total| after.min(total)))
    }

    /// Returns whether the residual would fit in the destination's free space.
    pub fn residual_fits_destination(&self) -> Option<bool> {
        Some(self.residual_bytes <= self.destination_space.available?)
    }
}

/// Files and figures produced by a scan.
#[derive(Clone, Debug)]
pub struct ScanOutput {
    pub plan: Plan,
    pub summary: ScanSummary,
    pub run_dir: PathBuf,
    pub plan_path: PathBuf,
    pub html_path: PathBuf,
    pub csv_path: PathBuf,
    pub source_stats: HashStats,
    pub destination_stats: HashStats,
    pub apply_command: String,
}

/// Canonical folders of one scan.
struct ScanRoots {
    source: PathBuf,
    destination: PathBuf,
    work_dir: PathBuf,
}

/// Inventories and match result of one scan.
struct ScanFindings {
    source: Inventory,
    destination: Inventory,
    outcome: MatchOutcome,
}

// ----------------------------------------------------------------------------------------------
// Scan
// ----------------------------------------------------------------------------------------------

/// Runs the whole pre-processing and returns where its outputs are.
pub fn run_scan(request: &ScanRequest) -> Result<ScanOutput> {
    request.config.validate()?;
    print_step("Checking folders");
    let roots = prepare_roots(request)?;
    print_step("Inventory");
    let (source, destination) = inventory_both(&roots, &request.config);
    print_inventory("source", &source);
    print_inventory("destination", &destination);
    print_step("Fingerprinting and matching");
    let mut findings = ScanFindings { source, destination, outcome: MatchOutcome::default() };
    let (plan, stats) = fingerprint_and_plan(request, &roots, &mut findings)?;
    print_step("Writing plan and report");
    let run_dir = create_run_dir(&roots.work_dir, request.mode)?;
    let plan_path = run_dir.join(PLAN_FILE_NAME);
    write_plan(&plan, &plan_path)?;
    let summary = summarize(&findings);
    let apply_command = apply_command_for(&plan_path, &plan);
    let html_path = run_dir.join(HTML_FILE_NAME);
    let csv_path = run_dir.join(CSV_FILE_NAME);
    let data = build_report(request, &findings, &summary, &plan, &apply_command);
    write_report(data, request.config.report_max_rows, &html_path, &csv_path)?;
    let (source_stats, destination_stats) = stats;
    Ok(ScanOutput {
        plan,
        summary,
        run_dir,
        plan_path,
        html_path,
        csv_path,
        source_stats,
        destination_stats,
        apply_command,
    })
}

/// Resolves and checks the source, destination and work folders.
fn prepare_roots(request: &ScanRequest) -> Result<ScanRoots> {
    let source = canonical_root(&request.source)?;
    let destination = canonical_root(&request.destination)?;
    ensure_disjoint_roots(&source, &destination)?;
    fs::create_dir_all(&request.work_dir)
        .with_context(|| format!("Cannot create work folder {}", request.work_dir.display()))?;
    let work_dir = canonical_root(&request.work_dir)?;
    if is_same_or_inside(&work_dir, &source) || is_same_or_inside(&source, &work_dir) {
        bail!(
            "Work folder {} overlaps the source {}: choose a work folder elsewhere (--work-dir).",
            display_path(&work_dir),
            display_path(&source)
        );
    }
    println!("  source:      {}", display_path(&source));
    println!("  destination: {}", display_path(&destination));
    println!("  work folder: {}", display_path(&work_dir));
    Ok(ScanRoots { source, destination, work_dir })
}

/// Walks both roots at the same time (they are usually on different disks).
fn inventory_both(roots: &ScanRoots, config: &Config) -> (Inventory, Inventory) {
    let options = WalkOptions {
        excluded_names: config.excluded_names.clone(),
        excluded_paths: vec![roots.work_dir.clone()],
    };
    std::thread::scope(|scope| {
        let source = scope.spawn(|| scan_tree(&roots.source, &options));
        let destination = scan_tree(&roots.destination, &options);
        (source.join().expect("source walk thread panicked"), destination)
    })
}

/// Prints the size of one inventory.
fn print_inventory(label: &str, inventory: &Inventory) {
    println!(
        "  {label}: {} files, {}, {} folders, {} skipped, {} errors",
        inventory.files.len(),
        format_bytes(inventory.total_bytes()),
        inventory.dirs.len(),
        inventory.skipped.len(),
        inventory.errors.len()
    );
}

/// Fingerprints, matches, applies safety filters, updates the index and builds the plan.
fn fingerprint_and_plan(
    request: &ScanRequest,
    roots: &ScanRoots,
    findings: &mut ScanFindings,
) -> Result<(Plan, (HashStats, HashStats))> {
    let config = &request.config;
    let mut index = HashIndex::open(&roots.work_dir.join(INDEX_FILE_NAME))?;
    let source_key = display_path(&roots.source);
    let destination_key = display_path(&roots.destination);
    let pool = build_pool(config.scan_threads)?;
    let meter = ProgressMeter::new("fingerprinted");
    let runtime = HashingRuntime { pool: &pool, meter: &meter };
    let mut source_prints =
        Fingerprints::new(&findings.source, index.load_root(&source_key)?, config.partial_hash_bytes);
    let mut destination_prints = Fingerprints::new(
        &findings.destination,
        index.load_root(&destination_key)?,
        config.partial_hash_bytes,
    );
    let mut context = MatchContext {
        mode: request.mode,
        include_empty_files: config.include_empty_files,
        source: &mut source_prints,
        destination: &mut destination_prints,
        runtime: &runtime,
    };
    let mut outcome = match_files(&mut context);
    meter.print_line();
    apply_safety_filters(&mut outcome, findings, config.delete_read_only, &pool);
    index.store_root(&source_key, &source_prints.rows_to_index())?;
    index.store_root(&destination_key, &destination_prints.rows_to_index())?;
    let plan = build_plan(request, findings, &outcome, &source_prints)?;
    let stats = (source_prints.stats, destination_prints.stats);
    findings.outcome = outcome;
    Ok((plan, stats))
}

/// Builds a thread pool of the requested size.
pub fn build_pool(threads: usize) -> Result<ThreadPool> {
    rayon::ThreadPoolBuilder::new().num_threads(threads).build().context("Cannot start worker threads")
}

/// Moves matches that must not be deleted (read-only, same physical file) to the residual.
fn apply_safety_filters(
    outcome: &mut MatchOutcome,
    findings: &ScanFindings,
    delete_read_only: bool,
    pool: &ThreadPool,
) {
    let rejections: Vec<Option<ResidualReason>> = pool.install(|| {
        outcome
            .matches
            .par_iter()
            .map(|file_match| safety_rejection(file_match, findings, delete_read_only))
            .collect()
    });
    let mut kept = Vec::with_capacity(outcome.matches.len());
    for (file_match, rejection) in outcome.matches.iter().zip(rejections) {
        match rejection {
            None => kept.push(*file_match),
            Some(reason) => outcome.residual.push((file_match.source, reason)),
        }
    }
    outcome.matches = kept;
    outcome.residual.sort_by_key(|&(index, _)| index);
}

/// Returns why a match must not be deleted, if any.
fn safety_rejection(
    file_match: &FileMatch,
    findings: &ScanFindings,
    delete_read_only: bool,
) -> Option<ResidualReason> {
    if findings.source.files[file_match.source].is_read_only && !delete_read_only {
        return Some(ResidualReason::ReadOnly);
    }
    let source_path = findings.source.path_of(file_match.source);
    let destination_path = findings.destination.path_of(file_match.destination);
    match same_file::is_same_file(&source_path, destination_path) {
        Ok(false) => {}
        Ok(true) => return Some(ResidualReason::SameFile),
        Err(_) => return Some(ResidualReason::Unreadable),
    }
    match has_meaningful_streams(&source_path) {
        Ok(false) => None,
        Ok(true) => Some(ResidualReason::AlternateStreams),
        Err(_) => Some(ResidualReason::Unreadable),
    }
}

/// Builds the frozen plan from the matches.
fn build_plan(
    request: &ScanRequest,
    findings: &ScanFindings,
    outcome: &MatchOutcome,
    source_prints: &Fingerprints,
) -> Result<Plan> {
    let actions = outcome
        .matches
        .iter()
        .map(|file_match| {
            let record = &findings.source.files[file_match.source];
            let digest = source_prints
                .full_digest(file_match.source)
                .with_context(|| format!("Internal error: no digest for matched file {}", record.rel))?;
            Ok(PlannedDeletion {
                source_rel: record.rel.clone(),
                destination_rel: findings.destination.files[file_match.destination].rel.clone(),
                size: record.size,
                source_mtime_ns: record.mtime_ns,
                blake3: to_hex(&digest),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Plan::new(PlanBody {
        created_at: timestamp_iso(),
        tool_version: TOOL_VERSION.to_string(),
        mode: request.mode,
        source_root: root_text(&findings.source.root)?,
        destination_root: root_text(&findings.destination.root)?,
        delete_read_only: request.config.delete_read_only,
        remove_emptied_dirs: request.config.remove_emptied_dirs,
        actions,
    })
}

/// Returns a canonical root as text (roots were checked to be valid Unicode).
fn root_text(root: &Path) -> Result<String> {
    root.to_str().map(str::to_string).context("Root path is not valid Unicode")
}

/// Creates a new, uniquely named run folder.
fn create_run_dir(work_dir: &Path, mode: MatchMode) -> Result<PathBuf> {
    let base = format!("{}-{}", timestamp_for_path(), mode.tag());
    let runs = work_dir.join(RUNS_DIR_NAME);
    for suffix in 1..=MAX_RUN_DIR_SUFFIX {
        let name = if suffix == 1 { base.clone() } else { format!("{base}-{suffix}") };
        let candidate = runs.join(name);
        if !candidate.exists() {
            fs::create_dir_all(&candidate)
                .with_context(|| format!("Cannot create {}", candidate.display()))?;
            return Ok(candidate);
        }
    }
    bail!("Cannot create a unique run folder under {}: clean it up.", runs.display())
}

/// Returns the command line that applies the plan.
fn apply_command_for(plan_path: &Path, plan: &Plan) -> String {
    let program =
        std::env::current_exe().map_or_else(|_| "fs-spacemaker".to_string(), |path| display_path(&path));
    format!(
        "\"{program}\" apply --plan \"{}\" --confirm {}",
        display_path(plan_path),
        plan.confirmation_code()
    )
}

// ----------------------------------------------------------------------------------------------
// Summary and report
// ----------------------------------------------------------------------------------------------

/// Computes the headline figures.
fn summarize(findings: &ScanFindings) -> ScanSummary {
    let source = &findings.source;
    let delete_bytes: u64 = findings.outcome.matches.iter().map(|m| source.files[m.source].size).sum();
    let delete_files = findings.outcome.matches.len() as u64;
    ScanSummary {
        source_files: source.files.len() as u64,
        source_bytes: source.total_bytes(),
        destination_files: findings.destination.files.len() as u64,
        destination_bytes: findings.destination.total_bytes(),
        delete_files,
        delete_bytes,
        residual_files: source.files.len() as u64 - delete_files,
        residual_bytes: source.total_bytes() - delete_bytes,
        skipped_entries: (source.skipped.len() + findings.destination.skipped.len()) as u64,
        error_count: (source.errors.len() + findings.destination.errors.len()) as u64,
        source_space: volume_space(&source.root),
        destination_space: volume_space(&findings.destination.root),
    }
}

/// Assembles the plan report.
fn build_report(
    request: &ScanRequest,
    findings: &ScanFindings,
    summary: &ScanSummary,
    plan: &Plan,
    apply_command: &str,
) -> ReportData {
    let removed: HashSet<usize> = findings.outcome.matches.iter().map(|m| m.source).collect();
    let folder_files: Vec<FolderFile> = findings
        .source
        .files
        .iter()
        .enumerate()
        .map(|(index, file)| FolderFile {
            rel: &file.rel,
            size: file.size,
            is_removed: removed.contains(&index),
        })
        .collect();
    ReportData {
        phase: Phase::Plan,
        title: "FS SpaceMaker: plan (nothing deleted yet)".to_string(),
        generated_at: timestamp_iso(),
        tool_version: TOOL_VERSION.to_string(),
        plan_id: plan.plan_id.clone(),
        apply_command: apply_command.to_string(),
        mode: request.mode,
        source_root: display_path(&findings.source.root),
        destination_root: display_path(&findings.destination.root),
        notes: build_notes(request, findings, summary),
        cards: build_cards(summary),
        volumes: build_volumes(findings, summary),
        folders: aggregate_folders(&findings.source.dirs, &folder_files),
        tables: build_tables(request.mode, findings, plan),
    }
}

/// Explains, in sentences, what applying the plan will do.
fn build_notes(request: &ScanRequest, findings: &ScanFindings, summary: &ScanSummary) -> Vec<String> {
    let source = display_path(&findings.source.root);
    let mut notes = vec![
        mode_explanation(request.mode).to_string(),
        format!(
            "{} files ({}) would be deleted from {source}. Nothing has been modified yet.",
            summary.delete_files,
            format_bytes(summary.delete_bytes)
        ),
        format!(
            "{} files ({}) would remain in the source: review them before moving them to the destination.",
            summary.residual_files,
            format_bytes(summary.residual_bytes)
        ),
        "Before each deletion, apply re-checks size and date, then compares source and destination \
         byte by byte; any difference skips the file. The destination is never modified."
            .to_string(),
    ];
    if let (Some(now), Some(after)) = (summary.source_space.available, summary.predicted_source_free()) {
        notes.push(format!(
            "Source volume free space: {} now, about {} after (logical sizes; compression, sparse files or \
             other hard links can make the real gain smaller).",
            format_bytes(now),
            format_bytes(after)
        ));
    }
    notes.push(residual_fit_note(summary));
    if request.config.remove_emptied_dirs {
        notes.push(
            "Folders emptied by the deletions will be removed; folders already empty are kept.".to_string(),
        );
    }
    if summary.error_count > 0 || summary.skipped_entries > 0 {
        notes.push(format!(
            "{} entries were skipped and {} could not be read: see the Skipped and Errors tabs.",
            summary.skipped_entries, summary.error_count
        ));
    }
    notes
}

/// Returns the one-sentence explanation of a mode.
fn mode_explanation(mode: MatchMode) -> &'static str {
    match mode {
        MatchMode::Content => {
            "Mode content: a source file is deletable when a destination file anywhere has identical content \
             (same size, same BLAKE3 digest), whatever its name or folder."
        }
        MatchMode::SamePath => {
            "Mode same-path: a source file is deletable when the destination file at the same relative \
             path has identical content."
        }
        MatchMode::Relocated => {
            "Mode relocated: a source folder is deletable when its whole subtree (same names, same \
             structure, identical content) exists under some destination folder, even renamed or moved."
        }
    }
}

/// Says whether the residual would fit in the destination.
fn residual_fit_note(summary: &ScanSummary) -> String {
    let residual = format_bytes(summary.residual_bytes);
    match (summary.residual_fits_destination(), summary.destination_space.available) {
        (Some(true), Some(free)) => format!(
            "The residual ({residual}) fits in the destination's free space ({}).",
            format_bytes(free)
        ),
        (Some(false), Some(free)) => format!(
            "Warning: the residual ({residual}) does NOT fit in the destination's free space ({}).",
            format_bytes(free)
        ),
        _ => format!("Destination free space unknown; residual is {residual}."),
    }
}

/// Builds the headline cards.
fn build_cards(summary: &ScanSummary) -> Vec<Card> {
    let fits = summary.residual_fits_destination();
    vec![
        card(
            "Source scanned",
            Some(summary.source_bytes),
            Some(summary.source_files),
            "regular files",
            Tone::Neutral,
        ),
        card(
            "Will be freed",
            Some(summary.delete_bytes),
            Some(summary.delete_files),
            "verified duplicates",
            Tone::Good,
        ),
        card(
            "Remains on source",
            Some(summary.residual_bytes),
            Some(summary.residual_files),
            "to evaluate",
            Tone::Warn,
        ),
        card("Source free after", summary.predicted_source_free(), None, "predicted", Tone::Good),
        card(
            "Destination free",
            summary.destination_space.available,
            None,
            if fits == Some(false) { "residual does NOT fit" } else { "residual fits" },
            if fits == Some(false) { Tone::Bad } else { Tone::Neutral },
        ),
        card(
            "Skipped / errors",
            None,
            Some(summary.skipped_entries + summary.error_count),
            "see tabs below",
            if summary.skipped_entries + summary.error_count > 0 { Tone::Warn } else { Tone::Neutral },
        ),
    ]
}

/// Builds one card.
pub fn card(label: &str, bytes: Option<u64>, count: Option<u64>, note: &str, tone: Tone) -> Card {
    Card { label: label.to_string(), bytes, count, note: note.to_string(), tone }
}

/// Builds the before/after volume bars.
fn build_volumes(findings: &ScanFindings, summary: &ScanSummary) -> Vec<VolumeBar> {
    vec![
        VolumeBar {
            label: "Source volume".to_string(),
            path: display_path(&findings.source.root),
            total: summary.source_space.total,
            free_before: summary.source_space.available,
            free_after: summary.predicted_source_free(),
            note: format!("{} freed by this plan", format_bytes(summary.delete_bytes)),
        },
        VolumeBar {
            label: "Destination volume".to_string(),
            path: display_path(&findings.destination.root),
            total: summary.destination_space.total,
            free_before: summary.destination_space.available,
            free_after: summary.destination_space.available,
            note: format!(
                "unchanged; moving the residual later would use {}",
                format_bytes(summary.residual_bytes)
            ),
        },
    ]
}

/// Builds the detail tables.
fn build_tables(mode: MatchMode, findings: &ScanFindings, plan: &Plan) -> Vec<Table> {
    let source_root = display_path(&findings.source.root);
    let destination_root = display_path(&findings.destination.root);
    let mut tables = vec![
        Table::new(
            "deletions",
            "Will be deleted",
            "Source files with an identical destination copy (re-verified byte by byte at apply time).",
            ["Source file", "Identical destination file", "Size", "Mode", "Digest"],
            (&source_root, &destination_root),
            deletion_rows(mode, plan),
        ),
        Table::new(
            "residual",
            "Remains on source",
            "Source files kept, with the reason. This is what you will need to move or evaluate.",
            ["Source file", "", "Size", "Reason", "Explanation"],
            (&source_root, ""),
            residual_rows(mode, findings),
        ),
    ];
    if mode == MatchMode::Relocated {
        tables.push(Table::new(
            "trees",
            "Relocated folders",
            "Source folders found whole under another destination folder.",
            ["Source folder", "Destination folder", "Size", "", "Files"],
            ("", ""),
            tree_rows(findings, &source_root, &destination_root),
        ));
    }
    tables.push(Table::new(
        "skipped",
        "Skipped",
        "Entries never touched: links, junctions, cloud placeholders, special files.",
        ["Path", "Side", "Size", "Reason", "Explanation"],
        ("", ""),
        skipped_rows(findings),
    ));
    tables.push(Table::new(
        "errors",
        "Errors",
        "Paths that could not be read during the inventory (never deleted).",
        ["Path", "Side", "", "", "Message"],
        ("", ""),
        error_rows(findings),
    ));
    tables
}

/// Rows of the deletion table.
fn deletion_rows(mode: MatchMode, plan: &Plan) -> Vec<ReportRow> {
    plan.body
        .actions
        .iter()
        .map(|action| ReportRow {
            path: action.source_rel.clone(),
            other: action.destination_rel.clone(),
            size: action.size,
            tag: mode.tag().to_string(),
            detail: action.blake3[..DIGEST_PREVIEW_CHARS].to_string(),
        })
        .collect()
}

/// Rows of the residual table.
fn residual_rows(mode: MatchMode, findings: &ScanFindings) -> Vec<ReportRow> {
    findings
        .outcome
        .residual
        .iter()
        .map(|&(index, reason)| ReportRow {
            path: findings.source.files[index].rel.clone(),
            other: String::new(),
            size: findings.source.files[index].size,
            tag: reason.tag().to_string(),
            detail: reason.describe(mode).to_string(),
        })
        .collect()
}

/// Rows of the relocated folder table (full paths).
fn tree_rows(findings: &ScanFindings, source_root: &str, destination_root: &str) -> Vec<ReportRow> {
    findings
        .outcome
        .trees
        .iter()
        .map(|tree| ReportRow {
            path: display_joined(source_root, &tree.source_dir),
            other: display_joined(destination_root, &tree.destination_dir),
            size: tree.bytes,
            tag: String::new(),
            detail: format!("{} files", tree.files),
        })
        .collect()
}

/// Rows of the skipped table, both sides.
fn skipped_rows(findings: &ScanFindings) -> Vec<ReportRow> {
    let sides = [("source", &findings.source), ("destination", &findings.destination)];
    sides
        .iter()
        .flat_map(|(side, inventory)| {
            inventory.skipped.iter().map(move |entry| ReportRow {
                path: entry.path.clone(),
                other: side.to_string(),
                size: entry.size,
                tag: entry.reason.tag().to_string(),
                detail: entry.reason.describe().to_string(),
            })
        })
        .collect()
}

/// Rows of the error table, both sides.
fn error_rows(findings: &ScanFindings) -> Vec<ReportRow> {
    let sides = [("source", &findings.source), ("destination", &findings.destination)];
    sides
        .iter()
        .flat_map(|(side, inventory)| {
            inventory.errors.iter().map(move |error| ReportRow {
                path: error.path.clone(),
                other: side.to_string(),
                size: 0,
                tag: "error".to_string(),
                detail: error.message.clone(),
            })
        })
        .collect()
}
