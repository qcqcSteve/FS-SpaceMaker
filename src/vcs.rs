//! Version-control metadata folders (.svn, .git, .hg, .bzr). They are never matched file by file:
//! deleting part of one would corrupt its working copy, and the destination's copies are internal
//! storage, not user files. Subversion metadata is summarized from its wc.db (repository URL and
//! uncommitted local changes, read without writing or locking) and, only when explicitly allowed,
//! removed once its working copy has been emptied by verified deletions.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{MAIN_SEPARATOR, Path};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::inventory::Inventory;
use crate::paths::{REL_SEPARATOR, display_path, parent_rel};
use crate::plan::PlannedMetadataRemoval;

// ----------------------------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------------------------

/// Folder names of each kind of metadata (compared case-insensitively). "_svn" is the old
/// Windows spelling of ".svn".
const SUBVERSION_NAMES: [&str; 2] = [".svn", "_svn"];
const GIT_NAMES: [&str; 1] = [".git"];
const MERCURIAL_NAMES: [&str; 1] = [".hg"];
const BAZAAR_NAMES: [&str; 1] = [".bzr"];
/// Subversion 1.7+ working copy database, inside the metadata folder.
const SVN_DATABASE_FILE_NAME: &str = "wc.db";
/// Repository root URL of the working copy.
const SVN_REPOSITORY_SQL: &str = "SELECT root FROM REPOSITORY ORDER BY id LIMIT 1";
/// Local operations not yet committed (additions, deletions, copies, moves).
const SVN_LOCAL_CHANGES_SQL: &str = "SELECT COUNT(*) FROM NODES WHERE op_depth > 0";
/// SQLite URI options: read-only and immutable, so nothing is ever written or locked.
const SQLITE_URI_OPTIONS: &str = "?mode=ro&immutable=1";
/// URI scheme and the separator used inside URIs.
const FILE_URI_SCHEME: &str = "file:";
const URI_SEPARATOR: char = '/';
/// Characters kept as is in a file URI path, besides ASCII letters and digits.
const URI_PLAIN_CHARS: [char; 6] = ['/', ':', '-', '.', '_', '~'];

// ----------------------------------------------------------------------------------------------
// Model
// ----------------------------------------------------------------------------------------------

/// Kind of version-control metadata folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MetadataKind {
    Subversion,
    Git,
    Mercurial,
    Bazaar,
}

impl MetadataKind {
    /// Returns the kind of a metadata folder name, if it is one.
    pub fn of_name(name: &str) -> Option<Self> {
        let kinds: [(Self, &[&str]); 4] = [
            (Self::Subversion, &SUBVERSION_NAMES),
            (Self::Git, &GIT_NAMES),
            (Self::Mercurial, &MERCURIAL_NAMES),
            (Self::Bazaar, &BAZAAR_NAMES),
        ];
        kinds
            .into_iter()
            .find(|(_, names)| names.iter().any(|known| known.eq_ignore_ascii_case(name)))
            .map(|(kind, _)| kind)
    }

    /// Returns true for centralized systems, whose history lives on the server: their metadata
    /// may be removed with an emptied working copy. Distributed systems keep the whole history
    /// in the folder, which is therefore never deleted.
    pub fn is_removable(self) -> bool {
        self == Self::Subversion
    }

    /// Returns a human name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Subversion => "Subversion",
            Self::Git => "Git",
            Self::Mercurial => "Mercurial",
            Self::Bazaar => "Bazaar",
        }
    }
}

/// What a Subversion working copy database says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SvnSummary {
    pub repository_url: String,
    pub local_changes: u64,
}

// ----------------------------------------------------------------------------------------------
// Description
// ----------------------------------------------------------------------------------------------

/// Reads the repository URL and uncommitted change count of a Subversion metadata folder.
/// Returns None for formats older than Subversion 1.7 or an unreadable database.
pub fn read_svn_summary(metadata_dir: &Path) -> Option<SvnSummary> {
    let database = metadata_dir.join(SVN_DATABASE_FILE_NAME);
    if !database.is_file() {
        return None;
    }
    let flags =
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(sqlite_uri(&database), flags).ok()?;
    let repository_url: String = connection.query_row(SVN_REPOSITORY_SQL, [], |row| row.get(0)).ok()?;
    let local_changes: i64 = connection.query_row(SVN_LOCAL_CHANGES_SQL, [], |row| row.get(0)).ok()?;
    Some(SvnSummary { repository_url, local_changes: u64::try_from(local_changes).unwrap_or(0) })
}

