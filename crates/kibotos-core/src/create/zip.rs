//! ZIP writer. Small files are read and deflated in parallel, a batch at a
//! time, then written in order; large files are streamed and their header
//! is patched once their size and CRC are known. Files that do not shrink
//! are stored. Encrypted archives (AES-256) go through the `zip` crate.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{Counted, CreateOptions, CreateReport, Item, Level, SkipCause, io_err, read_all, threads};
use crate::{Control, Error, Result};

/// Files up to this size are compressed in memory, in parallel.
const SMALL: u64 = 8 << 20;
const BATCH_BYTES: u64 = 96 << 20;
const BATCH_FILES: usize = 8192;
/// Streamed files above this size get Zip64 fields from the start.
const ZIP64_STREAM: u64 = 0xF000_0000;
const U32_MAX: u64 = u32::MAX as u64;
const STORED: u16 = 0;
const DEFLATED: u16 = 8;

fn deflate_level(level: Level) -> Option<u32> {
    match level {
        Level::Store => None,
        Level::Fast => Some(1),
        Level::Normal => Some(6),
        Level::Maximum => Some(9),
    }
}

/// A file compressed in memory.
struct Packed {
    method: u16,
    crc: u32,
    size: u64,
    data: Vec<u8>,
}

fn pack(item: &Item, level: Option<u32>, ctl: &Control) -> io::Result<Packed> {
    let raw = read_all(item, ctl)?;
    let mut crc = flate2::Crc::new();
    crc.update(&raw);
    let size = raw.len() as u64;
    if let Some(l) = level {
        if !raw.is_empty() {
            let mut enc = flate2::write::DeflateEncoder::new(Vec::with_capacity(raw.len() / 2), flate2::Compression::new(l));
            enc.write_all(&raw)?;
            let data = enc.finish()?;
            if data.len() < raw.len() {
                return Ok(Packed { method: DEFLATED, crc: crc.sum(), size, data });
            }
        }
    }
    Ok(Packed { method: STORED, crc: crc.sum(), size, data: raw })
}

/// Compresses a batch of items on `threads` threads; results in item order
/// (`None` for folders).
fn pack_batch(items: &[Item], level: Option<u32>, threads: usize, ctl: &Control) -> Vec<Option<io::Result<Packed>>> {
    let results: Vec<Mutex<Option<io::Result<Packed>>>> = items.iter().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    let work = items.iter().filter(|i| !i.dir).count();
    std::thread::scope(|s| {
        for _ in 0..threads.min(work).max(1) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    if item.dir || ctl.is_cancelled() {
                        continue;
                    }
                    *results[i].lock().unwrap() = Some(pack(item, level, ctl));
                }
            });
        }
    });
    results.into_iter().map(|m| m.into_inner().unwrap()).collect()
}

struct Record {
    name: Vec<u8>,
    method: u16,
    dos: (u16, u16),
    mtime: Option<i64>,
    crc: u32,
    csize: u64,
    usize: u64,
    offset: u64,
    attributes: u32,
}

impl Record {
    fn new(item: &Item) -> Record {
        let mut name = item.name.clone().into_bytes();
        if item.dir {
            name.push(b'/');
        }
        let dos = crate::time::secs_to_dos(crate::time::utc_to_local(item.modified.unwrap_or(0)));
        let attributes = if item.dir { item.attributes & 0xFF } else { item.attributes & 0xFF & !0x10 };
        Record { name, method: STORED, dos, mtime: item.modified, crc: 0, csize: 0, usize: 0, offset: 0, attributes }
    }

    fn flags(&self) -> u16 {
        // Bit 11: the name is UTF-8.
        if self.name.is_ascii() { 0 } else { 0x0800 }
    }

    /// "UT" extended timestamp: the exact modification time, in UTC.
    fn time_extra(&self) -> Vec<u8> {
        match self.mtime.and_then(|t| u32::try_from(t).ok()) {
            Some(t) => {
                let mut x = vec![0x55, 0x54, 5, 0, 1];
                x.extend_from_slice(&t.to_le_bytes());
                x
            }
            None => Vec::new(),
        }
    }
}

struct ZipOut<W: Write + Seek> {
    w: W,
    pos: u64,
    records: Vec<Record>,
}

impl<W: Write + Seek> ZipOut<W> {
    fn put(&mut self, b: &[u8]) -> io::Result<()> {
        self.w.write_all(b)?;
        self.pos += b.len() as u64;
        Ok(())
    }

