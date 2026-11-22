//! Report model shared by the plan (pre-processing) and apply (post-processing) phases, plus
//! writers: a self-contained interactive HTML page (data embedded as JSON) and a complete CSV.
//! The HTML keeps at most `report_max_rows` rows per table (largest first); the CSV keeps all.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Serialize, Serializer};

use crate::matching::MatchMode;
use crate::paths::{depth_of, display_joined, file_name_of, parent_rel};

// ----------------------------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------------------------

/// HTML page template; the placeholder is replaced by the report JSON.
const HTML_TEMPLATE: &str = include_str!("../assets/report.html");
const DATA_PLACEHOLDER: &str = "__REPORT_DATA__";
/// Maximum folders embedded in the HTML tree (shallowest kept first).
const MAX_REPORT_FOLDERS: usize = 200_000;
/// Byte-order mark so Excel opens the UTF-8 CSV with accents intact.
const CSV_UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";
const CSV_HEADER: [&str; 6] = ["category", "path", "other_path", "size_bytes", "tag", "detail"];

// ----------------------------------------------------------------------------------------------
// Model
// ----------------------------------------------------------------------------------------------

/// Which moment the report describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Plan,
    DryRun,
    Apply,
}

/// Colour hint of a summary card.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tone {
    Good,
    Warn,
    Bad,
    Neutral,
}

/// One headline figure.
#[derive(Clone, Debug, Serialize)]
pub struct Card {
    pub label: String,
    pub bytes: Option<u64>,
    pub count: Option<u64>,
    pub note: String,
    pub tone: Tone,
}

/// Space of one volume before and after the operation.
#[derive(Clone, Debug, Serialize)]
pub struct VolumeBar {
    pub label: String,
    pub path: String,
    pub total: Option<u64>,
    pub free_before: Option<u64>,
    pub free_after: Option<u64>,
    pub note: String,
}

/// Folder tree in columnar form (index = folder id, parent -1 for the root).
#[derive(Clone, Debug, Default, Serialize)]
pub struct FolderTable {
    pub parent: Vec<i64>,
    pub name: Vec<String>,
    pub before_bytes: Vec<u64>,
    pub removed_bytes: Vec<u64>,
    pub before_files: Vec<u64>,
    pub removed_files: Vec<u64>,
    pub is_truncated: bool,
}

/// A file counted in the folder tree.
#[derive(Clone, Copy, Debug)]
pub struct FolderFile<'a> {
    pub rel: &'a str,
    pub size: u64,
    pub is_removed: bool,
}

/// One table row: paths are relative to the table roots when those are set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportRow {
    pub path: String,
    pub other: String,
    pub size: u64,
    pub tag: String,
    pub detail: String,
}

impl Serialize for ReportRow {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (&self.path, &self.other, self.size, &self.tag, &self.detail).serialize(serializer)
    }
}

/// A list of rows of one category.
#[derive(Clone, Debug, Serialize)]
pub struct Table {
    pub id: String,
    pub title: String,
    pub description: String,
    pub columns: [String; 5],
    pub path_root: String,
    pub other_root: String,
    pub rows: Vec<ReportRow>,
    pub total_rows: usize,
    pub total_bytes: u64,
}

impl Table {
    /// Creates a table; `columns` labels path, other path, size, tag and detail ("" hides one).
    pub fn new(
        id: &str,
        title: &str,
        description: &str,
        columns: [&str; 5],
        roots: (&str, &str),
        rows: Vec<ReportRow>,
    ) -> Self {
        Self {
            id: id.to_string(),
            title: title.to_string(),
            description: description.to_string(),
            columns: columns.map(str::to_string),
            path_root: roots.0.to_string(),
            other_root: roots.1.to_string(),
            total_rows: rows.len(),
            total_bytes: rows.iter().map(|row| row.size).sum(),
            rows,
        }
    }
}

/// Everything shown in a report.
#[derive(Clone, Debug, Serialize)]
pub struct ReportData {
    pub phase: Phase,
    pub title: String,
    pub generated_at: String,
    pub tool_version: String,
    pub plan_id: String,
    pub apply_command: String,
    pub mode: MatchMode,
    pub source_root: String,
    pub destination_root: String,
    pub notes: Vec<String>,
    pub cards: Vec<Card>,
    pub volumes: Vec<VolumeBar>,
    pub folders: FolderTable,
    pub tables: Vec<Table>,
}

