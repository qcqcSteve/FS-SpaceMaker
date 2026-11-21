//! Persistent fingerprint index (SQLite) so later scans skip files whose size and modification
//! time are unchanged. The index only speeds up planning: apply always re-compares bytes.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::hashing::Digest;

// ----------------------------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------------------------

/// File name of the index inside the work folder.
pub const INDEX_FILE_NAME: &str = "index.sqlite";
/// How long to wait when another process holds the database lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(30);
/// Schema; `partial_bytes` records the prefix length the partial digest was computed with.
const SCHEMA_SQL: &str = "
    CREATE TABLE IF NOT EXISTS file_hash (
        root          TEXT    NOT NULL,
        rel           TEXT    NOT NULL,
        size          INTEGER NOT NULL,
        mtime_ns      INTEGER NOT NULL,
        partial_bytes INTEGER NOT NULL,
        partial       BLOB,
        full          BLOB,
        PRIMARY KEY (root, rel)
    ) WITHOUT ROWID;";
const SELECT_ROOT_SQL: &str =
    "SELECT rel, size, mtime_ns, partial_bytes, partial, full FROM file_hash WHERE root = ?1";
const UPSERT_SQL: &str = "INSERT OR REPLACE INTO file_hash
    (root, rel, size, mtime_ns, partial_bytes, partial, full) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)";
const COUNT_SQL: &str = "SELECT COUNT(*) FROM file_hash WHERE root = ?1";

// ----------------------------------------------------------------------------------------------
// Index
// ----------------------------------------------------------------------------------------------

/// Fingerprints known for one file, valid only while size and modification time are unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexedHashes {
    pub size: u64,
    pub mtime_ns: i64,
    pub partial_bytes: u64,
    pub partial: Option<Digest>,
    pub full: Option<Digest>,
}

/// Handle on the SQLite index file.
pub struct HashIndex {
    connection: Connection,
}

impl HashIndex {
    /// Opens (or creates) the index file.
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)
            .with_context(|| format!("Cannot open index {}: check the work folder.", path.display()))?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.execute_batch(SCHEMA_SQL).context("Cannot create the index schema")?;
        Ok(Self { connection })
    }

    /// Returns every indexed file of a root, keyed by relative path.
    pub fn load_root(&self, root: &str) -> Result<HashMap<String, IndexedHashes>> {
        let mut statement = self.connection.prepare(SELECT_ROOT_SQL)?;
        let rows = statement.query_map(params![root], |row| {
            let hashes = IndexedHashes {
                size: to_u64(row.get(1)?),
                mtime_ns: row.get(2)?,
                partial_bytes: to_u64(row.get(3)?),
                partial: to_digest(row.get(4)?),
                full: to_digest(row.get(5)?),
            };
            Ok((row.get::<_, String>(0)?, hashes))
        })?;
        rows.collect::<rusqlite::Result<HashMap<_, _>>>().context("Cannot read the index")
    }

    /// Inserts or replaces rows of a root in one transaction.
    pub fn store_root(&mut self, root: &str, rows: &[(String, IndexedHashes)]) -> Result<()> {
        let transaction = self.connection.transaction()?;
        {
            let mut statement = transaction.prepare(UPSERT_SQL)?;
            for (rel, hashes) in rows {
                statement.execute(params![
                    root,
                    rel,
                    to_i64(hashes.size),
                    hashes.mtime_ns,
                    to_i64(hashes.partial_bytes),
                    hashes.partial.as_ref().map(<[u8; 32]>::as_slice),
                    hashes.full.as_ref().map(<[u8; 32]>::as_slice),
                ])?;
            }
        }
        transaction.commit().context("Cannot write the index")
    }

    /// Returns how many files of a root are indexed.
    pub fn count_root(&self, root: &str) -> Result<u64> {
        let count: Option<i64> =
            self.connection.query_row(COUNT_SQL, params![root], |row| row.get(0)).optional()?;
        Ok(to_u64(count.unwrap_or(0)))
    }
}

/// Converts a stored digest blob; anything not 32 bytes long is treated as absent.
fn to_digest(blob: Option<Vec<u8>>) -> Option<Digest> {
    blob.and_then(|bytes| Digest::try_from(bytes.as_slice()).ok())
}

/// Converts a non-negative SQLite integer (sizes never exceed i64::MAX).
fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// Converts a size for SQLite storage.
fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

// ----------------------------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_reloads_rows_per_root() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = HashIndex::open(&dir.path().join(INDEX_FILE_NAME)).unwrap();
        let hashes =
            IndexedHashes { size: 10, mtime_ns: -5, partial_bytes: 4096, partial: Some([1; 32]), full: None };
        index.store_root("rootA", &[("a/b.txt".to_string(), hashes.clone())]).unwrap();
        let loaded = index.load_root("rootA").unwrap();
        assert_eq!(loaded.get("a/b.txt"), Some(&hashes));
        assert!(index.load_root("rootB").unwrap().is_empty());
        assert_eq!(index.count_root("rootA").unwrap(), 1);
    }

    #[test]
    fn replaces_existing_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(INDEX_FILE_NAME);
        let mut index = HashIndex::open(&path).unwrap();
        let mut hashes = IndexedHashes { size: 1, ..Default::default() };
        index.store_root("r", &[("f".to_string(), hashes.clone())]).unwrap();
        hashes.full = Some([9; 32]);
        index.store_root("r", &[("f".to_string(), hashes.clone())]).unwrap();
        drop(index);
        let reopened = HashIndex::open(&path).unwrap();
        assert_eq!(reopened.load_root("r").unwrap()["f"], hashes);
    }
}
