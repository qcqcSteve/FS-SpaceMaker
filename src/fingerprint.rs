//! Computes partial (prefix) and full content digests for chosen files of one inventory, in
//! parallel, reusing digests from the persistent index when size and modification time match.
//! Files no larger than the prefix length get their full digest directly (one read only).
//! Work is done in batches and each batch is saved to the index at once, so an interrupted scan
//! resumes where it stopped instead of reading everything again.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Mutex;

use rayon::ThreadPool;
use rayon::prelude::*;
use serde::Serialize;

use crate::hashing::{Digest, empty_digest, hash_prefix};
use crate::index::{HashIndex, IndexedHashes};
use crate::inventory::Inventory;
use crate::progress::ProgressMeter;

// ----------------------------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------------------------

/// A batch ends after this many files or bytes read, whichever comes first; each batch is then
/// saved to the index (at most a few minutes of reading are lost if the scan is interrupted).
const CHECKPOINT_FILES: usize = 20_000;
const CHECKPOINT_BYTES: u64 = 8 * 1024 * 1024 * 1024;

// ----------------------------------------------------------------------------------------------
// Model
// ----------------------------------------------------------------------------------------------

/// Which digest a step needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DigestKind {
    Partial,
    Full,
}

/// Work counters of one side.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct HashStats {
    pub files_read: u64,
    pub bytes_read: u64,
    pub index_hits: u64,
}

/// Shared execution resources for digest computation.
pub struct HashingRuntime<'a> {
    pub pool: &'a ThreadPool,
    pub meter: &'a ProgressMeter,
    /// Where each batch is saved; None keeps digests in memory only.
    pub index: Option<&'a Mutex<HashIndex>>,
}

/// Digests known for files of one inventory.
pub struct Fingerprints<'a> {
    inventory: &'a Inventory,
    index_root: String,
    indexed: HashMap<String, IndexedHashes>,
    partial_bytes: u64,
    partial: HashMap<usize, Digest>,
    full: HashMap<usize, Digest>,
    failures: HashMap<usize, String>,
    computed: HashSet<usize>,
    pub stats: HashStats,
}

impl<'a> Fingerprints<'a> {
    /// Creates an empty digest table backed by previously indexed digests.
    pub fn new(
        inventory: &'a Inventory,
        index_root: &str,
        indexed: HashMap<String, IndexedHashes>,
        partial_bytes: u64,
    ) -> Self {
        Self {
            inventory,
            index_root: index_root.to_string(),
            indexed,
            partial_bytes,
            partial: HashMap::new(),
            full: HashMap::new(),
            failures: HashMap::new(),
            computed: HashSet::new(),
            stats: HashStats::default(),
        }
    }

