//! Platform-specific file system safety helpers (Windows first, neutral fallbacks elsewhere):
//! opening a source file so nobody can write to it until it is deleted, checking that NTFS
//! alternate data streams are copied on the destination, cloud placeholders and "file in use" errors.

use std::fs::{File, Metadata};
use std::io;
use std::path::Path;

#[cfg(windows)]
use crate::hashing::compare_files;

// ----------------------------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------------------------

/// Windows attributes of files whose data is not on the local disk (OneDrive "online-only",
/// HSM): FILE_ATTRIBUTE_OFFLINE, RECALL_ON_OPEN and RECALL_ON_DATA_ACCESS. Reading them would
/// trigger a download, and they free no local space.
const CLOUD_ATTRIBUTE_MASK: u32 = 0x0000_1000 | 0x0004_0000 | 0x0040_0000;
/// Windows error codes meaning another process holds the file: sharing and lock violations.
const IN_USE_ERROR_CODES: [i32; 2] = [32, 33];
/// Bare NTFS stream names that carry no user data: the main stream ("") and the download origin mark.
const HARMLESS_STREAM_NAMES: [&str; 2] = ["", "Zone.Identifier"];
/// Suffix of every data stream name returned by Windows (":name:$DATA").
const STREAM_TYPE_SUFFIX: &str = ":$DATA";

// ----------------------------------------------------------------------------------------------
// Cloud placeholders
// ----------------------------------------------------------------------------------------------

/// Returns true if Windows file attributes describe a file whose data is stored remotely.
pub fn is_cloud_attributes(attributes: u32) -> bool {
    attributes & CLOUD_ATTRIBUTE_MASK != 0
}

/// Returns true if the file's data is stored remotely (Windows cloud files / HSM).
#[cfg(windows)]
pub fn is_cloud_placeholder(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    is_cloud_attributes(metadata.file_attributes())
}

/// Returns true if the file's data is stored remotely (never on this platform).
#[cfg(not(windows))]
pub fn is_cloud_placeholder(_metadata: &Metadata) -> bool {
    false
}

// ----------------------------------------------------------------------------------------------
// Locking
// ----------------------------------------------------------------------------------------------

/// Opens a source file for reading while forbidding other writers until the handle is closed.
/// Deletion stays allowed so the file can be deleted while still locked against writes.
#[cfg(windows)]
pub fn open_source_locked(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ};
    File::options().read(true).share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE).open(path)
}

/// Opens a source file for reading (no mandatory locking on this platform).
#[cfg(not(windows))]
pub fn open_source_locked(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// Returns true if an error means another program is using the file.
pub fn is_in_use_error(error: &io::Error) -> bool {
    cfg!(windows) && error.raw_os_error().is_some_and(|code| IN_USE_ERROR_CODES.contains(&code))
}

// ----------------------------------------------------------------------------------------------
// Alternate data streams
// ----------------------------------------------------------------------------------------------

/// Result of checking the alternate data streams of a source file against its destination copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamCheck {
    /// Every stream that may hold user data also exists, byte-identical, on the destination.
    Covered,
    /// This source stream is missing or different on the destination: the file must be kept.
    Uncovered(String),
}

/// Returns the bare name of a raw stream name (":name:$DATA" gives "name", "::$DATA" gives "").
pub fn bare_stream_name(raw: &str) -> &str {
    raw.strip_prefix(':').and_then(|rest| rest.strip_suffix(STREAM_TYPE_SUFFIX)).unwrap_or(raw)
}

