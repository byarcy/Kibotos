//! Calendar helpers (proleptic Gregorian, no time zone database needed).

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
pub(crate) fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y as i64 - 1 } else { y as i64 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// (year, month, day, hour, minute, second) for seconds since 1970-01-01.
pub(crate) fn civil_from_secs(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = (yoe + era * 400 + if m <= 2 { 1 } else { 0 }) as i32;
    (y, m, d, (rem / 3600) as u32, (rem % 3600 / 60) as u32, (rem % 60) as u32)
}

pub(crate) fn secs_from_civil(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
    days_from_civil(y, mo, d) * 86400 + (h * 3600 + mi * 60 + s) as i64
}

/// MS-DOS date/time (as used by ZIP and RAR4) to wall-clock seconds.
pub(crate) fn dos_to_secs(date: u16, time: u16) -> Option<i64> {
    let (y, m, d) = (1980 + (date >> 9) as i32, ((date >> 5) & 0xF) as u32, (date & 0x1F) as u32);
    let (h, mi, s) = ((time >> 11) as u32, ((time >> 5) & 0x3F) as u32, ((time & 0x1F) * 2) as u32);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    Some(secs_from_civil(y, m, d, h, mi, s))
}

/// Windows FILETIME (100 ns ticks since 1601) to Unix seconds.
pub(crate) fn filetime_to_unix(ft: u64) -> Option<i64> {
    if ft == 0 {
        return None;
    }
    Some((ft / 10_000_000) as i64 - 11_644_473_600)
}

/// Converts wall-clock seconds in the *current* local time zone to UTC.
#[cfg(windows)]
pub(crate) fn local_to_utc(local: i64) -> i64 {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::Time::TzSpecificLocalTimeToSystemTime;

    let (y, mo, d, h, mi, s) = civil_from_secs(local);
    let lt = SYSTEMTIME {
        wYear: y as u16,
        wMonth: mo as u16,
        wDayOfWeek: 0,
        wDay: d as u16,
        wHour: h as u16,
        wMinute: mi as u16,
        wSecond: s as u16,
        wMilliseconds: 0,
    };
    let mut ut: SYSTEMTIME = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers refer to valid SYSTEMTIME values; a null time zone
    // means "the currently active time zone".
    let ok = unsafe { TzSpecificLocalTimeToSystemTime(std::ptr::null(), &lt, &mut ut) };
    if ok == 0 {
        return local;
    }
    secs_from_civil(
        ut.wYear as i32,
        ut.wMonth as u32,
        ut.wDay as u32,
        ut.wHour as u32,
        ut.wMinute as u32,
        ut.wSecond as u32,
    )
}

#[cfg(not(windows))]
pub(crate) fn local_to_utc(local: i64) -> i64 {
    local
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_roundtrip() {
        for secs in [0i64, 951_782_400, 1_700_000_000, -86_400, 4_102_444_800] {
            let (y, mo, d, h, mi, s) = civil_from_secs(secs);
            assert_eq!(secs_from_civil(y, mo, d, h, mi, s), secs);
        }
        assert_eq!(civil_from_secs(951_782_400), (2000, 2, 29, 0, 0, 0));
    }

    #[test]
    fn dos_time() {
        // 2026-11-14 10:30:20
        let date = ((2026 - 1980) << 9) | (11 << 5) | 14;
        let time = (10 << 11) | (30 << 5) | 10;
        assert_eq!(civil_from_secs(dos_to_secs(date, time).unwrap()), (2026, 11, 14, 10, 30, 20));
        assert_eq!(dos_to_secs(0, 0), None);
    }

    #[test]
    fn filetime() {
        assert_eq!(filetime_to_unix(116_444_736_000_000_000), Some(0));
    }
}
