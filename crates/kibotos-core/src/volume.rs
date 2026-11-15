//! Split archives: several files that together hold one archive.
//!
//! - `name.7z.001`, `name.zip.001`, `name.001`, ...: plain byte splits (what
//!   7-Zip and most tools write). The parts are simply concatenated.
//! - `name.z01`, `name.z02`, ..., `name.zip`: a spanned ZIP. Its offsets
//!   count from the start of each part, so the central directory is
//!   rewritten in memory with offsets into the concatenation.
//! - `name.part1.rar`, `name.rar` + `name.r00`, ...: unRAR follows the
//!   volumes itself; it only has to start from the first one.
//!
//! Whatever part the user opens, reading starts at the first one.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Most parts followed, so a folder full of numbered files cannot make
/// opening one archive open thousands of files.
const MAX_PARTS: usize = 4096;
/// Largest central directory rewritten in memory.
const MAX_CENTRAL_DIR: u64 = 256 << 20;

/// `(file name without the number, number, digits)` for `a.7z.003`.
fn numbered(name: &str) -> Option<(&str, u32, usize)> {
    let dot = name.rfind('.')?;
    let digits = &name[dot + 1..];
    if digits.len() < 3 || digits.len() > 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((&name[..=dot], digits.parse().ok()?, digits.len()))
}

fn sibling(path: &Path, name: String) -> PathBuf {
    path.with_file_name(name)
}

fn file_name(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()
}

/// The parts of a plain byte split, in order, starting from part 1.
fn numbered_parts(path: &Path) -> Option<Result<Vec<PathBuf>>> {
    let (stem, n, width) = numbered(file_name(path)?)?;
    if n == 0 {
        return None;
    }
    let part = |i: u32| sibling(path, format!("{stem}{i:0width$}"));
    let mut parts = Vec::new();
    for i in 1.. {
        let p = part(i);
        if !p.is_file() {
            if i <= n {
                return Some(Err(Error::MissingVolume(file_name(&p).unwrap_or_default().to_owned())));
            }
            break;
        }
        parts.push(p);
        if parts.len() >= MAX_PARTS {
            break;
        }
    }
    Some(Ok(parts))
}

/// `name.z01` ... `name.zNN`, `name.zip`, when `path` belongs to a spanned ZIP.
fn zip_span_parts(path: &Path) -> Option<Result<Vec<PathBuf>>> {
    let name = file_name(path)?;
    let dot = name.rfind('.')?;
    let (stem, ext) = (&name[..dot], name[dot + 1..].to_ascii_lowercase());
    let is_piece = ext.len() >= 3 && ext.starts_with('z') && ext[1..].bytes().all(|b| b.is_ascii_digit());
    let first = sibling(path, format!("{stem}.z01"));
    if !(is_piece || (ext == "zip" && first.is_file())) {
        return None;
    }
    let last = sibling(path, format!("{stem}.zip"));
    let mut parts = Vec::new();
    for i in 1.. {
        let p = sibling(path, format!("{stem}.z{i:02}"));
        if !p.is_file() {
            break;
        }
        parts.push(p);
        if parts.len() >= MAX_PARTS {
            break;
        }
    }
    if parts.is_empty() {
        return Some(Err(Error::MissingVolume(format!("{stem}.z01"))));
    }
    if !last.is_file() {
        return Some(Err(Error::MissingVolume(format!("{stem}.zip"))));
    }
    parts.push(last);
    Some(Ok(parts))
}

/// For a RAR volume, the first volume of its set (unRAR reads the rest).
pub fn rar_first_volume(path: &Path) -> Option<PathBuf> {
    let name = file_name(path)?;
    let lower = name.to_ascii_lowercase();
    // name.part3.rar -> name.part1.rar (same number of digits).
    if let Some(stem) = lower.strip_suffix(".rar") {
        if let Some(i) = stem.rfind(".part") {
            let digits = &stem[i + 5..];
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                let first = format!("{}{:0w$}.rar", &name[..i + 5], 1, w = digits.len());
                return Some(sibling(path, first));
            }
        }
        return None;
    }
    // name.r00, name.r01 ... -> name.rar
    let dot = name.rfind('.')?;
    let ext = &lower[dot + 1..];
    if ext.len() == 3 && ext.starts_with('r') && ext[1..].bytes().all(|b| b.is_ascii_digit()) {
        return Some(sibling(path, format!("{}.rar", &name[..dot])));
    }
    None
}

