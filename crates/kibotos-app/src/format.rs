//! Cell text for the list: sizes and dates.

use kibotos_core::Timestamp;
use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::System::Time::SystemTimeToTzSpecificLocalTime;

use crate::strings::number;

pub fn size(bytes: Option<u64>) -> String {
    bytes.map(number).unwrap_or_default()
}

/// "14/11/2026 10:30" in local time.
pub fn date(t: Option<Timestamp>) -> String {
    let Some(t) = t else { return String::new() };
    let (mut y, mut mo, mut d, mut h, mut mi, _) = t.civil();
    if let Timestamp::Utc(_) = t {
        let utc = SYSTEMTIME {
            wYear: y as u16,
            wMonth: mo as u16,
            wDay: d as u16,
            wHour: h as u16,
            wMinute: mi as u16,
            ..Default::default()
        };
        let mut local = SYSTEMTIME::default();
        // SAFETY: valid pointers; `None` means the current time zone.
        if unsafe { SystemTimeToTzSpecificLocalTime(None, &utc, &mut local) }.is_ok() {
            (y, mo, d, h, mi) = (local.wYear as i32, local.wMonth as u32, local.wDay as u32, local.wHour as u32, local.wMinute as u32);
        }
    }
    format!("{d:02}/{mo:02}/{y:04} {h:02}:{mi:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_dates_are_not_shifted() {
        // 2026-11-14 10:30 as wall-clock time.
        assert_eq!(date(Some(Timestamp::Local(1_794_652_200))), "14/11/2026 10:30");
        assert_eq!(date(None), "");
        assert_eq!(size(Some(1234)), "1.234");
        assert_eq!(size(None), "");
    }
}
