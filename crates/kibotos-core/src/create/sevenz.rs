//! 7z writer. Files are sorted by extension (similar content compresses
//! better together) and packed into solid blocks that are compressed in
//! parallel, one block per thread; the blocks are written in order. Files
//! larger than a block are streamed alone with multi-threaded LZMA2.
//!
//! LZMA2 at high levels needs a lot of memory per thread (about eleven
//! times the dictionary), so the thread count is capped by a memory budget.

use std::fs::File;
use std::io::{self, BufWriter, Cursor};
use std::path::{Path, PathBuf};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

use sevenz_rust2::encoder_options::{AesEncoderOptions, Lzma2Options};
use sevenz_rust2::{ArchiveEntry, ArchiveWriter, EncoderConfiguration, EncoderMethod, NtTime, Password, SourceReader, prepare_block};

use super::{Counted, CreateOptions, CreateReport, Item, Level, SkipCause, io_err, threads};
use crate::{Control, Error, Result};

/// Memory the parallel LZMA2 encoders may use together.
const MEMORY_BUDGET: u64 = 1536 << 20;

fn preset(level: Level) -> u32 {
    match level {
        Level::Store => 0,
        Level::Fast => 1,
        Level::Normal => 5,
        Level::Maximum => 9,
    }
}

fn map_err(e: sevenz_rust2::Error, ctl: &Control) -> Error {
    match e {
        sevenz_rust2::Error::Io(e, _) => io_err(e, ctl),
        other if ctl.is_cancelled() => {
            let _ = other;
            Error::Cancelled
        }
        other => Error::Io(io::Error::other(other.to_string())),
    }
}

/// x86/x64 machine code: compressed better after the BCJ filter turns
/// relative call/jump addresses into absolute ones (as 7-Zip does).
const EXECUTABLE: &[&str] = &["exe", "dll", "sys", "ocx", "cpl", "scr", "drv", "efi", "com", "ax", "pyd", "node"];

fn is_executable(name: &str) -> bool {
    EXECUTABLE.iter().any(|e| extension(name).eq_ignore_ascii_case(e))
}

/// The coder chain: optional AES-256, then LZMA2 (or plain copy).
fn methods(opts: &CreateOptions, lzma2: Option<Lzma2Options>) -> Vec<EncoderConfiguration> {
    methods_with(opts, lzma2, false)
}

/// Data flows through the chain from the last coder to the first: BCJ,
/// then LZMA2, then AES.
fn methods_with(opts: &CreateOptions, lzma2: Option<Lzma2Options>, bcj: bool) -> Vec<EncoderConfiguration> {
    let mut m = Vec::new();
    if let Some(pw) = opts.password.as_deref().filter(|p| !p.is_empty()) {
        m.push(AesEncoderOptions::new(Password::from(pw)).into());
    }
    let compress = lzma2.is_some();
    m.push(match lzma2 {
        Some(o) => o.into(),
        None => EncoderConfiguration::new(EncoderMethod::COPY),
    });
    if bcj && compress {
        m.push(EncoderConfiguration::new(EncoderMethod::BCJ_X86_FILTER));
    }
    m
}

fn entry(item: &Item) -> ArchiveEntry {
    let mut e = if item.dir { ArchiveEntry::new_directory(&item.name) } else { ArchiveEntry::new_file(&item.name) };
    if let Some(t) = item.modified {
        let ticks = (t + 11_644_473_600).max(0) as u64 * 10_000_000;
        e.has_last_modified_date = true;
        e.last_modified_date = NtTime::from(ticks);
    }
    e.has_windows_attributes = true;
    e.windows_attributes = item.attributes;
    e
}

/// The result of compressing one block on a worker thread.
struct Done {
    block: std::result::Result<sevenz_rust2::PreparedBlock, sevenz_rust2::Error>,
    skipped: Vec<(PathBuf, SkipCause)>,
    files: u64,
    bytes: u64,
}

fn compress_block(block: &[&Item], methods: Arc<Vec<EncoderConfiguration>>, ctl: &Control) -> Done {
    let mut entries = Vec::with_capacity(block.len());
    let mut readers = Vec::with_capacity(block.len());
    let mut skipped = Vec::new();
    let mut bytes = 0;
    for item in block {
        if ctl.is_cancelled() {
            break;
        }
        match std::fs::read(&item.path) {
            Ok(data) => {
                bytes += data.len() as u64;
                let mut e = entry(item);
                e.size = data.len() as u64;
                entries.push(e);
                readers.push(SourceReader::new(Counted { inner: Cursor::new(data), ctl }));
            }
            Err(e) => skipped.push((item.path.clone(), SkipCause::Unreadable(e.to_string()))),
        }
    }
    let files = entries.len() as u64;
    Done { block: prepare_block(methods, entries, readers), skipped, files, bytes }
}

