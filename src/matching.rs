//! Decides, for each source file, whether an identical destination file exists in the chosen mode.
//! Identity is proven in three narrowing stages: bucket (size + mode label), prefix digest, full
//! digest. Only files sharing a bucket with the other side are ever read.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::fingerprint::{Fingerprints, HashingRuntime};
use crate::hashing::Digest;
use crate::inventory::{FileRecord, Inventory};
use crate::paths::{file_name_of, name_key};
use crate::tree_match::match_trees;

// ----------------------------------------------------------------------------------------------
// Model
// ----------------------------------------------------------------------------------------------

/// How a source file must correspond to a destination file to be deletable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum MatchMode {
    /// Identical content anywhere in the destination, whatever the name or folder.
    Content,
    /// Identical content at the same relative path in the destination.
    SamePath,
    /// Whole source folders found, identical, elsewhere in the destination.
    Relocated,
}

impl MatchMode {
    /// Returns the command-line spelling of the mode.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::SamePath => "same-path",
            Self::Relocated => "relocated",
        }
    }
}

/// Why a source file stays on the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResidualReason {
    NoCandidate,
    ContentDiffers,
    EmptyFileExcluded,
    Unreadable,
    NotInMatchedTree,
    ReadOnly,
    SameFile,
    AlternateStreams,
}

impl ResidualReason {
    /// Returns a stable machine tag.
    pub fn tag(self) -> &'static str {
        match self {
            Self::NoCandidate => "no-candidate",
            Self::ContentDiffers => "content-differs",
            Self::EmptyFileExcluded => "empty-file",
            Self::Unreadable => "unreadable",
            Self::NotInMatchedTree => "tree-incomplete",
            Self::ReadOnly => "read-only",
            Self::SameFile => "same-physical-file",
            Self::AlternateStreams => "alternate-streams",
        }
    }

    /// Returns a human explanation for the given mode.
    pub fn describe(self, mode: MatchMode) -> &'static str {
        match (self, mode) {
            (Self::NoCandidate, MatchMode::Content) => "no destination file has this size",
            (Self::NoCandidate, MatchMode::SamePath) => "no destination file of this size at the same path",
            (Self::NoCandidate, MatchMode::Relocated) => "no destination file with this name and size",
            (Self::ContentDiffers, _) => "destination candidates have different content",
            (Self::EmptyFileExcluded, _) => "empty file: frees nothing (see include_empty_files)",
            (Self::Unreadable, _) => "file could not be read",
            (Self::NotInMatchedTree, _) => "identical copy exists, but its folder is not complete elsewhere",
            (Self::ReadOnly, _) => "read-only file: protected (see delete_read_only)",
            (Self::SameFile, _) => "source and destination are the same physical file (hard link)",
            (Self::AlternateStreams, _) => {
                "hidden NTFS stream missing or different on the destination copy: protected"
            }
        }
    }
}

/// A source file proven identical to a destination file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileMatch {
    pub source: usize,
    pub destination: usize,
}

/// A source folder found whole under another destination folder (relocated mode).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelocatedTree {
    pub source_dir: String,
    pub destination_dir: String,
    pub bytes: u64,
    pub files: usize,
}

/// Result of matching every source file.
#[derive(Clone, Debug, Default)]
pub struct MatchOutcome {
    pub matches: Vec<FileMatch>,
    pub residual: Vec<(usize, ResidualReason)>,
    pub trees: Vec<RelocatedTree>,
}

/// Everything matching needs.
pub struct MatchContext<'a, 'b> {
    pub mode: MatchMode,
    pub include_empty_files: bool,
    pub source: &'b mut Fingerprints<'a>,
    pub destination: &'b mut Fingerprints<'a>,
    pub runtime: &'b HashingRuntime<'b>,
}

/// Size plus mode-specific label: two files can only be identical within one bucket.
type BucketKey = (u64, String);

/// Source files with their identical destination files, and the reasons of the others.
struct IdenticalFiles {
    identical: HashMap<usize, Vec<usize>>,
    reasons: Vec<(usize, ResidualReason)>,
}

