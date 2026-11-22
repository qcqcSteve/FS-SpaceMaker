//! Processing ("apply"): executes a reviewed plan. For each planned deletion it re-checks the source
//! (still present, same size and date), the destination (present, same size, not the same physical
//! file), compares both byte by byte and checks the digest recorded in the plan; only then is the
//! source file deleted. Every decision is appended to a journal immediately. Dry runs do all the
//! checks and delete nothing.

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;

use crate::config::Config;
use crate::format::{format_bytes, timestamp_iso};
use crate::hashing::{compare_readers, to_hex};
use crate::inventory::{Inventory, ScanError, WalkOptions, modified_ns, scan_tree};
use crate::outputs::apply_paths;
use crate::paths::{
    canonical_root, depth_of, display_joined, display_path, ensure_disjoint_roots, join_rel, name_key,
    parent_rel, rel_to_path,
};
use crate::plan::{Plan, PlannedDeletion, read_plan, verify_confirmation};
use crate::platform::{
    StreamCheck, check_streams, is_cloud_placeholder, is_in_use_error, open_source_locked,
};
use crate::progress::{ProgressMeter, print_step};
use crate::report::{
    Card, FolderFile, FolderTable, Phase, ReportData, ReportRow, Table, Tone, VolumeBar, aggregate_folders,
    write_report,
};
use crate::scan::{TOOL_VERSION, build_pool, card};
use crate::space::{VolumeSpace, volume_space};

// ----------------------------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------------------------

/// Column names of the journal.
const JOURNAL_HEADER: [&str; 6] =
    ["timestamp", "status", "source_path", "destination_path", "size_bytes", "detail"];

// ----------------------------------------------------------------------------------------------
// Model
// ----------------------------------------------------------------------------------------------

/// What to apply.
#[derive(Clone, Debug)]
pub struct ApplyRequest {
    pub plan_path: PathBuf,
    pub confirmation: Option<String>,
    pub is_dry_run: bool,
    pub config: Config,
}

/// Outcome of one planned deletion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActionStatus {
    Deleted,
    WouldDelete,
    SourceMissing,
    SourceChanged,
    DestinationMissing,
    DestinationChanged,
    ContentDiffers,
    SameFile,
    ReadOnly,
    CloudPlaceholder,
    AlternateStreams,
    InUse,
    Failed,
}

impl ActionStatus {
    /// Returns a stable machine tag.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Deleted => "deleted",
            Self::WouldDelete => "would-delete",
            Self::SourceMissing => "skipped-source-missing",
            Self::SourceChanged => "skipped-source-changed",
            Self::DestinationMissing => "skipped-destination-missing",
            Self::DestinationChanged => "skipped-destination-changed",
            Self::ContentDiffers => "skipped-content-differs",
            Self::SameFile => "skipped-same-file",
            Self::ReadOnly => "skipped-read-only",
            Self::CloudPlaceholder => "skipped-cloud-placeholder",
            Self::AlternateStreams => "skipped-alternate-streams",
            Self::InUse => "skipped-in-use",
            Self::Failed => "failed",
        }
    }

    /// Returns true for the statuses that mean the source file is (or would be) gone.
    pub fn is_removal(self) -> bool {
        matches!(self, Self::Deleted | Self::WouldDelete)
    }
}

/// Outcome of one planned deletion, with its explanation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionResult {
    pub status: ActionStatus,
    pub detail: String,
}

/// Everything an apply run produced.
#[derive(Clone, Debug)]
pub struct ApplyOutput {
    pub plan: Plan,
    pub is_dry_run: bool,
    pub results: Vec<ActionResult>,
    pub removed_dirs: Vec<String>,
    pub cleanup_errors: Vec<ScanError>,
    pub space_before: VolumeSpace,
    pub space_after: VolumeSpace,
    pub journal_path: PathBuf,
    pub html_path: PathBuf,
    pub csv_path: PathBuf,
}

