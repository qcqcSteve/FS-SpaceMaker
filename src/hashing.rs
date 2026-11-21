//! Content fingerprints (BLAKE3) and byte-by-byte file comparison, streamed through reusable buffers.
//! Files are read with plain sequential reads (no memory mapping) so a file shrinking during the
//! read produces an error instead of a crash.

use std::cell::RefCell;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

// ----------------------------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------------------------

/// A 256-bit BLAKE3 digest.
pub type Digest = [u8; 32];
/// Size of each read; large sequential reads are fastest on both SSDs and HDDs.
const IO_BUFFER_BYTES: usize = 1024 * 1024;

thread_local! {
    static PRIMARY_BUFFER: RefCell<Vec<u8>> = RefCell::new(vec![0; IO_BUFFER_BYTES]);
    static SECONDARY_BUFFER: RefCell<Vec<u8>> = RefCell::new(vec![0; IO_BUFFER_BYTES]);
}

// ----------------------------------------------------------------------------------------------
// Fingerprints
// ----------------------------------------------------------------------------------------------

/// Returns the digest of the first `limit` bytes of a file (`u64::MAX` for the whole file).
pub fn hash_prefix(path: &Path, limit: u64) -> io::Result<Digest> {
    let mut reader = File::open(path)?.take(limit);
    let mut hasher = blake3::Hasher::new();
    PRIMARY_BUFFER.with_borrow_mut(|buffer| -> io::Result<()> {
        loop {
            let count = read_full(&mut reader, buffer)?;
            if count == 0 {
                return Ok(());
            }
            hasher.update(&buffer[..count]);
        }
    })?;
    Ok(*hasher.finalize().as_bytes())
}

/// Returns the digest of a whole file.
pub fn hash_file(path: &Path) -> io::Result<Digest> {
    hash_prefix(path, u64::MAX)
}

/// Returns the digest of empty content.
pub fn empty_digest() -> Digest {
    *blake3::hash(&[]).as_bytes()
}

/// Returns the lowercase hexadecimal form of a digest.
pub fn to_hex(digest: &Digest) -> String {
    blake3::Hash::from_bytes(*digest).to_hex().to_string()
}

// ----------------------------------------------------------------------------------------------
// Comparison
// ----------------------------------------------------------------------------------------------

/// Compares two files byte by byte; returns the digest of the content when identical, else None.
pub fn compare_files(first: &Path, second: &Path) -> io::Result<Option<Digest>> {
    compare_readers(&mut File::open(first)?, &mut File::open(second)?)
}

/// Compares two open readers byte by byte; returns the digest of the content when identical.
pub fn compare_readers(
    first_reader: &mut impl Read,
    second_reader: &mut impl Read,
) -> io::Result<Option<Digest>> {
    let mut hasher = blake3::Hasher::new();
    PRIMARY_BUFFER.with_borrow_mut(|first_buffer| {
        SECONDARY_BUFFER.with_borrow_mut(|second_buffer| {
            loop {
                let first_count = read_full(first_reader, first_buffer)?;
                let second_count = read_full(second_reader, second_buffer)?;
                if first_buffer[..first_count] != second_buffer[..second_count] {
                    return Ok(None);
                }
                if first_count == 0 {
                    return Ok(Some(*hasher.finalize().as_bytes()));
                }
                hasher.update(&first_buffer[..first_count]);
            }
        })
    })
}

/// Fills `buffer` as far as possible; returns the byte count (0 at end of file).
fn read_full(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

// ----------------------------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Known BLAKE3 digest of "abc" (official test vector).
    const ABC_DIGEST_HEX: &str = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";

    #[test]
    fn matches_official_test_vector() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc");
        fs::write(&path, b"abc").unwrap();
        assert_eq!(to_hex(&hash_file(&path).unwrap()), ABC_DIGEST_HEX);
    }

    #[test]
    fn prefix_hash_only_reads_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        fs::write(&first, b"same-prefix-AAAA").unwrap();
        fs::write(&second, b"same-prefix-BBBB").unwrap();
        assert_eq!(hash_prefix(&first, 12).unwrap(), hash_prefix(&second, 12).unwrap());
        assert_ne!(hash_file(&first).unwrap(), hash_file(&second).unwrap());
    }

    #[test]
    fn empty_file_has_empty_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty");
        fs::write(&path, b"").unwrap();
        assert_eq!(hash_file(&path).unwrap(), empty_digest());
    }

    #[test]
    fn comparison_detects_every_difference() {
        let dir = tempfile::tempdir().unwrap();
        let base = vec![7u8; IO_BUFFER_BYTES * 2 + 17];
        let path_a = dir.path().join("a");
        fs::write(&path_a, &base).unwrap();
        let cases: [(&str, Vec<u8>, bool); 4] = [
            ("identical", base.clone(), true),
            (
                "last-byte",
                {
                    let mut v = base.clone();
                    *v.last_mut().unwrap() = 8;
                    v
                },
                false,
            ),
            ("shorter", base[..base.len() - 1].to_vec(), false),
            (
                "longer",
                {
                    let mut v = base.clone();
                    v.push(7);
                    v
                },
                false,
            ),
        ];
        for (name, content, is_expected_identical) in cases {
            let path_b = dir.path().join(name);
            fs::write(&path_b, &content).unwrap();
            let result = compare_files(&path_a, &path_b).unwrap();
            assert_eq!(result.is_some(), is_expected_identical, "case {name}");
        }
    }

    #[test]
    fn comparison_digest_equals_file_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a");
        let path_b = dir.path().join("b");
        fs::write(&path_a, b"content").unwrap();
        fs::write(&path_b, b"content").unwrap();
        assert_eq!(compare_files(&path_a, &path_b).unwrap(), Some(hash_file(&path_a).unwrap()));
    }

    #[test]
    fn missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(hash_file(&dir.path().join("missing")).is_err());
    }
}
