use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use crate::safety::{UnsafeReason, safe_join};
use crate::{Entry, EntryKind, Error, Result, Timestamp};

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
    /// Ask the conflict callback (see [`DiskSink::on_conflict`]).
    Ask,
}

/// Answer to "this file already exists".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conflict {
    Replace,
    Skip,
    ReplaceAll,
    SkipAll,
    /// Stop the extraction.
    Cancel,
}

pub type ConflictFn = Box<dyn FnMut(&Path, &Entry) -> Conflict + Send>;

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

impl ExtractReport {
    /// Entries refused for safety (unsafe paths, links), as opposed to
    /// files the user chose to keep.
    pub fn blocked(&self) -> impl Iterator<Item = &Skipped> {
        self.skipped.iter().filter(|s| s.reason != SkipReason::Exists)
    }
}

/// Files up to this size are buffered and written by the writer pool.
const SMALL_FILE: u64 = 256 * 1024;
/// Jobs queued per writer thread (bounds memory to about
/// threads * QUEUE * SMALL_FILE).
const QUEUE: usize = 32;

enum Current {
    None,
    File(PathBuf, BufWriter<File>),
    Memory(PathBuf, Vec<u8>),
}

/// Writes entries below a destination folder, after validating each path.
///
/// Creating a file on Windows is expensive (file system metadata, antivirus
/// scan on close), so extracting many small files is dominated by per-file
/// cost, not by decompression. Small files are therefore buffered in memory
/// and written by a pool of threads, so those costs overlap. Large files
/// stream straight to disk from the extraction thread.
///
/// Call [`DiskSink::close`] at the end to wait for pending writes and get
/// any write error.
pub struct DiskSink {
    dest: PathBuf,
    base: String,
    overwrite: Overwrite,
    on_conflict: Option<ConflictFn>,
    created: HashSet<PathBuf>,
    current: Current,
    pool: Option<WriterPool>,
    threads: usize,
    pub report: ExtractReport,
}

impl DiskSink {
    pub fn new(dest: impl Into<PathBuf>) -> DiskSink {
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 8);
        DiskSink {
            dest: dest.into(),
            base: String::new(),
            overwrite: Overwrite::Always,
            on_conflict: None,
            created: HashSet::new(),
            current: Current::None,
            pool: None,
            threads,
            report: ExtractReport::default(),
        }
    }

    pub fn overwrite(mut self, mode: Overwrite) -> Self {
        self.overwrite = mode;
        self
    }

    /// Callback for [`Overwrite::Ask`]. Called on the extraction thread.
    pub fn on_conflict(mut self, f: ConflictFn) -> Self {
        self.on_conflict = Some(f);
        self
    }

    /// Number of writer threads for small files (0 writes everything on the
    /// extraction thread).
    pub fn writer_threads(mut self, n: usize) -> Self {
        self.threads = n;
        self
    }

    /// Strips this archive folder (e.g. `"docs/2026"`) from entry paths, so
    /// extracting from inside a folder does not recreate its parents.
    pub fn base(mut self, folder: &str) -> Self {
        let f = folder.trim_matches('/');
        self.base = if f.is_empty() { String::new() } else { format!("{f}/") };
        self
    }

    /// Waits for queued writes and returns the report, or the first write
    /// error.
    pub fn close(mut self) -> Result<ExtractReport> {
        if let Some(pool) = self.pool.take() {
            pool.join()?;
        }
        Ok(std::mem::take(&mut self.report))
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

    /// Applies the overwrite policy. `Ok(false)` means skip the entry.
    fn may_write(&mut self, target: &Path, entry: &Entry) -> Result<bool> {
        if self.overwrite == Overwrite::Always || !target.exists() {
            return Ok(true);
        }
        if self.overwrite == Overwrite::Never {
            return Ok(false);
        }
        let Some(ask) = self.on_conflict.as_mut() else { return Ok(true) };
        match ask(target, entry) {
            Conflict::Replace => Ok(true),
            Conflict::Skip => Ok(false),
            Conflict::ReplaceAll => {
                self.overwrite = Overwrite::Always;
                Ok(true)
            }
            Conflict::SkipAll => {
                self.overwrite = Overwrite::Never;
                Ok(false)
            }
            Conflict::Cancel => Err(Error::Cancelled),
        }
    }
}

