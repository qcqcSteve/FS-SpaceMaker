//! Processing ("apply"): executes a reviewed plan. For each planned deletion it re-checks the source
//! (still present, same size and date), the destination (present, same size, not the same physical
//! file), then locks both files (nobody can write, rename or delete them), compares them byte by
//! byte and checks the digest recorded in the plan; only then is the source deleted, through the
//! very handle that was verified. Every decision is appended to a journal immediately. Dry runs do
//! all the checks and delete nothing.
//! Merge mode pairs files by name and size (no content comparison before a deletion) and moves
//! every other source file into the destination: a rename on the same volume, otherwise a copy
//! verified byte by byte before the source is deleted. Every empty source folder is then removed.

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
use crate::matching::MatchMode;
use crate::outputs::apply_paths;
use crate::paths::{
    canonical_root, depth_of, display_joined, display_path, ensure_disjoint_roots, join_rel, name_key,
    parent_rel, rel_to_path,
};
use crate::plan::{
    Plan, PlannedDeletion, PlannedFile, PlannedMetadataRemoval, PlannedMove, read_plan, verify_confirmation,
};
use crate::platform::{
    StreamCheck, check_streams, copy_preserving_metadata, delete_open_file, is_access_denied_error,
    is_already_exists_error, is_cloud_placeholder, is_cross_device_error, is_in_use_error,
    open_destination_locked, open_source_for_deletion, remove_file_forcing, rename_no_replace,
};
use crate::progress::{ProgressMeter, print_step};
use crate::report::{
    Card, FolderFile, FolderTable, Phase, ReportData, ReportRow, Table, Tone, VolumeBar, aggregate_folders,
    write_report,
};
use crate::scan::{TOOL_VERSION, build_pool, card};
use crate::space::{VolumeSpace, volume_space};
use crate::vcs::{holds_only_removable_metadata, remove_metadata_dir};

// ----------------------------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------------------------

/// Column names of the journal.
const JOURNAL_HEADER: [&str; 6] =
    ["timestamp", "status", "source_path", "destination_path", "size_bytes", "detail"];
/// Journal status of the line that opens each run (source and destination roots, plan identifier).
const JOURNAL_RUN_STARTED: &str = "run-started";
/// Suffix of the temporary name a file is copied to before it takes its final destination name,
/// so a partial copy never looks like a finished one.
const PARTIAL_COPY_SUFFIX: &str = ".fs-spacemaker-partial";

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
    Moved,
    WouldMove,
    TargetOccupied,
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
    AccessDenied,
    Failed,
}

impl ActionStatus {
    /// Returns a stable machine tag.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Deleted => "deleted",
            Self::WouldDelete => "would-delete",
            Self::Moved => "moved",
            Self::WouldMove => "would-move",
            Self::TargetOccupied => "skipped-target-occupied",
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
            Self::AccessDenied => "skipped-access-denied",
            Self::Failed => "failed",
        }
    }

    /// Returns true for the statuses that mean the source file is (or would be) gone.
    pub fn is_removal(self) -> bool {
        matches!(self, Self::Deleted | Self::WouldDelete | Self::Moved | Self::WouldMove)
    }
}

/// Outcome of one planned deletion, with its explanation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionResult {
    pub status: ActionStatus,
    pub detail: String,
}

/// Outcome of one planned Subversion metadata removal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetadataStatus {
    Removed,
    WouldRemove,
    Kept,
    Failed,
}

impl MetadataStatus {
    /// Returns a stable machine tag.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Removed => "deleted-svn-metadata",
            Self::WouldRemove => "would-delete-svn-metadata",
            Self::Kept => "kept-svn-metadata",
            Self::Failed => "failed-svn-metadata",
        }
    }
}

/// Outcome of one planned metadata removal, with its explanation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetadataResult {
    pub status: MetadataStatus,
    pub detail: String,
}

