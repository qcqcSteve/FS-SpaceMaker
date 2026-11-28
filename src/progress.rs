//! Thread-safe console progress meter: counts files and bytes, prints at most once per interval,
//! with the percentage and estimated time left when the total is known.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::format::{format_bytes, format_duration};

/// Minimum delay between two progress lines.
const PRINT_INTERVAL: Duration = Duration::from_secs(5);
/// Percentage scale.
const PERCENT: f64 = 100.0;

/// Prints a console section header.
pub fn print_step(name: &str) {
    println!("--- {name} ---");
}

/// Counts processed files and bytes for one long step.
pub struct ProgressMeter {
    label: String,
    files: AtomicU64,
    bytes: AtomicU64,
    total_bytes: Option<u64>,
    started: Instant,
    last_print: Mutex<Instant>,
}

impl ProgressMeter {
    /// Creates a meter whose lines start with `label`.
    pub fn new(label: &str) -> Self {
        Self {
            label: label.to_string(),
            files: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            total_bytes: None,
            started: Instant::now(),
            last_print: Mutex::new(Instant::now()),
        }
    }

    /// Creates a meter that also shows the percentage done and the estimated time left.
    pub fn with_total(label: &str, total_bytes: u64) -> Self {
        Self { total_bytes: Some(total_bytes), ..Self::new(label) }
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
        let progress = self.total_bytes.map(|total| self.describe_progress(bytes, total)).unwrap_or_default();
        eprintln!("  {}: {files} files, {}{progress}", self.label, format_bytes(bytes));
    }

    /// Returns " of TOTAL (P%), about T left" for a known total.
    fn describe_progress(&self, bytes: u64, total: u64) -> String {
        let fraction = if total == 0 { 1.0 } else { (bytes as f64 / total as f64).min(1.0) };
        let elapsed = self.started.elapsed().as_secs_f64();
        let left = if fraction > 0.0 { elapsed * (1.0 - fraction) / fraction } else { 0.0 };
        format!(
            " of {} ({:.1}%), about {} left",
            format_bytes(total),
            fraction * PERCENT,
            format_duration(left as u64)
        )
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

    #[test]
    fn known_totals_show_percentage_and_time_left() {
        let meter = ProgressMeter::with_total("test", 4000);
        let text = meter.describe_progress(1000, 4000);
        assert!(text.starts_with(" of 3.9 KB (25.0%), about "), "{text}");
        assert!(meter.describe_progress(0, 0).contains("(100.0%)"));
    }
}
