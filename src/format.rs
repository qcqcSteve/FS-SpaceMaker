//! Human-readable formatting helpers: byte sizes (1024-based, like Windows Explorer) and timestamps.

use chrono::Local;

// ----------------------------------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------------------------------

/// Unit labels for successive powers of 1024.
const BYTE_UNITS: [&str; 6] = ["bytes", "KB", "MB", "GB", "TB", "PB"];
/// Ratio between two consecutive byte units.
const UNIT_STEP: f64 = 1024.0;

// ----------------------------------------------------------------------------------------------
// Formatting
// ----------------------------------------------------------------------------------------------

/// Returns a size such as "12.3 GB" (exact count below 1 KB).
pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64;
    let mut unit_index = 0;
    while value >= UNIT_STEP && unit_index < BYTE_UNITS.len() - 1 {
        value /= UNIT_STEP;
        unit_index += 1;
    }
    format!("{value:.1} {}", BYTE_UNITS[unit_index])
}

/// Returns the local time in RFC 3339 format.
pub fn timestamp_iso() -> String {
    Local::now().to_rfc3339()
}

// ----------------------------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_small_sizes_exactly() {
        assert_eq!(format_bytes(0), "0 bytes");
        assert_eq!(format_bytes(1023), "1023 bytes");
    }

    #[test]
    fn formats_large_sizes_with_units() {
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536 * 1024), "1.5 MB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.0 GB");
    }
}
