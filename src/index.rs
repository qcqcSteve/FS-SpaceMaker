//! Persistent fingerprint index (SQLite) so later scans skip files whose size and modification
//! time are unchanged. Rows are keyed by absolute path, so a scan of a folder reuses what a scan
//! of its whole drive computed. The index only speeds up planning: apply always re-compares bytes.

use std::collections::{HashMap, HashSet};
use std::path::{MAIN_SEPARATOR, Path};
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
/// Separator used inside stored paths, on every platform.
const KEY_SEPARATOR: char = '/';
/// Character that sorts right after the separator: upper bound of a folder's key range.
const KEY_RANGE_END: char = '0';
/// Pruning at least this many rows compacts the database file afterwards.
const VACUUM_MIN_PRUNED_ROWS: usize = 100_000;
/// Schema; `partial_bytes` records the prefix length the partial digest was computed with.
const SCHEMA_SQL: &str = "
    CREATE TABLE IF NOT EXISTS file_digest (
        path          TEXT    NOT NULL PRIMARY KEY,
        size          INTEGER NOT NULL,
        mtime_ns      INTEGER NOT NULL,
        partial_bytes INTEGER NOT NULL,
        partial       BLOB,
        full          BLOB
    ) WITHOUT ROWID;";
const SELECT_RANGE_SQL: &str = "SELECT path, size, mtime_ns, partial_bytes, partial, full FROM file_digest \
     WHERE path >= ?1 AND path < ?2";
const UPSERT_SQL: &str = "INSERT OR REPLACE INTO file_digest
    (path, size, mtime_ns, partial_bytes, partial, full) VALUES (?1, ?2, ?3, ?4, ?5, ?6)";
const DELETE_SQL: &str = "DELETE FROM file_digest WHERE path = ?1";
const COUNT_RANGE_SQL: &str = "SELECT COUNT(*) FROM file_digest WHERE path >= ?1 AND path < ?2";
const VACUUM_SQL: &str = "VACUUM";
/// Layout of versions up to 1.0 (rows keyed by scan root and relative path), migrated on open.
const LEGACY_TABLE_SQL: &str = "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'file_hash'";
const LEGACY_SELECT_SQL: &str =
    "SELECT root, rel, size, mtime_ns, partial_bytes, partial, full FROM file_hash";
