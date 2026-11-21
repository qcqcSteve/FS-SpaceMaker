//! Thread-safe console progress meter: counts files and bytes, prints at most once per interval.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::format::format_bytes;

/// Minimum delay between two progress lines.
const PRINT_INTERVAL: Duration = Duration::from_secs(5);

/// Prints a console section header.
pub fn print_step(name: &str) {
    println!("--- {name} ---");
}

/// Counts processed files and bytes for one long step.
pub struct ProgressMeter {
    label: String,
    files: AtomicU64,
    bytes: AtomicU64,
    last_print: Mutex<Instant>,
}

impl ProgressMeter {
    /// Creates a meter whose lines start with `label`.
    pub fn new(label: &str) -> Self {
        Self {
            label: label.to_string(),
            files: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            last_print: Mutex::new(Instant::now()),
        }
    }

    /// Adds one processed file and prints a line if the interval has elapsed.
    pub fn record(&self, bytes: u64) {
        self.files.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        let Ok(mut last_print) = self.last_print.try_lock() else {
            return;
        };
        if last_print.elapsed() >= PRINT_INTERVAL {
            *last_print = Instant::now();
            self.print_line();
        }
    }

    /// Prints the current totals.
    pub fn print_line(&self) {
        let files = self.files.load(Ordering::Relaxed);
        let bytes = self.bytes.load(Ordering::Relaxed);
        eprintln!("  {}: {files} files, {}", self.label, format_bytes(bytes));
    }

    /// Returns the number of files recorded so far.
    pub fn files(&self) -> u64 {
        self.files.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_from_many_threads() {
        let meter = ProgressMeter::new("test");
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| (0..250).for_each(|_| meter.record(2)));
            }
        });
        assert_eq!(meter.files(), 1000);
        assert_eq!(meter.bytes.load(Ordering::Relaxed), 2000);
    }
}
