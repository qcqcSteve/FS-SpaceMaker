//! Platform-specific file system safety helpers (Windows first, neutral fallbacks elsewhere):
//! opening a source file so nobody can write to it until it is deleted, detecting NTFS alternate
//! data streams, cloud placeholders and "file in use" errors.

use std::fs::{File, Metadata};
use std::io;
use std::path::Path;

// ----------------------------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------------------------

/// Windows attributes of files whose data is not on the local disk (OneDrive "online-only",
/// HSM): FILE_ATTRIBUTE_OFFLINE, RECALL_ON_OPEN and RECALL_ON_DATA_ACCESS. Reading them would
/// trigger a download, and they free no local space.
const CLOUD_ATTRIBUTE_MASK: u32 = 0x0000_1000 | 0x0004_0000 | 0x0040_0000;
/// Windows error codes meaning another process holds the file: sharing and lock violations.
const IN_USE_ERROR_CODES: [i32; 2] = [32, 33];
/// NTFS stream names that carry no user data: the main stream and the download origin mark.
const HARMLESS_STREAM_NAMES: [&str; 2] = ["::$DATA", ":Zone.Identifier:$DATA"];

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

/// Returns true if an NTFS stream name may hold user data (anything but the main stream and the
/// download origin mark).
pub fn is_meaningful_stream(name: &str) -> bool {
    !HARMLESS_STREAM_NAMES.iter().any(|harmless| harmless.eq_ignore_ascii_case(name))
}

/// Returns true if the file has alternate data streams that may hold user data. Byte comparison
/// only covers the main stream, so such files are never deleted.
#[cfg(windows)]
pub fn has_meaningful_streams(path: &Path) -> io::Result<bool> {
    Ok(list_streams(path)?.iter().any(|name| is_meaningful_stream(name)))
}

/// Returns false: alternate data streams do not exist on this platform.
#[cfg(not(windows))]
pub fn has_meaningful_streams(_path: &Path) -> io::Result<bool> {
    Ok(false)
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
    fn only_user_streams_are_meaningful() {
        assert!(!is_meaningful_stream("::$DATA"));
        assert!(!is_meaningful_stream(":Zone.Identifier:$DATA"));
        assert!(!is_meaningful_stream(":zone.identifier:$data"));
        assert!(is_meaningful_stream(":secret:$DATA"));
    }

    #[cfg(windows)]
    #[test]
    fn detects_alternate_streams_on_ntfs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("file.txt");
        std::fs::write(&path, b"main").unwrap();
        assert!(!has_meaningful_streams(&path).unwrap());
        let mut zone = path.clone().into_os_string();
        zone.push(":Zone.Identifier");
        std::fs::write(&zone, b"[ZoneTransfer]").unwrap();
        assert!(!has_meaningful_streams(&path).unwrap());
        let mut secret = path.clone().into_os_string();
        secret.push(":secret");
        std::fs::write(&secret, b"hidden data").unwrap();
        assert!(has_meaningful_streams(&path).unwrap());
        assert_eq!(list_streams(&path).unwrap().len(), 3);
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