// ----------------------------------------------------------------------------------------------
// Folder aggregation
// ----------------------------------------------------------------------------------------------

/// Builds the folder tree with cumulative sizes (each folder includes its descendants).
pub fn aggregate_folders(dirs: &[String], files: &[FolderFile]) -> FolderTable {
    let mut ordered = collect_folder_rels(dirs, files);
    ordered.sort_by(|left, right| depth_of(left).cmp(&depth_of(right)).then_with(|| left.cmp(right)));
    let is_truncated = ordered.len() > MAX_REPORT_FOLDERS;
    ordered.truncate(MAX_REPORT_FOLDERS);
    let ids: HashMap<&str, usize> = ordered.iter().enumerate().map(|(id, rel)| (*rel, id)).collect();
    let count = ordered.len();
    let mut table = FolderTable {
        parent: ordered.iter().map(|rel| parent_id(rel, &ids)).collect(),
        name: ordered.iter().map(|rel| file_name_of(rel).to_string()).collect(),
        before_bytes: vec![0; count],
        removed_bytes: vec![0; count],
        before_files: vec![0; count],
        removed_files: vec![0; count],
        is_truncated,
    };
    for file in files {
        let id = nearest_folder_id(file.rel, &ids);
        add_file(&mut table, id, file);
    }
    for id in (1..count).rev() {
        let parent = usize::try_from(table.parent[id]).unwrap_or(0);
        propagate_to_parent(&mut table, id, parent);
    }
    table
}

/// Returns the folder list plus any missing ancestor of the files (root always included).
fn collect_folder_rels<'a>(dirs: &'a [String], files: &[FolderFile<'a>]) -> Vec<&'a str> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut ordered = Vec::new();
    let ancestors = files.iter().flat_map(|file| ancestors_of(file.rel));
    for rel in std::iter::once("").chain(dirs.iter().map(String::as_str)).chain(ancestors) {
        if seen.insert(rel) {
            ordered.push(rel);
        }
    }
    ordered
}

/// Returns every ancestor folder of a relative file path, root included.
fn ancestors_of(rel: &str) -> impl Iterator<Item = &str> {
    let mut current = Some(parent_rel(rel));
    std::iter::from_fn(move || {
        let folder = current?;
        current = if folder.is_empty() { None } else { Some(parent_rel(folder)) };
        Some(folder)
    })
}

/// Returns the id of a folder's parent (-1 for the root).
fn parent_id(rel: &str, ids: &HashMap<&str, usize>) -> i64 {
    if rel.is_empty() {
        return -1;
    }
    ids.get(parent_rel(rel)).map_or(0, |&id| i64::try_from(id).unwrap_or(0))
}

/// Returns the deepest folder of the table that contains the file.
fn nearest_folder_id(rel: &str, ids: &HashMap<&str, usize>) -> usize {
    ancestors_of(rel).find_map(|folder| ids.get(folder).copied()).unwrap_or(0)
}

/// Adds one file to its folder's own totals.
fn add_file(table: &mut FolderTable, id: usize, file: &FolderFile) {
    table.before_bytes[id] += file.size;
    table.before_files[id] += 1;
    if file.is_removed {
        table.removed_bytes[id] += file.size;
        table.removed_files[id] += 1;
    }
}

/// Adds a folder's cumulative totals to its parent.
fn propagate_to_parent(table: &mut FolderTable, id: usize, parent: usize) {
    table.before_bytes[parent] += table.before_bytes[id];
    table.removed_bytes[parent] += table.removed_bytes[id];
    table.before_files[parent] += table.before_files[id];
    table.removed_files[parent] += table.removed_files[id];
}

// ----------------------------------------------------------------------------------------------
// Writers
// ----------------------------------------------------------------------------------------------

/// Writes the complete CSV, then the HTML page with tables capped to `max_rows` (largest first).
pub fn write_report(mut data: ReportData, max_rows: usize, html_path: &Path, csv_path: &Path) -> Result<()> {
    write_csv(&data.tables, csv_path)?;
    for table in &mut data.tables {
        table.rows.sort_by(|left, right| right.size.cmp(&left.size).then_with(|| left.path.cmp(&right.path)));
        table.rows.truncate(max_rows);
    }
    write_html(&data, html_path)
}