/// Builds the read-only, immutable SQLite URI of a database file.
fn sqlite_uri(path: &Path) -> String {
    let text = display_path(path).replace(std::path::MAIN_SEPARATOR, &URI_SEPARATOR.to_string());
    // Empty authority, then the path: file:///C:/x, file:///home/x, file:////server/share/x.
    let mut uri = format!("{FILE_URI_SCHEME}{URI_SEPARATOR}{URI_SEPARATOR}");
    if !text.starts_with(URI_SEPARATOR) {
        uri.push(URI_SEPARATOR);
    }
    for byte in text.bytes() {
        let character = char::from(byte);
        if byte.is_ascii_alphanumeric() || URI_PLAIN_CHARS.contains(&character) {
            uri.push(character);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri.push_str(SQLITE_URI_OPTIONS);
    uri
}

// ----------------------------------------------------------------------------------------------
// Planning
// ----------------------------------------------------------------------------------------------

/// Returns the Subversion metadata folders whose whole working copy the plan empties: every
/// file below the working copy folder is planned for deletion, and nothing below it was
/// skipped, unreadable or another kind of metadata.
pub fn plan_metadata_removals(source: &Inventory, removed: &HashSet<usize>) -> Vec<PlannedMetadataRemoval> {
    let Some(blocked) = blocked_dirs(source, removed) else {
        return Vec::new();
    };
    source
        .metadata_dirs
        .iter()
        .filter(|dir| dir.kind.is_removable() && !blocked.contains(parent_rel(&dir.rel)))
        .map(|dir| PlannedMetadataRemoval {
            metadata_rel: dir.rel.clone(),
            files: dir.files,
            bytes: dir.bytes,
        })
        .collect()
}

/// Returns every location that keeps something after the plan, with all its ancestor folders,
/// or None when a kept entry cannot be located (then nothing is removable).
fn blocked_dirs(source: &Inventory, removed: &HashSet<usize>) -> Option<HashSet<String>> {
    let root_display = display_path(&source.root);
    let mut kept: Vec<String> = source
        .files
        .iter()
        .enumerate()
        .filter(|(index, _)| !removed.contains(index))
        .map(|(_, file)| file.rel.clone())
        .collect();
    kept.extend(
        source.metadata_dirs.iter().filter(|dir| !dir.kind.is_removable()).map(|dir| dir.rel.clone()),
    );
    let untouched =
        source.skipped.iter().map(|entry| &entry.path).chain(source.errors.iter().map(|error| &error.path));
    for path in untouched {
        kept.push(rel_of_display(&root_display, path)?);
    }
    let mut blocked = HashSet::new();
    for rel in &kept {
        insert_with_ancestors(&mut blocked, rel);
    }
    Some(blocked)
}

/// Inserts a relative path and its ancestors ("" last), stopping at the first one already known
/// (its ancestors were inserted with it).
fn insert_with_ancestors(set: &mut HashSet<String>, rel: &str) {
    let mut current = rel;
    while !set.contains(current) {
        set.insert(current.to_string());
        if current.is_empty() {
            return;
        }
        current = parent_rel(current);
    }
}

/// Converts a display path below the root display path into a '/'-separated relative path.
fn rel_of_display(root_display: &str, path: &str) -> Option<String> {
    let rest = path.strip_prefix(root_display)?;
    let rest = if rest.is_empty() || root_display.ends_with(MAIN_SEPARATOR) {
        rest
    } else {
        rest.strip_prefix(MAIN_SEPARATOR)?
    };
    Some(rest.replace(MAIN_SEPARATOR, &REL_SEPARATOR.to_string()))
}

/// Returns true if a folder holds nothing but folders and removable (Subversion) metadata:
/// no file, no link, no other kind of metadata anywhere below it.
pub fn holds_only_removable_metadata(dir: &Path) -> io::Result<bool> {
    let kind_of = |entry: &walkdir::DirEntry| entry.file_name().to_str().and_then(MetadataKind::of_name);
    let walker = WalkDir::new(dir).follow_links(false).min_depth(1).into_iter().filter_entry(|entry| {
        !(entry.file_type().is_dir() && kind_of(entry).is_some_and(MetadataKind::is_removable))
    });
    for entry in walker {
        let entry = entry.map_err(io::Error::other)?;
        if !entry.file_type().is_dir() || kind_of(&entry).is_some() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Removes a metadata folder and everything in it. Read-only attributes (Subversion marks its
/// pristine copies read-only) are cleared first; links inside are removed, never followed.
pub fn remove_metadata_dir(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(io::Error::other("metadata folder is a link or a file: left untouched"));
    }
    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry.map_err(io::Error::other)?;
        if entry.file_type().is_file() {
            clear_read_only(entry.path())?;
        }
    }
    fs::remove_dir_all(path)
}

/// Clears the read-only attribute of a file if it is set.
#[allow(clippy::permissions_set_readonly_false)]
fn clear_read_only(path: &Path) -> io::Result<()> {
    let mut permissions = fs::symlink_metadata(path)?.permissions();
    if permissions.readonly() {
        // On Windows this only clears FILE_ATTRIBUTE_READONLY; elsewhere it adds owner write.
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

// ----------------------------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal Subversion 1.7+ schema: the two tables read by `read_svn_summary`.
    const TEST_SCHEMA_SQL: &str = "
        CREATE TABLE REPOSITORY (id INTEGER PRIMARY KEY, root TEXT, uuid TEXT);
        CREATE TABLE NODES (local_relpath TEXT, op_depth INTEGER);
        INSERT INTO REPOSITORY VALUES (1, 'https://server/svn/Finance', 'uuid');
        INSERT INTO NODES VALUES ('', 0), ('a.txt', 1), ('b.txt', 1);";

    #[test]
    fn metadata_names_are_recognised_case_insensitively() {
        assert_eq!(MetadataKind::of_name(".SVN"), Some(MetadataKind::Subversion));
        assert_eq!(MetadataKind::of_name("_svn"), Some(MetadataKind::Subversion));
        assert_eq!(MetadataKind::of_name(".git"), Some(MetadataKind::Git));
        assert_eq!(MetadataKind::of_name("svn"), None);
        assert!(MetadataKind::Subversion.is_removable());
        assert!(!MetadataKind::Git.is_removable());
    }

    #[test]
    fn svn_summary_is_read_without_modifying_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let metadata_dir = dir.path().join("Fin é #1").join(".svn");
        fs::create_dir_all(&metadata_dir).unwrap();
        let database = metadata_dir.join(SVN_DATABASE_FILE_NAME);
        Connection::open(&database).unwrap().execute_batch(TEST_SCHEMA_SQL).unwrap();
        let before = fs::read(&database).unwrap();
        let summary = read_svn_summary(&metadata_dir).unwrap();
        assert_eq!(summary.repository_url, "https://server/svn/Finance");
        assert_eq!(summary.local_changes, 2);
        assert_eq!(fs::read(&database).unwrap(), before, "the database must be left byte-identical");
        let names: Vec<_> = fs::read_dir(&metadata_dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1, "no journal or lock file may be created: {names:?}");
    }

    #[test]
    fn old_or_missing_databases_give_no_summary() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_svn_summary(dir.path()).is_none());
        fs::write(dir.path().join(SVN_DATABASE_FILE_NAME), b"not a database").unwrap();
        assert!(read_svn_summary(dir.path()).is_none());
    }

    #[test]
    fn uris_escape_special_characters() {
        let uri = sqlite_uri(Path::new("a b#?%é"));
        assert!(uri.starts_with("file:///a%20b%23%3F%25%C3%A9"), "{uri}");
        assert!(uri.ends_with(SQLITE_URI_OPTIONS));
    }

    #[test]
    fn working_copy_emptiness_ignores_only_subversion_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".svn").join("pristine")).unwrap();
        fs::write(root.join(".svn").join("wc.db"), b"db").unwrap();
        fs::create_dir_all(root.join("empty").join("_svn")).unwrap();
        fs::write(root.join("empty").join("_svn").join("entries"), b"old format").unwrap();
        assert!(holds_only_removable_metadata(root).unwrap());
        fs::create_dir_all(root.join("sub").join(".git")).unwrap();
        assert!(!holds_only_removable_metadata(root).unwrap(), "a Git folder is never removable");
        fs::remove_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("empty").join("left.txt"), b"x").unwrap();
        assert!(!holds_only_removable_metadata(root).unwrap(), "a remaining file blocks removal");
    }

    #[test]
    fn display_paths_convert_only_below_the_root() {
        let sep = MAIN_SEPARATOR;
        let root = format!("S:{sep}Fin");
        assert_eq!(rel_of_display(&root, &format!("{root}{sep}a{sep}b")).as_deref(), Some("a/b"));
        assert_eq!(rel_of_display(&root, &root).as_deref(), Some(""));
        assert_eq!(rel_of_display(&root, &format!("S:{sep}Finance{sep}x")), None);
        let drive = format!("S:{sep}");
        assert_eq!(rel_of_display(&drive, &format!("S:{sep}x")).as_deref(), Some("x"));
    }

    #[test]
    fn ancestors_are_inserted_once() {
        let mut set = HashSet::new();
        insert_with_ancestors(&mut set, "a/b/c.txt");
        insert_with_ancestors(&mut set, "a/d.txt");
        let mut items: Vec<&str> = set.iter().map(String::as_str).collect();
        items.sort_unstable();
        assert_eq!(items, vec!["", "a", "a/b", "a/b/c.txt", "a/d.txt"]);
    }

    #[test]
    fn metadata_removal_handles_read_only_files() {
        let dir = tempfile::tempdir().unwrap();
        let metadata_dir = dir.path().join(".svn");
        let pristine = metadata_dir.join("pristine").join("ab");
        fs::create_dir_all(&pristine).unwrap();
        let file = pristine.join("abcdef.svn-base");
        fs::write(&file, b"base text").unwrap();
        let mut permissions = fs::metadata(&file).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&file, permissions).unwrap();
        remove_metadata_dir(&metadata_dir).unwrap();
        assert!(!metadata_dir.exists());
        assert!(dir.path().exists());
    }
}
