//! Path helpers: canonical roots, overlap guard, portable relative paths and name comparison keys.
//! Relative paths are stored with '/' separators so plans are portable and easy to validate.
//! Canonical roots keep the Windows verbatim prefix (\\?\) so paths longer than 260 characters work.

use std::path::{Component, MAIN_SEPARATOR, MAIN_SEPARATOR_STR, Path, PathBuf};

use anyhow::{Context, Result, bail};

// ----------------------------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------------------------

/// Separator used inside stored relative paths, on every platform.
pub const REL_SEPARATOR: char = '/';
/// Whether file names compare case-insensitively on this platform's usual file systems.
pub const IS_CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));
/// Windows verbatim prefixes produced by canonicalization, and their display replacements.
const VERBATIM_UNC_PREFIX: &str = r"\\?\UNC\";
const VERBATIM_PREFIX: &str = r"\\?\";
const UNC_PREFIX: &str = r"\\";

// ----------------------------------------------------------------------------------------------
// Roots
// ----------------------------------------------------------------------------------------------

/// Resolves a user-supplied folder to its canonical form; fails if it is missing or not a folder.
pub fn canonical_root(path: &Path) -> Result<PathBuf> {
    let canonical = path.canonicalize().with_context(|| {
        format!(
            "Folder not found or not accessible: {}. Check the path and your permissions.",
            path.display()
        )
    })?;
    if !canonical.is_dir() {
        bail!("Not a folder: {}. Give a folder or a drive root such as K:\\.", path.display());
    }
    if canonical.to_str().is_none() {
        bail!("Folder path is not valid Unicode: {}. Rename it or choose a parent folder.", path.display());
    }
    Ok(canonical)
}

/// Fails unless the two roots are distinct and neither one contains the other.
pub fn ensure_disjoint_roots(source: &Path, destination: &Path) -> Result<()> {
    let source_key = path_key(source);
    let destination_key = path_key(destination);
    if source_key == destination_key {
        bail!(
            "Source and destination are the same folder ({}). Choose two different folders.",
            display_path(source)
        );
    }
    if destination_key.starts_with(&source_key) {
        bail!(
            "Destination {} is inside source {}: deleting source files could destroy destination data. \
             Choose folders that do not contain each other.",
            display_path(destination),
            display_path(source)
        );
    }
    if source_key.starts_with(&destination_key) {
        bail!(
            "Source {} is inside destination {}: the source would match itself. \
             Choose folders that do not contain each other.",
            display_path(source),
            display_path(destination)
        );
    }
    if is_physically_within(destination, source) || is_physically_within(source, destination) {
        bail!(
            "Source {} and destination {} are the same storage location or one contains the other \
             (drive alias such as SUBST, network share or junction). Choose folders that do not contain \
             each other.",
            display_path(source),
            display_path(destination)
        );
    }
    Ok(())
}

/// Returns true if `inner` or one of its ancestors is physically the folder `outer`. This catches
/// aliases that path text cannot reveal: SUBST drives, \\machine\C$ shares, mapped drives.
fn is_physically_within(inner: &Path, outer: &Path) -> bool {
    inner.ancestors().any(|ancestor| same_file::is_same_file(ancestor, outer).unwrap_or(false))
}

/// Returns true when `path` is `root` itself or located below it (case rules of the platform).
pub fn is_same_or_inside(path: &Path, root: &Path) -> bool {
    path_key(path).starts_with(&path_key(root))
}

/// Returns the path components as comparison keys (case-folded where the platform ignores case).
fn path_key(path: &Path) -> Vec<String> {
    path.components().map(|component| name_key(&component.as_os_str().to_string_lossy())).collect()
}

/// Returns a path for display, without the Windows verbatim prefix.
pub fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(VERBATIM_UNC_PREFIX) {
        return format!("{UNC_PREFIX}{rest}");
    }
    text.strip_prefix(VERBATIM_PREFIX).unwrap_or(&text).to_string()
}

/// Returns the full display path of a stored relative path under a displayed root.
pub fn display_joined(root_display: &str, rel: &str) -> String {
    if rel.is_empty() {
        return root_display.to_string();
    }
    let native_rel = rel.replace(REL_SEPARATOR, MAIN_SEPARATOR_STR);
    if root_display.ends_with(MAIN_SEPARATOR) {
        return format!("{root_display}{native_rel}");
    }
    format!("{root_display}{MAIN_SEPARATOR}{native_rel}")
}

// ----------------------------------------------------------------------------------------------
// Relative paths
// ----------------------------------------------------------------------------------------------

/// Returns the '/'-separated path of `full` relative to `root`, or None if it is not valid Unicode.
pub fn rel_from(root: &Path, full: &Path) -> Option<String> {
    let relative = full.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return None;
        };
        parts.push(name.to_str()?);
    }
    Some(parts.join("/"))
}

/// Builds the native path of a relative path produced by this program (no validation).
pub fn rel_to_path(root: &Path, rel: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    if !rel.is_empty() {
        path.extend(rel.split(REL_SEPARATOR));
    }
    path
}

/// Builds the native path of a relative path read from a plan, rejecting anything unsafe.
pub fn join_rel(root: &Path, rel: &str) -> Result<PathBuf> {
    if rel.is_empty() || !rel.split(REL_SEPARATOR).all(is_plain_component) {
        bail!("Unsafe relative path in plan: {rel:?}. The plan may be corrupted or edited: run a new scan.");
    }
    Ok(rel_to_path(root, rel))
}

