//! Pre-processing ("scan"): inventories both roots, fingerprints only files that can match,
//! builds the frozen plan and writes plan.json, report.html and report.csv into the reports folder
//! (replacing the previous ones). Nothing is modified on disk outside the work and reports folders.
//! In merge mode the plan also lists, for every source file without a same-name-and-size twin, the
//! free destination path it will be moved to.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use rayon::ThreadPool;
use rayon::prelude::*;
use serde::Serialize;

use crate::config::Config;
use crate::fingerprint::{Fingerprints, HashStats, HashingRuntime};
use crate::format::{format_bytes, timestamp_iso};
use crate::hashing::to_hex;
use crate::index::{HashIndex, INDEX_FILE_NAME};
use crate::inventory::{Inventory, MetadataDir, WalkOptions, scan_tree};
use crate::matching::{FileMatch, MatchContext, MatchMode, MatchOutcome, ResidualReason, match_files};
use crate::outputs::{PLAN_FILE_NAME, SCAN_CSV_FILE_NAME, SCAN_HTML_FILE_NAME, clear_previous_outputs};
use crate::paths::{
    canonical_root, display_joined, display_path, ensure_disjoint_roots, is_same_or_inside, name_key,
};
use crate::plan::{Plan, PlanBody, PlannedDeletion, PlannedMove, write_plan};
use crate::platform::{StreamCheck, check_streams, is_access_denied_error, probe_delete_access};
use crate::progress::{ProgressMeter, print_step};
use crate::report::{
    Card, FolderFile, Phase, ReportData, ReportRow, Table, Tone, VolumeBar, aggregate_folders, write_report,
};
use crate::space::{VolumeSpace, volume_space};
use crate::targets::TargetAllocator;
use crate::vcs::{MetadataKind, plan_metadata_removals, read_svn_summary};

// ----------------------------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------------------------

/// Program version recorded in plans and reports.
pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Hex characters of the digest shown in the deletion table.
const DIGEST_PREVIEW_CHARS: usize = 16;
/// Prefix that makes PowerShell (the default Windows shell) run a quoted program path.
const SHELL_CALL_PREFIX: &str = if cfg!(windows) { "& " } else { "" };
/// Tag of version-control metadata rows in the Skipped table.
const METADATA_TAG: &str = "version-control";
/// Merge mode: residual reasons that still allow moving the file (only its deletion was refused).
const MOVABLE_REASONS: [ResidualReason; 4] = [
    ResidualReason::NoCandidate,
    ResidualReason::ReadOnly,
    ResidualReason::SameFile,
    ResidualReason::AlternateStreams,
];
/// Detail shown instead of a digest for merge-mode deletions.
const NAME_AND_SIZE_DETAIL: &str = "same name and size";

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
    /// Fingerprint index.
    pub work_dir: PathBuf,
    /// Plan, reports and journals.
    pub reports_dir: PathBuf,
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
    /// Merge mode: files moved into the destination.
    pub move_files: u64,
    pub move_bytes: u64,
    /// Subversion metadata planned for removal with its emptied working copy.
    pub metadata_removal_bytes: u64,
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
        let after = self.source_space.available? + self.delete_bytes + self.metadata_removal_bytes;
        Some(self.source_space.total.map_or(after, |total| after.min(total)))
    }

    /// Returns whether the residual and the planned moves would fit in the destination's free space.
    pub fn residual_fits_destination(&self) -> Option<bool> {
        Some(self.residual_bytes + self.move_bytes <= self.destination_space.available?)
    }
}

/// Files and figures produced by a scan.
#[derive(Clone, Debug)]
pub struct ScanOutput {
    pub plan: Plan,
    pub summary: ScanSummary,
    pub output_dir: PathBuf,
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
    reports_dir: PathBuf,
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
    // One set of outputs: the previous plan and reports are replaced.
    clear_previous_outputs(&roots.reports_dir)?;
    let output_dir = roots.reports_dir.clone();
    let plan_path = output_dir.join(PLAN_FILE_NAME);
    write_plan(&plan, &plan_path)?;
    let summary = summarize(&findings, &plan);
    let apply_command = apply_command_for(&plan_path, &plan);
    let html_path = output_dir.join(SCAN_HTML_FILE_NAME);
    let csv_path = output_dir.join(SCAN_CSV_FILE_NAME);
    let data = build_report(request, &findings, &summary, &plan, &apply_command);
    write_report(data, request.config.report_max_rows, &html_path, &csv_path)?;
    let (source_stats, destination_stats) = stats;
    Ok(ScanOutput {
        plan,
        summary,
        output_dir,
        plan_path,
        html_path,
        csv_path,
        source_stats,
        destination_stats,
        apply_command,
    })
}