/// The file to show and reopen for `path`: the first part of a split.
pub fn first_part(path: &Path) -> PathBuf {
    if let Some(first) = rar_first_volume(path) {
        return first;
    }
    if let Some(Ok(parts)) = numbered_parts(path).or_else(|| zip_span_parts(path)) {
        return parts.into_iter().next().unwrap_or_else(|| path.to_owned());
    }
    path.to_owned()
}

/// Whether a file name looks like one part of a split archive.
pub fn is_part_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let Some(dot) = lower.rfind('.') else { return false };
    let ext = &lower[dot + 1..];
    numbered(&lower).is_some_and(|(_, n, _)| n >= 1)
        || (ext.len() >= 3 && ext.starts_with('z') && ext[1..].bytes().all(|b| b.is_ascii_digit()))
        || (ext.len() == 3 && ext.starts_with('r') && ext[1..].bytes().all(|b| b.is_ascii_digit()))
}

struct Part {
    file: File,
    start: u64,
    len: u64,
}

/// An archive file, or the concatenation of its parts, as one seekable
/// stream. Reads past `cut` come from `tail` (a rewritten ZIP directory).
pub struct Source {
    parts: Vec<Part>,
    pos: u64,
    len: u64,
    cut: u64,
    tail: Vec<u8>,
    /// Name of the part after the last one found, for error messages.
    next_part: Option<String>,
}

impl Source {
    /// Opens `path`, joining it with the other parts if it is a split.
    pub fn open(path: &Path) -> Result<Source> {
        if let Some(parts) = zip_span_parts(path) {
            let mut s = Source::join(&parts?)?;
            s.patch_zip_span()?;
            return Ok(s);
        }
        if let Some(parts) = numbered_parts(path) {
            let parts = parts?;
            let mut s = Source::join(&parts)?;
            if let Some((stem, _, width)) = parts.last().and_then(|p| file_name(p)).and_then(numbered) {
                s.next_part = Some(format!("{stem}{:0width$}", parts.len() + 1));
            }
            return Ok(s);
        }
        Source::join(&[path.to_owned()])
    }