pub(crate) fn write(temp: &Path, items: &[Item], opts: &CreateOptions, ctl: &Control, report: &mut CreateReport) -> Result<()> {
    let level = preset(opts.level);
    let store = opts.level == Level::Store;
    let dict = u64::from(lzma_rust2::Lzma2Options::with_preset(level).lzma_options.dict_size);
    let per_thread = if store { 1 } else { dict * 11 + (16 << 20) };
    let workers = threads(opts).min((MEMORY_BUDGET / per_thread) as usize).max(1);
    // Four dictionaries, like 7-Zip's LZMA2 blocks: smaller blocks would
    // add parallelism but cost more ratio than they save time (measured).
    let block_target = if store { 64 << 20 } else { (dict * 4).max(8 << 20) };

    let mut w = ArchiveWriter::new(BufWriter::with_capacity(1 << 20, File::create(temp)?)).map_err(|e| map_err(e, ctl))?;
    let single = Arc::new(methods(opts, (!store).then(|| Lzma2Options::from_level(level))));
    let with_bcj = Arc::new(methods_with(opts, (!store).then(|| Lzma2Options::from_level(level)), true));
    // Large files are split into independent chunks the size of a block,
    // compressed on all threads.
    let streaming = (!store).then(|| Lzma2Options::from_level_mt(level, threads(opts) as u32, block_target));
    w.set_content_methods(methods(opts, streaming.clone()));
    if opts.password.as_deref().is_some_and(|p| !p.is_empty()) {
        w.set_encrypt_header(opts.encrypt_names);
    }

    // Folders and empty files carry no data: they go first, in order.
    for item in items.iter().filter(|i| i.dir || i.size == 0) {
        ctl.check()?;
        w.push_archive_entry::<&[u8]>(entry(item), None).map_err(|e| map_err(e, ctl))?;
        if item.dir {
            report.dirs += 1;
        } else {
            ctl.add_entry();
            report.files += 1;
        }
    }

    // Large files: one by one, streamed.
    let mut small: Vec<&Item> = Vec::new();
    for item in items.iter().filter(|i| !i.dir && i.size > 0) {
        if item.size <= block_target {
            small.push(item);
            continue;
        }
        ctl.check()?;
        let file = match File::open(&item.path) {
            Ok(f) => f,
            Err(e) => {
                report.skipped.push((item.path.clone(), SkipCause::Unreadable(e.to_string())));
                continue;
            }
        };
        let reader = Counted { inner: io::BufReader::with_capacity(1 << 20, file), ctl };
        w.set_content_methods(methods_with(opts, streaming.clone(), is_executable(&item.name)));
        w.push_archive_entry(entry(item), Some(reader)).map_err(|e| map_err(e, ctl))?;
        ctl.add_entry();
        report.files += 1;
        report.bytes_in += item.size;
    }

    // Small files: sorted by extension, cut into solid blocks; programs
    // get blocks of their own, with the BCJ filter.
    small.sort_by(|a, b| {
        (is_executable(&a.name), extension(&a.name), &a.name).cmp(&(is_executable(&b.name), extension(&b.name), &b.name))
    });
    let mut blocks: Vec<(bool, Vec<&Item>)> = Vec::new();
    let mut size = 0;
    for item in small {
        let exe = is_executable(&item.name);
        if blocks.last().is_none_or(|(e, _)| *e != exe) || size + item.size > block_target {
            blocks.push((exe, Vec::new()));
            size = 0;
        }
        size += item.size;
        blocks.last_mut().unwrap().1.push(item);
    }
    // A pool of workers compresses blocks while this thread writes them in
    // order. Workers stay at most `ahead` blocks past the one being
    // written, which bounds the memory held by finished blocks.
    let next = AtomicUsize::new(0);
    let written = AtomicUsize::new(0);
    let ahead = workers * 2;
    // Set when this thread gives up (an error), so the workers stop too.
    let stop = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel::<(usize, Done)>();
    let result = std::thread::scope(|s| -> Result<()> {
        for _ in 0..workers.min(blocks.len()) {
            let tx = tx.clone();
            let (blocks, next, written, single, with_bcj, stop) = (&blocks, &next, &written, &single, &with_bcj, &stop);
            s.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((exe, block)) = blocks.get(i) else { break };
                    let halted = || ctl.is_cancelled() || stop.load(Ordering::Relaxed);
                    while i >= written.load(Ordering::Acquire) + ahead && !halted() {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    if halted() {
                        break;
                    }
                    let m = Arc::clone(if *exe { with_bcj } else { single });
                    if tx.send((i, compress_block(block, m, ctl))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        let mut pending = BTreeMap::new();
        for want in 0..blocks.len() {
            let d = loop {
                if let Some(d) = pending.remove(&want) {
                    break d;
                }
                match rx.recv() {
                    Ok((i, d)) => {
                        pending.insert(i, d);
                    }
                    // Every worker stopped: only a cancellation does that.
                    Err(_) => return Err(Error::Cancelled),
                }
            };
            if let Err(e) = ctl.check() {
                stop.store(true, Ordering::Relaxed);
                return Err(e);
            }
            report.skipped.extend(d.skipped);
            let pushed = d.block.map_err(|e| map_err(e, ctl)).and_then(|b| w.push_prepared_block(b).map(|_| ()).map_err(|e| map_err(e, ctl)));
            if let Err(e) = pushed {
                // Stop the workers before leaving the scope.
                stop.store(true, Ordering::Relaxed);
                return Err(e);
            }
            written.store(want + 1, Ordering::Release);
            for _ in 0..d.files {
                ctl.add_entry();
            }
            report.files += d.files;
            report.bytes_in += d.bytes;
        }
        Ok(())
    });
    result?;
    w.finish().map_err(|e| io_err(e, ctl))?;
    Ok(())
}

fn extension(name: &str) -> &str {
    let file = name.rsplit('/').next().unwrap_or(name);
    file.rfind('.').map_or("", |i| &file[i + 1..])
}