impl ApplyOutput {
    /// Returns how many actions ended with a status.
    pub fn count(&self, status: ActionStatus) -> usize {
        self.results.iter().filter(|result| result.status == status).count()
    }

    /// Returns the files and bytes removed (or that would be, in a dry run).
    pub fn removed_totals(&self) -> (u64, u64) {
        self.removals().fold((0, 0), |(files, bytes), action| (files + 1, bytes + action.size))
    }

    /// Returns how many actions were skipped by a safety check.
    pub fn skipped_count(&self) -> usize {
        self.results
            .iter()
            .filter(|result| !result.status.is_removal() && result.status != ActionStatus::Failed)
            .count()
    }

    /// Returns true if any action or cleanup failed on an I/O error.
    pub fn has_failures(&self) -> bool {
        self.count(ActionStatus::Failed) > 0 || !self.cleanup_errors.is_empty()
    }

    /// Returns the planned deletions whose result is a removal.
    fn removals(&self) -> impl Iterator<Item = &PlannedDeletion> {
        self.plan
            .body
            .actions
            .iter()
            .zip(&self.results)
            .filter(|(_, result)| result.status.is_removal())
            .map(|(action, _)| action)
    }
}

/// Fixed facts shared by every action.
struct ActionContext {
    source_root: PathBuf,
    destination_root: PathBuf,
    is_dry_run: bool,
    delete_read_only: bool,
    ignored_stream_names: Vec<String>,
    is_aborted: AtomicBool,
}

/// A refused action.
struct Rejection {
    status: ActionStatus,
    detail: String,
}

/// Returns a rejection.
fn reject(status: ActionStatus, detail: impl Into<String>) -> Rejection {
    Rejection { status, detail: detail.into() }
}

// ----------------------------------------------------------------------------------------------
// Apply
// ----------------------------------------------------------------------------------------------

/// Runs the whole processing of a plan.
pub fn run_apply(request: &ApplyRequest) -> Result<ApplyOutput> {
    request.config.validate()?;
    print_step("Reading plan");
    let plan = read_plan(&request.plan_path)?;
    if !request.is_dry_run {
        verify_confirmation(&plan, request.confirmation.as_deref())?;
    }
    let context = prepare_context(&plan, request.is_dry_run)?;
    println!(
        "  plan:        {} ({} deletions, {})",
        plan.plan_id,
        plan.body.actions.len(),
        format_bytes(plan.total_bytes())
    );
    let paths = apply_paths(&output_dir_of(&request.plan_path), request.is_dry_run);
    // A dry run replaces its journal; a real apply appends, so no trace of a deletion is ever lost.
    let journal = Journal::open(&paths.journal, &context, !request.is_dry_run)?;
    let space_before = volume_space(&context.source_root);
    print_step(if request.is_dry_run {
        "Verifying (dry run: nothing is deleted)"
    } else {
        "Verifying and deleting"
    });
    let results = execute_all(&plan, &context, &journal, request.config.apply_threads)?;
    let (removed_dirs, cleanup_errors) = if !request.is_dry_run && plan.body.remove_emptied_dirs {
        print_step("Removing emptied folders");
        remove_emptied_dirs(&context.source_root, &plan, &results)
    } else {
        (Vec::new(), Vec::new())
    };
    let space_after = volume_space(&context.source_root);
    print_step("Writing report");
    let output = ApplyOutput {
        plan,
        is_dry_run: request.is_dry_run,
        results,
        removed_dirs,
        cleanup_errors,
        space_before,
        space_after,
        html_path: paths.html,
        csv_path: paths.csv,
        journal_path: paths.journal,
    };
    write_apply_report(&output, &context, &request.config)?;
    Ok(output)
}

