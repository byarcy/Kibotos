//! Validation of untrusted entry paths before extraction.
//!
//! Policy (v0.1):
//! - Absolute paths, drive letters, UNC paths and `..` components are rejected.
//! - Reserved Windows device names (`CON`, `NUL`, `COM1`, ...) are rejected.
//! - Characters Windows cannot store (`<>"|?*:` and control characters) are
//!   replaced with `_`, as WinRAR does. `:` must never survive, because a
//!   component like `C:x` would make `PathBuf::push` leave the destination,
//!   and `file:stream` would write an alternate data stream.
//! - Trailing dots and spaces are trimmed (Windows drops them silently, which
//!   could otherwise turn `.. ` into `..`).
//! - Links are never created (see `DiskSink`).

use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsafeReason {
    Empty,
    Absolute,
    Traversal,
    ReservedName,
}

impl fmt::Display for UnsafeReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            UnsafeReason::Empty => "empty path",
            UnsafeReason::Absolute => "absolute path",
            UnsafeReason::Traversal => "path traversal (..)",
            UnsafeReason::ReservedName => "reserved Windows device name",
        })
    }
}

const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "CLOCK$", "COM0", "COM1", "COM2", "COM3",
    "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "COM¹", "COM²", "COM³", "LPT0", "LPT1", "LPT2",
    "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²", "LPT³",
];

fn is_reserved(component: &str) -> bool {
    // Windows ignores everything after the first dot and trailing spaces:
    // "nul.txt" and "CON .tar" are devices too.
    let stem = component.split('.').next().unwrap_or("").trim_end_matches(' ');
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem))
}

fn is_drive(component: &str) -> bool {
    let b = component.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Returns the sanitized relative components of an entry path.
pub fn sanitize(entry_path: &str) -> Result<Vec<String>, UnsafeReason> {
    if entry_path.starts_with('/') || entry_path.starts_with('\\') {
        return Err(UnsafeReason::Absolute);
    }
    let mut out = Vec::new();
    for (i, raw) in entry_path.split(['/', '\\']).enumerate() {
        if raw.is_empty() || raw == "." {
            continue;
        }
        if raw == ".." {
            return Err(UnsafeReason::Traversal);
        }
        if i == 0 && is_drive(raw) {
            return Err(UnsafeReason::Absolute);
        }
        let mut comp: String = raw
            .chars()
            .map(|c| match c {
                '<' | '>' | '"' | '|' | '?' | '*' | ':' => '_',
                c if (c as u32) < 0x20 => '_',
                c => c,
            })
            .collect();
        let trimmed = comp.trim_end_matches(['.', ' ']).len();
        comp.truncate(trimmed);
        if comp.is_empty() {
            // Only dots and spaces, like "..." or ".. ": Windows would turn
            // these into ".." or nothing.
            return Err(UnsafeReason::Traversal);
        }
        if is_reserved(&comp) {
            return Err(UnsafeReason::ReservedName);
        }
        out.push(comp);
    }
    if out.is_empty() {
        return Err(UnsafeReason::Empty);
    }
    Ok(out)
}

/// Joins an untrusted entry path to `dest`, guaranteeing the result stays
/// inside `dest`.
pub fn safe_join(dest: &Path, entry_path: &str) -> Result<PathBuf, UnsafeReason> {
    let comps = sanitize(entry_path)?;
    let mut p = dest.to_path_buf();
    for c in &comps {
        p.push(c);
    }
    debug_assert!(p.starts_with(dest));
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(p: &str) -> String {
        sanitize(p).unwrap().join("/")
    }

    #[test]
    fn accepts_normal_paths() {
        assert_eq!(ok("a/b/c.txt"), "a/b/c.txt");
        assert_eq!(ok("./a//b"), "a/b");
        assert_eq!(ok("dir\\file"), "dir/file");
        assert_eq!(ok("con-fig/console.txt"), "con-fig/console.txt");
        assert_eq!(ok("COM10.txt"), "COM10.txt");
        assert_eq!(ok("ñandú/日本.txt"), "ñandú/日本.txt");
    }

    #[test]
    fn rejects_traversal() {
        assert_eq!(sanitize("../x"), Err(UnsafeReason::Traversal));
        assert_eq!(sanitize("a/../../x"), Err(UnsafeReason::Traversal));
        assert_eq!(sanitize("a\\..\\x"), Err(UnsafeReason::Traversal));
        assert_eq!(sanitize("a/.. /x"), Err(UnsafeReason::Traversal));
        assert_eq!(sanitize("a/.../x"), Err(UnsafeReason::Traversal));
    }

    #[test]
    fn rejects_absolute() {
        assert_eq!(sanitize("/etc/passwd"), Err(UnsafeReason::Absolute));
        assert_eq!(sanitize("\\Windows\\x"), Err(UnsafeReason::Absolute));
        assert_eq!(sanitize("//server/share/x"), Err(UnsafeReason::Absolute));
        assert_eq!(sanitize("C:/Windows/x"), Err(UnsafeReason::Absolute));
        assert_eq!(sanitize("C:x"), Err(UnsafeReason::Absolute));
    }

    #[test]
    fn rejects_devices() {
        for p in ["CON", "nul.txt", "a/Aux", "lpt1.tar.gz", "COM1 .txt", "conin$", "COM¹"] {
            assert_eq!(sanitize(p), Err(UnsafeReason::ReservedName), "{p}");
        }
    }

    #[test]
    fn replaces_bad_characters() {
        assert_eq!(ok("a/file:stream"), "a/file_stream");
        assert_eq!(ok("a/b:c"), "a/b_c");
        assert_eq!(ok("what?.txt"), "what_.txt");
        assert_eq!(ok("name. . "), "name");
        assert_eq!(ok("tab\there"), "tab_here");
    }

    #[test]
    fn empty_is_rejected() {
        assert_eq!(sanitize(""), Err(UnsafeReason::Empty));
        assert_eq!(sanitize("./"), Err(UnsafeReason::Empty));
    }

    #[test]
    fn join_stays_inside() {
        let dest = Path::new("C:\\out");
        let p = safe_join(dest, "a/b:c/d").unwrap();
        assert_eq!(p, Path::new("C:\\out\\a\\b_c\\d"));
        assert!(safe_join(dest, "D:evil").is_err());
    }
}
