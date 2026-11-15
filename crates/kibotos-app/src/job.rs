//! An extraction running on a worker thread, shared by the main window and
//! the context-menu modes.
//!
//! The worker owns its own `ArchiveReader` (it reopens the archive), so the
//! UI never blocks on it. It reports back to a window with two messages:
//! `WM_APP_EXTRACTED` (posted when done) and `WM_APP_CONFLICT` (sent, and
//! waited for, when a file already exists).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use kibotos_core::zone::{Zone, ZonePolicy};
use kibotos_core::{
    ArchiveReader, Conflict, Control, DiskSink, Entry, EntryId, EntryKind, ExtractReport, ExtractSink, OpenOptions,
    Overwrite, Timestamp,
};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, SendMessageW, WM_APP};

use crate::{dialogs, progress};

pub const WM_APP_EXTRACTED: u32 = WM_APP + 3;
/// Sent (blocking) by the worker: "this file exists, what now?"
pub const WM_APP_CONFLICT: u32 = WM_APP + 4;

/// Where the files go.
#[derive(Clone, Debug)]
pub enum Dest {
    /// Exactly this folder.
    Folder(PathBuf),
    /// "Extract here": straight into `parent` when the archive holds a
    /// single item at its root, otherwise into `parent\<folder_name>` so
    /// many loose files are not scattered.
    Smart { parent: PathBuf, folder_name: String },
}

impl Dest {
    /// Folder shown to the user before the archive has been read.
    pub fn shown(&self) -> &Path {
        match self {
            Dest::Folder(p) | Dest::Smart { parent: p, .. } => p,
        }
    }
}

pub struct ExtractJob {
    pub archive: PathBuf,
    pub password: Option<String>,
    /// `None` extracts everything.
    pub ids: Option<Vec<EntryId>>,
    /// Archive folders stripped from paths, so selected items land by
    /// name (empty keeps full paths). See `DiskSink::bases`.
    pub bases: Vec<String>,
    pub dest: Dest,
    /// Check the password on one small encrypted entry before writing.
    pub verify: bool,
    /// Open this extracted file afterwards (double-click on a file).
    pub open_after: Option<PathBuf>,
    /// With `open_after`: show Windows' "Open with" list instead.
    pub open_with: bool,
}

pub struct ExtractDone {
    pub job: ExtractJob,
    /// The folder actually used (resolved from `Dest`).
    pub dest: PathBuf,
    pub result: kibotos_core::Result<ExtractReport>,
    pub secs: f64,
}

struct ConflictQuery {
    target: PathBuf,
    size: Option<u64>,
    modified: Option<Timestamp>,
}

/// Folder name for an archive: "fotos.zip" -> "fotos",
/// "copia.tar.gz" -> "copia", "serie.part1.rar" -> "serie".
pub fn folder_name(archive: &Path) -> String {
    let file = archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut name = file.as_str();
    // "fotos.7z.001" -> "fotos.7z", "fotos.z01" -> "fotos" (split archives).
    if kibotos_core::is_part_name(name) {
        if let Some((stem, ext)) = name.rsplit_once('.') {
            if !stem.is_empty() {
                name = stem;
                if !ext.bytes().all(|b| b.is_ascii_digit()) {
                    // .z01 and .r00 parts already lost the archive extension.
                    name = name.trim_end_matches('.');
                }
            }
        }
    }
    // Strip archive-like extensions, at most two (".tar.gz").
    const EXTS: &[&str] = &[
        "zip", "jar", "apk", "cbz", "7z", "rar", "cbr", "tar", "gz", "tgz", "bz2", "tbz", "tbz2", "xz", "txz", "zst",
        "tzst", "gzip", "bzip2", "zstd",
    ];
    for _ in 0..2 {
        match name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() && EXTS.iter().any(|e| e.eq_ignore_ascii_case(ext)) => name = stem,
            _ => break,
        }
    }
    // "serie.part1" -> "serie"
    if let Some((stem, part)) = name.rsplit_once('.') {
        let p = part.to_ascii_lowercase();
        if !stem.is_empty() && p.starts_with("part") && p[4..].chars().all(|c| c.is_ascii_digit()) && p.len() > 4 {
            name = stem;
        }
    }
    let name = name.trim_end_matches(['.', ' ']);
    if name.is_empty() || name.eq_ignore_ascii_case(&file) {
        // No archive extension: the folder would collide with the file.
        format!("{file} (extraído)")
    } else {
        name.to_string()
    }
}

/// Number of distinct items at the archive root ("a/x", "a/y", "b" -> 2).
pub fn root_items(entries: &[Entry]) -> usize {
    let mut roots = std::collections::HashSet::new();
    for e in entries {
        let first = e.path.trim_start_matches('/').split('/').next().unwrap_or("");
        if !first.is_empty() {
            roots.insert(first.to_lowercase());
            if roots.len() > 1 {
                break;
            }
        }
    }
    roots.len()
}

/// Discards data: used to test a password on one small entry.
struct NullSink;

impl ExtractSink for NullSink {
    fn begin(&mut self, _: &Entry) -> kibotos_core::Result<Option<Box<dyn std::io::Write + '_>>> {
        Ok(Some(Box::new(std::io::sink())))
    }
    fn finish(&mut self, _: &Entry) -> kibotos_core::Result<()> {
        Ok(())
    }
}