/// Resolves the plan roots and checks they still designate the scanned folders.
fn prepare_context(plan: &Plan, is_dry_run: bool) -> Result<ActionContext> {
    let source_root = resolve_plan_root(&plan.body.source_root, "source")?;
    let destination_root = resolve_plan_root(&plan.body.destination_root, "destination")?;
    ensure_disjoint_roots(&source_root, &destination_root)?;
    println!("  source:      {}", display_path(&source_root));
    println!("  destination: {}", display_path(&destination_root));
    Ok(ActionContext {
        source_root,
        destination_root,
        is_dry_run,
        delete_read_only: plan.body.delete_read_only,
        ignored_stream_names: plan.body.ignored_stream_names.clone(),
        is_aborted: AtomicBool::new(false),
    })
}

/// Canonicalizes a plan root and fails if it now resolves somewhere else.
fn resolve_plan_root(recorded: &str, side: &str) -> Result<PathBuf> {
    let root = canonical_root(Path::new(recorded)).with_context(|| {
        format!("The {side} folder of the plan is not available: connect the drive or run a new scan.")
    })?;
    if name_key(&display_path(&root)) != name_key(&display_path(Path::new(recorded))) {
        bail!(
            "The {side} folder {recorded} now resolves to {}: nothing deleted. Run a new scan.",
            display_path(&root)
        );
    }
    Ok(root)
}

/// Returns the folder holding the plan, where the apply outputs are written.
fn output_dir_of(plan_path: &Path) -> PathBuf {
    plan_path.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf()
}

/// Executes every action in parallel and journals each decision as soon as it is taken.
fn execute_all(
    plan: &Plan,
    context: &ActionContext,
    journal: &Journal,
    threads: usize,
) -> Result<Vec<ActionResult>> {
    let pool = build_pool(threads)?;
    let meter = ProgressMeter::new("verified");
    let results: Vec<ActionResult> = pool.install(|| {
        plan.body
            .actions
            .par_iter()
            .map(|action| {
                let result = execute_action(context, action);
                if let Err(error) = journal.record(action, &result) {
                    // Without a journal there is no trace of further deletions: stop deleting.
                    context.is_aborted.store(true, Ordering::SeqCst);
                    eprintln!("ERROR: journal write failed ({error}); remaining deletions are cancelled.");
                }
                meter.record(action.size);
                result
            })
            .collect()
    });
    meter.print_line();
    journal.flush()?;
    Ok(results)
}

/// Verifies one planned deletion and performs it; never panics, always returns a result.
fn execute_action(context: &ActionContext, action: &PlannedDeletion) -> ActionResult {
    match verify_and_delete(context, action) {
        Ok(status) => ActionResult { status, detail: String::new() },
        Err(rejection) => ActionResult { status: rejection.status, detail: rejection.detail },
    }
}

/// Runs every safety check, then deletes the source file (unless dry run).
fn verify_and_delete(context: &ActionContext, action: &PlannedDeletion) -> Result<ActionStatus, Rejection> {
    if context.is_aborted.load(Ordering::SeqCst) {
        return Err(reject(ActionStatus::Failed, "cancelled: journal could not be written"));
    }
    let source_path = join_rel(&context.source_root, &action.source_rel)
        .map_err(|e| reject(ActionStatus::Failed, e.to_string()))?;
    let destination_path = join_rel(&context.destination_root, &action.destination_rel)
        .map_err(|e| reject(ActionStatus::Failed, e.to_string()))?;
    let source_metadata = check_preconditions(context, action, &source_path, &destination_path)?;
    // From here until the deletion, no other program can write to the source (Windows).
    let mut source_file =
        open_source_locked(&source_path).map_err(|error| io_rejection("cannot open source", &error))?;
    check_identical_content(&mut source_file, &destination_path, &action.blake3)?;
    check_source_unchanged(&source_path, action)?;
    if context.is_dry_run {
        return Ok(ActionStatus::WouldDelete);
    }
    delete_file(&source_path, &source_metadata)?;
    drop(source_file);
    Ok(ActionStatus::Deleted)
}

