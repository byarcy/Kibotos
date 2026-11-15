//! ISO 9660 disc images, with Joliet (Windows) or Rock Ridge (Unix) long
//! names. The directory tree is untrusted: loops, absurd sizes and extents
//! past the end of the image are rejected.
//!
//! Images with a UDF file system (DVD and Blu-ray images, most images over
//! 4 GiB, Windows installation media) are read through UDF, which has the
//! complete tree; ISO 9660 is the fallback.

use std::collections::HashSet;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::open_file;
use crate::pump::{Pump, Selection};
use crate::{ArchiveReader, Control, Entry, EntryId, EntryKind, Error, ExtractSink, Format, Result, Timestamp};

const SECTOR: u64 = 2048;
const FIRST_DESCRIPTOR: u64 = 16;
const MAX_DEPTH: usize = 64;
const MAX_ENTRIES: usize = 4_000_000;
/// Largest single directory read into memory.
const MAX_DIR_BYTES: u64 = 64 << 20;

pub struct IsoBackend {
    path: PathBuf,
    entries: Vec<Entry>,
    /// Per entry: byte ranges (offset, length) holding the file's data.
    extents: Vec<Vec<(u64, u64)>>,
    len: u64,
}

fn bad(msg: &str) -> Error {
    Error::Corrupt(format!("ISO image: {msg}"))
}

fn u32_le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// Recording date: years since 1900, month, day, hour, minute, second,
/// offset from UTC in 15-minute units.
fn record_time(d: &[u8]) -> Option<Timestamp> {
    if d.len() < 7 || d[1] == 0 || d[2] == 0 {
        return None;
    }
    let secs = crate::time::secs_from_civil(1900 + i32::from(d[0]), u32::from(d[1]), u32::from(d[2]), u32::from(d[3]), u32::from(d[4]), u32::from(d[5]));
    let offset = i64::from(d[6] as i8) * 15 * 60;
    Some(Timestamp::Utc(secs - offset))
}

/// Rock Ridge alternate name (`NM` entries) in a record's system-use area.
fn rock_ridge_name(su: &[u8]) -> Option<String> {
    let mut name = Vec::new();
    let mut found = false;
    let mut p = 0usize;
    while p + 4 <= su.len() {
        let len = su[p + 2] as usize;
        if len < 4 || p + len > su.len() {
            break;
        }
        if &su[p..p + 2] == b"NM" && len >= 5 {
            let flags = su[p + 4];
            // Flags 2 and 4 mean "." and "..": not names.
            if flags & 0x06 == 0 {
                name.extend_from_slice(&su[p + 5..p + len]);
                found = true;
            }
        }
        p += len;
    }
    found.then(|| String::from_utf8_lossy(&name).into_owned())
}

/// Plain ISO 9660 name: "README.TXT;1" -> "README.TXT", "FILE.;1" -> "FILE".
fn iso_name(raw: &[u8], joliet: bool) -> String {
    let mut s = if joliet {
        let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        raw.iter().map(|&b| b as char).collect()
    };
    if let Some(i) = s.rfind(';') {
        if s[i + 1..].bytes().all(|b| b.is_ascii_digit()) {
            s.truncate(i);
        }
    }
    if s.ends_with('.') && !joliet {
        s.pop();
    }
    s
}

struct Walk<'a, R> {
    r: &'a mut R,
    len: u64,
    block: u64,
    joliet: bool,
    entries: Vec<Entry>,
    extents: Vec<Vec<(u64, u64)>>,
    seen: HashSet<u64>,
}