/// Decodes the smallest selected encrypted entry, so a wrong password is
/// reported before anything is written to disk.
fn verify_password(reader: &mut dyn ArchiveReader, ids: &[EntryId]) -> kibotos_core::Result<()> {
    const MAX_TEST: u64 = 16 << 20;
    let entries = reader.entries();
    let smallest = ids
        .iter()
        .map(|&id| &entries[id as usize])
        .filter(|e| e.encrypted && e.kind == EntryKind::File)
        .min_by_key(|e| e.size.unwrap_or(u64::MAX));
    match smallest {
        Some(e) if e.size.unwrap_or(u64::MAX) <= MAX_TEST => {
            let id = e.id;
            reader.extract(&[id], &mut NullSink, &Control::new())
        }
        _ => Ok(()),
    }
}

fn conflict_from_code(code: isize) -> Conflict {
    match code {
        1 => Conflict::Replace,
        2 => Conflict::Skip,
        3 => Conflict::ReplaceAll,
        4 => Conflict::SkipAll,
        _ => Conflict::Cancel,
    }
}

fn conflict_code(c: Conflict) -> isize {
    match c {
        Conflict::Replace => 1,
        Conflict::Skip => 2,
        Conflict::ReplaceAll => 3,
        Conflict::SkipAll => 4,
        Conflict::Cancel => 5,
    }
}

/// Handles `WM_APP_CONFLICT` on the UI thread: asks on top of the progress
/// window (shown now if it was still hidden).
///
/// # Safety
/// `lp` must be the `LPARAM` of a `WM_APP_CONFLICT` sent by [`spawn`].
pub unsafe fn answer_conflict(fallback_owner: HWND, lp: LPARAM) -> isize {
    let q = unsafe { &*(lp.0 as *const ConflictQuery) };
    let owner = progress::show_now().unwrap_or(fallback_owner);
    conflict_code(dialogs::ask_conflict(owner, &q.target, q.size, q.modified))
}

/// Takes ownership of the result posted with `WM_APP_EXTRACTED`.
///
/// # Safety
/// `lp` must be the `LPARAM` of a `WM_APP_EXTRACTED` posted by [`spawn`],
/// and this must be called once per message.
pub unsafe fn take_done(lp: LPARAM) -> ExtractDone {
    *unsafe { Box::from_raw(lp.0 as *mut ExtractDone) }
}

/// Runs `job` on a worker thread. `target` receives the messages.
pub fn spawn(target: HWND, job: ExtractJob, ctl: Arc<Control>) {
    let target = target.0 as isize;
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let mut dest = job.dest.shown().to_path_buf();
        let result = (|| {
            let opts = OpenOptions { password: job.password.clone() };
            let mut reader = kibotos_core::open(&job.archive, &opts)?;
            let all: Vec<EntryId>;
            let ids = match &job.ids {
                Some(ids) => ids.as_slice(),
                None => {
                    all = (0..reader.entries().len() as EntryId).collect();
                    &all
                }
            };
            if let Dest::Smart { parent, folder_name } = &job.dest {
                dest = if root_items(reader.entries()) > 1 { parent.join(folder_name) } else { parent.clone() };
            }
            if job.verify {
                verify_password(reader.as_mut(), ids)?;
            }
            let ask = Box::new(move |path: &Path, entry: &Entry| {
                let q = ConflictQuery { target: path.to_path_buf(), size: entry.size, modified: entry.modified };
                // Blocks this worker until the UI thread answers.
                let r = unsafe {
                    SendMessageW(HWND(target as _), WM_APP_CONFLICT, None, Some(LPARAM(&q as *const _ as isize)))
                };
                conflict_from_code(r.0)
            });
            // Mark of the Web: everything opened from a downloaded archive,
            // risky types when extracting in bulk.
            let policy = if job.open_after.is_some() { ZonePolicy::All } else { ZonePolicy::Risky };
            let zone = kibotos_core::zone::read(&job.archive).map(|data| Zone { data, policy });
            let mut sink = DiskSink::new(&dest).bases(&job.bases).overwrite(Overwrite::Ask).on_conflict(ask).zone(zone);
            let extracted = reader.extract(ids, &mut sink, &ctl);
            let closed = sink.close();
            extracted?;
            closed
        })();
        let done = Box::into_raw(Box::new(ExtractDone { job, dest, result, secs: t0.elapsed().as_secs_f64() }));
        unsafe {
            if PostMessageW(Some(HWND(target as _)), WM_APP_EXTRACTED, WPARAM(0), LPARAM(done as isize)).is_err() {
                drop(Box::from_raw(done));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use kibotos_core::EntryKind;

    #[test]
    fn folder_names() {
        let f = |s: &str| folder_name(Path::new(s));
        assert_eq!(f(r"C:\x\fotos.zip"), "fotos");
        assert_eq!(f(r"C:\x\copia.tar.gz"), "copia");
        assert_eq!(f(r"C:\x\copia.TGZ"), "copia");
        assert_eq!(f(r"C:\x\serie.part1.rar"), "serie");
        assert_eq!(f(r"C:\x\fotos.7z.001"), "fotos");
        assert_eq!(f(r"C:\x\fotos.z01"), "fotos");
        assert_eq!(f(r"C:\x\copia.tar.gz.002"), "copia");
        assert_eq!(f(r"C:\x\notas.v2.zip"), "notas.v2");
        assert_eq!(f(r"C:\x\sin-extension"), "sin-extension (extraído)");
        assert_eq!(f(r"C:\x\.zip"), ".zip (extraído)");
    }

    #[test]
    fn root_item_count() {
        let e = |p: &str| Entry::new(0, p, EntryKind::File);
        assert_eq!(root_items(&[e("a/x"), e("a/y"), e("A/z")]), 1);
        assert_eq!(root_items(&[e("a/x"), e("b")]), 2);
        assert_eq!(root_items(&[e("solo.txt")]), 1);
        assert_eq!(root_items(&[]), 0);
    }
}
