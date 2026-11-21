//! Computes partial (prefix) and full content digests for chosen files of one inventory, in
//! parallel, reusing digests from the persistent index when size and modification time match.
//! Files no larger than the prefix length get their full digest directly (one read only).

use std::collections::{HashMap, HashSet};
use std::io;

use rayon::ThreadPool;
use rayon::prelude::*;
use serde::Serialize;

use crate::hashing::{Digest, empty_digest, hash_prefix};
use crate::index::IndexedHashes;
use crate::inventory::Inventory;
use crate::progress::ProgressMeter;

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
}

/// Digests known for files of one inventory.
pub struct Fingerprints<'a> {
    inventory: &'a Inventory,
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
        indexed: HashMap<String, IndexedHashes>,
        partial_bytes: u64,
    ) -> Self {
        Self {
            inventory,
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
        let results = self.compute_all(&pending, runtime);
        for ((index, kind), result) in pending.into_iter().zip(results) {
            self.record_result(index, kind, result);
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
        runtime.pool.install(|| {
            pending
                .par_iter()
                .map(|&(index, kind)| {
                    let limit = if kind == DigestKind::Partial { partial_bytes } else { u64::MAX };
                    let result = hash_prefix(&inventory.path_of(index), limit);
                    runtime.meter.record(inventory.files[index].size.min(limit));
                    result
                })
                .collect()
        })
    }

    /// Stores one computed digest or its error.
    fn record_result(&mut self, index: usize, kind: DigestKind, result: io::Result<Digest>) {
        match result {
            Ok(digest) => {
                let size = self.inventory.files[index].size;
                let limit = if kind == DigestKind::Partial { self.partial_bytes } else { u64::MAX };
                self.stats.files_read += 1;
                self.stats.bytes_read += size.min(limit);
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

    #[test]
    fn computes_partial_then_full_and_reuses_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("big"), vec![1u8; 10_000]).unwrap();
        fs::write(root.join("small"), b"tiny").unwrap();
        let inventory = scan_tree(&root, &WalkOptions::default());
        let (pool, meter) = runtime_parts();
        let runtime = HashingRuntime { pool: &pool, meter: &meter };
        let mut fingerprints = Fingerprints::new(&inventory, HashMap::new(), SMALL_PREFIX);
        fingerprints.ensure_partial(&[0, 1], &runtime);
        assert!(fingerprints.full_digest(0).is_none(), "big file must not be fully read yet");
        assert_eq!(fingerprints.full_digest(1), Some(hash_file(&root.join("small")).unwrap()));
        fingerprints.ensure_full(&[0], &runtime);
        assert_eq!(fingerprints.full_digest(0), Some(hash_file(&root.join("big")).unwrap()));
        assert_eq!(fingerprints.stats.files_read, 3);

        let indexed: HashMap<String, IndexedHashes> = fingerprints.rows_to_index().into_iter().collect();
        let mut reused = Fingerprints::new(&inventory, indexed, SMALL_PREFIX);
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
        let runtime = HashingRuntime { pool: &pool, meter: &meter };
        let mut fingerprints = Fingerprints::new(&inventory, indexed, SMALL_PREFIX);
        fingerprints.ensure_full(&[0], &runtime);
        assert_eq!(fingerprints.full_digest(0), Some(hash_file(&root.join("f")).unwrap()));
    }

    #[test]
    fn unreadable_files_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("gone"), b"data").unwrap();
        let inventory = scan_tree(&root, &WalkOptions::default());
        fs::remove_file(root.join("gone")).unwrap();
        let (pool, meter) = runtime_parts();
        let runtime = HashingRuntime { pool: &pool, meter: &meter };
        let mut fingerprints = Fingerprints::new(&inventory, HashMap::new(), SMALL_PREFIX);
        fingerprints.ensure_full(&[0], &runtime);
        assert!(fingerprints.full_digest(0).is_none());
        assert!(fingerprints.failure(0).is_some());
    }
}
