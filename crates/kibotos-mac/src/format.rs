//! Sizes and dates for the list, the way Finder shows them.

use kibotos_core::Timestamp;

/// "1.2 MB" (decimal units, as Finder uses).
pub fn size(n: Option<u64>) -> String {
    let Some(n) = n else { return String::new() };
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    if n < 1000 {
        return format!("{n} {}", if n == 1 { "byte" } else { "bytes" });
    }
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if v < 10.0 { format!("{v:.1} {}", UNITS[unit]) } else { format!("{v:.0} {}", UNITS[unit]) }
}

/// Seconds east of UTC for the local time zone at `utc`.
fn local_offset(utc: i64) -> i64 {
    // SAFETY: a zeroed tm is a valid output buffer.
    unsafe {
        let t = utc as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() { 0 } else { tm.tm_gmtoff as i64 }
    }
}

/// "2026-11-24 16:14", in local time.
pub fn date(t: Option<Timestamp>) -> String {
    let Some(t) = t else { return String::new() };
    let local = match t {
        Timestamp::Utc(s) => Timestamp::Local(s + local_offset(s)),
        other => other,
    };
    let (y, mo, d, h, mi, _) = local.civil();
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
}