    fn join(paths: &[PathBuf]) -> Result<Source> {
        let mut parts = Vec::with_capacity(paths.len());
        let mut start = 0u64;
        for p in paths {
            let file = File::open(p)?;
            let len = file.metadata()?.len();
            parts.push(Part { file, start, len });
            start += len;
        }
        Ok(Source { parts, pos: 0, len: start, cut: start, tail: Vec::new(), next_part: None })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_split(&self) -> bool {
        self.parts.len() > 1
    }

    /// For a plain split that failed to open: the part that may be missing.
    pub fn next_part(&self) -> Option<&str> {
        self.next_part.as_deref()
    }

    /// Reads at an absolute offset of the joined parts (ignoring `tail`).
    fn read_raw(&self, mut pos: u64, buf: &mut [u8]) -> io::Result<usize> {
        let i = self.parts.partition_point(|p| p.start + p.len <= pos);
        let Some(part) = self.parts.get(i) else { return Ok(0) };
        let n = (buf.len() as u64).min(part.start + part.len - pos) as usize;
        pos -= part.start;
        read_at(&part.file, &mut buf[..n], pos)
    }

    fn read_exact_raw(&self, mut pos: u64, mut buf: &mut [u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let n = self.read_raw(pos, buf)?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            pos += n as u64;
            buf = &mut buf[n..];
        }
        Ok(())
    }

    /// Rewrites a spanned ZIP's central directory so its offsets count from
    /// the start of the first part, and marks it as a single-disk archive.
    fn patch_zip_span(&mut self) -> Result<()> {
        let bad = || Error::Corrupt("damaged split ZIP directory".into());
        let starts: Vec<u64> = self.parts.iter().map(|p| p.start).collect();
        let last = self.parts.last().ok_or_else(bad)?;
        // End of central directory record: search the last 64 KiB + 22.
        let window = last.len.min(65_557);
        let mut tail = vec![0u8; window as usize];
        self.read_exact_raw(self.len - window, &mut tail)?;
        let at = (0..tail.len().saturating_sub(21)).rev().find(|&i| tail[i..].starts_with(b"PK\x05\x06")).ok_or_else(bad)?;
        let eocd_pos = self.len - window + at as u64;
        let e = &tail[at..];
        let u16_at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let u32_at = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u64_at = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let this_disk = u16_at(e, 4) as u64;
        let mut cd_disk = u16_at(e, 6) as u64;
        let mut count = u16_at(e, 10) as u64;
        let mut cd_size = u32_at(e, 12) as u64;
        let mut cd_off = u32_at(e, 16) as u64;
        let comment_len = (u16_at(e, 20) as usize).min(e.len() - 22);
        let comment = e[22..22 + comment_len].to_vec();
        let mut disks = this_disk + 1;
        // Zip64 locator just before the record.
        if eocd_pos >= 20 {
            let mut loc = [0u8; 20];
            self.read_exact_raw(eocd_pos - 20, &mut loc)?;
            if loc.starts_with(b"PK\x06\x07") {
                let disk = u32_at(&loc, 4) as usize;
                let rec = starts.get(disk).ok_or_else(bad)? + u64_at(&loc, 8);
                let mut z = [0u8; 56];
                self.read_exact_raw(rec, &mut z)?;
                if !z.starts_with(b"PK\x06\x06") {
                    return Err(bad());
                }
                disks = u32_at(&z, 16) as u64 + 1;
                cd_disk = u32_at(&z, 20) as u64;
                count = u64_at(&z, 32);
                cd_size = u64_at(&z, 40);
                cd_off = u64_at(&z, 48);
            }
        }
        if disks as usize != self.parts.len() {
            // The record names the number of the last part: some are missing.
            let missing = (self.parts.len()..disks as usize).next().unwrap_or(1);
            return Err(Error::MissingVolume(format!("z{missing:02}")));
        }
        if cd_size > MAX_CENTRAL_DIR {
            return Err(Error::Unsupported("ZIP directory too large".into()));
        }
        let cd_start = starts.get(cd_disk as usize).ok_or_else(bad)? + cd_off;
        let mut cd = vec![0u8; cd_size as usize];
        self.read_exact_raw(cd_start, &mut cd)?;

        let mut out = Vec::with_capacity(cd.len() + 1024);
        let mut p = 0usize;
        let mut records = 0u64;
        while records < count {
            let rec = cd.get(p..p + 46).ok_or_else(bad)?;
            if !rec.starts_with(b"PK\x01\x02") {
                return Err(bad());
            }
            let (name_len, extra_len, comment_len) = (u16_at(rec, 28) as usize, u16_at(rec, 30) as usize, u16_at(rec, 32) as usize);
            let total = 46 + name_len + extra_len + comment_len;
            let full = cd.get(p..p + total).ok_or_else(bad)?;
            let (comp, uncomp) = (u32_at(full, 20), u32_at(full, 24));
            let mut disk = u16_at(full, 34) as u64;
            let mut off = u32_at(full, 42) as u64;
            // Parse extra fields, reading (and dropping) the Zip64 one.
            let extra = &full[46 + name_len..46 + name_len + extra_len];
            let mut kept = Vec::new();
            let (mut big_uncomp, mut big_comp) = (None, None);
            let mut x = 0usize;
            while x + 4 <= extra.len() {
                let (id, size) = (u16_at(extra, x), u16_at(extra, x + 2) as usize);
                let data = extra.get(x + 4..x + 4 + size).ok_or_else(bad)?;
                if id == 1 {
                    let mut v = data.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap()));
                    if uncomp == u32::MAX {
                        big_uncomp = v.next();
                    }
                    if comp == u32::MAX {
                        big_comp = v.next();
                    }
                    if off == u32::MAX as u64 {
                        off = v.next().ok_or_else(bad)?;
                    }
                    if disk == 0xFFFF {
                        let rest = &data[data.len().min(data.len() / 8 * 8)..];
                        disk = rest.get(..4).map(|b| u32_at(b, 0) as u64).unwrap_or(disk);
                    }
                } else {
                    kept.extend_from_slice(&extra[x..x + 4 + size]);
                }
                x += 4 + size;
            }
            let abs = starts.get(disk as usize).ok_or_else(bad)? + off;
            let mut zip64 = Vec::new();
            if let Some(v) = big_uncomp {
                zip64.extend_from_slice(&v.to_le_bytes());
            }
            if let Some(v) = big_comp {
                zip64.extend_from_slice(&v.to_le_bytes());
            }
            let fixed_off = if abs >= u32::MAX as u64 {
                zip64.extend_from_slice(&abs.to_le_bytes());
                u32::MAX
            } else {
                abs as u32
            };
            let mut new_extra = Vec::new();
            if !zip64.is_empty() {
                new_extra.extend_from_slice(&1u16.to_le_bytes());
                new_extra.extend_from_slice(&(zip64.len() as u16).to_le_bytes());
                new_extra.extend_from_slice(&zip64);
            }
            new_extra.extend_from_slice(&kept);
            let mut head = full[..46].to_vec();
            head[30..32].copy_from_slice(&(new_extra.len() as u16).to_le_bytes());
            head[34..36].copy_from_slice(&0u16.to_le_bytes());
            head[42..46].copy_from_slice(&fixed_off.to_le_bytes());
            out.extend_from_slice(&head);
            out.extend_from_slice(&full[46..46 + name_len]);
            out.extend_from_slice(&new_extra);
            out.extend_from_slice(&full[46 + name_len + extra_len..]);
            p += total;
            records += 1;
        }