    /// Returns the inventory these digests describe.
    pub fn inventory(&self) -> &'a Inventory {
        self.inventory
    }

    /// Returns the prefix digest (the full digest for files no larger than the prefix).
    pub fn partial_digest(&self, index: usize) -> Option<Digest> {
        match self.effective_kind(index, DigestKind::Partial) {
            DigestKind::Partial => self.partial.get(&index).copied(),
            DigestKind::Full => self.full.get(&index).copied(),
        }
    }

    /// Returns the full-content digest.
    pub fn full_digest(&self, index: usize) -> Option<Digest> {
        self.full.get(&index).copied()
    }

    /// Returns the read error of a file, if any.
    pub fn failure(&self, index: usize) -> Option<&str> {
        self.failures.get(&index).map(String::as_str)
    }

    /// Makes sure prefix digests exist for the given files.
    pub fn ensure_partial(&mut self, indices: &[usize], runtime: &HashingRuntime) {
        self.ensure(indices, DigestKind::Partial, runtime);
    }

    /// Makes sure full digests exist for the given files.
    pub fn ensure_full(&mut self, indices: &[usize], runtime: &HashingRuntime) {
        self.ensure(indices, DigestKind::Full, runtime);
    }

    /// Returns index rows for every file whose digests were computed during this run.
    pub fn rows_to_index(&self) -> Vec<(String, IndexedHashes)> {
        let mut rows: Vec<(String, IndexedHashes)> =
            self.computed.iter().map(|&index| self.row_for(index)).collect();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        rows
    }

    /// Builds the index row of one file, keeping still-valid digests from the old row.
    fn row_for(&self, index: usize) -> (String, IndexedHashes) {
        let record = &self.inventory.files[index];
        let previous = self.valid_indexed(index);
        let partial = self.partial.get(&index).copied().or_else(|| {
            previous.filter(|row| row.partial_bytes == self.partial_bytes).and_then(|row| row.partial)
        });
        let full = self.full.get(&index).copied().or_else(|| previous.and_then(|row| row.full));
        let row = IndexedHashes {
            size: record.size,
            mtime_ns: record.mtime_ns,
            partial_bytes: self.partial_bytes,
            partial,
            full,
        };
        (record.rel.clone(), row)
    }

    /// Returns the kind actually needed: small files only ever need a full digest.
    fn effective_kind(&self, index: usize, kind: DigestKind) -> DigestKind {
        if self.inventory.files[index].size <= self.partial_bytes { DigestKind::Full } else { kind }
    }

    /// Returns the indexed row of a file if its size and modification time are unchanged.
    fn valid_indexed(&self, index: usize) -> Option<&IndexedHashes> {
        let record = &self.inventory.files[index];
        self.indexed.get(&record.rel).filter(|row| row.size == record.size && row.mtime_ns == record.mtime_ns)
    }

    /// Returns a usable digest from the index, if any.
    fn indexed_digest(&self, index: usize, kind: DigestKind) -> Option<Digest> {
        let row = self.valid_indexed(index)?;
        match kind {
            DigestKind::Full => row.full,
            DigestKind::Partial if row.partial_bytes == self.partial_bytes => row.partial,
            DigestKind::Partial => None,
        }
    }

    /// Returns the digest already known in memory.
    fn known_digest(&self, index: usize, kind: DigestKind) -> Option<Digest> {
        match kind {
            DigestKind::Partial => self.partial.get(&index).copied(),
            DigestKind::Full => self.full.get(&index).copied(),
        }
    }

    /// Stores a digest in memory.
    fn remember(&mut self, index: usize, kind: DigestKind, digest: Digest) {
        match kind {
            DigestKind::Partial => self.partial.insert(index, digest),
            DigestKind::Full => self.full.insert(index, digest),
        };
    }

    /// Computes the missing digests of one kind, in parallel.
    fn ensure(&mut self, indices: &[usize], requested: DigestKind, runtime: &HashingRuntime) {
        let mut pending: Vec<(usize, DigestKind)> = Vec::new();
        for &index in indices {
            let kind = self.effective_kind(index, requested);
            if self.failures.contains_key(&index) || self.known_digest(index, kind).is_some() {
                continue;
            }
            if self.inventory.files[index].size == 0 {
                self.remember(index, kind, empty_digest());
                continue;
            }
            if let Some(digest) = self.indexed_digest(index, kind) {
                self.stats.index_hits += 1;
                self.remember(index, kind, digest);
                continue;
            }
            pending.push((index, kind));
        }
        pending.sort_unstable_by_key(|&(index, _)| index);
        pending.dedup();
        for batch in self.batches(&pending) {
            let results = self.compute_all(batch, runtime);
            for (&(index, kind), result) in batch.iter().zip(results) {
                self.record_result(index, kind, result);
            }
            self.save_batch(batch, runtime);
        }
    }

    /// Splits pending work into batches bounded by file count and bytes to read.
    fn batches<'p>(&self, pending: &'p [(usize, DigestKind)]) -> Vec<&'p [(usize, DigestKind)]> {
        let mut batches = Vec::new();
        let (mut start, mut bytes) = (0, 0_u64);
        for (position, &(index, kind)) in pending.iter().enumerate() {
            bytes = bytes.saturating_add(self.bytes_to_read(index, kind));
            if position + 1 - start >= CHECKPOINT_FILES || bytes >= CHECKPOINT_BYTES {
                batches.push(&pending[start..=position]);
                (start, bytes) = (position + 1, 0);
            }
        }
        if start < pending.len() {
            batches.push(&pending[start..]);
        }
        batches
    }

    /// Returns how many bytes digesting one file reads.
    fn bytes_to_read(&self, index: usize, kind: DigestKind) -> u64 {
        let size = self.inventory.files[index].size;
        if kind == DigestKind::Partial { size.min(self.partial_bytes) } else { size }
    }

    /// Saves the digests of a finished batch to the index. A failure only costs speed on the next
    /// scan, so it is reported and the scan goes on.
    fn save_batch(&self, batch: &[(usize, DigestKind)], runtime: &HashingRuntime) {
        let Some(index) = runtime.index else {
            return;
        };
        let rows: Vec<(String, IndexedHashes)> = batch
            .iter()
            .filter(|(position, _)| self.computed.contains(position))
            .map(|&(position, _)| self.row_for(position))
            .collect();
        let saved = match index.lock() {
            Ok(mut index) => index.store_root(&self.index_root, &rows),
            Err(_) => Err(anyhow::anyhow!("index lock poisoned")),
        };
        if let Err(error) = saved {
            eprintln!("  warning: cannot save fingerprints to the index ({error:#}); the scan goes on.");
        }
    }

    /// Reads and digests the pending files on the thread pool.
    fn compute_all(
        &self,
        pending: &[(usize, DigestKind)],
        runtime: &HashingRuntime,
    ) -> Vec<io::Result<Digest>> {
        let inventory = self.inventory;
        let partial_bytes = self.partial_bytes;
        let meter = runtime.meter;
        runtime.pool.install(|| {
            pending
                .par_iter()
                .map(|&(index, kind)| {
                    let limit = if kind == DigestKind::Partial { partial_bytes } else { u64::MAX };
                    let result = hash_prefix(&inventory.path_of(index), limit);
                    meter.record(inventory.files[index].size.min(limit));
                    result
                })
                .collect()
        })
    }

    /// Stores one computed digest or its error.
    fn record_result(&mut self, index: usize, kind: DigestKind, result: io::Result<Digest>) {
        match result {
            Ok(digest) => {
                self.stats.files_read += 1;
                self.stats.bytes_read += self.bytes_to_read(index, kind);
                self.computed.insert(index);
                self.remember(index, kind, digest);
            }
            Err(error) => {
                self.failures.insert(index, error.to_string());
            }
        }
    }
}

