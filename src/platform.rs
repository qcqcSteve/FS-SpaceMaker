//! Platform-specific file system safety helpers (Windows first, neutral fallbacks elsewhere):
//! opening source and destination files so nobody can change them until the source is deleted,
//! deleting through the verified handle, checking delete rights, checking that NTFS alternate data
//! streams are copied on the destination, cloud placeholders and "in use" / "access denied" errors,
//! and moving files (merge mode) without ever replacing an existing destination entry.

use std::fs::{File, Metadata};
use std::io;
use std::path::Path;

#[cfg(windows)]
use crate::hashing::compare_files;
use crate::paths::matches_name_pattern;

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
/// Error codes of a rename between two volumes: ERROR_NOT_SAME_DEVICE (Windows), EXDEV (Unix).
const CROSS_DEVICE_ERROR_CODE: i32 = if cfg!(windows) { 17 } else { 18 };

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
// Locking and deletion through the verified handle
// ----------------------------------------------------------------------------------------------

/// Opens a source file for reading and deletion. Other programs may still read it, but none can
/// write, rename or delete it until the handle is closed, so the file finally deleted is exactly
/// the one verified. A link is opened as a link, never followed.
#[cfg(windows)]
pub fn open_source_for_deletion(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_SHARE_READ,
    };
    File::options()
        .access_mode(FILE_GENERIC_READ | DELETE)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// Opens a source file for reading (no mandatory locking on this platform).