/// Everything an apply run produced.
#[derive(Clone, Debug)]
pub struct ApplyOutput {
    pub plan: Plan,
    pub is_dry_run: bool,
    pub results: Vec<ActionResult>,
    /// Merge mode: one result per planned move.
    pub move_results: Vec<ActionResult>,
    pub metadata_results: Vec<MetadataResult>,
    pub removed_dirs: Vec<String>,
    pub cleanup_errors: Vec<ScanError>,
    pub space_before: VolumeSpace,
    pub space_after: VolumeSpace,
    pub journal_path: PathBuf,
    pub html_path: PathBuf,
    pub csv_path: PathBuf,
}

impl ApplyOutput {
    /// Returns how many deletions and moves ended with a status.
    pub fn count(&self, status: ActionStatus) -> usize {
        self.all_results().filter(|result| result.status == status).count()
    }

    /// Returns the files and bytes removed (or that would be, in a dry run).
    pub fn removed_totals(&self) -> (u64, u64) {
        self.removals().fold((0, 0), |(files, bytes), action| (files + 1, bytes + action.size))
    }

    /// Returns the files and bytes moved into the destination (or that would be, in a dry run).
    pub fn moved_totals(&self) -> (u64, u64) {
        self.moved().fold((0, 0), |(files, bytes), planned| (files + 1, bytes + planned.size))
    }

    /// Returns how many deletions and moves were skipped by a safety check.
    pub fn skipped_count(&self) -> usize {
        self.all_results()
            .filter(|result| !result.status.is_removal() && result.status != ActionStatus::Failed)
            .count()
    }

    /// Returns the results of every deletion, then of every move.
    fn all_results(&self) -> impl Iterator<Item = &ActionResult> {
        self.results.iter().chain(&self.move_results)
    }

    /// Returns the planned moves that were (or would be) performed.
    fn moved(&self) -> impl Iterator<Item = &PlannedMove> {
        self.plan
            .body
            .moves
            .iter()
            .zip(&self.move_results)
            .filter(|(_, result)| result.status.is_removal())
            .map(|(planned, _)| planned)
    }

    /// Returns true if any action or cleanup failed on an I/O error.
    pub fn has_failures(&self) -> bool {
        self.count(ActionStatus::Failed) > 0
            || !self.cleanup_errors.is_empty()
            || self.metadata_results.iter().any(|result| result.status == MetadataStatus::Failed)
    }

