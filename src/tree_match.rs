//! Relocated mode: finds source folders whose whole subtree (every file, same names, same
//! structure, identical content) exists under some destination folder, whatever that folder's
//! own name or location. Folders are evaluated bottom-up by intersecting candidate sets.

use std::collections::{HashMap, HashSet};

use crate::inventory::Inventory;
use crate::matching::{FileMatch, RelocatedTree};
use crate::paths::{file_name_of, name_key, parent_rel};

// ----------------------------------------------------------------------------------------------
// Model
// ----------------------------------------------------------------------------------------------

/// Result of relocated matching.
#[derive(Clone, Debug, Default)]
pub struct TreeOutcome {
    pub matches: Vec<FileMatch>,
    pub trees: Vec<RelocatedTree>,
}

/// Destination folders a source folder could correspond to.
#[derive(Clone, Debug)]
enum Candidates {
    /// The folder holds no file at any depth: it constrains nothing.
    Unconstrained,
    /// Destination folder ids holding the whole subtree.
    Within(HashSet<usize>),
}

impl Candidates {
    /// Returns the intersection with another candidate set.
    fn intersect(self, other: HashSet<usize>) -> Self {
        match self {
            Self::Unconstrained => Self::Within(other),
            Self::Within(current) => Self::Within(current.intersection(&other).copied().collect()),
        }
    }

    /// Returns true if no destination folder can hold the subtree.
    fn is_impossible(&self) -> bool {
        matches!(self, Self::Within(set) if set.is_empty())
    }

    /// Returns true if at least one destination folder holds the subtree.
    fn is_matched(&self) -> bool {
        matches!(self, Self::Within(set) if !set.is_empty())
    }
}

/// Folder tree of one inventory, indexed by id (ids follow the pre-order of `Inventory::dirs`).
struct DirTable {
    rels: Vec<String>,
    name_keys: Vec<String>,
    parents: Vec<Option<usize>>,
    child_dirs: Vec<Vec<usize>>,
    child_files: Vec<Vec<usize>>,
    dir_by_name: HashMap<(usize, String), usize>,
    file_by_name: HashMap<(usize, String), usize>,
    file_parent: Vec<Option<usize>>,
    file_name_keys: Vec<String>,
}

impl DirTable {
    /// Builds the folder table of an inventory.
    fn build(inventory: &Inventory) -> Self {
        let count = inventory.dirs.len();
        let mut table = Self {
            rels: inventory.dirs.clone(),
            name_keys: inventory.dirs.iter().map(|rel| name_key(file_name_of(rel))).collect(),
            parents: vec![None; count],
            child_dirs: vec![Vec::new(); count],
            child_files: vec![Vec::new(); count],
            dir_by_name: HashMap::new(),
            file_by_name: HashMap::new(),
            file_parent: Vec::with_capacity(inventory.files.len()),
            file_name_keys: inventory.files.iter().map(|file| name_key(file_name_of(&file.rel))).collect(),
        };
        let ids: HashMap<&str, usize> =
            inventory.dirs.iter().enumerate().map(|(id, rel)| (rel.as_str(), id)).collect();
        for (id, rel) in inventory.dirs.iter().enumerate().filter(|(_, rel)| !rel.is_empty()) {
            let Some(&parent) = ids.get(parent_rel(rel)) else { continue };
            table.parents[id] = Some(parent);
            table.child_dirs[parent].push(id);
            table.dir_by_name.entry((parent, table.name_keys[id].clone())).or_insert(id);
        }
        for (index, file) in inventory.files.iter().enumerate() {
            let parent = ids.get(parent_rel(&file.rel)).copied();
            table.file_parent.push(parent);
            if let Some(parent) = parent {
                table.child_files[parent].push(index);
                table.file_by_name.entry((parent, table.file_name_keys[index].clone())).or_insert(index);
            }
        }
        table
    }
}

// ----------------------------------------------------------------------------------------------
// Matching
// ----------------------------------------------------------------------------------------------

/// Matches whole source folders; `identical` lists, per source file, the destination files with
/// the same name, size and content.
pub fn match_trees(
    source: &Inventory,
    destination: &Inventory,
    identical: &HashMap<usize, Vec<usize>>,
) -> TreeOutcome {
    let source_table = DirTable::build(source);
    let destination_table = DirTable::build(destination);
    let candidates = compute_candidates(&source_table, &destination_table, identical);
    let mut outcome = TreeOutcome::default();
    for source_dir in topmost_matches(&source_table, &candidates) {
        let Candidates::Within(set) = &candidates[source_dir] else { continue };
        let destination_dir = choose_destination_dir(&source_table, &destination_table, source_dir, set);
        let matches = map_subtree(
            &source_table,
            &destination_table,
            &candidates,
            identical,
            (source_dir, destination_dir),
        );
        outcome.trees.push(RelocatedTree {
            source_dir: source_table.rels[source_dir].clone(),
            destination_dir: destination_table.rels[destination_dir].clone(),
            bytes: matches.iter().map(|file_match| source.files[file_match.source].size).sum(),
            files: matches.len(),
        });
        outcome.matches.extend(matches);
    }
    outcome
}