impl<R: Read + Seek> Walk<'_, R> {
    fn read(&mut self, at: u64, len: u64) -> Result<Vec<u8>> {
        if at.checked_add(len).is_none_or(|end| end > self.len) {
            return Err(bad("directory past the end of the image"));
        }
        let mut buf = vec![0u8; len as usize];
        self.r.seek(SeekFrom::Start(at))?;
        self.r.read_exact(&mut buf).map_err(Error::from_read)?;
        Ok(buf)
    }

    fn dir(&mut self, prefix: &str, lba: u64, size: u64, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH || size > MAX_DIR_BYTES || !self.seen.insert(lba) {
            return Err(bad("directory loop or excessive nesting"));
        }
        let data = self.read(lba * self.block, size)?;
        let mut subdirs = Vec::new();
        let mut last_multi: Option<usize> = None;
        let mut p = 0usize;
        while p < data.len() {
            let len = data[p] as usize;
            if len == 0 {
                // Records never cross a sector: skip to the next one.
                p = (p / self.block as usize + 1) * self.block as usize;
                continue;
            }
            if len < 34 || p + len > data.len() {
                return Err(bad("damaged directory record"));
            }
            let r = &data[p..p + len];
            p += len;
            let name_len = r[32] as usize;
            if 33 + name_len > r.len() {
                return Err(bad("damaged directory record"));
            }
            let raw = &r[33..33 + name_len];
            if raw == [0] || raw == [1] {
                continue; // "." and ".."
            }
            let su = &r[(33 + name_len + (1 - name_len % 2)).min(r.len())..];
            let name = (!self.joliet).then(|| rock_ridge_name(su)).flatten().unwrap_or_else(|| iso_name(raw, self.joliet));
            if name.is_empty() || name.contains('/') || name.contains('\0') {
                continue;
            }
            let flags = r[25];
            let ext_attr = u64::from(r[1]);
            let start = (u64::from(u32_le(r, 2)) + ext_attr) * self.block;
            let length = u64::from(u32_le(r, 10));
            let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            if flags & 0x02 != 0 {
                subdirs.push((path.clone(), u64::from(u32_le(r, 2)), length));
                let mut e = Entry::new(self.entries.len(), &path, EntryKind::Dir);
                e.modified = record_time(&r[18..25]);
                self.entries.push(e);
                self.extents.push(Vec::new());
                last_multi = None;
                continue;
            }
            if start.checked_add(length).is_none_or(|end| end > self.len) {
                return Err(bad("file data past the end of the image"));
            }
            // Files over 4 GiB are several records with the same name.
            if let Some(i) = last_multi {
                if *self.entries[i].path == *path {
                    self.extents[i].push((start, length));
                    let size = self.entries[i].size.unwrap_or(0) + length;
                    self.entries[i].size = Some(size);
                    last_multi = (flags & 0x80 != 0).then_some(i);
                    continue;
                }
            }
            let mut e = Entry::new(self.entries.len(), &path, EntryKind::File);
            e.size = Some(length);
            e.modified = record_time(&r[18..25]);
            last_multi = (flags & 0x80 != 0).then_some(self.entries.len());
            self.entries.push(e);
            self.extents.push(vec![(start, length)]);
            if self.entries.len() > MAX_ENTRIES {
                return Err(Error::Unsupported("too many files in the image".into()));
            }
        }
        for (path, lba, len) in subdirs {
            self.dir(&path, lba, len, depth + 1)?;
        }
        Ok(())
    }
}

/// Whether the image has a UDF file system (Volume Recognition Sequence).
fn has_udf<R: Read + Seek>(r: &mut R, len: u64) -> bool {
    for i in 0..64u64 {
        let at = (FIRST_DESCRIPTOR + i) * SECTOR;
        if at + 6 > len {
            return false;
        }
        let mut h = [0u8; 6];
        if r.seek(SeekFrom::Start(at)).is_err() || r.read_exact(&mut h).is_err() {
            return false;
        }
        match &h[1..6] {
            b"NSR02" | b"NSR03" => return true,
            b"CD001" | b"BEA01" | b"BOOT2" | b"CDW02" => {}
            b"TEA01" => return false,
            _ => return false,
        }
    }
    false
}

impl IsoBackend {
    pub fn open(path: &Path) -> Result<IsoBackend> {
        let (mut file, len) = open_file(path)?;
        let udf = if has_udf(&mut file, len) { Some(super::udf::read(&mut file, len)) } else { None };
        match (udf, Self::open_iso9660(&mut file, len)) {
            (Some(Ok((entries, extents))), _) | (_, Ok((entries, extents))) => {
                Ok(IsoBackend { path: path.to_owned(), entries, extents, len })
            }
            (Some(Err(e)), _) | (None, Err(e)) => Err(e),
        }
    }