/// Runs the checks that need no file content; returns the source metadata.
fn check_preconditions(
    context: &ActionContext,
    action: &PlannedDeletion,
    source_path: &Path,
    destination_path: &Path,
) -> Result<Metadata, Rejection> {
    let source_metadata = check_source_unchanged(source_path, action)?;
    check_destination_present(destination_path, action.size)?;
    check_not_same_file(source_path, destination_path)?;
    check_streams_copied(source_path, destination_path, &context.ignored_stream_names)?;
    check_read_only_allowed(&source_metadata, context.delete_read_only)?;
    Ok(source_metadata)
}

/// Converts an I/O error into a rejection ("in use" when another program holds the file).
fn io_rejection(action: &str, error: &io::Error) -> Rejection {
    if is_in_use_error(error) {
        return reject(ActionStatus::InUse, format!("{action}: file is in use by another program"));
    }
    reject(ActionStatus::Failed, format!("{action}: {error}"))
}

/// Fails unless every NTFS alternate data stream of the source that may hold user data exists,
/// byte-identical, on the destination copy (the main comparison does not cover streams).
fn check_streams_copied(
    source: &Path,
    destination: &Path,
    ignored_patterns: &[String],
) -> Result<(), Rejection> {
    match check_streams(source, destination, ignored_patterns) {
        Ok(StreamCheck::Covered) => Ok(()),
        Ok(StreamCheck::Uncovered(name)) => Err(reject(
            ActionStatus::AlternateStreams,
            format!("hidden NTFS stream {name:?} is missing or different on the destination copy"),
        )),
        Err(error) => Err(io_rejection("cannot compare alternate data streams", &error)),
    }
}

/// Fails unless the source is still a regular local file with the planned size and date.
fn check_source_unchanged(path: &Path, action: &PlannedDeletion) -> Result<Metadata, Rejection> {
    let metadata = fs::symlink_metadata(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => reject(ActionStatus::SourceMissing, "source file no longer exists"),
        _ => reject(ActionStatus::Failed, format!("cannot read source metadata: {error}")),
    })?;
    if !metadata.file_type().is_file() {
        return Err(reject(ActionStatus::SourceChanged, "source is no longer a regular file"));
    }
    if is_cloud_placeholder(&metadata) {
        return Err(reject(ActionStatus::CloudPlaceholder, "source became a cloud placeholder"));
    }
    if metadata.len() != action.size || modified_ns(&metadata) != action.source_mtime_ns {
        return Err(reject(
            ActionStatus::SourceChanged,
            "source size or modification time changed since the scan",
        ));
    }
    Ok(metadata)
}

/// Fails unless the destination is a regular local file of the expected size.
fn check_destination_present(path: &Path, size: u64) -> Result<(), Rejection> {
    let metadata = fs::symlink_metadata(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => {
            reject(ActionStatus::DestinationMissing, "destination copy no longer exists")
        }
        _ => reject(ActionStatus::Failed, format!("cannot read destination metadata: {error}")),
    })?;
    if !metadata.file_type().is_file() {
        return Err(reject(ActionStatus::DestinationChanged, "destination is no longer a regular file"));
    }
    if is_cloud_placeholder(&metadata) {
        return Err(reject(ActionStatus::CloudPlaceholder, "destination is a cloud placeholder"));
    }
    if metadata.len() != size {
        return Err(reject(ActionStatus::DestinationChanged, "destination size changed since the scan"));
    }
    Ok(())
}

/// Fails if both paths designate the same physical file (hard link, junction, mount).
fn check_not_same_file(source: &Path, destination: &Path) -> Result<(), Rejection> {
    match same_file::is_same_file(source, destination) {
        Ok(false) => Ok(()),
        Ok(true) => Err(reject(ActionStatus::SameFile, "source and destination are the same physical file")),
        Err(error) => Err(io_rejection("cannot compare file identities", &error)),
    }
}

