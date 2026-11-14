use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::safety::{UnsafeReason, safe_join};
use crate::{Entry, EntryKind, Result, Timestamp};

/// Receives extracted entries. Backends call `begin` for each selected entry
/// in archive order, write its data to the returned writer, then call
/// `finish` (on success) or `abort` (on any error or cancellation).
pub trait ExtractSink {
    /// Returns a writer for the entry's data, or `None` to skip it
    /// (directories, blocked paths, existing files...). `finish`/`abort` are
    /// only called when a writer was returned.
    fn begin(&mut self, entry: &Entry) -> Result<Option<Box<dyn Write + '_>>>;

    /// The entry was written completely.
    fn finish(&mut self, entry: &Entry) -> Result<()>;

    /// Writing the entry failed or was cancelled; remove partial output.
    fn abort(&mut self, _entry: &Entry) {}
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Overwrite {
    /// Replace existing files.
    #[default]
    Always,
    /// Keep existing files and skip the entry.
    Never,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkipReason {
    Unsafe(UnsafeReason),
    /// Links are not created in v0.1.
    Link,
    Exists,
}

#[derive(Clone, Debug)]
pub struct Skipped {
    pub path: String,
    pub reason: SkipReason,
}

/// What a [`DiskSink`] did.
#[derive(Clone, Debug, Default)]
pub struct ExtractReport {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub skipped: Vec<Skipped>,
}

/// Writes entries below a destination folder, after validating each path.
pub struct DiskSink {
    dest: PathBuf,
    base: String,
    overwrite: Overwrite,
    created: HashSet<PathBuf>,
    current: Option<(PathBuf, BufWriter<File>)>,
    pub report: ExtractReport,
}

impl DiskSink {
    pub fn new(dest: impl Into<PathBuf>) -> DiskSink {
        DiskSink {
            dest: dest.into(),
            base: String::new(),
            overwrite: Overwrite::Always,
            created: HashSet::new(),
            current: None,
            report: ExtractReport::default(),
        }
    }

    pub fn overwrite(mut self, mode: Overwrite) -> Self {
        self.overwrite = mode;
        self
    }

    /// Strips this archive folder (e.g. `"docs/2026"`) from entry paths, so
    /// extracting from inside a folder does not recreate its parents.
    pub fn base(mut self, folder: &str) -> Self {
        let f = folder.trim_matches('/');
        self.base = if f.is_empty() { String::new() } else { format!("{f}/") };
        self
    }

    fn skip(&mut self, entry: &Entry, reason: SkipReason) -> Result<Option<Box<dyn Write + '_>>> {
        self.report.skipped.push(Skipped { path: entry.path.to_string(), reason });
        Ok(None)
    }

    fn ensure_dir(&mut self, dir: &Path) -> io::Result<()> {
        if self.created.contains(dir) {
            return Ok(());
        }
        fs::create_dir_all(dir)?;
        // Cache every ancestor so siblings do not hit the file system again.
        for anc in dir.ancestors() {
            if anc == self.dest || !self.created.insert(anc.to_path_buf()) {
                break;
            }
        }
        Ok(())
    }
}

impl ExtractSink for DiskSink {
    fn begin(&mut self, entry: &Entry) -> Result<Option<Box<dyn Write + '_>>> {
        let rel = entry.path.strip_prefix(self.base.as_str()).unwrap_or(&entry.path);
        let target = match safe_join(&self.dest, rel) {
            Ok(p) => p,
            Err(r) => return self.skip(entry, SkipReason::Unsafe(r)),
        };
        match entry.kind {
            EntryKind::Link => self.skip(entry, SkipReason::Link),
            EntryKind::Dir => {
                self.ensure_dir(&target)?;
                self.report.dirs += 1;
                Ok(None)
            }
            EntryKind::File => {
                if let Some(parent) = target.parent() {
                    self.ensure_dir(parent)?;
                }
                if self.overwrite == Overwrite::Never && target.exists() {
                    return self.skip(entry, SkipReason::Exists);
                }
                let file = File::create(&target)?;
                self.current = Some((target, BufWriter::with_capacity(256 * 1024, file)));
                let w = &mut self.current.as_mut().unwrap().1;
                Ok(Some(Box::new(w)))
            }
        }
    }

    fn finish(&mut self, entry: &Entry) -> Result<()> {
        if let Some((_, w)) = self.current.take() {
            let file = w.into_inner().map_err(|e| e.into_error())?;
            if let Some(t) = entry.modified {
                let utc = match t {
                    Timestamp::Utc(s) => s,
                    Timestamp::Local(s) => crate::time::local_to_utc(s),
                };
                if let Some(st) = to_system_time(utc) {
                    let _ = file.set_modified(st);
                }
            }
            self.report.files += 1;
            self.report.bytes += entry.size.unwrap_or(0);
        }
        Ok(())
    }

    fn abort(&mut self, _entry: &Entry) {
        if let Some((path, w)) = self.current.take() {
            drop(w);
            let _ = fs::remove_file(path);
        }
    }
}

fn to_system_time(unix: i64) -> Option<SystemTime> {
    if unix >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(unix as u64))
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs(unix.unsigned_abs()))
    }
}

/// Collects file contents in memory, keyed by entry path. Used by tests and
/// for small previews.
#[derive(Debug, Default)]
pub struct MemorySink {
    pub files: HashMap<String, Vec<u8>>,
    pub dirs: Vec<String>,
    current: Vec<u8>,
}

impl MemorySink {
    pub fn new() -> MemorySink {
        MemorySink::default()
    }
}

impl ExtractSink for MemorySink {
    fn begin(&mut self, entry: &Entry) -> Result<Option<Box<dyn Write + '_>>> {
        if entry.is_dir() {
            self.dirs.push(entry.path.to_string());
            return Ok(None);
        }
        self.current.clear();
        Ok(Some(Box::new(&mut self.current)))
    }

    fn finish(&mut self, entry: &Entry) -> Result<()> {
        self.files.insert(entry.path.to_string(), std::mem::take(&mut self.current));
        Ok(())
    }

    fn abort(&mut self, _entry: &Entry) {
        self.current.clear();
    }
}