const LEGACY_DROP_SQL: &str = "DROP TABLE file_hash";

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
    /// Opens (or creates) the index file, migrating the old layout if present.
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)
            .with_context(|| format!("Cannot open index {}: check the work folder.", path.display()))?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.execute_batch(SCHEMA_SQL).context("Cannot create the index schema")?;
        let mut index = Self { connection };
        index.migrate_legacy_rows().context("Cannot migrate the index to the new layout")?;
        Ok(index)
    }

    /// Returns every indexed file below a root (display path), keyed by '/'-separated relative path.
    pub fn load_root(&self, root: &str) -> Result<HashMap<String, IndexedHashes>> {
        let prefix = root_prefix(root);
        let mut statement = self.connection.prepare(SELECT_RANGE_SQL)?;
        let rows = statement.query_map(params![prefix, range_end(&prefix)], |row| {
            let path: String = row.get(0)?;
            Ok((path[prefix.len()..].to_string(), read_hashes(row, 1)?))
        })?;
        rows.collect::<rusqlite::Result<HashMap<_, _>>>().context("Cannot read the index")
    }

    /// Inserts or replaces rows of files below a root in one transaction.
    pub fn store_root(&mut self, root: &str, rows: &[(String, IndexedHashes)]) -> Result<()> {
        let prefix = root_prefix(root);
        let transaction = self.connection.transaction()?;
        {
            let mut statement = transaction.prepare(UPSERT_SQL)?;
            for (rel, hashes) in rows {
                write_hashes(&mut statement, &format!("{prefix}{rel}"), hashes)?;
            }
        }
        transaction.commit().context("Cannot write the index")
    }

    /// Deletes the rows below a root whose file no longer exists; returns how many were deleted.
    /// A large prune compacts the file so the index does not keep growing.
    pub fn prune_root(&mut self, root: &str, live_rels: &HashSet<&str>) -> Result<usize> {
        let stale: Vec<String> =
            self.load_root(root)?.into_keys().filter(|rel| !live_rels.contains(rel.as_str())).collect();
        let prefix = root_prefix(root);
        let transaction = self.connection.transaction()?;
        {
            let mut statement = transaction.prepare(DELETE_SQL)?;
            for rel in &stale {
                statement.execute(params![format!("{prefix}{rel}")])?;
            }
        }
        transaction.commit().context("Cannot prune the index")?;
        if stale.len() >= VACUUM_MIN_PRUNED_ROWS {
            self.connection.execute_batch(VACUUM_SQL).context("Cannot compact the index")?;
        }
        Ok(stale.len())
    }

    /// Returns how many files below a root are indexed.
    pub fn count_root(&self, root: &str) -> Result<u64> {
        let prefix = root_prefix(root);
        let count: Option<i64> = self
            .connection
            .query_row(COUNT_RANGE_SQL, params![prefix, range_end(&prefix)], |row| row.get(0))
            .optional()?;
        Ok(to_u64(count.unwrap_or(0)))
    }

    /// Moves rows of the old (root, rel) layout to absolute-path keys, then drops the old table.
    fn migrate_legacy_rows(&mut self) -> Result<()> {
        let has_legacy: Option<String> =
            self.connection.query_row(LEGACY_TABLE_SQL, [], |row| row.get(0)).optional()?;
        if has_legacy.is_none() {
            return Ok(());
        }
        let transaction = self.connection.transaction()?;
        {
            let mut select = transaction.prepare(LEGACY_SELECT_SQL)?;
            let mut upsert = transaction.prepare(UPSERT_SQL)?;
            let mut rows = select.query([])?;
            while let Some(row) = rows.next()? {
                let root: String = row.get(0)?;
                let rel: String = row.get(1)?;
                write_hashes(&mut upsert, &format!("{}{rel}", root_prefix(&root)), &read_hashes(row, 2)?)?;
            }
        }
        transaction.execute_batch(LEGACY_DROP_SQL)?;
        transaction.commit()?;
        self.connection.execute_batch(VACUUM_SQL)?;
        Ok(())
    }
}

/// Returns the key prefix of a root display path: '/' separators and exactly one trailing '/'.
fn root_prefix(root: &str) -> String {
    let normalized = root.replace(MAIN_SEPARATOR, &KEY_SEPARATOR.to_string());
    format!("{}{KEY_SEPARATOR}", normalized.trim_end_matches(KEY_SEPARATOR))
}

/// Returns the first key after every key starting with `prefix` (which ends with the separator).
fn range_end(prefix: &str) -> String {
    format!("{}{KEY_RANGE_END}", &prefix[..prefix.len() - KEY_SEPARATOR.len_utf8()])
}

/// Reads the five hash columns starting at `first`.
fn read_hashes(row: &rusqlite::Row, first: usize) -> rusqlite::Result<IndexedHashes> {
    Ok(IndexedHashes {
        size: to_u64(row.get(first)?),
        mtime_ns: row.get(first + 1)?,
        partial_bytes: to_u64(row.get(first + 2)?),
        partial: to_digest(row.get(first + 3)?),
        full: to_digest(row.get(first + 4)?),
    })
}