    /// Writes a local header; with `zip64`, sizes go in a Zip64 field.
    fn local_header(&mut self, r: &Record, zip64: bool) -> io::Result<()> {
        let mut h = Vec::with_capacity(30 + r.name.len() + 29);
        h.extend_from_slice(b"PK\x03\x04");
        h.extend_from_slice(&(if zip64 { 45u16 } else { 20 }).to_le_bytes());
        h.extend_from_slice(&r.flags().to_le_bytes());
        h.extend_from_slice(&r.method.to_le_bytes());
        h.extend_from_slice(&r.dos.1.to_le_bytes());
        h.extend_from_slice(&r.dos.0.to_le_bytes());
        h.extend_from_slice(&r.crc.to_le_bytes());
        let (cs, us) = if zip64 { (u32::MAX, u32::MAX) } else { (r.csize as u32, r.usize as u32) };
        h.extend_from_slice(&cs.to_le_bytes());
        h.extend_from_slice(&us.to_le_bytes());
        let mut extra = Vec::new();
        if zip64 {
            extra.extend_from_slice(&[1, 0, 16, 0]);
            extra.extend_from_slice(&r.usize.to_le_bytes());
            extra.extend_from_slice(&r.csize.to_le_bytes());
        }
        extra.extend_from_slice(&r.time_extra());
        h.extend_from_slice(&(r.name.len() as u16).to_le_bytes());
        h.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        h.extend_from_slice(&r.name);
        h.extend_from_slice(&extra);
        self.put(&h)
    }

    fn add_dir(&mut self, item: &Item) -> io::Result<()> {
        let mut r = Record::new(item);
        r.offset = self.pos;
        self.local_header(&r, false)?;
        self.records.push(r);
        Ok(())
    }

    fn add_packed(&mut self, item: &Item, p: Packed) -> io::Result<()> {
        let mut r = Record::new(item);
        r.method = p.method;
        r.crc = p.crc;
        r.usize = p.size;
        r.csize = p.data.len() as u64;
        r.offset = self.pos;
        let zip64 = r.usize >= U32_MAX || r.csize >= U32_MAX;
        self.local_header(&r, zip64)?;
        self.put(&p.data)?;
        self.records.push(r);
        Ok(())
    }

    /// Streams a large file, then patches its header with size and CRC.
    fn add_streamed(&mut self, item: &Item, file: File, level: Option<u32>, ctl: &Control) -> Result<()> {
        let mut r = Record::new(item);
        r.method = if level.is_some() { DEFLATED } else { STORED };
        r.offset = self.pos;
        let zip64 = item.size > ZIP64_STREAM;
        self.local_header(&r, zip64)?;
        let data_start = self.pos;

        let mut src = Counted { inner: io::BufReader::with_capacity(1 << 20, file), ctl };
        let mut crc = CrcReader { inner: &mut src, crc: flate2::Crc::new(), n: 0 };
        let mut out = CountW { w: &mut self.w, n: 0 };
        match level {
            Some(l) => {
                let mut enc = flate2::write::DeflateEncoder::new(&mut out, flate2::Compression::new(l));
                io::copy(&mut crc, &mut enc).map_err(|e| io_err(e, ctl))?;
                enc.finish().map_err(|e| io_err(e, ctl))?;
            }
            None => {
                io::copy(&mut crc, &mut out).map_err(|e| io_err(e, ctl))?;
            }
        }
        let csize = out.n;
        self.pos += csize;
        r.crc = crc.crc.sum();
        r.usize = crc.n;
        r.csize = csize;
        if !zip64 && (r.usize >= U32_MAX || r.csize >= U32_MAX) {
            return Err(Error::Unsupported(format!("{} grew past 4 GiB while being compressed", item.name)));
        }
        // Patch CRC and sizes (in the Zip64 field when it is there).
        let end = self.pos;
        self.w.seek(SeekFrom::Start(r.offset + 14))?;
        self.w.write_all(&r.crc.to_le_bytes())?;
        if zip64 {
            let at = r.offset + 30 + r.name.len() as u64 + 4;
            self.w.seek(SeekFrom::Start(at))?;
            self.w.write_all(&r.usize.to_le_bytes())?;
            self.w.write_all(&r.csize.to_le_bytes())?;
        } else {
            self.w.write_all(&(r.csize as u32).to_le_bytes())?;
            self.w.write_all(&(r.usize as u32).to_le_bytes())?;
        }
        self.w.seek(SeekFrom::Start(end))?;
        debug_assert_eq!(data_start + csize, end);
        self.records.push(r);
        Ok(())
    }