/// Resolves and checks the source, destination, work and reports folders.
fn prepare_roots(request: &ScanRequest) -> Result<ScanRoots> {
    let source = canonical_root(&request.source)?;
    let destination = canonical_root(&request.destination)?;
    ensure_disjoint_roots(&source, &destination)?;
    let work_dir = prepare_output_dir(&request.work_dir, &source, ("Work", "--work-dir"))?;
    let reports_dir = prepare_output_dir(&request.reports_dir, &source, ("Reports", "--reports-dir"))?;
    println!("  source:      {}", display_path(&source));
    println!("  destination: {}", display_path(&destination));
    println!("  work folder: {}", display_path(&work_dir));
    println!("  reports:     {}", display_path(&reports_dir));
    Ok(ScanRoots { source, destination, work_dir, reports_dir })
}

/// Creates an output folder and fails if it overlaps the source (its files could be deleted).
fn prepare_output_dir(dir: &Path, source: &Path, (label, option): (&str, &str)) -> Result<PathBuf> {
    fs::create_dir_all(dir).with_context(|| format!("Cannot create {label} folder {}", dir.display()))?;
    let canonical = canonical_root(dir)?;
    if is_same_or_inside(&canonical, source) || is_same_or_inside(source, &canonical) {
        bail!(
            "{label} folder {} overlaps the source {}: choose a folder elsewhere ({option}).",
            display_path(&canonical),
            display_path(source)
        );
    }
    Ok(canonical)
}