/// Writes every row of every table to one CSV file with full paths.
fn write_csv(tables: &[Table], path: &Path) -> Result<()> {
    let mut file =
        BufWriter::new(File::create(path).with_context(|| format!("Cannot create {}", path.display()))?);
    file.write_all(CSV_UTF8_BOM)?;
    let mut writer = csv::Writer::from_writer(file);
    writer.write_record(CSV_HEADER)?;
    for table in tables {
        for row in &table.rows {
            let full_path = full_path_of(&table.path_root, &row.path);
            let full_other = full_path_of(&table.other_root, &row.other);
            let size = row.size.to_string();
            writer.write_record([
                table.id.as_str(),
                &full_path,
                &full_other,
                &size,
                &row.tag,
                &row.detail,
            ])?;
        }
    }
    writer.flush().with_context(|| format!("Cannot write {}", path.display()))
}

/// Returns the full display path of a row path under an optional root ("" stays "").
fn full_path_of(root: &str, rel: &str) -> String {
    if root.is_empty() || rel.is_empty() { rel.to_string() } else { display_joined(root, rel) }
}

/// Writes the self-contained HTML page.
fn write_html(data: &ReportData, path: &Path) -> Result<()> {
    let json = serde_json::to_string(data).context("Cannot serialize the report")?;
    // A literal "</" inside the JSON could close the <script> element early.
    let safe_json = json.replace("</", "<\\/");
    let page = HTML_TEMPLATE.replacen(DATA_PLACEHOLDER, &safe_json, 1);
    fs::write(path, page).with_context(|| format!("Cannot write {}", path.display()))
}

// ----------------------------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn file(rel: &str, size: u64, is_removed: bool) -> FolderFile<'_> {
        FolderFile { rel, size, is_removed }
    }

    #[test]
    fn folder_totals_are_cumulative() {
        let dirs = vec![String::new(), "a".to_string(), "a/b".to_string(), "c".to_string()];
        let files = [file("a/b/x", 10, true), file("a/y", 5, false), file("top", 1, true)];
        let table = aggregate_folders(&dirs, &files);
        assert_eq!(table.name, vec!["", "a", "c", "b"]);
        assert_eq!(table.parent, vec![-1, 0, 0, 1]);
        assert_eq!(table.before_bytes, vec![16, 15, 0, 10]);
        assert_eq!(table.removed_bytes, vec![11, 10, 0, 10]);
        assert_eq!(table.before_files, vec![3, 2, 0, 1]);
    }

    #[test]
    fn missing_ancestors_are_added() {
        let files = [file("gone/deep/x", 4, true)];
        let table = aggregate_folders(&[String::new()], &files);
        assert_eq!(table.name, vec!["", "gone", "deep"]);
        assert_eq!(table.removed_bytes, vec![4, 4, 4]);
    }

    #[test]
    fn writes_csv_and_html_with_escaped_data() {
        let dir = tempfile::tempdir().unwrap();
        let row = ReportRow {
            path: "a/</script>.txt".to_string(),
            other: String::new(),
            size: 3,
            tag: "t".to_string(),
            detail: "d, with comma".to_string(),
        };
        let table = Table::new(
            "residual",
            "Residual",
            "",
            ["Path", "", "Size", "Tag", "Detail"],
            ("ROOT", ""),
            vec![row],
        );
        let data = ReportData {
            phase: Phase::Plan,
            title: "t".to_string(),
            generated_at: String::new(),
            tool_version: String::new(),
            plan_id: String::new(),
            apply_command: String::new(),
            mode: MatchMode::Content,
            source_root: "ROOT".to_string(),
            destination_root: String::new(),
            notes: Vec::new(),
            cards: Vec::new(),
            volumes: Vec::new(),
            folders: FolderTable::default(),
            tables: vec![table],
        };
        let html_path = dir.path().join("report.html");
        let csv_path = dir.path().join("report.csv");
        write_report(data, 10, &html_path, &csv_path).unwrap();
        let html = fs::read_to_string(&html_path).unwrap();
        assert!(!html.contains(DATA_PLACEHOLDER));
        assert!(html.contains(r"a/<\/script>.txt"));
        let csv_bytes = fs::read(&csv_path).unwrap();
        assert!(csv_bytes.starts_with(CSV_UTF8_BOM));
        let csv_text = String::from_utf8(csv_bytes[CSV_UTF8_BOM.len()..].to_vec()).unwrap();
        assert!(csv_text.contains("\"d, with comma\""));
        assert!(csv_text.contains(&display_joined("ROOT", "a/</script>.txt")));
    }
}