// ----------------------------------------------------------------------------------------------
// Matching
// ----------------------------------------------------------------------------------------------

/// Matches every source file against the destination according to the mode.
pub fn match_files(context: &mut MatchContext) -> MatchOutcome {
    let source = context.source.inventory();
    let destination = context.destination.inventory();
    let (eligible, mut residual) = select_eligible(source, context.mode, context.include_empty_files);
    let found = find_identical(context, &eligible);
    residual.extend(found.reasons);
    let mut outcome = if context.mode == MatchMode::Relocated {
        let tree_outcome = match_trees(source, destination, &found.identical);
        let matched: std::collections::HashSet<usize> =
            tree_outcome.matches.iter().map(|m| m.source).collect();
        residual.extend(
            found
                .identical
                .keys()
                .filter(|index| !matched.contains(index))
                .map(|&index| (index, ResidualReason::NotInMatchedTree)),
        );
        MatchOutcome { matches: tree_outcome.matches, residual, trees: tree_outcome.trees }
    } else {
        let matches = found
            .identical
            .iter()
            .map(|(&index, candidates)| FileMatch {
                source: index,
                destination: choose_destination(source, destination, index, candidates),
            })
            .collect();
        MatchOutcome { matches, residual, trees: Vec::new() }
    };
    outcome.matches.sort_by_key(|file_match| file_match.source);
    outcome.residual.sort_by_key(|&(index, _)| index);
    outcome
}

/// Splits source files into those worth matching and those excluded up front.
fn select_eligible(
    source: &Inventory,
    mode: MatchMode,
    include_empty_files: bool,
) -> (Vec<usize>, Vec<(usize, ResidualReason)>) {
    let excludes_empty = mode == MatchMode::Content && !include_empty_files;
    let (empty, eligible): (Vec<usize>, Vec<usize>) =
        (0..source.files.len()).partition(|&index| excludes_empty && source.files[index].size == 0);
    let residual = empty.into_iter().map(|index| (index, ResidualReason::EmptyFileExcluded)).collect();
    (eligible, residual)
}

/// Returns the bucket of a file for the mode.
fn bucket_key(record: &FileRecord, mode: MatchMode) -> BucketKey {
    let label = match mode {
        MatchMode::Content => String::new(),
        MatchMode::SamePath => name_key(&record.rel),
        MatchMode::Relocated => name_key(file_name_of(&record.rel)),
    };
    (record.size, label)
}

/// Finds, for each eligible source file, every destination file with identical content.
fn find_identical(context: &mut MatchContext, eligible: &[usize]) -> IdenticalFiles {
    let source = context.source.inventory();
    let destination = context.destination.inventory();
    let destination_keys: Vec<BucketKey> =
        destination.files.iter().map(|file| bucket_key(file, context.mode)).collect();
    let mut buckets: HashMap<&BucketKey, Vec<usize>> = HashMap::new();
    for (index, key) in destination_keys.iter().enumerate() {
        buckets.entry(key).or_default().push(index);
    }
    let mut reasons = Vec::new();
    let mut candidates: Vec<(usize, BucketKey)> = Vec::new();
    for &index in eligible {
        let key = bucket_key(&source.files[index], context.mode);
        if buckets.contains_key(&key) {
            candidates.push((index, key))
        } else {
            reasons.push((index, ResidualReason::NoCandidate))
        }
    }
    let destination_pool = collect_destinations(&buckets, candidates.iter().map(|(_, key)| key));
    let candidates =
        narrow_by_digest(context, candidates, &destination_pool, &destination_keys, false, &mut reasons);
    let destination_pool =
        collect_destinations_by_digest(context, &candidates, &destination_pool, &destination_keys, false);
    let finalists =
        narrow_by_digest(context, candidates, &destination_pool, &destination_keys, true, &mut reasons);
    let full_groups = group_by_digest(context.destination, &destination_pool, &destination_keys, true);
    let identical = finalists
        .into_iter()
        .filter_map(|(index, key)| {
            let digest = context.source.full_digest(index)?;
            full_groups.get(&(key, digest)).map(|list| (index, list.clone()))
        })
        .collect();
    IdenticalFiles { identical, reasons }
}

