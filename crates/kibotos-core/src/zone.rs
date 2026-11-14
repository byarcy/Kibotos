//! Mark of the Web: Windows records where a downloaded file came from in a
//! `Zone.Identifier` alternate data stream. If an archive carries it, the
//! files extracted from it must carry it too, or SmartScreen and Office
//! Protected View would be bypassed simply by putting a file in a ZIP.

use std::ffi::OsString;
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

fn stream(path: &Path) -> OsString {
    let mut s = path.as_os_str().to_owned();
    s.push(":Zone.Identifier");
    s
}

/// Reads the Mark of the Web of a file, if it has one.
pub fn read(archive: &Path) -> Option<Vec<u8>> {
    if !cfg!(windows) {
        return None;
    }
    std::fs::read(stream(archive)).ok().filter(|d| !d.is_empty())
}

impl Zone {
    /// Copies the mark onto `file` if the policy covers it. Errors are
    /// ignored: file systems without streams (FAT32, network shares)
    /// cannot store it.
    pub fn apply(&self, file: &Path) {
        let wanted = match self.policy {
            ZonePolicy::All => true,
            ZonePolicy::Risky => file
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| RISKY.iter().any(|r| r.eq_ignore_ascii_case(e))),
        };
        if wanted {
            let _ = std::fs::write(stream(file), &self.data);
        }
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
