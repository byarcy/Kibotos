//! ZIP passwords are bytes, and the format never said which encoding: 7-Zip
//! and most Windows tools use the system's ANSI code page, others UTF-8.
//! Kibotos writes the ANSI form when the password fits in it (so 7-Zip can
//! open the archive) and, when reading, tries UTF-8 and then ANSI.

/// The password in the system's ANSI code page, if it differs from UTF-8
/// and every character can be represented exactly.
#[cfg(windows)]
pub(crate) fn ansi_bytes(s: &str) -> Option<Vec<u8>> {
    use windows_sys::Win32::Globalization::{CP_ACP, WC_NO_BEST_FIT_CHARS, WideCharToMultiByte};
    if s.is_ascii() || s.is_empty() {
        return None;
    }
    let wide: Vec<u16> = s.encode_utf16().collect();
    let mut used_default = 0i32;
    // SAFETY: the first call only measures; the second writes into a buffer
    // of exactly that size. `used_default` is a valid BOOL out-pointer.
    unsafe {
        let n = WideCharToMultiByte(CP_ACP, WC_NO_BEST_FIT_CHARS, wide.as_ptr(), wide.len() as i32, std::ptr::null_mut(), 0, std::ptr::null(), &mut used_default);
        if n <= 0 || used_default != 0 {
            return None;
        }
        let mut out = vec![0u8; n as usize];
        let n = WideCharToMultiByte(CP_ACP, WC_NO_BEST_FIT_CHARS, wide.as_ptr(), wide.len() as i32, out.as_mut_ptr(), n, std::ptr::null(), &mut used_default);
        (n > 0 && used_default == 0).then_some(out)
    }
}

#[cfg(not(windows))]
pub(crate) fn ansi_bytes(_s: &str) -> Option<Vec<u8>> {
    None
}

/// The bytes Kibotos uses to encrypt a ZIP with `password`.
pub(crate) fn zip_password_for_writing(password: &str) -> Vec<u8> {
    ansi_bytes(password).unwrap_or_else(|| password.as_bytes().to_vec())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn ascii_needs_no_conversion() {
        assert_eq!(ansi_bytes("secreto123"), None);
        assert_eq!(zip_password_for_writing("abc"), b"abc");
        // Not representable in any Western code page: stays UTF-8.
        assert_eq!(zip_password_for_writing("日本"), "日本".as_bytes());
    }
}