        let new_size = out.len() as u64;
        let new_off = cd_start;
        if count >= 0xFFFF || new_size >= u32::MAX as u64 || new_off >= u32::MAX as u64 {
            let z64_pos = new_off + new_size;
            out.extend_from_slice(b"PK\x06\x06");
            out.extend_from_slice(&44u64.to_le_bytes());
            out.extend_from_slice(&45u16.to_le_bytes());
            out.extend_from_slice(&45u16.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&count.to_le_bytes());
            out.extend_from_slice(&count.to_le_bytes());
            out.extend_from_slice(&new_size.to_le_bytes());
            out.extend_from_slice(&new_off.to_le_bytes());
            out.extend_from_slice(b"PK\x06\x07");
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&z64_pos.to_le_bytes());
            out.extend_from_slice(&1u32.to_le_bytes());
        }
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&[0, 0, 0, 0]);
        let c16 = count.min(0xFFFF) as u16;
        out.extend_from_slice(&c16.to_le_bytes());
        out.extend_from_slice(&c16.to_le_bytes());
        out.extend_from_slice(&(new_size.min(u32::MAX as u64) as u32).to_le_bytes());
        out.extend_from_slice(&(new_off.min(u32::MAX as u64) as u32).to_le_bytes());
        out.extend_from_slice(&(comment.len() as u16).to_le_bytes());
        out.extend_from_slice(&comment);

        self.cut = cd_start;
        self.len = cd_start + out.len() as u64;
        self.tail = out;
        Ok(())
    }
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], pos: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, pos)
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], pos: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, pos)
}

impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let n = if self.pos >= self.cut {
            let from = &self.tail[(self.pos - self.cut) as usize..];
            let n = from.len().min(buf.len());
            buf[..n].copy_from_slice(&from[..n]);
            n
        } else {
            let limit = ((self.cut - self.pos) as usize).min(buf.len());
            self.read_raw(self.pos, &mut buf[..limit])?
        };
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Source {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = pos.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before start"))?;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(numbered("a.7z.003"), Some(("a.7z.", 3, 3)));
        assert_eq!(numbered("a.7z"), None);
        assert_eq!(numbered("a.12"), None);
        assert!(is_part_name("x.7z.001"));
        assert!(is_part_name("x.Z05"));
        assert!(is_part_name("x.r00"));
        assert!(!is_part_name("x.zip"));
        assert!(!is_part_name("x.zst"));
        assert_eq!(rar_first_volume(Path::new("d/a.part03.rar")), Some(PathBuf::from("d/a.part01.rar")));
        assert_eq!(rar_first_volume(Path::new("d/a.r05")), Some(PathBuf::from("d/a.rar")));
        assert_eq!(rar_first_volume(Path::new("d/a.rar")), None);
    }

    #[test]
    fn joins_parts() {
        let dir = std::env::temp_dir().join(format!("kibotos-vol-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 7) as u8).collect();
        for (i, chunk) in data.chunks(3000).enumerate() {
            std::fs::write(dir.join(format!("x.bin.{:03}", i + 1)), chunk).unwrap();
        }
        let mut s = Source::open(&dir.join("x.bin.003")).unwrap();
        assert_eq!(s.len(), 10_000);
        let mut all = Vec::new();
        s.read_to_end(&mut all).unwrap();
        assert_eq!(all, data);
        s.seek(SeekFrom::Start(2999)).unwrap();
        let mut two = [0u8; 2];
        s.read_exact(&mut two).unwrap();
        assert_eq!(two, [data[2999], data[3000]]);
        assert_eq!(first_part(&dir.join("x.bin.004")), dir.join("x.bin.001"));
        std::fs::remove_file(dir.join("x.bin.002")).unwrap();
        assert!(matches!(Source::open(&dir.join("x.bin.003")), Err(Error::MissingVolume(_))));
        let _ = std::fs::remove_dir_all(dir);
    }
}