#[cfg(not(windows))]
pub fn open_source_for_deletion(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// Opens a destination file for reading while forbidding writers, renames and deletion until the
/// handle is closed: the copy cannot change between the comparison and the source deletion.
#[cfg(windows)]
pub fn open_destination_locked(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};
    File::options()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// Opens a destination file for reading (no mandatory locking on this platform).
#[cfg(not(windows))]
pub fn open_destination_locked(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// Checks that the current user is allowed to delete a file, without reading, locking or
/// changing it (protected system files refuse the DELETE access right).
#[cfg(windows)]
pub fn probe_delete_access(path: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    };
    File::options()
        .access_mode(DELETE | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map(drop)
}

/// Accepts every file: deletion rights are checked by the deletion itself on this platform.
#[cfg(not(windows))]
pub fn probe_delete_access(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Deletes the file behind a handle from `open_source_for_deletion`. The name disappears at once
/// (POSIX semantics) where the volume supports it; elsewhere it disappears when the handle closes.
#[cfg(windows)]
pub fn delete_open_file(file: File, path: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX,
        FileDispositionInfo, FileDispositionInfoEx,
    };
    let extended = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    let error = match set_disposition(&file, FileDispositionInfoEx, &extended) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    if !is_unsupported_error(&error) {
        return Err(error);
    }
    // Older Windows or FAT/exFAT volumes: classic disposition, read-only attribute cleared first.
    clear_read_only(path)?;
    set_disposition(&file, FileDispositionInfo, &FILE_DISPOSITION_INFO { DeleteFile: true })
}

/// Deletes the file by path (no deletion through a handle on this platform).
#[cfg(not(windows))]
pub fn delete_open_file(file: File, path: &Path) -> io::Result<()> {
    drop(file);
    std::fs::remove_file(path)
}

/// Applies one disposition structure to an open file.
#[cfg(windows)]
fn set_disposition<T>(
    file: &File,
    class: windows_sys::Win32::Storage::FileSystem::FILE_INFO_BY_HANDLE_CLASS,
    info: &T,
) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle;
    let size = u32::try_from(size_of::<T>()).map_err(io::Error::other)?;
    // SAFETY: the handle is open for the lifetime of `file`, `info` points to `size` readable bytes
    // of the structure matching `class`.
    let is_done =
        unsafe { SetFileInformationByHandle(file.as_raw_handle(), class, (&raw const *info).cast(), size) };
    if is_done == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Clears the read-only attribute of a file if it is set.
#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)]
fn clear_read_only(path: &Path) -> io::Result<()> {
    let mut permissions = std::fs::symlink_metadata(path)?.permissions();
    if !permissions.readonly() {
        return Ok(());
    }
    // On Windows this only clears FILE_ATTRIBUTE_READONLY.
    permissions.set_readonly(false);
    std::fs::set_permissions(path, permissions)
}

/// Returns true for errors meaning "this request is not supported by this volume or system".
#[cfg(windows)]
fn is_unsupported_error(error: &io::Error) -> bool {
    use windows_sys::Win32::Foundation::{
        ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED,
    };
    let codes = [ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED];
    error.raw_os_error().is_some_and(|code| codes.iter().any(|&known| i64::from(known) == i64::from(code)))
}

/// Returns true if an error means another program is using the file.
pub fn is_in_use_error(error: &io::Error) -> bool {
    cfg!(windows) && error.raw_os_error().is_some_and(|code| IN_USE_ERROR_CODES.contains(&code))
}

/// Returns true if an error means the current user has no right to perform the operation.
pub fn is_access_denied_error(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
}

// ----------------------------------------------------------------------------------------------
// Moving files
// ----------------------------------------------------------------------------------------------

/// Renames a file, failing with `AlreadyExists` instead of replacing an existing entry (the
/// standard rename replaces it). Fails with a cross-device error between two volumes.
#[cfg(windows)]
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
    let wide =
        |path: &Path| -> Vec<u16> { path.as_os_str().encode_wide().chain(std::iter::once(0)).collect() };
    let (from_wide, to_wide) = (wide(from), wide(to));
    // SAFETY: both buffers are NUL-terminated UTF-16 strings that outlive the call. Without
    // MOVEFILE_REPLACE_EXISTING an existing target is never replaced.
    let is_done = unsafe { MoveFileExW(from_wide.as_ptr(), to_wide.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if is_done == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Renames a file without replacing an existing entry: a new hard link, then the old name removed.
#[cfg(not(windows))]
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::hard_link(from, to)?;
    std::fs::remove_file(from)
}

/// Returns true if a rename failed only because both paths are on different volumes.
pub fn is_cross_device_error(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::CrossesDevices || error.raw_os_error() == Some(CROSS_DEVICE_ERROR_CODE)
}

/// Returns true if an operation failed because its target already exists.
pub fn is_already_exists_error(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::AlreadyExists
}

/// Copies a file with its modification time and, on Windows, its attributes and alternate data
/// streams (the standard copy uses CopyFileExW there). An existing `to` is replaced: callers copy
/// to a private temporary name only.
pub fn copy_preserving_metadata(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::copy(from, to)?;
    preserve_modified_time(from, to)
}

/// Windows: CopyFileExW already kept the modification time.
#[cfg(windows)]
fn preserve_modified_time(_from: &Path, _to: &Path) -> io::Result<()> {
    Ok(())
}

/// Gives the copy the modification time of the original (the standard copy does not keep it).
#[cfg(not(windows))]
fn preserve_modified_time(from: &Path, to: &Path) -> io::Result<()> {
    let modified = std::fs::metadata(from)?.modified()?;
    File::open(to)?.set_modified(modified)
}

/// Removes a file even if it is read-only; a file already gone counts as removed.
pub fn remove_file_forcing(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    match clear_read_only(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        result => result?,
    }
    match std::fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
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

/// Returns true if a stream may hold user data: not the main stream, not the download origin
/// mark, and not declared ignorable by the user.
pub fn is_meaningful_stream(bare_name: &str, ignored_patterns: &[String]) -> bool {
    let is_harmless = HARMLESS_STREAM_NAMES.iter().any(|harmless| harmless.eq_ignore_ascii_case(bare_name));
    !is_harmless && !ignored_patterns.iter().any(|pattern| matches_name_pattern(bare_name, pattern))
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
    let is_end = error.raw_os_error().is_some_and(|code| i64::from(code) == i64::from(end_of_list));
    is_end || is_unsupported_error(error)
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
    fn source_handle_blocks_writers_renames_and_deletes_until_it_deletes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.txt");
        std::fs::write(&path, b"data").unwrap();
        let handle = open_source_for_deletion(&path).unwrap();
        let writer = File::options().write(true).open(&path);
        assert!(writer.as_ref().is_err_and(is_in_use_error), "writers must be refused: {writer:?}");
        assert!(std::fs::rename(&path, dir.path().join("moved.txt")).is_err(), "renames must be refused");
        assert!(std::fs::remove_file(&path).is_err(), "other deletions must be refused");
        assert!(File::open(&path).is_ok(), "readers are still allowed");
        delete_open_file(handle, &path).unwrap();
        assert!(!path.exists());
    }

    #[cfg(windows)]
    #[test]
    fn read_only_files_are_deleted_through_the_handle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("read-only.txt");
        std::fs::write(&path, b"data").unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions).unwrap();
        delete_open_file(open_source_for_deletion(&path).unwrap(), &path).unwrap();
        assert!(!path.exists());
    }

    #[cfg(windows)]
    #[test]
    fn destination_handle_blocks_writers_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("copy.txt");
        std::fs::write(&path, b"data").unwrap();
        let handle = open_destination_locked(&path).unwrap();
        assert!(File::options().write(true).open(&path).is_err());
        assert!(std::fs::remove_file(&path).is_err());
        drop(handle);
        assert!(path.exists());
    }

    #[test]
    fn rename_never_replaces_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from.txt");
        let to = dir.path().join("to.txt");
        std::fs::write(&from, b"new").unwrap();
        std::fs::write(&to, b"old").unwrap();
        let error = rename_no_replace(&from, &to).unwrap_err();
        assert!(is_already_exists_error(&error), "{error:?}");
        assert_eq!(std::fs::read(&to).unwrap(), b"old");
        std::fs::remove_file(&to).unwrap();
        rename_no_replace(&from, &to).unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), b"new");
        assert!(!from.exists());
    }

    #[test]
    fn copies_keep_the_modification_time_and_read_only_copies_can_be_removed() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from.txt");
        let to = dir.path().join("to.txt");
        std::fs::write(&from, b"data").unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        File::options().write(true).open(&from).unwrap().set_modified(old).unwrap();
        let mut permissions = std::fs::metadata(&from).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&from, permissions).unwrap();
        copy_preserving_metadata(&from, &to).unwrap();
        assert_eq!(std::fs::metadata(&to).unwrap().modified().unwrap(), old);
        remove_file_forcing(&to).unwrap();
        remove_file_forcing(&to).unwrap();
        assert!(!to.exists());
        remove_file_forcing(&from).unwrap();
    }

    #[test]
    fn delete_access_is_granted_on_ordinary_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, b"data").unwrap();
        probe_delete_access(&path).unwrap();
        assert!(path.exists(), "probing never deletes");
    }
}
