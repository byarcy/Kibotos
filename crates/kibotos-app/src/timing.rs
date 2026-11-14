//! Startup measurements. Set `KIBOTOS_TIMING=<file>` to append timings,
//! measured from process creation (as Windows records it), to that file.

use std::io::Write;
use std::sync::OnceLock;

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

static FILE: OnceLock<Option<String>> = OnceLock::new();

fn ft(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

/// Milliseconds since the process was created.
pub fn since_process_start() -> f64 {
    let (mut c, mut e, mut k, mut u) = Default::default();
    // SAFETY: valid out pointers for the current process pseudo-handle.
    if unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) }.is_err() {
        return 0.0;
    }
    let now = unsafe { GetSystemTimePreciseAsFileTime() };
    (ft(now).saturating_sub(ft(c))) as f64 / 10_000.0
}

pub fn enabled() -> bool {
    FILE.get_or_init(|| std::env::var("KIBOTOS_TIMING").ok()).is_some()
}

/// Records `label: <ms since process start>` (plus extra text) if enabled.
pub fn mark(label: &str, extra: &str) {
    if !enabled() {
        return;
    }
    let ms = since_process_start();
    if let Some(Some(path)) = FILE.get() {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{label}: {ms:.1} ms {extra}");
        }
    }
}