/// Returns true if `part` is a single ordinary file name (no separator, drive, '..' or stream).
fn is_plain_component(part: &str) -> bool {
    if part.contains('\0') || (cfg!(windows) && part.contains(':')) {
        return false;
    }
    let mut components = Path::new(part).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(name)), None) if name == part
    )
}

/// Returns the parent of a relative path ("" for top-level entries).
pub fn parent_rel(rel: &str) -> &str {
    rel.rsplit_once(REL_SEPARATOR).map_or("", |(parent, _)| parent)
}

/// Returns the last component of a relative path.
pub fn file_name_of(rel: &str) -> &str {
    rel.rsplit_once(REL_SEPARATOR).map_or(rel, |(_, name)| name)
}

/// Returns the number of components of a relative path (0 for the root).
pub fn depth_of(rel: &str) -> usize {
    if rel.is_empty() { 0 } else { rel.matches(REL_SEPARATOR).count() + 1 }
}

/// Returns `rel` without the leading `ancestor` directory ("" when equal).
pub fn strip_ancestor<'a>(rel: &'a str, ancestor: &str) -> Option<&'a str> {
    if ancestor.is_empty() {
        return Some(rel);
    }
    let rest = rel.strip_prefix(ancestor)?;
    if rest.is_empty() {
        return Some(rest);
    }
    rest.strip_prefix(REL_SEPARATOR)
}

/// Joins two relative paths.
pub fn join_rels(parent: &str, child: &str) -> String {
    match (parent.is_empty(), child.is_empty()) {
        (true, _) => child.to_string(),
        (_, true) => parent.to_string(),
        _ => format!("{parent}{REL_SEPARATOR}{child}"),
    }
}

/// Returns the comparison key of a name (case-folded where the platform ignores case).
pub fn name_key(name: &str) -> String {
    if IS_CASE_INSENSITIVE { name.to_lowercase() } else { name.to_string() }
}

// ----------------------------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_plan_paths() {
        let root = Path::new("root");
        for rel in ["", "..", "a/../b", "a//b", "/a", "a/./b", "a\0b"] {
            assert!(join_rel(root, rel).is_err(), "should reject {rel:?}");
        }
        if cfg!(windows) {
            for rel in ["C:", "a\\b", "file:stream", "C:/x"] {
                assert!(join_rel(root, rel).is_err(), "should reject {rel:?}");
            }
        }
    }

    #[test]
    fn accepts_ordinary_plan_paths() {
        let path = join_rel(Path::new("root"), "dir/été/file.txt").unwrap();
        assert!(path.ends_with(Path::new("dir").join("été").join("file.txt")));
    }

    #[test]
    fn relative_path_helpers_agree() {
        assert_eq!(parent_rel("a/b/c.txt"), "a/b");
        assert_eq!(parent_rel("c.txt"), "");
        assert_eq!(file_name_of("a/b/c.txt"), "c.txt");
        assert_eq!(depth_of(""), 0);
        assert_eq!(depth_of("a/b/c.txt"), 3);
        assert_eq!(strip_ancestor("a/b/c", "a"), Some("b/c"));
        assert_eq!(strip_ancestor("ab/c", "a"), None);
        assert_eq!(strip_ancestor("a/b", ""), Some("a/b"));
        assert_eq!(join_rels("", "x"), "x");
        assert_eq!(join_rels("a", "x"), "a/x");
    }

    #[test]
    fn rel_from_builds_portable_paths() {
        let root = Path::new("root");
        let full = root.join("a").join("b.txt");
        assert_eq!(rel_from(root, &full).as_deref(), Some("a/b.txt"));
        assert_eq!(rel_from(root, root).as_deref(), Some(""));
    }

    #[test]
    fn detects_overlapping_roots() {
        let base = Path::new("base");
        assert!(ensure_disjoint_roots(base, base).is_err());
        assert!(ensure_disjoint_roots(base, &base.join("inner")).is_err());
        assert!(ensure_disjoint_roots(&base.join("inner"), base).is_err());
        assert!(ensure_disjoint_roots(&base.join("a"), &base.join("b")).is_ok());
        assert!(ensure_disjoint_roots(&base.join("ab"), &base.join("a")).is_ok());
    }

    #[test]
    fn overlap_guard_ignores_case_where_platform_does() {
        let result = ensure_disjoint_roots(Path::new("Base"), Path::new("base").join("x").as_path());
        assert_eq!(result.is_err(), IS_CASE_INSENSITIVE);
    }

    #[test]
    fn display_strips_verbatim_prefix() {
        assert_eq!(display_path(Path::new(r"\\?\C:\data")), r"C:\data");
        assert_eq!(display_path(Path::new(r"\\?\UNC\server\share")), r"\\server\share");
    }

    #[test]
    fn display_joined_uses_native_separator() {
        let joined = display_joined("root", "a/b");
        assert_eq!(joined, format!("root{MAIN_SEPARATOR}a{MAIN_SEPARATOR}b"));
        assert_eq!(display_joined(&format!("C:{MAIN_SEPARATOR}"), "a"), format!("C:{MAIN_SEPARATOR}a"));
    }
}