/// Computes the candidate destination folders of every source folder, children first.
fn compute_candidates(
    source: &DirTable,
    destination: &DirTable,
    identical: &HashMap<usize, Vec<usize>>,
) -> Vec<Candidates> {
    let mut candidates = vec![Candidates::Unconstrained; source.rels.len()];
    for dir in (0..source.rels.len()).rev() {
        let mut current = Candidates::Unconstrained;
        for &file in &source.child_files[dir] {
            let parents = identical
                .get(&file)
                .map(|twins| twins.iter().filter_map(|&twin| destination.file_parent[twin]).collect())
                .unwrap_or_default();
            current = current.intersect(parents);
            if current.is_impossible() {
                break;
            }
        }
        for &child in &source.child_dirs[dir] {
            if current.is_impossible() {
                break;
            }
            let Candidates::Within(child_set) = &candidates[child] else { continue };
            let lifted = child_set
                .iter()
                .filter(|&&holder| destination.name_keys[holder] == source.name_keys[child])
                .filter_map(|&holder| destination.parents[holder])
                .collect();
            current = current.intersect(lifted);
        }
        candidates[dir] = current;
    }
    candidates
}

/// Returns matched source folders whose parent folder is not matched.
fn topmost_matches(source: &DirTable, candidates: &[Candidates]) -> Vec<usize> {
    (0..source.rels.len())
        .filter(|&dir| candidates[dir].is_matched())
        .filter(|&dir| source.parents[dir].is_none_or(|parent| !candidates[parent].is_matched()))
        .collect()
}

/// Picks the destination folder: same relative path if possible, else the first by path.
fn choose_destination_dir(
    source: &DirTable,
    destination: &DirTable,
    source_dir: usize,
    set: &HashSet<usize>,
) -> usize {
    let source_key = name_key(&source.rels[source_dir]);
    let mut options: Vec<usize> = set.iter().copied().collect();
    options.sort_by(|&left, &right| destination.rels[left].cmp(&destination.rels[right]));
    options.iter().copied().find(|&dir| name_key(&destination.rels[dir]) == source_key).unwrap_or(options[0])
}

/// Pairs every file of a matched source subtree with its twin in the chosen destination folder.
fn map_subtree(
    source: &DirTable,
    destination: &DirTable,
    candidates: &[Candidates],
    identical: &HashMap<usize, Vec<usize>>,
    roots: (usize, usize),
) -> Vec<FileMatch> {
    let mut matches = Vec::new();
    let mut stack = vec![roots];
    while let Some((source_dir, destination_dir)) = stack.pop() {
        for &file in &source.child_files[source_dir] {
            let key = (destination_dir, source.file_name_keys[file].clone());
            let twin = destination.file_by_name.get(&key).copied();
            if let Some(twin) =
                twin.filter(|twin| identical.get(&file).is_some_and(|twins| twins.contains(twin)))
            {
                matches.push(FileMatch { source: file, destination: twin });
            }
        }
        for &child in &source.child_dirs[source_dir] {
            if matches!(candidates[child], Candidates::Unconstrained) {
                continue;
            }
            if let Some(&twin_dir) =
                destination.dir_by_name.get(&(destination_dir, source.name_keys[child].clone()))
            {
                stack.push((child, twin_dir));
            }
        }
    }
    matches
}

