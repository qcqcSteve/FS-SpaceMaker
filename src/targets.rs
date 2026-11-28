//! Merge mode: chooses where each moved source file lands in the destination. A file keeps its
//! relative path when that path is free; otherwise it gets the first free numbered name
//! ("report (2).pdf"). A source folder whose name is taken by a destination file is renamed the
//! same way, consistently for everything inside it. Nothing in the destination is ever overwritten.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::inventory::Inventory;
use crate::paths::{display_path, file_name_of, join_rels, name_key, parent_rel, rel_from};

// ----------------------------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------------------------

/// First number given to a renamed entry: "name (2)" sits next to the original "name".
const FIRST_COPY_NUMBER: u32 = 2;
/// Separator between a file stem and its extension.
const EXTENSION_SEPARATOR: char = '.';

// ----------------------------------------------------------------------------------------------
// Allocation
// ----------------------------------------------------------------------------------------------

/// Destination paths already used, as comparison keys, and the folders chosen for source folders.
pub struct TargetAllocator {
    /// Files, links and metadata folders: never replaced, never merged into.
    occupied: HashSet<String>,
    /// Folders by key, with their spelling: a source folder of the same name merges into them.
    folders: HashMap<String, String>,
    folder_targets: HashMap<String, String>,
}

impl TargetAllocator {
    /// Starts from everything the destination inventory holds.
    pub fn new(destination: &Inventory) -> Self {
        let occupied = destination
            .files
            .iter()
            .map(|file| file.rel.as_str())
            .chain(destination.metadata_dirs.iter().map(|dir| dir.rel.as_str()))
            .map(name_key)
            .chain(skipped_keys(destination))
            .collect();
        let folders = destination.dirs.iter().map(|dir| (name_key(dir), dir.clone())).collect();
        Self { occupied, folders, folder_targets: HashMap::new() }
    }

    /// Returns the free destination path chosen for a source file, and reserves it.
    pub fn allocate_file(&mut self, source_rel: &str) -> String {
        let folder = self.folder_target(parent_rel(source_rel));
        let name = file_name_of(source_rel);
        let target =
            first_free(&folder, name, |key| !self.occupied.contains(key) && !self.folders.contains_key(key));
        self.occupied.insert(name_key(&target));
        target
    }

    /// Returns the destination folder chosen for a source folder ("" is the root).
    fn folder_target(&mut self, source_dir: &str) -> String {
        if source_dir.is_empty() {
            return String::new();
        }
        if let Some(target) = self.folder_targets.get(source_dir) {
            return target.clone();
        }
        let parent = self.folder_target(parent_rel(source_dir));
        let candidate = first_free(&parent, file_name_of(source_dir), |key| !self.occupied.contains(key));
        // Merging into an existing folder keeps its spelling (it may differ in case only).
        let target = self.folders.entry(name_key(&candidate)).or_insert(candidate).clone();
        self.folder_targets.insert(source_dir.to_string(), target.clone());
        target
    }
}

/// Returns `folder/name`, or the first numbered variant accepted by `is_free` (given a name key).
fn first_free(folder: &str, name: &str, is_free: impl Fn(&str) -> bool) -> String {
    let plain = join_rels(folder, name);
    if is_free(&name_key(&plain)) {
        return plain;
    }
    (FIRST_COPY_NUMBER..)
        .map(|number| join_rels(folder, &numbered_name(name, number)))
        .find(|candidate| is_free(&name_key(candidate)))
        .expect("an unbounded range always yields a free name")
}

/// Inserts a copy number before the extension: "a.tar.gz" gives "a.tar (2).gz", ".env" gives ".env (2)".
pub fn numbered_name(name: &str, number: u32) -> String {
    match name.rfind(EXTENSION_SEPARATOR) {
        Some(dot) if dot > 0 => format!("{} ({number}){}", &name[..dot], &name[dot..]),
        _ => format!("{name} ({number})"),
    }
}

/// Returns the keys of skipped destination entries (links, placeholders), stored as full paths.
fn skipped_keys(destination: &Inventory) -> Vec<String> {
    let root = display_path(&destination.root);
    destination
        .skipped
        .iter()
        .filter_map(|entry| rel_from(Path::new(&root), Path::new(&entry.path)))
        .map(|rel| name_key(&rel))
        .collect()
}

// ----------------------------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::FileRecord;
    use std::path::PathBuf;

    fn destination(files: &[&str], dirs: &[&str]) -> Inventory {
        Inventory {
            root: PathBuf::from("root"),
            files: files
                .iter()
                .map(|rel| FileRecord { rel: rel.to_string(), size: 1, mtime_ns: 0, is_read_only: false })
                .collect(),
            dirs: std::iter::once("").chain(dirs.iter().copied()).map(str::to_string).collect(),
            metadata_dirs: Vec::new(),
            skipped: Vec::new(),
            errors: Vec::new(),
        }
    }

    #[test]
    fn numbers_go_before_the_extension() {
        assert_eq!(numbered_name("report.pdf", 2), "report (2).pdf");
        assert_eq!(numbered_name("a.tar.gz", 3), "a.tar (3).gz");
        assert_eq!(numbered_name(".env", 2), ".env (2)");
        assert_eq!(numbered_name("README", 2), "README (2)");
    }

    #[test]
    fn free_paths_are_kept_and_taken_ones_are_numbered() {
        let mut allocator = TargetAllocator::new(&destination(&["a/x.txt", "a/x (2).txt"], &["a"]));
        assert_eq!(allocator.allocate_file("a/y.txt"), "a/y.txt");
        assert_eq!(allocator.allocate_file("a/x.txt"), "a/x (3).txt");
        assert_eq!(allocator.allocate_file("b/x.txt"), "b/x.txt");
    }

    #[test]
    fn two_source_files_never_get_the_same_target() {
        let mut allocator = TargetAllocator::new(&destination(&[], &[]));
        assert_eq!(allocator.allocate_file("dup (2).txt"), "dup (2).txt");
        assert_eq!(allocator.allocate_file("dup.txt"), "dup.txt");
    }

    #[test]
    fn a_folder_blocked_by_a_file_is_renamed_for_all_its_content() {
        let mut allocator = TargetAllocator::new(&destination(&["docs"], &[]));
        assert_eq!(allocator.allocate_file("docs/a.txt"), "docs (2)/a.txt");
        assert_eq!(allocator.allocate_file("docs/sub/b.txt"), "docs (2)/sub/b.txt");
    }

    #[test]
    fn a_file_never_replaces_a_folder() {
        let mut allocator = TargetAllocator::new(&destination(&[], &["photos"]));
        assert_eq!(allocator.allocate_file("photos"), "photos (2)");
    }

    #[test]
    fn names_compare_with_the_platform_case_rules() {
        let mut allocator = TargetAllocator::new(&destination(&["A/X.TXT"], &["A"]));
        let target = allocator.allocate_file("a/x.txt");
        if crate::paths::IS_CASE_INSENSITIVE {
            assert_eq!(target, "A/x (2).txt", "merges into the existing folder, renames the file");
        } else {
            assert_eq!(target, "a/x.txt");
        }
    }
}