// ----------------------------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashing::hash_file;
    use crate::inventory::{WalkOptions, scan_tree};
    use std::fs;

    const SMALL_PREFIX: u64 = 4096;

    fn runtime_parts() -> (ThreadPool, ProgressMeter) {
        (rayon::ThreadPoolBuilder::new().num_threads(2).build().unwrap(), ProgressMeter::new("test"))
    }

    const TEST_ROOT: &str = "root";

    #[test]
    fn computes_partial_then_full_and_reuses_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("big"), vec![1u8; 10_000]).unwrap();
        fs::write(root.join("small"), b"tiny").unwrap();
        let inventory = scan_tree(&root, &WalkOptions::default());
        let (pool, meter) = runtime_parts();
        let runtime = HashingRuntime { pool: &pool, meter: &meter, index: None };
        let mut fingerprints = Fingerprints::new(&inventory, TEST_ROOT, HashMap::new(), SMALL_PREFIX);
        fingerprints.ensure_partial(&[0, 1], &runtime);
        assert!(fingerprints.full_digest(0).is_none(), "big file must not be fully read yet");
        assert_eq!(fingerprints.full_digest(1), Some(hash_file(&root.join("small")).unwrap()));
        fingerprints.ensure_full(&[0], &runtime);
        assert_eq!(fingerprints.full_digest(0), Some(hash_file(&root.join("big")).unwrap()));
        assert_eq!(fingerprints.stats.files_read, 3);

        let indexed: HashMap<String, IndexedHashes> = fingerprints.rows_to_index().into_iter().collect();
        let mut reused = Fingerprints::new(&inventory, TEST_ROOT, indexed, SMALL_PREFIX);
        reused.ensure_partial(&[0, 1], &runtime);
        reused.ensure_full(&[0, 1], &runtime);
        assert_eq!(reused.stats.files_read, 0);
        assert_eq!(reused.full_digest(0), fingerprints.full_digest(0));
    }

    #[test]
    fn stale_index_rows_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("f"), b"current").unwrap();
        let inventory = scan_tree(&root, &WalkOptions::default());
        let stale = IndexedHashes {
            size: inventory.files[0].size,
            mtime_ns: inventory.files[0].mtime_ns - 1,
            partial_bytes: SMALL_PREFIX,
            partial: None,
            full: Some([0; 32]),
        };
        let indexed = HashMap::from([("f".to_string(), stale)]);
        let (pool, meter) = runtime_parts();
        let runtime = HashingRuntime { pool: &pool, meter: &meter, index: None };
        let mut fingerprints = Fingerprints::new(&inventory, TEST_ROOT, indexed, SMALL_PREFIX);
        fingerprints.ensure_full(&[0], &runtime);
        assert_eq!(fingerprints.full_digest(0), Some(hash_file(&root.join("f")).unwrap()));
    }

    #[test]
    fn every_batch_is_saved_to_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for name in ["a", "b", "c"] {
            fs::write(root.join(name), name.repeat(10)).unwrap();
        }
        let inventory = scan_tree(&root, &WalkOptions::default());
        let (pool, meter) = runtime_parts();
        let index = Mutex::new(HashIndex::open(&dir.path().join("index.sqlite")).unwrap());
        let runtime = HashingRuntime { pool: &pool, meter: &meter, index: Some(&index) };
        let mut fingerprints = Fingerprints::new(&inventory, TEST_ROOT, HashMap::new(), SMALL_PREFIX);
        fingerprints.ensure_full(&[0, 2], &runtime);
        let saved = index.lock().unwrap().load_root(TEST_ROOT).unwrap();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved["a"].full, fingerprints.full_digest(0));
        assert!(!saved.contains_key("b"), "files never read are not indexed");
    }

    #[test]
    fn batches_respect_the_file_limit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("f"), b"x").unwrap();
        let inventory = scan_tree(&root, &WalkOptions::default());
        let fingerprints = Fingerprints::new(&inventory, TEST_ROOT, HashMap::new(), SMALL_PREFIX);
        let pending = vec![(0, DigestKind::Full); CHECKPOINT_FILES * 2 + 1];
        let sizes: Vec<usize> = fingerprints.batches(&pending).iter().map(|batch| batch.len()).collect();
        assert_eq!(sizes, vec![CHECKPOINT_FILES, CHECKPOINT_FILES, 1]);
    }

    #[test]
    fn unreadable_files_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("gone"), b"data").unwrap();
        let inventory = scan_tree(&root, &WalkOptions::default());
        fs::remove_file(root.join("gone")).unwrap();
        let (pool, meter) = runtime_parts();
        let runtime = HashingRuntime { pool: &pool, meter: &meter, index: None };
        let mut fingerprints = Fingerprints::new(&inventory, TEST_ROOT, HashMap::new(), SMALL_PREFIX);
        fingerprints.ensure_full(&[0], &runtime);
        assert!(fingerprints.full_digest(0).is_none());
        assert!(fingerprints.failure(0).is_some());
    }
}