// ----------------------------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::FileRecord;
    use std::path::PathBuf;

    /// Builds an inventory from file paths (folders derived) with sizes standing in for content.
    fn inventory_of(files: &[&str]) -> Inventory {
        let mut dirs: Vec<String> = vec![String::new()];
        for file in files {
            let mut parent = parent_rel(file);
            let mut chain = Vec::new();
            while !parent.is_empty() {
                chain.push(parent.to_string());
                parent = parent_rel(parent);
            }
            dirs.extend(chain.into_iter().rev());
        }
        dirs.sort_by_key(|rel| (crate::paths::depth_of(rel), rel.clone()));
        dirs.dedup();
        let records = files
            .iter()
            .map(|rel| FileRecord { rel: rel.to_string(), size: 1, mtime_ns: 0, is_read_only: false })
            .collect();
        Inventory { root: PathBuf::new(), files: records, dirs, skipped: Vec::new(), errors: Vec::new() }
    }

    /// Declares identical pairs by relative path.
    fn identical_of(
        source: &Inventory,
        destination: &Inventory,
        pairs: &[(&str, &str)],
    ) -> HashMap<usize, Vec<usize>> {
        let find = |inventory: &Inventory, rel: &str| {
            inventory.files.iter().position(|file| file.rel == rel).unwrap()
        };
        let mut identical: HashMap<usize, Vec<usize>> = HashMap::new();
        for (source_rel, destination_rel) in pairs {
            identical.entry(find(source, source_rel)).or_default().push(find(destination, destination_rel));
        }
        identical
    }

    #[test]
    fn finds_renamed_and_moved_tree() {
        let source = inventory_of(&["proj/a.txt", "proj/sub/b.txt", "other.txt"]);
        let destination =
            inventory_of(&["backup/old-proj/a.txt", "backup/old-proj/sub/b.txt", "backup/old-proj/extra"]);
        let identical = identical_of(
            &source,
            &destination,
            &[("proj/a.txt", "backup/old-proj/a.txt"), ("proj/sub/b.txt", "backup/old-proj/sub/b.txt")],
        );
        let outcome = match_trees(&source, &destination, &identical);
        assert_eq!(outcome.trees.len(), 1);
        assert_eq!(outcome.trees[0].source_dir, "proj");
        assert_eq!(outcome.trees[0].destination_dir, "backup/old-proj");
        assert_eq!(outcome.matches.len(), 2);
    }

    #[test]
    fn incomplete_tree_is_not_matched() {
        let source = inventory_of(&["proj/a.txt", "proj/b.txt"]);
        let destination = inventory_of(&["copy/a.txt"]);
        let identical = identical_of(&source, &destination, &[("proj/a.txt", "copy/a.txt")]);
        let outcome = match_trees(&source, &destination, &identical);
        assert!(outcome.trees.is_empty());
        assert!(outcome.matches.is_empty());
    }

    #[test]
    fn subfolder_names_must_match() {
        let source = inventory_of(&["proj/sub/a.txt"]);
        let destination = inventory_of(&["copy/renamed-sub/a.txt"]);
        let identical = identical_of(&source, &destination, &[("proj/sub/a.txt", "copy/renamed-sub/a.txt")]);
        let outcome = match_trees(&source, &destination, &identical);
        assert_eq!(outcome.trees.len(), 1, "the deepest folder still matches on its own");
        assert_eq!(outcome.trees[0].source_dir, "proj/sub");
        assert_eq!(outcome.trees[0].destination_dir, "copy/renamed-sub");
    }

    #[test]
    fn files_scattered_across_folders_do_not_form_a_tree() {
        let source = inventory_of(&["proj/a.txt", "proj/b.txt"]);
        let destination = inventory_of(&["x/a.txt", "y/b.txt"]);
        let identical =
            identical_of(&source, &destination, &[("proj/a.txt", "x/a.txt"), ("proj/b.txt", "y/b.txt")]);
        let outcome = match_trees(&source, &destination, &identical);
        assert!(outcome.matches.is_empty());
    }

    #[test]
    fn prefers_destination_at_same_path() {
        let source = inventory_of(&["proj/a.txt", "unique.txt"]);
        let destination = inventory_of(&["aaa/proj-copy/a.txt", "proj/a.txt"]);
        let identical = identical_of(
            &source,
            &destination,
            &[("proj/a.txt", "aaa/proj-copy/a.txt"), ("proj/a.txt", "proj/a.txt")],
        );
        let outcome = match_trees(&source, &destination, &identical);
        assert_eq!(outcome.trees[0].destination_dir, "proj");
    }

    #[test]
    fn whole_source_root_can_match() {
        let source = inventory_of(&["a.txt", "d/b.txt"]);
        let destination = inventory_of(&["2020/a.txt", "2020/d/b.txt"]);
        let identical =
            identical_of(&source, &destination, &[("a.txt", "2020/a.txt"), ("d/b.txt", "2020/d/b.txt")]);
        let outcome = match_trees(&source, &destination, &identical);
        assert_eq!(outcome.trees.len(), 1);
        assert_eq!(outcome.trees[0].source_dir, "");
        assert_eq!(outcome.trees[0].destination_dir, "2020");
        assert_eq!(outcome.matches.len(), 2);
    }
}