/// Fails unless both files are byte-identical and the content is the one fingerprinted by the scan.
fn check_identical_content(
    source: &mut File,
    destination: &Path,
    planned_digest: &str,
) -> Result<(), Rejection> {
    let mut destination_file =
        File::open(destination).map_err(|error| io_rejection("cannot open destination", &error))?;
    let comparison = compare_readers(source, &mut destination_file)
        .map_err(|error| io_rejection("cannot read for comparison", &error))?;
    let Some(digest) = comparison else {
        return Err(reject(ActionStatus::ContentDiffers, "byte-by-byte comparison found a difference"));
    };
    if to_hex(&digest) != planned_digest {
        return Err(reject(
            ActionStatus::SourceChanged,
            "content differs from the content fingerprinted at scan time",
        ));
    }
    Ok(())
}

/// Fails for read-only files unless the plan allows deleting them.
fn check_read_only_allowed(metadata: &Metadata, delete_read_only: bool) -> Result<(), Rejection> {
    if metadata.permissions().readonly() && !delete_read_only {
        return Err(reject(ActionStatus::ReadOnly, "read-only file is protected (delete_read_only is off)"));
    }
    Ok(())
}

/// Deletes the source file, clearing the Windows read-only attribute first when needed.
fn delete_file(path: &Path, metadata: &Metadata) -> Result<(), Rejection> {
    if cfg!(windows) && metadata.permissions().readonly() {
        clear_read_only(path, metadata).map_err(|error| io_rejection("cannot clear read-only", &error))?;
    }
    fs::remove_file(path).map_err(|error| io_rejection("delete failed", &error))
}

/// Clears the read-only flag of a file.
#[allow(clippy::permissions_set_readonly_false)]
fn clear_read_only(path: &Path, metadata: &Metadata) -> io::Result<()> {
    let mut permissions = metadata.permissions();
    // On Windows this only clears FILE_ATTRIBUTE_READONLY; it is never called on other systems.
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions)
}

// ----------------------------------------------------------------------------------------------
// Journal
// ----------------------------------------------------------------------------------------------

/// Append-only CSV journal, flushed after every line.
struct Journal {
    writer: Mutex<csv::Writer<File>>,
    source_root: String,
    destination_root: String,
}

impl Journal {
    /// Opens the journal, appending to an existing one or starting a new file with its header.
    fn open(path: &Path, context: &ActionContext, is_append: bool) -> Result<Self> {
        let has_content = is_append && fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0);
        let file = File::options()
            .create(true)
            .write(true)
            .append(is_append)
            .truncate(!is_append)
            .open(path)
            .with_context(|| format!("Cannot open journal {}: nothing deleted.", path.display()))?;
        let mut writer = csv::Writer::from_writer(file);
        if !has_content {
            writer.write_record(JOURNAL_HEADER)?;
        }
        writer.flush()?;
        Ok(Self {
            writer: Mutex::new(writer),
            source_root: display_path(&context.source_root),
            destination_root: display_path(&context.destination_root),
        })
    }

    /// Appends one decision and flushes it to disk.
    fn record(&self, action: &PlannedDeletion, result: &ActionResult) -> Result<()> {
        let mut writer = self.writer.lock().map_err(|_| anyhow::anyhow!("journal lock poisoned"))?;
        writer.write_record([
            timestamp_iso().as_str(),
            result.status.tag(),
            &display_joined(&self.source_root, &action.source_rel),
            &display_joined(&self.destination_root, &action.destination_rel),
            &action.size.to_string(),
            &result.detail,
        ])?;
        writer.flush()?;
        Ok(())
    }

    /// Flushes pending lines.
    fn flush(&self) -> Result<()> {
        let mut writer = self.writer.lock().map_err(|_| anyhow::anyhow!("journal lock poisoned"))?;
        writer.flush().context("Cannot write the journal")
    }
}

// ----------------------------------------------------------------------------------------------
// Emptied folders
// ----------------------------------------------------------------------------------------------