/// Returns the destination files of the buckets used by at least one source candidate.
fn collect_destinations<'k>(
    buckets: &HashMap<&BucketKey, Vec<usize>>,
    keys: impl Iterator<Item = &'k BucketKey>,
) -> Vec<usize> {
    let mut pool: Vec<usize> = keys.filter_map(|key| buckets.get(key)).flatten().copied().collect();
    pool.sort_unstable();
    pool.dedup();
    pool
}

/// Returns the destination files whose (bucket, prefix digest) is shared by a source candidate.
fn collect_destinations_by_digest(
    context: &MatchContext,
    candidates: &[(usize, BucketKey)],
    destination_pool: &[usize],
    destination_keys: &[BucketKey],
    is_full: bool,
) -> Vec<usize> {
    let groups = group_by_digest(context.destination, destination_pool, destination_keys, is_full);
    let mut pool: Vec<usize> = candidates
        .iter()
        .filter_map(|(index, key)| {
            let digest = digest_of(context.source, *index, is_full)?;
            groups.get(&(key.clone(), digest))
        })
        .flatten()
        .copied()
        .collect();
    pool.sort_unstable();
    pool.dedup();
    pool
}

/// Computes one digest stage on both sides and keeps the candidates still sharing a group.
fn narrow_by_digest(
    context: &mut MatchContext,
    candidates: Vec<(usize, BucketKey)>,
    destination_pool: &[usize],
    destination_keys: &[BucketKey],
    is_full: bool,
    reasons: &mut Vec<(usize, ResidualReason)>,
) -> Vec<(usize, BucketKey)> {
    let source_indices: Vec<usize> = candidates.iter().map(|(index, _)| *index).collect();
    ensure_digests(context.source, &source_indices, is_full, context.runtime);
    ensure_digests(context.destination, destination_pool, is_full, context.runtime);
    let groups = group_by_digest(context.destination, destination_pool, destination_keys, is_full);
    let mut kept = Vec::new();
    for (index, key) in candidates {
        match digest_of(context.source, index, is_full) {
            None => reasons.push((index, ResidualReason::Unreadable)),
            Some(digest) if groups.contains_key(&(key.clone(), digest)) => kept.push((index, key)),
            Some(_) => reasons.push((index, ResidualReason::ContentDiffers)),
        }
    }
    kept
}

/// Groups destination files by (bucket, digest), sorted by index for deterministic choices.
fn group_by_digest(
    fingerprints: &Fingerprints,
    pool: &[usize],
    keys: &[BucketKey],
    is_full: bool,
) -> HashMap<(BucketKey, Digest), Vec<usize>> {
    let mut groups: HashMap<(BucketKey, Digest), Vec<usize>> = HashMap::new();
    for &index in pool {
        if let Some(digest) = digest_of(fingerprints, index, is_full) {
            groups.entry((keys[index].clone(), digest)).or_default().push(index);
        }
    }
    groups
}

/// Computes the digest stage for the given files.
fn ensure_digests(
    fingerprints: &mut Fingerprints,
    indices: &[usize],
    is_full: bool,
    runtime: &HashingRuntime,
) {
    if is_full {
        fingerprints.ensure_full(indices, runtime)
    } else {
        fingerprints.ensure_partial(indices, runtime)
    }
}

/// Returns the digest of the stage.
fn digest_of(fingerprints: &Fingerprints, index: usize, is_full: bool) -> Option<Digest> {
    if is_full { fingerprints.full_digest(index) } else { fingerprints.partial_digest(index) }
}

/// Picks the destination twin: the one at the same relative path if any, else the first found.
fn choose_destination(
    source: &Inventory,
    destination: &Inventory,
    index: usize,
    candidates: &[usize],
) -> usize {
    let source_key = name_key(&source.files[index].rel);
    candidates
        .iter()
        .copied()
        .find(|&candidate| name_key(&destination.files[candidate].rel) == source_key)
        .unwrap_or(candidates[0])
}