/// Executes the upsert statement for one file.
fn write_hashes(statement: &mut rusqlite::Statement, key: &str, hashes: &IndexedHashes) -> Result<()> {
    statement.execute(params![
        key,
        to_i64(hashes.size),
        hashes.mtime_ns,
        to_i64(hashes.partial_bytes),
        hashes.partial.as_ref().map(<[u8; 32]>::as_slice),
        hashes.full.as_ref().map(<[u8; 32]>::as_slice),
    ])?;
    Ok(())
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
    use std::path::MAIN_SEPARATOR_STR;

    /// Builds a root display path from components with the native separator.
    fn native_root(parts: &[&str]) -> String {
        parts.join(MAIN_SEPARATOR_STR)
    }

    fn sample(size: u64) -> IndexedHashes {
        IndexedHashes { size, mtime_ns: -5, partial_bytes: 4096, partial: Some([1; 32]), full: None }
    }

    #[test]
    fn stores_and_reloads_rows_per_root() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = HashIndex::open(&dir.path().join(INDEX_FILE_NAME)).unwrap();
        index.store_root("rootA", &[("a/b.txt".to_string(), sample(10))]).unwrap();
        let loaded = index.load_root("rootA").unwrap();
        assert_eq!(loaded.get("a/b.txt"), Some(&sample(10)));
        assert!(index.load_root("rootB").unwrap().is_empty());
        assert!(index.load_root("root").unwrap().is_empty(), "a name prefix is not a parent folder");
        assert_eq!(index.count_root("rootA").unwrap(), 1);
    }

    #[test]
    fn a_folder_reuses_rows_stored_for_its_drive() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = HashIndex::open(&dir.path().join(INDEX_FILE_NAME)).unwrap();
        let drive = native_root(&["S:", ""]);
        let rows = [("Fin/a.txt".to_string(), sample(1)), ("Finance/a.txt".to_string(), sample(2))];
        index.store_root(&drive, &rows).unwrap();
        let fin = index.load_root(&native_root(&["S:", "Fin"])).unwrap();
        assert_eq!(fin.len(), 1, "S:/Finance must not leak into S:/Fin");
        assert_eq!(fin["a.txt"], sample(1));
        assert_eq!(index.load_root(&native_root(&["S:", "Finance"])).unwrap()["a.txt"], sample(2));
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

    #[test]
    fn prunes_rows_of_files_that_no_longer_exist() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = HashIndex::open(&dir.path().join(INDEX_FILE_NAME)).unwrap();
        let rows = [("kept".to_string(), sample(1)), ("gone".to_string(), sample(2))];
        index.store_root("r", &rows).unwrap();
        index.store_root("other", &rows).unwrap();
        assert_eq!(index.prune_root("r", &HashSet::from(["kept"])).unwrap(), 1);
        assert_eq!(index.load_root("r").unwrap().into_keys().collect::<Vec<_>>(), vec!["kept"]);
        assert_eq!(index.count_root("other").unwrap(), 2, "other roots are untouched");
    }

    #[test]
    fn migrates_the_legacy_layout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(INDEX_FILE_NAME);
        let legacy = Connection::open(&path).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE file_hash (root TEXT, rel TEXT, size INTEGER, mtime_ns INTEGER,
                 partial_bytes INTEGER, partial BLOB, full BLOB, PRIMARY KEY (root, rel)) WITHOUT ROWID;",
            )
            .unwrap();
        let drive = native_root(&["K:", ""]);
        legacy
            .execute(
                "INSERT INTO file_hash VALUES (?1, 'dir/f.txt', 10, -5, 4096, ?2, NULL)",
                params![drive, [1u8; 32].as_slice()],
            )
            .unwrap();
        drop(legacy);
        let index = HashIndex::open(&path).unwrap();
        assert_eq!(index.load_root(&drive).unwrap()["dir/f.txt"], sample(10));
        assert_eq!(index.load_root(&native_root(&["K:", "dir"])).unwrap()["f.txt"], sample(10));
        let has_legacy: Option<String> =
            index.connection.query_row(LEGACY_TABLE_SQL, [], |row| row.get(0)).optional().unwrap();
        assert!(has_legacy.is_none(), "the old table is dropped");
    }
}