    /// Central directory and end records.
    fn finish(mut self) -> io::Result<W> {
        let cd_start = self.pos;
        let records = std::mem::take(&mut self.records);
        for r in &records {
            let mut zip64 = Vec::new();
            if r.usize >= U32_MAX {
                zip64.extend_from_slice(&r.usize.to_le_bytes());
            }
            if r.csize >= U32_MAX {
                zip64.extend_from_slice(&r.csize.to_le_bytes());
            }
            if r.offset >= U32_MAX {
                zip64.extend_from_slice(&r.offset.to_le_bytes());
            }
            let mut extra = Vec::new();
            if !zip64.is_empty() {
                extra.extend_from_slice(&1u16.to_le_bytes());
                extra.extend_from_slice(&(zip64.len() as u16).to_le_bytes());
                extra.extend_from_slice(&zip64);
            }
            extra.extend_from_slice(&r.time_extra());
            let mut h = Vec::with_capacity(46 + r.name.len() + extra.len());
            h.extend_from_slice(b"PK\x01\x02");
            h.extend_from_slice(&63u16.to_le_bytes());
            h.extend_from_slice(&(if zip64.is_empty() { 20u16 } else { 45 }).to_le_bytes());
            h.extend_from_slice(&r.flags().to_le_bytes());
            h.extend_from_slice(&r.method.to_le_bytes());
            h.extend_from_slice(&r.dos.1.to_le_bytes());
            h.extend_from_slice(&r.dos.0.to_le_bytes());
            h.extend_from_slice(&r.crc.to_le_bytes());
            h.extend_from_slice(&(r.csize.min(U32_MAX) as u32).to_le_bytes());
            h.extend_from_slice(&(r.usize.min(U32_MAX) as u32).to_le_bytes());
            h.extend_from_slice(&(r.name.len() as u16).to_le_bytes());
            h.extend_from_slice(&(extra.len() as u16).to_le_bytes());
            h.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // comment length, disk, internal attributes
            h.extend_from_slice(&r.attributes.to_le_bytes());
            h.extend_from_slice(&(r.offset.min(U32_MAX) as u32).to_le_bytes());
            h.extend_from_slice(&r.name);
            h.extend_from_slice(&extra);
            self.put(&h)?;
        }
        let (count, cd_size) = (records.len() as u64, self.pos - cd_start);
        if count >= 0xFFFF || cd_size >= U32_MAX || cd_start >= U32_MAX {
            let z64 = self.pos;
            let mut e = Vec::with_capacity(76);
            e.extend_from_slice(b"PK\x06\x06");
            e.extend_from_slice(&44u64.to_le_bytes());
            e.extend_from_slice(&63u16.to_le_bytes());
            e.extend_from_slice(&45u16.to_le_bytes());
            e.extend_from_slice(&[0; 8]);
            e.extend_from_slice(&count.to_le_bytes());
            e.extend_from_slice(&count.to_le_bytes());
            e.extend_from_slice(&cd_size.to_le_bytes());
            e.extend_from_slice(&cd_start.to_le_bytes());
            e.extend_from_slice(b"PK\x06\x07");
            e.extend_from_slice(&0u32.to_le_bytes());
            e.extend_from_slice(&z64.to_le_bytes());
            e.extend_from_slice(&1u32.to_le_bytes());
            self.put(&e)?;
        }
        let mut e = Vec::with_capacity(22);
        e.extend_from_slice(b"PK\x05\x06");
        e.extend_from_slice(&[0, 0, 0, 0]);
        let c16 = count.min(0xFFFF) as u16;
        e.extend_from_slice(&c16.to_le_bytes());
        e.extend_from_slice(&c16.to_le_bytes());
        e.extend_from_slice(&(cd_size.min(U32_MAX) as u32).to_le_bytes());
        e.extend_from_slice(&(cd_start.min(U32_MAX) as u32).to_le_bytes());
        e.extend_from_slice(&[0, 0]);
        self.put(&e)?;
        self.w.flush()?;
        Ok(self.w)
    }
}

/// Counts the bytes written through it.
struct CountW<'a, W> {
    w: &'a mut W,
    n: u64,
}

impl<W: Write> Write for CountW<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.w.write(buf)?;
        self.n += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

/// Computes the CRC-32 of what is read through it.
struct CrcReader<'a, R> {
    inner: &'a mut R,
    crc: flate2::Crc,
    /// Bytes read (the CRC's own counter is 32-bit).
    n: u64,
}

