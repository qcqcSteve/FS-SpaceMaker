//! Walks a root folder without following links or junctions and records regular files, folders,
//! version-control metadata folders (measured whole, never added to the file list), skipped
//! entries (links, cloud placeholders, special files, non-Unicode names) and read errors.

use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use walkdir::{DirEntry, WalkDir};

use crate::paths::{display_path, is_same_or_inside, matches_name_pattern, rel_from, rel_to_path};
use crate::platform::is_cloud_placeholder;
use crate::vcs::MetadataKind;

// ----------------------------------------------------------------------------------------------
// Model
// ----------------------------------------------------------------------------------------------

/// A regular file found under a root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRecord {
    pub rel: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub is_read_only: bool,
}

/// Why an entry was left out of the inventory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    Link,
    CloudPlaceholder,
    NonUnicodeName,
    SpecialFile,
}

impl SkipReason {
    /// Returns a stable machine tag.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Link => "link",
            Self::CloudPlaceholder => "cloud-placeholder",
            Self::NonUnicodeName => "non-unicode-name",
            Self::SpecialFile => "special-file",
        }
    }

    /// Returns a human explanation.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Link => "symbolic link or junction: never followed, never deleted",
            Self::CloudPlaceholder => "cloud placeholder (online-only): data is not on this disk",
            Self::NonUnicodeName => "name is not valid Unicode: left untouched",
            Self::SpecialFile => "not a regular file: left untouched",
        }
    }
}

/// An entry left out of the inventory.
#[derive(Clone, Debug)]
pub struct SkippedEntry {
    pub path: String,
    pub size: u64,
    pub reason: SkipReason,
}

/// A version-control metadata folder (or a ".git" link file), measured but never inventoried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetadataDir {
    pub rel: String,
    pub kind: MetadataKind,
    pub files: u64,
    pub bytes: u64,
}

/// A path that could not be read.
#[derive(Clone, Debug)]
pub struct ScanError {
    pub path: String,
    pub message: String,
}

/// Everything found under one root. `dirs` is in pre-order: a parent always precedes its children.
#[derive(Clone, Debug)]
pub struct Inventory {
    pub root: PathBuf,
    pub files: Vec<FileRecord>,
    pub dirs: Vec<String>,
    pub metadata_dirs: Vec<MetadataDir>,
    pub skipped: Vec<SkippedEntry>,
    pub errors: Vec<ScanError>,
}

impl Inventory {
    /// Returns an empty inventory for a root.
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            files: Vec::new(),
            dirs: Vec::new(),
            metadata_dirs: Vec::new(),
            skipped: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// Returns the native path of a file of this inventory.
    pub fn path_of(&self, index: usize) -> PathBuf {
        rel_to_path(&self.root, &self.files[index].rel)
    }

    /// Returns the native path of a relative path below this inventory's root.
    pub fn path_of_rel(&self, rel: &str) -> PathBuf {
        rel_to_path(&self.root, rel)
    }

    /// Returns the sum of file sizes.
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.size).sum()
    }

    /// Records a skipped entry.
    fn skip(&mut self, path: &Path, size: u64, reason: SkipReason) {
        self.skipped.push(SkippedEntry { path: display_path(path), size, reason });
    }

    /// Records a read error.
    fn record_error(&mut self, path: &Path, message: String) {
        self.errors.push(ScanError { path: display_path(path), message });
    }
}

/// Entries to leave out of a walk.
#[derive(Clone, Debug, Default)]
pub struct WalkOptions {
    /// Name patterns (case-insensitive, a trailing '*' matches a prefix).
    pub excluded_names: Vec<String>,
    pub excluded_paths: Vec<PathBuf>,
}

// ----------------------------------------------------------------------------------------------
// Walk
// ----------------------------------------------------------------------------------------------

/// Walks `root` (already canonical) and returns its inventory.
pub fn scan_tree(root: &Path, options: &WalkOptions) -> Inventory {
    let mut inventory = Inventory::new(root);
    let mut walker = WalkDir::new(root).follow_links(false).sort_by_file_name().into_iter();
    while let Some(next) = walker.next() {
        let entry = match next {
            Ok(entry) => entry,
            Err(error) => {
                let path = error.path().unwrap_or(root).to_path_buf();
                inventory.record_error(&path, error.to_string());
                continue;
            }
        };
        if entry.depth() == 0 {
            inventory.dirs.push(String::new());
            continue;
        }
        let is_excluded = is_excluded(&entry, options);
        let rel = if is_excluded { None } else { rel_from(root, entry.path()) };
        if rel.is_none() && !is_excluded {
            inventory.skip(entry.path(), 0, SkipReason::NonUnicodeName);
        }
        let is_walked = match rel {
            Some(rel) => record_entry(&mut inventory, &entry, rel),
            None => false,
        };
        if entry.file_type().is_dir() && !is_walked {
            walker.skip_current_dir();
        }
    }
    inventory
}

/// Returns true if the entry's name or location is excluded.
fn is_excluded(entry: &DirEntry, options: &WalkOptions) -> bool {
    let name = entry.file_name().to_string_lossy();
    options.excluded_names.iter().any(|pattern| matches_name_pattern(&name, pattern))
        || options.excluded_paths.iter().any(|path| is_same_or_inside(entry.path(), path))
}

/// Classifies one walked entry and records it; returns false for a folder not to walk into.
fn record_entry(inventory: &mut Inventory, entry: &DirEntry, rel: String) -> bool {
    let metadata_kind = entry.file_name().to_str().and_then(MetadataKind::of_name);
    if let Some(kind) = metadata_kind.filter(|_| !entry.file_type().is_symlink()) {
        inventory.metadata_dirs.push(measure_metadata(entry, rel, kind));
        return false;
    }
    record_plain_entry(inventory, entry, rel);
    true
}

