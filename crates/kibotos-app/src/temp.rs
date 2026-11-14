//! Temporary copies of files opened from an archive.
//!
//! Each running Kibotós gets its own folder, `%TEMP%\Kibotos\<pid>-<start>`,
//! with one numbered subfolder per opened file (so two files with the same
//! name never clash). The folder is removed when Kibotós exits. Files still
//! held open by another program cannot be deleted then; the next start
//! removes folders whose process is gone (the start time in the name guards
//! against a reused process id).

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::System::Threading::{GetCurrentProcessId, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

struct Opened {
    file: PathBuf,
    /// Folder of the archive it came from (where copies are offered).
    archive_dir: PathBuf,
    mtime: Option<SystemTime>,
}

thread_local! {
    static OPENED: RefCell<Vec<Opened>> = const { RefCell::new(Vec::new()) };
    static NEXT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn root() -> PathBuf {
    std::env::temp_dir().join("Kibotos")
}

/// Creation time of a running process (100 ns ticks), if it exists.
fn process_start(pid: u32) -> Option<u64> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let ok = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u).is_ok();
        let _ = CloseHandle(h);
        ok.then_some(((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64)
    }
}

pub fn session_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let pid = unsafe { GetCurrentProcessId() };
        root().join(format!("{pid}-{}", process_start(pid).unwrap_or(0)))
    })
}

/// A new, empty folder for one opened file.
pub fn new_slot() -> io::Result<PathBuf> {
    let n = NEXT.with(|c| {
        c.set(c.get() + 1);
        c.get()
    });
    let dir = session_dir().join(n.to_string());
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Remembers an extracted file so changes to it can be detected later.
pub fn track(file: &Path, archive: &Path) {
    let mtime = std::fs::metadata(file).and_then(|m| m.modified()).ok();
    let archive_dir = archive.parent().map(Path::to_path_buf).unwrap_or_default();
    OPENED.with(|o| o.borrow_mut().push(Opened { file: file.to_path_buf(), archive_dir, mtime }));
}

/// Opened files that were saved by the program that opened them.
pub fn modified() -> Vec<(PathBuf, PathBuf)> {
    OPENED.with(|o| {
        o.borrow()
            .iter()
            .filter(|f| {
                let now = std::fs::metadata(&f.file).and_then(|m| m.modified()).ok();
                now.is_some() && now != f.mtime
            })
            .map(|f| (f.file.clone(), f.archive_dir.clone()))
            .collect()
    })
}

/// Copies a modified file next to its archive as "name (modificado).ext",
/// never overwriting an existing file. Returns the new path.
pub fn save_copy(file: &Path, dir: &Path) -> io::Result<PathBuf> {
    let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = file.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    for n in 1..1000 {
        let suffix = if n == 1 { String::new() } else { format!(" {n}") };
        let target = dir.join(format!("{stem} ({}{suffix}){ext}", crate::strings::t::MODIFIED_SUFFIX));
        if !target.exists() {
            std::fs::copy(file, &target)?;
            return Ok(target);
        }
    }
    Err(io::Error::other("no free name"))
}

/// Removes this session's folder (best effort: files still open elsewhere
/// stay until the next start).
pub fn cleanup() {
    OPENED.with(|o| o.borrow_mut().clear());
    let _ = std::fs::remove_dir_all(session_dir());
}

/// Removes folders left by Kibotós processes that are no longer running.
/// Runs on a background thread at startup.
pub fn clean_stale() {
    let Ok(dir) = std::fs::read_dir(root()) else { return };
    let own = session_dir().to_path_buf();
    for entry in dir.flatten() {
        let path = entry.path();
        if path == own {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((pid, start)) = name.split_once('-') else { continue };
        let (Ok(pid), Ok(start)) = (pid.parse::<u32>(), start.parse::<u64>()) else { continue };
        if process_start(pid) != Some(start) {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}