/// Removes source folders left empty by the deletions (deepest first; the root is kept).
fn remove_emptied_dirs(
    source_root: &Path,
    plan: &Plan,
    results: &[ActionResult],
) -> (Vec<String>, Vec<ScanError>) {
    let mut folders: BTreeSet<&str> = BTreeSet::new();
    for (action, _) in
        plan.body.actions.iter().zip(results).filter(|(_, result)| result.status == ActionStatus::Deleted)
    {
        let mut folder = parent_rel(&action.source_rel);
        while !folder.is_empty() && folders.insert(folder) {
            folder = parent_rel(folder);
        }
    }
    let mut ordered: Vec<&str> = folders.into_iter().collect();
    ordered.sort_by_key(|rel| Reverse(depth_of(rel)));
    let mut removed = Vec::new();
    let mut errors = Vec::new();
    for rel in ordered {
        let path = rel_to_path(source_root, rel);
        match fs::remove_dir(&path) {
            Ok(()) => removed.push(rel.to_string()),
            Err(error)
                if matches!(error.kind(), io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound) => {}
            Err(error) => errors.push(ScanError { path: display_path(&path), message: error.to_string() }),
        }
    }
    (removed, errors)
}

// ----------------------------------------------------------------------------------------------
// Report
// ----------------------------------------------------------------------------------------------

/// Re-inventories the source and writes the post-processing report.
fn write_apply_report(output: &ApplyOutput, context: &ActionContext, config: &Config) -> Result<()> {
    let options = WalkOptions { excluded_names: config.excluded_names.clone(), excluded_paths: Vec::new() };
    let after = scan_tree(&context.source_root, &options);
    let data = build_report(output, context, &after);
    write_report(data, config.report_max_rows, &output.html_path, &output.csv_path)
}

/// Assembles the post-processing report.
fn build_report(output: &ApplyOutput, context: &ActionContext, after: &Inventory) -> ReportData {
    let source_root = display_path(&context.source_root);
    let destination_root = display_path(&context.destination_root);
    let (phase, title) = if output.is_dry_run {
        (Phase::DryRun, "FS SpaceMaker: dry run (nothing deleted)")
    } else {
        (Phase::Apply, "FS SpaceMaker: result of the processing")
    };
    ReportData {
        phase,
        title: title.to_string(),
        generated_at: timestamp_iso(),
        tool_version: TOOL_VERSION.to_string(),
        plan_id: output.plan.plan_id.clone(),
        apply_command: String::new(),
        mode: output.plan.body.mode,
        source_root: source_root.clone(),
        destination_root: destination_root.clone(),
        notes: build_notes(output),
        cards: build_cards(output),
        volumes: vec![build_source_volume(output, &source_root)],
        folders: build_folders(output, after),
        tables: build_tables(output, after, (&source_root, &destination_root)),
    }
}

/// Explains, in sentences, what happened.
fn build_notes(output: &ApplyOutput) -> Vec<String> {
    let (files, bytes) = output.removed_totals();
    let verb = if output.is_dry_run { "would be deleted" } else { "were deleted" };
    let mut notes = vec![
        format!("{files} files ({}) {verb} after byte-by-byte verification.", format_bytes(bytes)),
        format!(
            "{} planned deletions were skipped by a safety check and {} failed: those files are untouched.",
            output.skipped_count(),
            output.count(ActionStatus::Failed)
        ),
        format!("Journal of every decision: {}", display_path(&output.journal_path)),
    ];
    if !output.is_dry_run {
        notes.push(format!("{} emptied folders were removed.", output.removed_dirs.len()));
    }
    notes
}