/// Returns true if a bare stream name matches a pattern (case-insensitive, trailing '*' = prefix).
pub fn matches_stream_pattern(name: &str, pattern: &str) -> bool {
    let name = name.to_lowercase();
    let pattern = pattern.to_lowercase();
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

/// Returns true if a stream may hold user data: not the main stream, not the download origin
/// mark, and not declared ignorable by the user.
pub fn is_meaningful_stream(bare_name: &str, ignored_patterns: &[String]) -> bool {
    let is_harmless = HARMLESS_STREAM_NAMES.iter().any(|harmless| harmless.eq_ignore_ascii_case(bare_name));
    !is_harmless && !ignored_patterns.iter().any(|pattern| matches_stream_pattern(bare_name, pattern))
}

/// Checks that every meaningful stream of the source exists, byte-identical, on the destination.
/// Byte comparison of the main content does not cover streams, hence this separate check.
#[cfg(windows)]
pub fn check_streams(
    source: &Path,
    destination: &Path,
    ignored_patterns: &[String],
) -> io::Result<StreamCheck> {
    for raw in list_streams(source)? {
        let name = bare_stream_name(&raw);
        if is_meaningful_stream(name, ignored_patterns) && !is_stream_copied(source, destination, name)? {
            return Ok(StreamCheck::Uncovered(name.to_string()));
        }
    }
    Ok(StreamCheck::Covered)
}

/// Returns Covered: alternate data streams do not exist on this platform.
#[cfg(not(windows))]
pub fn check_streams(
    _source: &Path,
    _destination: &Path,
    _ignored_patterns: &[String],
) -> io::Result<StreamCheck> {
    Ok(StreamCheck::Covered)
}

/// Returns true if the named stream exists on the destination with identical bytes.
#[cfg(windows)]
fn is_stream_copied(source: &Path, destination: &Path, name: &str) -> io::Result<bool> {
    match compare_files(&stream_path(source, name), &stream_path(destination, name)) {
        Ok(comparison) => Ok(comparison.is_some()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Returns the path that opens a named stream of a file ("file:name").
#[cfg(windows)]
fn stream_path(path: &Path, name: &str) -> std::path::PathBuf {
    let mut text = path.as_os_str().to_owned();
    text.push(":");
    text.push(name);
    std::path::PathBuf::from(text)
}

/// Lists the NTFS stream names of a file ("::$DATA" for the main stream).
#[cfg(windows)]
fn list_streams(path: &Path) -> io::Result<Vec<String>> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{ERROR_HANDLE_EOF, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        FindClose, FindFirstStreamW, FindNextStreamW, FindStreamInfoStandard, WIN32_FIND_STREAM_DATA,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut data = WIN32_FIND_STREAM_DATA::default();
    // SAFETY: `wide` is NUL-terminated and `data` is a valid, writable WIN32_FIND_STREAM_DATA.
    let handle =
        unsafe { FindFirstStreamW(wide.as_ptr(), FindStreamInfoStandard, (&raw mut data).cast(), 0) };
    if handle == INVALID_HANDLE_VALUE {
        let error = io::Error::last_os_error();
        return if is_streamless_error(&error, ERROR_HANDLE_EOF) { Ok(Vec::new()) } else { Err(error) };
    }
    let mut names = vec![stream_name(&data)];
    loop {
        // SAFETY: `handle` is a valid find handle and `data` is writable.
        if unsafe { FindNextStreamW(handle, (&raw mut data).cast()) } == 0 {
            break;
        }
        names.push(stream_name(&data));
    }
    let error = io::Error::last_os_error();
    // SAFETY: `handle` came from FindFirstStreamW and is closed exactly once.
    unsafe { FindClose(handle) };
    if !is_streamless_error(&error, ERROR_HANDLE_EOF) {
        return Err(error);
    }
    Ok(names)
}

/// Returns true for errors that mean "no (more) streams" or "streams not supported here" (FAT).
#[cfg(windows)]
fn is_streamless_error(error: &io::Error, end_of_list: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED,
    };
    let codes = [end_of_list, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED];
    error.raw_os_error().is_some_and(|code| codes.iter().any(|&known| i64::from(known) == i64::from(code)))
}

/// Decodes the stream name of a find record.
#[cfg(windows)]
fn stream_name(data: &windows_sys::Win32::Storage::FileSystem::WIN32_FIND_STREAM_DATA) -> String {
    let length = data.cStreamName.iter().position(|&unit| unit == 0).unwrap_or(data.cStreamName.len());
    String::from_utf16_lossy(&data.cStreamName[..length])
}

// ----------------------------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_attributes_are_recognised() {
        const ARCHIVE: u32 = 0x20;
        const OFFLINE: u32 = 0x1000;
        const RECALL_ON_OPEN: u32 = 0x4_0000;
        const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
        assert!(!is_cloud_attributes(ARCHIVE));
        for attribute in [OFFLINE, RECALL_ON_OPEN, RECALL_ON_DATA_ACCESS] {
            assert!(is_cloud_attributes(attribute | ARCHIVE));
        }
    }

    #[test]
    fn raw_stream_names_are_reduced_to_bare_names() {
        assert_eq!(bare_stream_name("::$DATA"), "");
        assert_eq!(bare_stream_name(":com.dropbox.attrs:$DATA"), "com.dropbox.attrs");
    }

    #[test]
    fn only_user_streams_are_meaningful() {
        let none: Vec<String> = Vec::new();
        assert!(!is_meaningful_stream("", &none));
        assert!(!is_meaningful_stream("Zone.Identifier", &none));
        assert!(!is_meaningful_stream("zone.identifier", &none));
        assert!(is_meaningful_stream("secret", &none));
        assert!(is_meaningful_stream("com.dropbox.attrs", &none));
    }

    #[test]
    fn ignored_patterns_support_a_trailing_wildcard() {
        let ignored = vec!["com.dropbox.*".to_string(), "Exact.Name".to_string()];
        assert!(!is_meaningful_stream("com.dropbox.attributes", &ignored));
        assert!(!is_meaningful_stream("COM.DROPBOX.internal", &ignored));
        assert!(!is_meaningful_stream("exact.name", &ignored));
        assert!(is_meaningful_stream("exact.name.longer", &ignored));
        assert!(is_meaningful_stream("com.other", &ignored));
    }

    #[cfg(windows)]
    fn write_stream(path: &Path, name: &str, content: &[u8]) {
        std::fs::write(stream_path(path, name), content).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn streams_are_covered_only_by_an_identical_copy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("source.txt");
        let destination = root.join("destination.txt");
        std::fs::write(&source, b"main").unwrap();
        std::fs::write(&destination, b"main").unwrap();
        let none: Vec<String> = Vec::new();
        write_stream(&source, "Zone.Identifier", b"[ZoneTransfer]");
        assert_eq!(check_streams(&source, &destination, &none).unwrap(), StreamCheck::Covered);
        write_stream(&source, "notes", b"hidden data");
        assert_eq!(
            check_streams(&source, &destination, &none).unwrap(),
            StreamCheck::Uncovered("notes".into())
        );
        write_stream(&destination, "notes", b"HIDDEN DATA");
        assert_eq!(
            check_streams(&source, &destination, &none).unwrap(),
            StreamCheck::Uncovered("notes".into())
        );
        write_stream(&destination, "notes", b"hidden data");
        write_stream(&destination, "extra", b"only on the destination");
        assert_eq!(check_streams(&source, &destination, &none).unwrap(), StreamCheck::Covered);
        assert_eq!(list_streams(&source).unwrap().len(), 3);
    }

    #[cfg(windows)]
    #[test]
    fn ignored_streams_need_no_copy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("source.txt");
        let destination = root.join("destination.txt");
        std::fs::write(&source, b"main").unwrap();
        std::fs::write(&destination, b"main").unwrap();
        write_stream(&source, "com.dropbox.attributes", b"sync metadata");
        let ignored = vec!["com.dropbox.*".to_string()];
        assert_eq!(check_streams(&source, &destination, &ignored).unwrap(), StreamCheck::Covered);
        assert!(matches!(check_streams(&source, &destination, &[]).unwrap(), StreamCheck::Uncovered(_)));
    }

    #[cfg(windows)]
    #[test]
    fn locked_source_rejects_writers_but_allows_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.txt");
        std::fs::write(&path, b"data").unwrap();
        let handle = open_source_locked(&path).unwrap();
        let writer = File::options().write(true).open(&path);
        assert!(writer.as_ref().is_err_and(is_in_use_error), "writers must be refused: {writer:?}");
        std::fs::remove_file(&path).unwrap();
        drop(handle);
        assert!(!path.exists());
    }
}