impl<R: io::Read> io::Read for CrcReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.crc.update(&buf[..n]);
        self.n += n as u64;
        Ok(n)
    }
}

pub(super) fn write(temp: &Path, items: &[Item], opts: &CreateOptions, ctl: &Control, report: &mut CreateReport) -> Result<()> {
    if let Some(pw) = opts.password.as_deref().filter(|p| !p.is_empty()) {
        return write_aes(temp, items, opts.level, pw, ctl, report);
    }
    let level = deflate_level(opts.level);
    let threads = threads(opts);
    let mut z = ZipOut { w: BufWriter::with_capacity(1 << 20, File::create(temp)?), pos: 0, records: Vec::new() };
    let mut i = 0;
    while i < items.len() {
        ctl.check()?;
        let start = i;
        let mut bytes = 0;
        while i < items.len() && (items[i].dir || items[i].size <= SMALL) && i - start < BATCH_FILES && bytes < BATCH_BYTES {
            bytes += items[i].size;
            i += 1;
        }
        if i > start {
            let batch = &items[start..i];
            for (item, packed) in batch.iter().zip(pack_batch(batch, level, threads, ctl)) {
                ctl.check()?;
                match packed {
                    None => z.add_dir(item)?,
                    Some(Ok(p)) => {
                        z.add_packed(item, p)?;
                        ctl.add_entry();
                        report.files += 1;
                    }
                    Some(Err(e)) => skip(report, &item.path, e, ctl)?,
                }
                if item.dir {
                    report.dirs += 1;
                }
            }
            report.bytes_in += bytes;
            continue;
        }
        let item = &items[i];
        i += 1;
        match File::open(&item.path) {
            Ok(f) => {
                z.add_streamed(item, f, level, ctl)?;
                ctl.add_entry();
                report.files += 1;
                report.bytes_in += item.size;
            }
            Err(e) => skip(report, &item.path, e, ctl)?,
        }
    }
    z.finish()?;
    Ok(())
}

/// Records an unreadable file, unless the error is really a cancellation.
fn skip(report: &mut CreateReport, path: &Path, e: io::Error, ctl: &Control) -> Result<()> {
    ctl.check()?;
    report.skipped.push((PathBuf::from(path), SkipCause::Unreadable(e.to_string())));
    Ok(())
}

/// AES-256 encrypted ZIP (WinZip AE-2), written by the `zip` crate.
fn write_aes(temp: &Path, items: &[Item], level: Level, password: &str, ctl: &Control, report: &mut CreateReport) -> Result<()> {
    use zip::write::SimpleFileOptions;
    let map = |e: zip::result::ZipError| match e {
        zip::result::ZipError::Io(e) => io_err(e, ctl),
        other => Error::Io(io::Error::other(other.to_string())),
    };
    let pw_bytes = crate::codepage::zip_password_for_writing(password);
    let mut z = zip::ZipWriter::new(BufWriter::with_capacity(1 << 20, File::create(temp)?));
    for item in items {
        ctl.check()?;
        let (y, mo, d, h, mi, s) = crate::time::civil_from_secs(crate::time::utc_to_local(item.modified.unwrap_or(0)));
        let when = zip::DateTime::from_date_and_time(y.clamp(1980, 2107) as u16, mo as u8, d as u8, h as u8, mi as u8, s as u8)
            .unwrap_or_default();
        let base = SimpleFileOptions::default().last_modified_time(when);
        if item.dir {
            z.add_directory(format!("{}/", item.name), base).map_err(map)?;
            report.dirs += 1;
            continue;
        }
        let file = match File::open(&item.path) {
            Ok(f) => f,
            Err(e) => {
                skip(report, &item.path, e, ctl)?;
                continue;
            }
        };
        let opts = match deflate_level(level) {
            Some(l) => base.compression_method(zip::CompressionMethod::Deflated).compression_level(Some(i64::from(l))),
            None => base.compression_method(zip::CompressionMethod::Stored),
        }
        .large_file(item.size > ZIP64_STREAM)
        .with_aes_encryption_bytes(zip::AesMode::Aes256, &pw_bytes);
        z.start_file(item.name.as_str(), opts).map_err(map)?;
        io::copy(&mut Counted { inner: io::BufReader::with_capacity(1 << 20, file), ctl }, &mut z).map_err(|e| io_err(e, ctl))?;
        ctl.add_entry();
        report.files += 1;
        report.bytes_in += item.size;
    }
    z.finish().map_err(map)?;
    Ok(())
}