    /// Returns the planned metadata removals that were (or would be) performed.
    pub fn metadata_removals(&self) -> impl Iterator<Item = &PlannedMetadataRemoval> {
        self.plan
            .body
            .metadata_removals
            .iter()
            .zip(&self.metadata_results)
            .filter(|(_, result)| {
                matches!(result.status, MetadataStatus::Removed | MetadataStatus::WouldRemove)
            })
            .map(|(removal, _)| removal)
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
    /// False in merge mode, whose deletions rest on name and size only.
    compares_content: bool,
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
        "  plan:        {} ({} deletions, {}; {} moves, {})",
        plan.plan_id,
        plan.body.actions.len(),
        format_bytes(plan.total_bytes()),
        plan.body.moves.len(),
        format_bytes(plan.moved_bytes())
    );
    let paths = apply_paths(&output_dir_of(&request.plan_path), request.is_dry_run);
    // A dry run replaces its journal; a real apply appends, so no trace of a deletion is ever lost.
    let journal = Journal::open(&paths.journal, &context, !request.is_dry_run)?;
    journal.record_run_start(&plan, request.is_dry_run)?;
    let space_before = volume_space(&context.source_root);
    print_step(if request.is_dry_run {
        "Verifying (dry run: nothing is deleted)"
    } else {
        "Verifying and deleting"
    });
    let threads = request.config.apply_threads;
    let results = execute_all(&plan.body.actions, &context, &journal, (threads, verify_and_delete))?;
    let move_results = if plan.body.moves.is_empty() {
        Vec::new()
    } else {
        print_step(if request.is_dry_run { "Checking moves (dry run: nothing moved)" } else { "Moving" });
        execute_all(&plan.body.moves, &context, &journal, (threads, verify_and_move))?
    };
    let metadata_results = remove_planned_metadata(&plan, &context, &results, &journal)?;
    let (removed_dirs, cleanup_errors) = if request.is_dry_run {
        (Vec::new(), Vec::new())
    } else if plan.body.mode == MatchMode::Merge {
        print_step("Removing every empty source folder");
        remove_all_empty_dirs(&context.source_root, &request.config)
    } else if plan.body.remove_emptied_dirs {
        print_step("Removing emptied folders");
        remove_emptied_dirs(&context.source_root, &plan, &results, &metadata_results)
    } else {
        (Vec::new(), Vec::new())
    };
    let space_after = volume_space(&context.source_root);
    print_step("Writing report");
    let output = ApplyOutput {
        plan,
        is_dry_run: request.is_dry_run,
        results,
        move_results,
        metadata_results,
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
        compares_content: plan.body.mode != MatchMode::Merge,
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

/// One planned operation: verifies its file, then performs it (unless dry run).
type Operation<T> = fn(&ActionContext, &T) -> Result<ActionStatus, Rejection>;

/// Executes every planned operation in parallel (`threads` workers) and journals each decision as
/// soon as it is taken; never panics, every operation gets a result.
fn execute_all<T: PlannedFile + Sync>(
    planned: &[T],
    context: &ActionContext,
    journal: &Journal,
    (threads, operation): (usize, Operation<T>),
) -> Result<Vec<ActionResult>> {
    let pool = build_pool(threads)?;
    let meter = ProgressMeter::with_total("verified", planned.iter().map(PlannedFile::size).sum());
    let results: Vec<ActionResult> = pool.install(|| {
        planned
            .par_iter()
            .map(|item| {
                let result = match operation(context, item) {
                    Ok(status) => ActionResult { status, detail: String::new() },
                    Err(rejection) => ActionResult { status: rejection.status, detail: rejection.detail },
                };
                if let Err(error) = journal.record(item, &result) {
                    // Without a journal there is no trace of further changes: stop changing anything.
                    context.is_aborted.store(true, Ordering::SeqCst);
                    eprintln!("ERROR: journal write failed ({error}); remaining operations are cancelled.");
                }
                meter.record(item.size());
                result
            })
            .collect()
    });
    meter.print_line();
    journal.flush()?;
    Ok(results)
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
    check_preconditions(context, action, &source_path, &destination_path)?;
    // From here until the deletion, no other program can write, rename or delete either file
    // (Windows), and the checks below apply to the open handles, not to whatever the paths hold.
    let mut source_file = open_source_for_deletion(&source_path)
        .map_err(|error| io_rejection("cannot open source for deletion", &error))?;
    let source_metadata =
        source_file.metadata().map_err(|error| io_rejection("cannot read source metadata", &error))?;
    check_source_metadata(&source_metadata, action)?;
    let mut destination_file = open_destination_locked(&destination_path)
        .map_err(|error| io_rejection("cannot open destination", &error))?;
    check_destination_handle(&destination_file, action.size)?;
    if context.compares_content {
        check_identical_content(&mut source_file, &mut destination_file, &action.blake3)?;
    }
    if context.is_dry_run {
        return Ok(ActionStatus::WouldDelete);
    }
    delete_open_file(source_file, &source_path).map_err(|error| io_rejection("delete failed", &error))?;
    drop(destination_file);
    Ok(ActionStatus::Deleted)
}

/// Runs the checks that need no file content and no lock.
fn check_preconditions(
    context: &ActionContext,
    action: &PlannedDeletion,
    source_path: &Path,
    destination_path: &Path,
) -> Result<(), Rejection> {
    let source_metadata = check_source_unchanged(source_path, action)?;
    check_destination_present(destination_path, action.size)?;
    if !context.compares_content {
        check_same_name(source_path, destination_path)?;
    }
    check_not_same_file(source_path, destination_path)?;
    check_streams_copied(source_path, destination_path, &context.ignored_stream_names)?;
    check_read_only_allowed(&source_metadata, context.delete_read_only)
}

/// Converts an I/O error into a rejection: "in use" when another program holds the file,
/// "access denied" when the user may not touch it, otherwise a failure.
fn io_rejection(action: &str, error: &io::Error) -> Rejection {
    if is_in_use_error(error) {
        return reject(ActionStatus::InUse, format!("{action}: file is in use by another program"));
    }
    if is_access_denied_error(error) {
        return reject(
            ActionStatus::AccessDenied,
            format!("{action}: access denied (protected system file or other owner)"),
        );
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
fn check_source_unchanged(path: &Path, action: &impl PlannedFile) -> Result<Metadata, Rejection> {
    let metadata = fs::symlink_metadata(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => reject(ActionStatus::SourceMissing, "source file no longer exists"),
        _ => io_rejection("cannot read source metadata", &error),
    })?;
    check_source_metadata(&metadata, action)?;
    Ok(metadata)
}

/// Fails unless metadata describes a regular local file with the planned size and date.
fn check_source_metadata(metadata: &Metadata, action: &impl PlannedFile) -> Result<(), Rejection> {
    if !metadata.file_type().is_file() {
        return Err(reject(ActionStatus::SourceChanged, "source is no longer a regular file"));
    }
    if is_cloud_placeholder(metadata) {
        return Err(reject(ActionStatus::CloudPlaceholder, "source became a cloud placeholder"));
    }
    if metadata.len() != action.size() || modified_ns(metadata) != action.source_mtime_ns() {
        return Err(reject(
            ActionStatus::SourceChanged,
            "source size or modification time changed since the scan",
        ));
    }
    Ok(())
}

/// Merge mode: fails unless the destination copy still has the source's file name (case rules of
/// the platform), the only other fact a name-and-size pairing rests on.
fn check_same_name(source: &Path, destination: &Path) -> Result<(), Rejection> {
    let name = |path: &Path| path.file_name().map(|name| name_key(&name.to_string_lossy()));
    if name(source) != name(destination) {
        return Err(reject(ActionStatus::DestinationChanged, "destination file has another name"));
    }
    Ok(())
}

/// Fails unless the destination is a regular local file of the expected size.
fn check_destination_present(path: &Path, size: u64) -> Result<(), Rejection> {
    let metadata = fs::symlink_metadata(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => {
            reject(ActionStatus::DestinationMissing, "destination copy no longer exists")
        }
        _ => io_rejection("cannot read destination metadata", &error),
    })?;
    check_destination_metadata(&metadata, size)
}

/// Fails unless the locked destination handle is still a regular local file of the expected size.
fn check_destination_handle(file: &File, size: u64) -> Result<(), Rejection> {
    let metadata =
        file.metadata().map_err(|error| io_rejection("cannot read destination metadata", &error))?;
    check_destination_metadata(&metadata, size)
}

/// Fails unless destination metadata describes a regular local file of the expected size.
fn check_destination_metadata(metadata: &Metadata, size: u64) -> Result<(), Rejection> {
    if !metadata.file_type().is_file() {
        return Err(reject(ActionStatus::DestinationChanged, "destination is no longer a regular file"));
    }
    if is_cloud_placeholder(metadata) {
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
    destination: &mut File,
    planned_digest: &str,
) -> Result<(), Rejection> {
    let comparison = compare_readers(source, destination)
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

// ----------------------------------------------------------------------------------------------
// Moves (merge mode)
// ----------------------------------------------------------------------------------------------

/// Moves one source file to its planned destination path, never replacing anything there: a
/// rename on the same volume, otherwise a verified copy followed by the deletion of the source.
fn verify_and_move(context: &ActionContext, planned: &PlannedMove) -> Result<ActionStatus, Rejection> {
    if context.is_aborted.load(Ordering::SeqCst) {
        return Err(reject(ActionStatus::Failed, "cancelled: journal could not be written"));
    }
    let source_path = join_rel(&context.source_root, &planned.source_rel)
        .map_err(|e| reject(ActionStatus::Failed, e.to_string()))?;
    let target_path = join_rel(&context.destination_root, &planned.destination_rel)
        .map_err(|e| reject(ActionStatus::Failed, e.to_string()))?;
    check_source_unchanged(&source_path, planned)?;
    check_target_free(&context.destination_root, &planned.destination_rel)?;
    if context.is_dry_run {
        return Ok(ActionStatus::WouldMove);
    }
    if let Some(folder) = target_path.parent() {
        fs::create_dir_all(folder)
            .map_err(|error| io_rejection("cannot create the destination folder", &error))?;
    }
    match rename_no_replace(&source_path, &target_path) {
        Ok(()) => Ok(ActionStatus::Moved),
        Err(error) if is_cross_device_error(&error) => {
            copy_then_delete(context, planned, &source_path, &target_path)
        }
        Err(error) if is_already_exists_error(&error) => Err(occupied_rejection()),
        Err(error) => Err(io_rejection("move failed", &error)),
    }
}

/// Fails if the target path, or a file in place of one of its folders, exists in the destination.
fn check_target_free(destination_root: &Path, target_rel: &str) -> Result<(), Rejection> {
    let mut folder = parent_rel(target_rel);
    while !folder.is_empty() {
        match fs::symlink_metadata(rel_to_path(destination_root, folder)) {
            Ok(metadata) if !metadata.file_type().is_dir() => {
                return Err(reject(ActionStatus::TargetOccupied, format!("{folder:?} is not a folder")));
            }
            _ => folder = parent_rel(folder),
        }
    }
    match fs::symlink_metadata(rel_to_path(destination_root, target_rel)) {
        Ok(_) => Err(occupied_rejection()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_rejection("cannot check the destination path", &error)),
    }
}

/// Returns the rejection of a move whose destination path appeared after the scan.
fn occupied_rejection() -> Rejection {
    reject(ActionStatus::TargetOccupied, "destination path is taken since the scan: nothing replaced")
}

/// Copies the source to a temporary name next to the target, verifies the copy byte by byte
/// against the locked source, gives it its final name, then deletes the source. Any failure
/// removes the temporary copy and leaves the source untouched.
fn copy_then_delete(
    context: &ActionContext,
    planned: &PlannedMove,
    source_path: &Path,
    target_path: &Path,
) -> Result<ActionStatus, Rejection> {
    let mut partial_name = target_path.as_os_str().to_owned();
    partial_name.push(PARTIAL_COPY_SUFFIX);
    let partial_path = PathBuf::from(partial_name);
    copy_preserving_metadata(source_path, &partial_path)
        .map_err(|error| io_rejection("copy to the destination failed", &error))?;
    let outcome = finish_copied_move(context, planned, (source_path, &partial_path, target_path));
    if outcome.is_err()
        && let Err(error) = remove_file_forcing(&partial_path)
    {
        eprintln!("WARNING: temporary copy {} could not be removed: {error}", display_path(&partial_path));
    }
    outcome
}

/// Verifies a temporary copy, renames it to the target and deletes the source through its locked
/// handle. Paths are (source, temporary copy, target).
fn finish_copied_move(
    context: &ActionContext,
    planned: &PlannedMove,
    (source_path, partial_path, target_path): (&Path, &Path, &Path),
) -> Result<ActionStatus, Rejection> {
    let mut source_file = open_source_for_deletion(source_path)
        .map_err(|error| io_rejection("cannot open source for deletion", &error))?;
    let source_metadata =
        source_file.metadata().map_err(|error| io_rejection("cannot read source metadata", &error))?;
    check_source_metadata(&source_metadata, planned)?;
    let mut copy = open_destination_locked(partial_path)
        .map_err(|error| io_rejection("cannot open the copy", &error))?;
    let comparison = compare_readers(&mut source_file, &mut copy)
        .map_err(|error| io_rejection("cannot read for comparison", &error))?;
    drop(copy);
    if comparison.is_none() {
        return Err(reject(ActionStatus::ContentDiffers, "the copy differs from the source"));
    }
    check_streams_copied(source_path, partial_path, &context.ignored_stream_names)?;
    rename_no_replace(partial_path, target_path).map_err(|error| {
        if is_already_exists_error(&error) {
            occupied_rejection()
        } else {
            io_rejection("rename failed", &error)
        }
    })?;
    delete_open_file(source_file, source_path).map_err(|error| {
        reject(
            ActionStatus::Failed,
            format!("copied to the destination, but the source could not be deleted: {error}"),
        )
    })?;
    Ok(ActionStatus::Moved)
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

    /// Appends the line that opens a run: which plan, which roots, real or dry run.
    fn record_run_start(&self, plan: &Plan, is_dry_run: bool) -> Result<()> {
        let kind = if is_dry_run { "dry run" } else { "apply" };
        let detail = format!(
            "{kind} of plan {} ({} planned deletions, {} planned moves)",
            plan.plan_id,
            plan.body.actions.len(),
            plan.body.moves.len()
        );
        let roots = (self.source_root.clone(), self.destination_root.clone());
        self.write_line(JOURNAL_RUN_STARTED, &roots.0, &roots.1, plan.total_bytes(), &detail)
    }

    /// Appends one decision and flushes it to disk.
    fn record(&self, planned: &impl PlannedFile, result: &ActionResult) -> Result<()> {
        self.write_line(
            result.status.tag(),
            &display_joined(&self.source_root, planned.source_rel()),
            &display_joined(&self.destination_root, planned.destination_rel()),
            planned.size(),
            &result.detail,
        )
    }

    /// Appends one metadata removal decision and flushes it to disk.
    fn record_metadata(&self, removal: &PlannedMetadataRemoval, result: &MetadataResult) -> Result<()> {
        let path = display_joined(&self.source_root, &removal.metadata_rel);
        self.write_line(result.status.tag(), &path, "", removal.bytes, &result.detail)
    }

    /// Writes one line and flushes it to disk.
    fn write_line(
        &self,
        status: &str,
        source: &str,
        destination: &str,
        size: u64,
        detail: &str,
    ) -> Result<()> {
        let mut writer = self.writer.lock().map_err(|_| anyhow::anyhow!("journal lock poisoned"))?;
        writer.write_record([
            timestamp_iso().as_str(),
            status,
            source,
            destination,
            &size.to_string(),
            detail,
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
// Subversion metadata
// ----------------------------------------------------------------------------------------------

/// Removes each planned Subversion metadata folder whose working copy now holds nothing else
/// (deepest first). A dry run reports the removals its deletions would allow.
fn remove_planned_metadata(
    plan: &Plan,
    context: &ActionContext,
    results: &[ActionResult],
    journal: &Journal,
) -> Result<Vec<MetadataResult>> {
    let removals = &plan.body.metadata_removals;
    if removals.is_empty() {
        return Ok(Vec::new());
    }
    print_step("Subversion metadata of emptied working copies");
    let mut order: Vec<usize> = (0..removals.len()).collect();
    order.sort_by_key(|&position| Reverse(depth_of(&removals[position].metadata_rel)));
    let mut outcomes = vec![None; removals.len()];
    for position in order {
        let removal = &removals[position];
        let result = if context.is_dry_run {
            predict_metadata_removal(plan, results, removal)
        } else {
            remove_metadata(context, removal)
        };
        journal.record_metadata(removal, &result)?;
        outcomes[position] = Some(result);
    }
    Ok(outcomes.into_iter().flatten().collect())
}

/// Dry run: the removal would happen if every planned deletion of its working copy succeeds.
fn predict_metadata_removal(
    plan: &Plan,
    results: &[ActionResult],
    removal: &PlannedMetadataRemoval,
) -> MetadataResult {
    let working_copy = parent_rel(&removal.metadata_rel);
    let is_blocked = plan.body.actions.iter().zip(results).any(|(action, result)| {
        is_inside_rel(&action.source_rel, working_copy) && !result.status.is_removal()
    });
    if is_blocked {
        return metadata_result(MetadataStatus::Kept, "some files of the working copy would be kept");
    }
    metadata_result(MetadataStatus::WouldRemove, "checked again on disk after the real deletions")
}

/// Removes one metadata folder if its working copy holds nothing but metadata and folders.
fn remove_metadata(context: &ActionContext, removal: &PlannedMetadataRemoval) -> MetadataResult {
    let (metadata_path, working_copy) = match join_rel(&context.source_root, &removal.metadata_rel) {
        Ok(path) => (path, rel_to_path(&context.source_root, parent_rel(&removal.metadata_rel))),
        Err(error) => return metadata_result(MetadataStatus::Failed, &error.to_string()),
    };
    match holds_only_removable_metadata(&working_copy) {
        Ok(true) => {}
        Ok(false) => return metadata_result(MetadataStatus::Kept, "the working copy still holds files"),
        Err(error) => return metadata_result(MetadataStatus::Failed, &format!("cannot check: {error}")),
    }
    match remove_metadata_dir(&metadata_path) {
        Ok(()) => metadata_result(MetadataStatus::Removed, ""),
        Err(error) => metadata_result(MetadataStatus::Failed, &format!("removal failed: {error}")),
    }
}

/// Builds a metadata result.
fn metadata_result(status: MetadataStatus, detail: &str) -> MetadataResult {
    MetadataResult { status, detail: detail.to_string() }
}

/// Returns true if `rel` is `dir` or below it ("" contains everything).
fn is_inside_rel(rel: &str, dir: &str) -> bool {
    dir.is_empty() || rel == dir || rel.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'))
}

// ----------------------------------------------------------------------------------------------
// Emptied folders
// ----------------------------------------------------------------------------------------------

/// Removes source folders left empty by the deletions and metadata removals (deepest first; the
/// root is kept).
fn remove_emptied_dirs(
    source_root: &Path,
    plan: &Plan,
    results: &[ActionResult],
    metadata_results: &[MetadataResult],
) -> (Vec<String>, Vec<ScanError>) {
    let deleted_files = plan
        .body
        .actions
        .iter()
        .zip(results)
        .filter(|(_, result)| result.status == ActionStatus::Deleted)
        .map(|(action, _)| action.source_rel.as_str());
    let removed_metadata = plan
        .body
        .metadata_removals
        .iter()
        .zip(metadata_results)
        .filter(|(_, result)| result.status == MetadataStatus::Removed)
        .map(|(removal, _)| removal.metadata_rel.as_str());
    let mut folders: BTreeSet<&str> = BTreeSet::new();
    for removed in deleted_files.chain(removed_metadata) {
        let mut folder = parent_rel(removed);
        while !folder.is_empty() && folders.insert(folder) {
            folder = parent_rel(folder);
        }
    }
    let mut ordered: Vec<&str> = folders.into_iter().collect();
    ordered.sort_by_key(|rel| Reverse(depth_of(rel)));
    remove_dirs_if_empty(source_root, ordered.into_iter())
}

/// Merge mode: removes every empty source folder, deepest first, including folders that were
/// already empty before (the root is kept). Folders holding anything left are kept.
fn remove_all_empty_dirs(source_root: &Path, config: &Config) -> (Vec<String>, Vec<ScanError>) {
    let options = WalkOptions { excluded_names: config.excluded_names.clone(), excluded_paths: Vec::new() };
    let mut folders = scan_tree(source_root, &options).dirs;
    folders.retain(|rel| !rel.is_empty());
    folders.sort_by_key(|rel| Reverse(depth_of(rel)));
    remove_dirs_if_empty(source_root, folders.iter().map(String::as_str))
}

/// Removes each folder that is empty, in the given order; returns those removed and the errors.
fn remove_dirs_if_empty<'a>(
    source_root: &Path,
    ordered: impl Iterator<Item = &'a str>,
) -> (Vec<String>, Vec<ScanError>) {
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
    let proof = if output.plan.body.mode == MatchMode::Merge {
        "because a destination file has the same name and size (content not compared)"
    } else {
        "after byte-by-byte verification"
    };
    let mut notes = vec![
        format!("{files} files ({}) {verb} {proof}.", format_bytes(bytes)),
        format!(
            "{} planned deletions were skipped by a safety check and {} failed: those files are untouched.",
            output.skipped_count(),
            output.count(ActionStatus::Failed)
        ),
        format!("Journal of every decision: {}", display_path(&output.journal_path)),
    ];
    if !output.plan.body.moves.is_empty() {
        let (moved_files, moved_bytes) = output.moved_totals();
        let verb = if output.is_dry_run { "would be moved" } else { "were moved" };
        notes.push(format!(
            "{moved_files} of {} planned files ({}) {verb} into the destination, never replacing anything.",
            output.plan.body.moves.len(),
            format_bytes(moved_bytes)
        ));
    }
    if !output.is_dry_run {
        notes.push(format!("{} emptied folders were removed.", output.removed_dirs.len()));
    }
    if !output.plan.body.metadata_removals.is_empty() {
        let (count, bytes) = output
            .metadata_removals()
            .fold((0, 0), |(count, bytes), removal| (count + 1, bytes + removal.bytes));
        let verb = if output.is_dry_run { "would be removed" } else { "were removed" };
        notes.push(format!(
            "{count} of {} Subversion metadata folders ({}) {verb} with their emptied working copy.",
            output.plan.body.metadata_removals.len(),
            format_bytes(bytes)
        ));
    }
    notes
}

/// Builds the headline cards.
fn build_cards(output: &ApplyOutput) -> Vec<Card> {
    let (files, bytes) = output.removed_totals();
    let failed = output.count(ActionStatus::Failed) as u64;
    let removed_label = if output.is_dry_run { "Would be deleted" } else { "Deleted" };
    let is_merge = output.plan.body.mode == MatchMode::Merge;
    let removed_note = if is_merge { "same name and size" } else { "verified identical" };
    let mut cards = vec![card(removed_label, Some(bytes), Some(files), removed_note, Tone::Good)];
    if is_merge {
        let (moved_files, moved_bytes) = output.moved_totals();
        let moved_label = if output.is_dry_run { "Would be moved" } else { "Moved" };
        cards.push(card(
            moved_label,
            Some(moved_bytes),
            Some(moved_files),
            "into the destination",
            Tone::Good,
        ));
    }
    cards.extend([
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
    ]);
    cards
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
    let removed: Vec<(&str, u64)> = output
        .removals()
        .map(|action| (action.source_rel.as_str(), action.size))
        .chain(output.moved().map(|planned| (planned.source_rel.as_str(), planned.size)))
        .collect();
    let removed_rels: HashSet<&str> = removed.iter().map(|&(rel, _)| rel).collect();
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
        files.extend(removed.iter().map(|&(rel, size)| FolderFile { rel, size, is_removed: true }));
    }
    aggregate_folders(&after.dirs, &files)
}

/// Builds the detail tables.
fn build_tables(output: &ApplyOutput, after: &Inventory, roots: (&str, &str)) -> Vec<Table> {
    let deletion_rows = result_rows(&output.plan.body.actions, &output.results);
    let metadata_rows =
        output.plan.body.metadata_removals.iter().zip(&output.metadata_results).map(|(removal, result)| {
            ReportRow {
                path: removal.metadata_rel.clone(),
                other: String::new(),
                size: removal.bytes,
                tag: result.status.tag().to_string(),
                detail: result.detail.clone(),
            }
        });
    let folder_rows = metadata_rows
        .chain(output.removed_dirs.iter().map(|rel| ReportRow {
            path: rel.clone(),
            other: String::new(),
            size: 0,
            tag: "removed-folder".to_string(),
            detail: String::new(),
        }))
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
    let mut tables = vec![Table::new(
        "results",
        "Planned deletions",
        "Result of every planned deletion. Anything not 'deleted' was left untouched.",
        ["Source file", "Destination copy", "Size", "Status", "Detail"],
        roots,
        deletion_rows,
    )];
    if !output.plan.body.moves.is_empty() {
        tables.push(Table::new(
            "moves",
            "Planned moves",
            "Result of every planned move into the destination. Anything not 'moved' was left untouched.",
            ["Source file", "Destination path", "Size", "Status", "Detail"],
            roots,
            result_rows(&output.plan.body.moves, &output.move_results),
        ));
    }
    tables.extend([
        Table::new(
            "folders",
            "Removed folders",
            "Subversion metadata of emptied working copies, and source folders the deletions emptied.",
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
    ]);
    tables
}

/// Rows of a result table: one per planned operation, with its status.
fn result_rows(planned: &[impl PlannedFile], results: &[ActionResult]) -> Vec<ReportRow> {
    planned
        .iter()
        .zip(results)
        .map(|(item, result)| ReportRow {
            path: item.source_rel().to_string(),
            other: item.destination_rel().to_string(),
            size: item.size(),
            tag: result.status.tag().to_string(),
            detail: result.detail.clone(),
        })
        .collect()
}