/// Builds the headline cards.
fn build_cards(output: &ApplyOutput) -> Vec<Card> {
    let (files, bytes) = output.removed_totals();
    let failed = output.count(ActionStatus::Failed) as u64;
    let removed_label = if output.is_dry_run { "Would be deleted" } else { "Deleted" };
    vec![
        card(removed_label, Some(bytes), Some(files), "verified identical", Tone::Good),
        card("Skipped (safety)", None, Some(output.skipped_count() as u64), "left untouched", Tone::Warn),
        card(
            "Failed",
            None,
            Some(failed),
            "left untouched",
            if failed > 0 { Tone::Bad } else { Tone::Neutral },
        ),
        card("Source free before", output.space_before.available, None, "measured", Tone::Neutral),
        card(
            "Source free after",
            free_after(output),
            None,
            if output.is_dry_run { "predicted" } else { "measured" },
            Tone::Good,
        ),
        card(
            "Folders removed",
            None,
            Some(output.removed_dirs.len() as u64),
            "emptied by deletions",
            Tone::Neutral,
        ),
    ]
}

/// Returns the measured (or, for a dry run, predicted) free space after processing.
fn free_after(output: &ApplyOutput) -> Option<u64> {
    if output.is_dry_run {
        let after = output.space_before.available? + output.removed_totals().1;
        return Some(output.space_before.total.map_or(after, |total| after.min(total)));
    }
    output.space_after.available
}

/// Builds the before/after bar of the source volume.
fn build_source_volume(output: &ApplyOutput, source_root: &str) -> VolumeBar {
    VolumeBar {
        label: "Source volume".to_string(),
        path: source_root.to_string(),
        total: output.space_before.total,
        free_before: output.space_before.available,
        free_after: free_after(output),
        note: format!(
            "{} {}",
            format_bytes(output.removed_totals().1),
            if output.is_dry_run { "would be freed" } else { "freed" }
        ),
    }
}

/// Builds the folder tree: what remains now plus what was removed.
fn build_folders(output: &ApplyOutput, after: &Inventory) -> FolderTable {
    let removed_rels: HashSet<&str> = output.removals().map(|action| action.source_rel.as_str()).collect();
    let mut files: Vec<FolderFile> = after
        .files
        .iter()
        .map(|file| FolderFile {
            rel: &file.rel,
            size: file.size,
            is_removed: output.is_dry_run && removed_rels.contains(file.rel.as_str()),
        })
        .collect();
    if !output.is_dry_run {
        files.extend(output.removals().map(|action| FolderFile {
            rel: &action.source_rel,
            size: action.size,
            is_removed: true,
        }));
    }
    aggregate_folders(&after.dirs, &files)
}

/// Builds the detail tables.
fn build_tables(output: &ApplyOutput, after: &Inventory, roots: (&str, &str)) -> Vec<Table> {
    let result_rows = output
        .plan
        .body
        .actions
        .iter()
        .zip(&output.results)
        .map(|(action, result)| ReportRow {
            path: action.source_rel.clone(),
            other: action.destination_rel.clone(),
            size: action.size,
            tag: result.status.tag().to_string(),
            detail: result.detail.clone(),
        })
        .collect();
    let folder_rows = output
        .removed_dirs
        .iter()
        .map(|rel| ReportRow {
            path: rel.clone(),
            other: String::new(),
            size: 0,
            tag: "removed-folder".to_string(),
            detail: String::new(),
        })
        .collect();
    let error_rows = output
        .cleanup_errors
        .iter()
        .chain(&after.errors)
        .map(|error| ReportRow {
            path: error.path.clone(),
            other: String::new(),
            size: 0,
            tag: "error".to_string(),
            detail: error.message.clone(),
        })
        .collect();
    vec![
        Table::new(
            "results",
            "Planned deletions",
            "Result of every planned deletion. Anything not 'deleted' was left untouched.",
            ["Source file", "Destination copy", "Size", "Status", "Detail"],
            roots,
            result_rows,
        ),
        Table::new(
            "folders",
            "Removed folders",
            "Source folders removed because the deletions emptied them.",
            ["Folder", "", "", "Status", ""],
            (roots.0, ""),
            folder_rows,
        ),
        Table::new(
            "errors",
            "Errors",
            "Cleanup or post-inventory errors.",
            ["Path", "", "", "", "Message"],
            ("", ""),
            error_rows,
        ),
    ]
}
