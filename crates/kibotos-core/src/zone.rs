//! Mark of the Web: Windows records where a downloaded file came from in a
//! `Zone.Identifier` alternate data stream, and macOS in the
//! `com.apple.quarantine` extended attribute. If an archive carries it, the
//! files extracted from it must carry it too, or SmartScreen, Office
//! Protected View or Gatekeeper would be bypassed simply by putting a file
//! in a ZIP.

use std::path::Path;

/// Which extracted files receive the archive's mark.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ZonePolicy {
    /// Every file (used when opening a single file from the archive).
    All,
    /// Files Windows treats as risky: programs, scripts, shortcuts, disk
    /// images and documents that can carry macros. Writing the mark costs
    /// an extra file operation, so bulk extractions mark only these.
    #[default]
    Risky,
}

/// The mark to copy onto extracted files.
#[derive(Clone, Debug)]
pub struct Zone {
    pub data: Vec<u8>,
    pub policy: ZonePolicy,
}

const RISKY: &[&str] = &[
    // programs and installers
    "exe", "com", "scr", "pif", "cpl", "dll", "sys", "msi", "msp", "msix", "msixbundle", "appx", "appxbundle",
    "application", "appref-ms", "jar", "xll",
    // scripts and shortcuts
    "bat", "cmd", "ps1", "psm1", "psd1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "hta", "lnk", "url", "reg", "scf",
    "inf", "chm", "settingcontent-ms",
    // disk images
    "iso", "img", "vhd", "vhdx",
    // documents that can run code
    "doc", "docx", "docm", "dot", "dotm", "xls", "xlsx", "xlsm", "xlsb", "xlam", "xlt", "xltm", "ppt", "pptx", "pptm",
    "ppam", "pps", "ppsm", "rtf", "pdf", "one", "htm", "html", "mht", "mhtml", "svg",
];

/// What macOS treats as risky, on top of [`RISKY`]: apps, installers,
/// disk images, scripts and the files that open them.
#[cfg(target_os = "macos")]
const RISKY_MAC: &[&str] = &[
    "app", "pkg", "mpkg", "dmg", "sparseimage", "sparsebundle", "command", "tool", "sh", "bash", "zsh", "csh", "ksh",
    "py", "pl", "rb", "scpt", "scptd", "applescript", "workflow", "action", "terminal", "webloc", "inetloc", "fileloc",
    "dylib", "bundle", "plugin", "kext", "prefpane", "saver", "osax", "mobileconfig",
];

#[cfg(windows)]
fn stream(path: &Path) -> std::ffi::OsString {
    let mut s = path.as_os_str().to_owned();
    s.push(":Zone.Identifier");
    s
}

#[cfg(windows)]
fn read_mark(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(stream(path)).ok()
}

#[cfg(windows)]
fn write_mark(path: &Path, data: &[u8]) {
    let _ = std::fs::write(stream(path), data);
}

#[cfg(target_os = "macos")]
const QUARANTINE: &std::ffi::CStr = c"com.apple.quarantine";

#[cfg(target_os = "macos")]
fn c_path(path: &Path) -> Option<std::ffi::CString> {
    std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str())).ok()
}

#[cfg(target_os = "macos")]
fn read_mark(path: &Path) -> Option<Vec<u8>> {
    let p = c_path(path)?;
    // SAFETY: valid C strings; the first call only asks for the size.
    unsafe {
        let n = libc::getxattr(p.as_ptr(), QUARANTINE.as_ptr(), std::ptr::null_mut(), 0, 0, 0);
        if n <= 0 {
            return None;
        }
        let mut buf = vec![0u8; n as usize];
        let n = libc::getxattr(p.as_ptr(), QUARANTINE.as_ptr(), buf.as_mut_ptr().cast(), buf.len(), 0, 0);
        if n <= 0 {
            return None;
        }
        buf.truncate(n as usize);
        Some(buf)
    }
}