impl ExtractSink for DiskSink {
    fn begin(&mut self, entry: &Entry) -> Result<Option<Box<dyn Write + '_>>> {
        if let Some(pool) = &self.pool {
            pool.check()?;
        }
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
                if !self.may_write(&target, entry)? {
                    return self.skip(entry, SkipReason::Exists);
                }
                let small = self.threads > 0 && entry.size.is_some_and(|s| s <= SMALL_FILE);
                self.current = if small {
                    Current::Memory(target, Vec::with_capacity(entry.size.unwrap_or(0) as usize))
                } else {
                    let file = File::create(&target)?;
                    Current::File(target, BufWriter::with_capacity(256 * 1024, file))
                };
                Ok(Some(match &mut self.current {
                    Current::Memory(_, buf) => Box::new(buf),
                    Current::File(_, w) => Box::new(w),
                    Current::None => unreachable!(),
                }))
            }
        }
    }

    fn finish(&mut self, entry: &Entry) -> Result<()> {
        let mtime = entry.modified.and_then(|t| {
            to_system_time(match t {
                Timestamp::Utc(s) => s,
                Timestamp::Local(s) => crate::time::local_to_utc(s),
            })
        });
        match std::mem::replace(&mut self.current, Current::None) {
            Current::None => return Ok(()),
            Current::File(_, w) => {
                let file = w.into_inner().map_err(|e| e.into_error())?;
                if let Some(t) = mtime {
                    let _ = file.set_modified(t);
                }
            }
            Current::Memory(path, data) => {
                let threads = self.threads;
                let pool = self.pool.get_or_insert_with(|| WriterPool::new(threads));
                pool.submit(Job { path, data, mtime })?;
            }
        }
        self.report.files += 1;
        self.report.bytes += entry.size.unwrap_or(0);
        Ok(())
    }

    fn abort(&mut self, _entry: &Entry) {
        if let Current::File(path, w) = std::mem::replace(&mut self.current, Current::None) {
            drop(w);
            let _ = fs::remove_file(path);
        }
    }
}

impl Drop for DiskSink {
    fn drop(&mut self) {
        // Pending small files are complete; finish writing them.
        if let Some(pool) = self.pool.take() {
            let _ = pool.join();
        }
    }
}

struct Job {
    path: PathBuf,
    data: Vec<u8>,
    mtime: Option<SystemTime>,
}

impl Job {
    fn write(self) -> io::Result<()> {
        let mut f = File::create(&self.path)?;
        f.write_all(&self.data)?;
        if let Some(t) = self.mtime {
            let _ = f.set_modified(t);
        }
        Ok(())
    }
}

struct WriterPool {
    senders: Vec<SyncSender<Job>>,
    handles: Vec<JoinHandle<()>>,
    error: Arc<Mutex<Option<io::Error>>>,
}

impl WriterPool {
    fn new(threads: usize) -> WriterPool {
        let error = Arc::new(Mutex::new(None));
        let mut senders = Vec::new();
        let mut handles = Vec::new();
        for _ in 0..threads.max(1) {
            let (tx, rx) = sync_channel::<Job>(QUEUE);
            let error = Arc::clone(&error);
            senders.push(tx);
            handles.push(std::thread::spawn(move || {
                for job in rx {
                    if let Err(e) = job.write() {
                        let mut slot = error.lock().unwrap_or_else(|p| p.into_inner());
                        slot.get_or_insert(e);
                    }
                }
            }));
        }
        WriterPool { senders, handles, error }
    }

    /// The same path always goes to the same thread, so if an archive holds
    /// a path twice the later entry still wins.
    fn submit(&self, job: Job) -> Result<()> {
        self.check()?;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        job.path.hash(&mut h);
        let i = (h.finish() % self.senders.len() as u64) as usize;
        self.senders[i].send(job).map_err(|_| Error::Io(io::Error::other("writer thread stopped")))
    }

    fn check(&self) -> Result<()> {
        let mut slot = self.error.lock().unwrap_or_else(|p| p.into_inner());
        match slot.take() {
            Some(e) => Err(Error::Io(e)),
            None => Ok(()),
        }
    }

    fn join(self) -> Result<()> {
        drop(self.senders);
        for h in self.handles {
            let _ = h.join();
        }
        let mut slot = self.error.lock().unwrap_or_else(|p| p.into_inner());
        match slot.take() {
            Some(e) => Err(Error::Io(e)),
            None => Ok(()),
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