/// Measures a metadata folder (or file) without following links.
fn measure_metadata(entry: &DirEntry, rel: String, kind: MetadataKind) -> MetadataDir {
    let (files, bytes) = WalkDir::new(entry.path())
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|inner| inner.file_type().is_file())
        .fold((0, 0), |(files, bytes), inner| {
            (files + 1, bytes + inner.metadata().map_or(0, |metadata| metadata.len()))
        });
    MetadataDir { rel, kind, files, bytes }
}

/// Records a link, folder, special file, cloud placeholder or regular file.
fn record_plain_entry(inventory: &mut Inventory, entry: &DirEntry, rel: String) {
    let file_type = entry.file_type();
    if file_type.is_symlink() {
        return inventory.skip(entry.path(), 0, SkipReason::Link);
    }
    if file_type.is_dir() {
        return inventory.dirs.push(rel);
    }
    let metadata = match entry.metadata() {
        Ok(metadata) => metadata,
        Err(error) => return inventory.record_error(entry.path(), error.to_string()),
    };
    if !file_type.is_file() {
        return inventory.skip(entry.path(), 0, SkipReason::SpecialFile);
    }
    if is_cloud_placeholder(&metadata) {
        return inventory.skip(entry.path(), metadata.len(), SkipReason::CloudPlaceholder);
    }
    inventory.files.push(FileRecord {
        rel,
        size: metadata.len(),
        mtime_ns: modified_ns(&metadata),
        is_read_only: metadata.permissions().readonly(),
    });
}

// ----------------------------------------------------------------------------------------------
// Metadata helpers
// ----------------------------------------------------------------------------------------------

/// Returns the modification time in nanoseconds since the Unix epoch (negative before 1970).
pub fn modified_ns(metadata: &Metadata) -> i64 {
    let Ok(modified) = metadata.modified() else {
        return 0;
    };
    match modified.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_nanos()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

// ----------------------------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn records_files_and_dirs_in_pre_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("a").join("b")).unwrap();
        fs::write(root.join("a").join("b").join("f.txt"), b"12345").unwrap();
        fs::write(root.join("top.txt"), b"1").unwrap();
        fs::create_dir(root.join("empty")).unwrap();
        let inventory = scan_tree(&root, &WalkOptions::default());
        assert_eq!(inventory.dirs, vec!["", "a", "a/b", "empty"]);
        let rels: Vec<&str> = inventory.files.iter().map(|file| file.rel.as_str()).collect();
        assert_eq!(rels, vec!["a/b/f.txt", "top.txt"]);
        assert_eq!(inventory.total_bytes(), 6);
        assert!(inventory.errors.is_empty());
    }

    #[test]
    fn excluded_names_and_paths_are_not_walked() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("$RECYCLE.BIN")).unwrap();
        fs::write(root.join("$RECYCLE.BIN").join("x"), b"x").unwrap();
        fs::create_dir_all(root.join("work")).unwrap();
        fs::write(root.join("work").join("y"), b"y").unwrap();
        fs::write(root.join("kept"), b"k").unwrap();
        let options = WalkOptions {
            excluded_names: vec!["$recycle.bin".to_string()],
            excluded_paths: vec![root.join("work")],
        };
        let inventory = scan_tree(&root, &options);
        assert_eq!(inventory.files.len(), 1);
        assert_eq!(inventory.files[0].rel, "kept");
        assert_eq!(inventory.dirs, vec![""]);
    }

    #[test]
    fn name_patterns_exclude_trash_folders() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join(".Trash-1000").join("files")).unwrap();
        fs::write(root.join(".Trash-1000").join("files").join("old.txt"), b"x").unwrap();
        fs::write(root.join("kept.txt"), b"k").unwrap();
        let options = WalkOptions { excluded_names: vec![".trash-*".to_string()], ..WalkOptions::default() };
        let inventory = scan_tree(&root, &options);
        assert_eq!(inventory.files.len(), 1);
        assert_eq!(inventory.dirs, vec![""]);
    }

    #[test]
    fn version_control_folders_are_measured_not_inventoried() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let pristine = root.join("wc").join(".svn").join("pristine");
        fs::create_dir_all(&pristine).unwrap();
        fs::write(pristine.join("base"), b"12345").unwrap();
        fs::write(root.join("wc").join(".svn").join("wc.db"), b"db").unwrap();
        fs::write(root.join("wc").join("file.txt"), b"12345").unwrap();
        fs::create_dir_all(root.join("repo").join(".git")).unwrap();
        let inventory = scan_tree(&root, &WalkOptions::default());
        let rels: Vec<&str> = inventory.files.iter().map(|file| file.rel.as_str()).collect();
        assert_eq!(rels, vec!["wc/file.txt"]);
        assert_eq!(inventory.dirs, vec!["", "repo", "wc"]);
        let svn = MetadataDir { rel: "wc/.svn".into(), kind: MetadataKind::Subversion, files: 2, bytes: 7 };
        let git = MetadataDir { rel: "repo/.git".into(), kind: MetadataKind::Git, files: 0, bytes: 0 };
        assert_eq!(inventory.metadata_dirs, vec![git, svn]);
    }

    #[test]
    fn modification_time_is_preserved_in_nanoseconds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, b"x").unwrap();
        let metadata = fs::metadata(&path).unwrap();
        assert!(modified_ns(&metadata) > 0);
    }
}