#[cfg(target_os = "macos")]
fn write_mark(path: &Path, data: &[u8]) {
    if let Some(p) = c_path(path) {
        // SAFETY: valid C strings and buffer.
        unsafe {
            libc::setxattr(p.as_ptr(), QUARANTINE.as_ptr(), data.as_ptr().cast(), data.len(), 0, 0);
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn read_mark(_path: &Path) -> Option<Vec<u8>> {
    None
}

#[cfg(not(any(windows, target_os = "macos")))]
fn write_mark(_path: &Path, _data: &[u8]) {}

/// Reads the Mark of the Web of a file, if it has one.
pub fn read(archive: &Path) -> Option<Vec<u8>> {
    read_mark(archive).filter(|d| !d.is_empty())
}

/// Whether the policy's "risky" rule covers `file`.
fn risky(file: &Path) -> bool {
    let ext = file.extension().and_then(|e| e.to_str());
    let listed = |list: &[&str]| ext.is_some_and(|e| list.iter().any(|r| r.eq_ignore_ascii_case(e)));
    #[cfg(target_os = "macos")]
    {
        // Unix programs usually have no extension, and everything inside
        // an app or installer bundle belongs to that program.
        let in_bundle = file.parent().is_some_and(|dir| {
            dir.components().any(|c| {
                let c = c.as_os_str().to_string_lossy().to_ascii_lowercase();
                [".app", ".pkg", ".bundle", ".framework", ".plugin", ".kext"].iter().any(|b| c.ends_with(b))
            })
        });
        if ext.is_none() || in_bundle || listed(RISKY_MAC) {
            return true;
        }
    }
    listed(RISKY)
}

impl Zone {
    /// Copies the mark onto `file` if the policy covers it. Errors are
    /// ignored: file systems without streams (FAT32, network shares)
    /// cannot store it.
    pub fn apply(&self, file: &Path) {
        let wanted = match self.policy {
            ZonePolicy::All => true,
            ZonePolicy::Risky => risky(file),
        };
        if wanted {
            write_mark(file, &self.data);
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod mac_tests {
    use super::*;

    #[test]
    fn copies_quarantine_by_policy() {
        let dir = std::env::temp_dir().join(format!("kibotos-quarantine-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Demo.app/Contents/MacOS")).unwrap();
        let archive = dir.join("a.zip");
        std::fs::write(&archive, b"zip").unwrap();
        assert!(read(&archive).is_none());
        let mark = b"0083;6745abcd;Safari;12345678-1234-1234-1234-123456789ABC".to_vec();
        write_mark(&archive, &mark);
        assert_eq!(read(&archive).as_deref(), Some(&mark[..]));

        let files = ["setup.EXE", "notes.txt", "run.command", "tool", "Demo.app/Contents/MacOS/Demo", "photo.jpg"];
        for f in files {
            std::fs::write(dir.join(f), b"x").unwrap();
            Zone { data: mark.clone(), policy: ZonePolicy::Risky }.apply(&dir.join(f));
        }
        let marked: Vec<bool> = files.iter().map(|f| read(&dir.join(f)).is_some()).collect();
        assert_eq!(marked, [true, false, true, true, true, false]);
        Zone { data: mark.clone(), policy: ZonePolicy::All }.apply(&dir.join("notes.txt"));
        assert_eq!(read(&dir.join("notes.txt")).as_deref(), Some(&mark[..]));
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn copies_mark_by_policy() {
        let dir = tempfile_dir();
        let archive = dir.join("a.zip");
        std::fs::write(&archive, b"zip").unwrap();
        assert!(read(&archive).is_none());
        std::fs::write(stream(&archive), b"[ZoneTransfer]\r\nZoneId=3\r\n").unwrap();
        let data = read(&archive).unwrap();

        let (exe, txt) = (dir.join("setup.EXE"), dir.join("notes.txt"));
        std::fs::write(&exe, b"x").unwrap();
        std::fs::write(&txt, b"x").unwrap();
        let risky = Zone { data: data.clone(), policy: ZonePolicy::Risky };
        risky.apply(&exe);
        risky.apply(&txt);
        assert!(read(&exe).is_some());
        assert!(read(&txt).is_none());
        Zone { data, policy: ZonePolicy::All }.apply(&txt);
        assert_eq!(read(&txt).unwrap(), b"[ZoneTransfer]\r\nZoneId=3\r\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("kibotos-zone-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
