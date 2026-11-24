use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use crate::{Error, Result};

/// Decompression-bomb guards.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Hard cap on bytes written by one extraction.
    pub max_total_bytes: u64,
    /// Output may not exceed `archive size * max_ratio`...
    pub max_ratio: u64,
    /// ...once more than this many bytes have been written. Small archives
    /// with high ratios (a file of zeros) are normal and stay allowed.
    pub ratio_threshold: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_total_bytes: 1 << 40,   // 1 TiB
            max_ratio: 1000,            // 7-Zip/zip bombs typically exceed 1000:1
            ratio_threshold: 1 << 30,   // 1 GiB
        }
    }
}

/// Shared state between a worker running an extraction and the UI thread:
/// cancellation flag and progress counters. All methods take `&self`.
#[derive(Debug, Default)]
pub struct Control {
    pub limits: Limits,
    cancel: AtomicBool,
    bytes_done: AtomicU64,
    bytes_total: AtomicU64,
    entries_done: AtomicU64,
    entries_total: AtomicU64,
}

/// A snapshot of extraction progress.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub entries_done: u64,
    pub entries_total: u64,
}

impl Control {
    pub fn new() -> Control {
        Control::default()
    }

    pub fn with_limits(limits: Limits) -> Control {
        Control { limits, ..Control::default() }
    }

    /// Requests cancellation. The worker stops at the next data chunk.
    pub fn cancel(&self) {
        self.cancel.store(true, Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Relaxed)
    }

    pub fn progress(&self) -> Progress {
        Progress {
            bytes_done: self.bytes_done.load(Relaxed),
            bytes_total: self.bytes_total.load(Relaxed),
            entries_done: self.entries_done.load(Relaxed),
            entries_total: self.entries_total.load(Relaxed),
        }
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.is_cancelled() { Err(Error::Cancelled) } else { Ok(()) }
    }

    pub(crate) fn set_totals(&self, bytes: u64, entries: u64) {
        self.bytes_total.store(bytes, Relaxed);
        self.entries_total.store(entries, Relaxed);
    }

    /// Adds written bytes and returns the new total.
    pub(crate) fn add_bytes(&self, n: u64) -> u64 {
        self.bytes_done.fetch_add(n, Relaxed) + n
    }

    /// Starts counting again from zero (a second phase of one operation).
    pub(crate) fn restart(&self) {
        self.bytes_done.store(0, Relaxed);
        self.entries_done.store(0, Relaxed);
    }

    pub(crate) fn add_entry(&self) {
        self.entries_done.fetch_add(1, Relaxed);
    }
}