    fn open_iso9660<R: Read + Seek>(file: &mut R, len: u64) -> Result<super::udf::Tree> {
        let (mut primary, mut joliet) = (None, None);
        for i in 0..64u64 {
            let at = (FIRST_DESCRIPTOR + i) * SECTOR;
            if at + SECTOR > len {
                break;
            }
            let mut d = vec![0u8; SECTOR as usize];
            file.seek(SeekFrom::Start(at))?;
            file.read_exact(&mut d).map_err(Error::from_read)?;
            if &d[1..6] != b"CD001" {
                break;
            }
            match d[0] {
                1 if primary.is_none() => primary = Some(d),
                2 if joliet.is_none() && matches!(&d[88..91], b"%/@" | b"%/C" | b"%/E") => joliet = Some(d),
                255 => break,
                _ => {}
            }
        }
        let is_joliet = joliet.is_some();
        let desc = joliet.or(primary).ok_or_else(|| bad("no volume descriptor"))?;
        let block = u64::from(u16::from_le_bytes([desc[128], desc[129]]));
        if !matches!(block, 512 | 1024 | 2048) {
            return Err(bad("unsupported block size"));
        }
        let root = &desc[156..190];
        let (root_lba, root_len) = (u64::from(u32_le(root, 2)), u64::from(u32_le(root, 10)));
        let mut w = Walk {
            r: file,
            len,
            block,
            joliet: is_joliet,
            entries: Vec::new(),
            extents: Vec::new(),
            seen: HashSet::new(),
        };
        w.dir("", root_lba, root_len, 0)?;
        Ok((w.entries, w.extents))
    }
}

/// The data of a file spread over one or more extents.
struct Extents<'a, R> {
    r: &'a mut R,
    list: &'a [(u64, u64)],
    left: u64,
    /// The current extent is sparse (reads as zeros).
    zeros: bool,
}

impl<R: Read + Seek> Read for Extents<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.left == 0 {
            let Some(((start, len), rest)) = self.list.split_first() else { return Ok(0) };
            self.zeros = *start == super::udf::ZEROS;
            if !self.zeros {
                self.r.seek(SeekFrom::Start(*start))?;
            }
            self.left = *len;
            self.list = rest;
        }
        let n = (buf.len() as u64).min(self.left) as usize;
        let n = if self.zeros {
            buf[..n].fill(0);
            n
        } else {
            self.r.read(&mut buf[..n])?
        };
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.left -= n as u64;
        Ok(n)
    }
}

impl ArchiveReader for IsoBackend {
    fn format(&self) -> Format {
        Format::Iso
    }

    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn set_password(&mut self, _password: Option<&str>) {}

    fn extract(&mut self, ids: &[EntryId], sink: &mut dyn ExtractSink, ctl: &Control) -> Result<()> {
        let sel = Selection::new(ids, &self.entries, ctl)?;
        let mut order: Vec<usize> = (0..self.entries.len()).filter(|&i| sel.contains(i)).collect();
        // Directories first, then files in disc order.
        order.sort_by_key(|&i| (self.entries[i].kind == EntryKind::File, self.extents[i].first().map_or(0, |e| e.0)));
        let mut pump = Pump::new(ctl, self.len);
        let (mut file, _) = open_file(&self.path)?;
        for i in order {
            let mut data = Extents { r: &mut file, list: &self.extents[i], left: 0, zeros: false };
            pump.run(&self.entries[i], &mut data, sink, &Error::from_read)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(iso_name(b"README.TXT;1", false), "README.TXT");
        assert_eq!(iso_name(b"FILE.;1", false), "FILE");
        let j: Vec<u8> = "Año.txt;1".encode_utf16().flat_map(u16::to_be_bytes).collect();
        assert_eq!(iso_name(&j, true), "Año.txt");
        let su = [b'N', b'M', 9, 1, 0, b'l', b'o', b'n', b'g'];
        assert_eq!(rock_ridge_name(&su).as_deref(), Some("long"));
    }
}
