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
    /// The archive it came from, and its path there.
    archive: PathBuf,
    entry: String,
    /// Modification time when it was extracted or last saved back.
    mtime: Option<SystemTime>,
    /// The version the user chose not to save yet ("Ahora no"): not asked
    /// about again, but still offered when Kibotós closes.
    declined: Option<SystemTime>,
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

fn mtime(file: &Path) -> Option<SystemTime> {
    std::fs::metadata(file).and_then(|m| m.modified()).ok()
}

impl Opened {
    fn changed(&self) -> bool {
        let now = mtime(&self.file);
        now.is_some() && now != self.mtime
    }
}

/// Remembers an extracted file (`entry` of `archive`) so changes to it can
/// be detected later.
pub fn track(file: &Path, archive: &Path, entry: &str) {
    let opened = Opened { file: file.to_path_buf(), archive: archive.to_path_buf(), entry: entry.to_owned(), mtime: mtime(file), declined: None };
    OPENED.with(|o| o.borrow_mut().push(opened));
}

/// Opened files that were saved by the program that opened them, with the
/// folder of their archive.
pub fn modified() -> Vec<(PathBuf, PathBuf)> {
    OPENED.with(|o| {
        o.borrow()
            .iter()
            .filter(|f| f.changed())
            .map(|f| (f.file.clone(), f.archive.parent().map(Path::to_path_buf).unwrap_or_default()))
            .collect()
    })
}

/// Files opened from `archive` that changed since they were extracted or
/// last saved back: (file, entry path in the archive).
pub fn edited(archive: &Path) -> Vec<(PathBuf, String)> {
    OPENED.with(|o| {
        o.borrow()
            .iter()
            .filter(|f| f.archive == archive && f.changed() && mtime(&f.file) != f.declined)
            .map(|f| (f.file.clone(), f.entry.clone()))
            .collect()
    })
}

/// Every file opened from `archive` that changed, including the ones the
/// user chose not to save yet: (file, entry path in the archive).
pub fn changed_from(archive: &Path) -> Vec<(PathBuf, String)> {
    OPENED.with(|o| {
        o.borrow()
            .iter()
            .filter(|f| f.archive == archive && f.changed())
            .map(|f| (f.file.clone(), f.entry.clone()))
            .collect()
    })
}

/// Stops asking about the current version of `file`.
pub fn decline(file: &Path) {
    let now = mtime(file);
    OPENED.with(|o| {
        for f in o.borrow_mut().iter_mut().filter(|f| f.file == file) {
            f.declined = now;
        }
    });
}

/// Takes the current version of `file` as the new baseline (it was saved
/// into the archive, or the user chose not to).
pub fn accept(file: &Path) {
    let now = mtime(file);
    OPENED.with(|o| {
        for f in o.borrow_mut().iter_mut().filter(|f| f.file == file) {
            f.mtime = now;
        }
    });
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