/// Walks both roots at the same time (they are usually on different disks).
fn inventory_both(roots: &ScanRoots, config: &Config) -> (Inventory, Inventory) {
    let options = WalkOptions {
        excluded_names: config.excluded_names.clone(),
        excluded_paths: vec![roots.work_dir.clone(), roots.reports_dir.clone()],
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
    let index = HashIndex::open(&roots.work_dir.join(INDEX_FILE_NAME))?;
    let source_key = display_path(&roots.source);
    let destination_key = display_path(&roots.destination);
    let source_indexed = index.load_root(&source_key)?;
    let destination_indexed = index.load_root(&destination_key)?;
    let index = Mutex::new(index);
    let pool = build_pool(config.scan_threads)?;
    let meter = ProgressMeter::new("fingerprinted");
    let runtime = HashingRuntime { pool: &pool, meter: &meter, index: Some(&index) };
    let mut source_prints =
        Fingerprints::new(&findings.source, &source_key, source_indexed, config.partial_hash_bytes);
    let mut destination_prints = Fingerprints::new(
        &findings.destination,
        &destination_key,
        destination_indexed,
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
    apply_safety_filters(&mut outcome, findings, config, &pool);
    if request.mode == MatchMode::Merge {
        select_moves(&mut outcome, &findings.source, &pool);
    }
    let mut index = index.into_inner().map_err(|_| anyhow::anyhow!("index lock poisoned"))?;
    prune_index(&mut index, &source_key, &findings.source)?;
    prune_index(&mut index, &destination_key, &findings.destination)?;
    let plan = build_plan(request, findings, &outcome, &source_prints)?;
    let stats = (source_prints.stats, destination_prints.stats);
    findings.outcome = outcome;
    Ok((plan, stats))
}

/// Forgets index rows of files that no longer exist below a scanned root.
fn prune_index(index: &mut HashIndex, root_key: &str, inventory: &Inventory) -> Result<()> {
    let live: HashSet<&str> = inventory.files.iter().map(|file| file.rel.as_str()).collect();
    let pruned = index.prune_root(root_key, &live)?;
    if pruned > 0 {
        println!("  index: {pruned} entries of files that no longer exist were removed ({root_key})");
    }
    Ok(())
}

/// Builds a thread pool of the requested size.
pub fn build_pool(threads: usize) -> Result<ThreadPool> {
    rayon::ThreadPoolBuilder::new().num_threads(threads).build().context("Cannot start worker threads")
}

/// Moves matches that must not be deleted (read-only, same physical file, uncopied streams) to
/// the residual.
fn apply_safety_filters(
    outcome: &mut MatchOutcome,
    findings: &ScanFindings,
    config: &Config,
    pool: &ThreadPool,
) {
    let rejections: Vec<Option<ResidualReason>> = pool.install(|| {
        outcome.matches.par_iter().map(|file_match| safety_rejection(file_match, findings, config)).collect()
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
    config: &Config,
) -> Option<ResidualReason> {
    if findings.source.files[file_match.source].is_read_only && !config.delete_read_only {
        return Some(ResidualReason::ReadOnly);
    }
    let source_path = findings.source.path_of(file_match.source);
    if let Some(reason) = delete_access_rejection(&source_path) {
        return Some(reason);
    }
    let destination_path = findings.destination.path_of(file_match.destination);
    match same_file::is_same_file(&source_path, &destination_path) {
        Ok(false) => {}
        Ok(true) => return Some(ResidualReason::SameFile),
        Err(_) => return Some(ResidualReason::Unreadable),
    }
    match check_streams(&source_path, &destination_path, &config.ignored_stream_names) {
        Ok(StreamCheck::Covered) => None,
        Ok(StreamCheck::Uncovered(_)) => Some(ResidualReason::AlternateStreams),
        Err(_) => Some(ResidualReason::Unreadable),
    }
}

/// Merge mode: turns every residual file that may be moved into a planned move (a move deletes the
/// source too, so the delete permission is checked first).
fn select_moves(outcome: &mut MatchOutcome, source: &Inventory, pool: &ThreadPool) {
    let residual = std::mem::take(&mut outcome.residual);
    let decided: Vec<(usize, Option<ResidualReason>)> = pool.install(|| {
        residual
            .into_par_iter()
            .map(|(index, reason)| match MOVABLE_REASONS.contains(&reason) {
                true => (index, delete_access_rejection(&source.path_of(index))),
                false => (index, Some(reason)),
            })
            .collect()
    });
    for (index, rejection) in decided {
        match rejection {
            None => outcome.moves.push(index),
            Some(reason) => outcome.residual.push((index, reason)),
        }
    }
}

/// Returns why the current user could not delete the file, if so (a file in use now may be free
/// at apply time, so only a refused permission counts).
fn delete_access_rejection(path: &Path) -> Option<ResidualReason> {
    match probe_delete_access(path) {
        Ok(()) => None,
        Err(error) if is_access_denied_error(&error) => Some(ResidualReason::AccessDenied),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Some(ResidualReason::Unreadable),
        Err(_) => None,
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
            let blake3 = if request.mode == MatchMode::Merge {
                String::new()
            } else {
                let digest = source_prints
                    .full_digest(file_match.source)
                    .with_context(|| format!("Internal error: no digest for matched file {}", record.rel))?;
                to_hex(&digest)
            };
            Ok(PlannedDeletion {
                source_rel: record.rel.clone(),
                destination_rel: findings.destination.files[file_match.destination].rel.clone(),
                size: record.size,
                source_mtime_ns: record.mtime_ns,
                blake3,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let metadata_removals = if request.config.remove_svn_metadata {
        let removed: HashSet<usize> = outcome
            .matches
            .iter()
            .map(|file_match| file_match.source)
            .chain(outcome.moves.iter().copied())
            .collect();
        plan_metadata_removals(&findings.source, &removed)
    } else {
        Vec::new()
    };
    Plan::new(PlanBody {
        created_at: timestamp_iso(),
        tool_version: TOOL_VERSION.to_string(),
        mode: request.mode,
        source_root: root_text(&findings.source.root)?,
        destination_root: root_text(&findings.destination.root)?,
        delete_read_only: request.config.delete_read_only,
        remove_emptied_dirs: request.config.remove_emptied_dirs,
        ignored_stream_names: request.config.ignored_stream_names.clone(),
        metadata_removals,
        actions,
        moves: plan_moves(findings, outcome),
    })
}

/// Merge mode: gives each file to move a destination path that is free, in source order.
fn plan_moves(findings: &ScanFindings, outcome: &MatchOutcome) -> Vec<PlannedMove> {
    let mut allocator = TargetAllocator::new(&findings.destination);
    outcome
        .moves
        .iter()
        .map(|&index| {
            let record = &findings.source.files[index];
            PlannedMove {
                source_rel: record.rel.clone(),
                destination_rel: allocator.allocate_file(&record.rel),
                size: record.size,
                source_mtime_ns: record.mtime_ns,
            }
        })
        .collect()
}

/// Returns a canonical root as text (roots were checked to be valid Unicode).
fn root_text(root: &Path) -> Result<String> {
    root.to_str().map(str::to_string).context("Root path is not valid Unicode")
}

/// Returns the command line that applies the plan, ready to paste in PowerShell on Windows.
fn apply_command_for(plan_path: &Path, plan: &Plan) -> String {
    let program =
        std::env::current_exe().map_or_else(|_| "fs-spacemaker".to_string(), |path| display_path(&path));
    format!(
        "{SHELL_CALL_PREFIX}\"{program}\" apply --plan \"{}\" --confirm {}",
        display_path(plan_path),
        plan.confirmation_code()
    )
}

// ----------------------------------------------------------------------------------------------
// Summary and report
// ----------------------------------------------------------------------------------------------

/// Computes the headline figures.
fn summarize(findings: &ScanFindings, plan: &Plan) -> ScanSummary {
    let source = &findings.source;
    let delete_bytes: u64 = findings.outcome.matches.iter().map(|m| source.files[m.source].size).sum();
    let delete_files = findings.outcome.matches.len() as u64;
    let move_files = plan.body.moves.len() as u64;
    let move_bytes = plan.moved_bytes();
    ScanSummary {
        source_files: source.files.len() as u64,
        source_bytes: source.total_bytes(),
        destination_files: findings.destination.files.len() as u64,
        destination_bytes: findings.destination.total_bytes(),
        delete_files,
        delete_bytes,
        move_files,
        move_bytes,
        metadata_removal_bytes: plan.body.metadata_removals.iter().map(|removal| removal.bytes).sum(),
        residual_files: source.files.len() as u64 - delete_files - move_files,
        residual_bytes: source.total_bytes() - delete_bytes - move_bytes,
        skipped_entries: (source.skipped.len()
            + source.metadata_dirs.len()
            + findings.destination.skipped.len()
            + findings.destination.metadata_dirs.len()) as u64,
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
    let removed: HashSet<usize> = findings
        .outcome
        .matches
        .iter()
        .map(|m| m.source)
        .chain(findings.outcome.moves.iter().copied())
        .collect();
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
        notes: build_notes(request, findings, summary, plan),
        cards: build_cards(summary),
        volumes: build_volumes(findings, summary),
        folders: aggregate_folders(&findings.source.dirs, &folder_files),
        tables: build_tables(request, findings, plan),
    }
}

/// Explains, in sentences, what applying the plan will do.
fn build_notes(
    request: &ScanRequest,
    findings: &ScanFindings,
    summary: &ScanSummary,
    plan: &Plan,
) -> Vec<String> {
    let source = display_path(&findings.source.root);
    let mut notes = vec![mode_explanation(request.mode).to_string()];
    if request.mode == MatchMode::Merge {
        notes.extend(merge_notes(summary));
    }
    notes.extend([
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
        deletion_check_note(request.mode).to_string(),
    ]);
    if let (Some(now), Some(after)) = (summary.source_space.available, summary.predicted_source_free()) {
        notes.push(format!(
            "Source volume free space: {} now, about {} after (logical sizes; compression, sparse files or \
             other hard links can make the real gain smaller).",
            format_bytes(now),
            format_bytes(after)
        ));
    }
    notes.push(residual_fit_note(summary));
    if request.mode == MatchMode::Merge {
        notes.push("Every empty source folder will then be removed, so the source ends empty.".to_string());
    } else if request.config.remove_emptied_dirs {
        notes.push(
            "Folders emptied by the deletions will be removed; folders already empty are kept.".to_string(),
        );
    }
    notes.extend(metadata_notes(request, findings, plan));
    if summary.error_count > 0 || summary.skipped_entries > 0 {
        notes.push(format!(
            "{} entries were skipped and {} could not be read: see the Skipped and Errors tabs.",
            summary.skipped_entries, summary.error_count
        ));
    }
    notes
}

/// Merge mode: what moves, and the warning that deletions rest on name and size only.
fn merge_notes(summary: &ScanSummary) -> Vec<String> {
    vec![
        format!(
            "{} files ({}) would be moved into the destination, at the same relative path; a name already \
             taken there gets a number, e.g. \"report (2).pdf\". Nothing in the destination is replaced.",
            summary.move_files,
            format_bytes(summary.move_bytes)
        ),
        "Warning: files are paired by name and size only, their content is NOT compared. A source file \
         is deleted when a destination file anywhere has the same name and size, even if its content \
         differs. Review the Will be deleted tab."
            .to_string(),
        "On the same drive a move is an instant rename and frees no space; between drives the file is \
         copied, the copy is verified byte by byte, then the source is deleted."
            .to_string(),
    ]
}

/// Says what apply checks before each deletion.
fn deletion_check_note(mode: MatchMode) -> &'static str {
    match mode {
        MatchMode::Merge => {
            "Before each deletion, apply re-checks the source size and date and that the destination file \
             still has the same name and size. The destination is never modified, only added to."
        }
        _ => {
            "Before each deletion, apply re-checks size and date, then compares source and destination \
             byte by byte; any difference skips the file. The destination is never modified."
        }
    }
}

/// Explains how version-control metadata folders of the source are handled.
fn metadata_notes(request: &ScanRequest, findings: &ScanFindings, plan: &Plan) -> Vec<String> {
    let metadata = &findings.source.metadata_dirs;
    if metadata.is_empty() {
        return Vec::new();
    }
    let bytes: u64 = metadata.iter().map(|dir| dir.bytes).sum();
    let mut notes = vec![format!(
        "{} version-control metadata folders ({}) were found in the source (.svn, .git, ...): they are \
         never compared or deleted file by file, which would corrupt their working copy.",
        metadata.len(),
        format_bytes(bytes)
    )];
    let removals = &plan.body.metadata_removals;
    let has_svn = metadata.iter().any(|dir| dir.kind == MetadataKind::Subversion);
    if !removals.is_empty() {
        notes.push(format!(
            "{} Subversion metadata folders ({}) will be removed once their working copy is emptied by \
             the verified deletions (checked again at apply time). History stays in the repository.",
            removals.len(),
            format_bytes(removals.iter().map(|removal| removal.bytes).sum())
        ));
    } else if has_svn && !request.config.remove_svn_metadata {
        notes.push(
            "Subversion metadata is kept. To also remove it when its whole working copy is deleted, scan \
             again with --remove-svn-metadata."
                .to_string(),
        );
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
        MatchMode::Merge => {
            "Mode merge: a source file is deleted when a destination file anywhere has the same name and \
             size; every other source file is moved into the destination, so the source ends empty."
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
    let mut cards = vec![
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
    ];
    if summary.move_files > 0 {
        cards.push(card(
            "Will be moved",
            Some(summary.move_bytes),
            Some(summary.move_files),
            "into the destination",
            Tone::Good,
        ));
    }
    cards.extend([
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
    ]);
    cards
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
            note: if summary.move_files > 0 {
                format!(
                    "{} will be moved here (no space used on the same drive)",
                    format_bytes(summary.move_bytes)
                )
            } else {
                format!(
                    "unchanged; moving the residual later would use {}",
                    format_bytes(summary.residual_bytes)
                )
            },
        },
    ]
}

/// Builds the detail tables.
fn build_tables(request: &ScanRequest, findings: &ScanFindings, plan: &Plan) -> Vec<Table> {
    let mode = request.mode;
    let source_root = display_path(&findings.source.root);
    let destination_root = display_path(&findings.destination.root);
    let (deletion_description, twin_column) = if mode == MatchMode::Merge {
        (
            "Source files with a destination file of the same name and size (content NOT compared).",
            "Same name and size",
        )
    } else {
        (
            "Source files with an identical destination copy (re-verified byte by byte at apply time).",
            "Digest",
        )
    };
    let mut tables = vec![Table::new(
        "deletions",
        "Will be deleted",
        deletion_description,
        ["Source file", "Destination file", "Size", "Mode", twin_column],
        (&source_root, &destination_root),
        deletion_rows(mode, plan),
    )];
    if mode == MatchMode::Merge {
        tables.push(Table::new(
            "moves",
            "Will be moved",
            "Source files without a same-name-and-size twin, with the free destination path they move to.",
            ["Source file", "Destination path", "Size", "", "Detail"],
            (&source_root, &destination_root),
            move_rows(plan),
        ));
    }
    tables.extend([Table::new(
        "residual",
        "Remains on source",
        "Source files kept, with the reason. This is what you will need to move or evaluate.",
        ["Source file", "", "Size", "Reason", "Explanation"],
        (&source_root, ""),
        residual_rows(mode, findings),
    )]);
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
        "Entries never touched: links, junctions, cloud placeholders, special files, version-control \
         metadata (unless removed with its emptied working copy).",
        ["Path", "Side", "Size", "Reason", "Explanation"],
        ("", ""),
        skipped_rows(findings, plan),
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
            detail: action
                .blake3
                .get(..DIGEST_PREVIEW_CHARS)
                .map_or_else(|| NAME_AND_SIZE_DETAIL.to_string(), str::to_string),
        })
        .collect()
}

/// Rows of the move table (merge mode).
fn move_rows(plan: &Plan) -> Vec<ReportRow> {
    plan.body
        .moves
        .iter()
        .map(|planned| ReportRow {
            path: planned.source_rel.clone(),
            other: planned.destination_rel.clone(),
            size: planned.size,
            tag: "move".to_string(),
            detail: if name_key(&planned.source_rel) == name_key(&planned.destination_rel) {
                String::new()
            } else {
                "renamed: the original path is taken in the destination".to_string()
            },
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
fn skipped_rows(findings: &ScanFindings, plan: &Plan) -> Vec<ReportRow> {
    let sides = [("source", &findings.source), ("destination", &findings.destination)];
    let mut rows: Vec<ReportRow> = sides
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
        .collect();
    let planned: HashSet<&str> =
        plan.body.metadata_removals.iter().map(|removal| removal.metadata_rel.as_str()).collect();
    for (side, inventory) in sides {
        let root = display_path(&inventory.root);
        rows.extend(inventory.metadata_dirs.iter().map(|dir| ReportRow {
            path: display_joined(&root, &dir.rel),
            other: side.to_string(),
            size: dir.bytes,
            tag: METADATA_TAG.to_string(),
            detail: describe_metadata(inventory, dir, side == "source", planned.contains(dir.rel.as_str())),
        }));
    }
    rows
}

/// Explains one version-control metadata folder and what happens to it.
fn describe_metadata(inventory: &Inventory, dir: &MetadataDir, is_source: bool, is_planned: bool) -> String {
    let mut text = format!("{} metadata, {} files", dir.kind.label(), dir.files);
    if dir.kind == MetadataKind::Subversion {
        match read_svn_summary(&inventory.path_of_rel(&dir.rel)) {
            Some(summary) => text.push_str(&format!(
                "; repository {}; {} uncommitted local changes",
                summary.repository_url, summary.local_changes
            )),
            None => text.push_str("; working copy format older than Subversion 1.7 or unreadable"),
        }
    }
    let outcome = match (is_source, is_planned, dir.kind.is_removable()) {
        (false, _, _) => "never used as a copy",
        (true, true, _) => "removed once its working copy is emptied",
        (true, false, true) => "kept",
        (true, false, false) => "holds the whole history: never deleted",
    };
    format!("{text}: {outcome}")
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
