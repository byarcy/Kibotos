//! UDF (ECMA-167 / OSTA UDF 1.02 to 2.60), the file system of DVD and
//! Blu-ray images and of most disc images over 4 GiB. Read-only, physical
//! and metadata partitions (sparable partitions are read as physical).
//!
//! Every structure comes from an untrusted file: tags are checked, and
//! loops, nesting and sizes are bounded.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};

use crate::{Entry, EntryKind, Error, Result, Timestamp};

const MAX_DEPTH: usize = 64;
const MAX_ENTRIES: usize = 4_000_000;
const MAX_DIR_BYTES: u64 = 64 << 20;
const MAX_EXTENTS: usize = 1 << 20;
/// Marks an extent that reads as zeros (allocated but not recorded).
pub(crate) const ZEROS: u64 = u64::MAX;

/// Byte ranges (offset, length) holding a file's data, in order.
pub(crate) type Ranges = Vec<(u64, u64)>;
/// Entries of a disc image, and the data ranges of each.
pub(crate) type Tree = (Vec<Entry>, Vec<Ranges>);

/// What a file entry says about a file.
struct FileInfo {
    file_type: u8,
    size: u64,
    modified: Option<Timestamp>,
    extents: Ranges,
}

const TAG_AVDP: u16 = 2;
const TAG_PARTITION: u16 = 5;
const TAG_LOGICAL_VOLUME: u16 = 6;
const TAG_TERMINATOR: u16 = 8;
const TAG_FSD: u16 = 256;
const TAG_FID: u16 = 257;
const TAG_AED: u16 = 258;
const TAG_FILE_ENTRY: u16 = 261;
const TAG_EXT_FILE_ENTRY: u16 = 266;

fn bad(msg: &str) -> Error {
    Error::Corrupt(format!("UDF: {msg}"))
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// The tag identifier of a descriptor, if its checksum is right.
fn tag(b: &[u8]) -> Option<u16> {
    if b.len() < 16 {
        return None;
    }
    let sum = b[..16].iter().enumerate().filter(|&(i, _)| i != 4).fold(0u8, |s, (_, &x)| s.wrapping_add(x));
    (sum == b[4]).then(|| u16_at(b, 0))
}

/// OSTA compressed Unicode: first byte 8 (one byte per char) or 16 (UTF-16BE).
fn cs0(b: &[u8]) -> String {
    match b.split_first() {
        Some((8, rest)) => rest.iter().map(|&c| c as char).collect(),
        Some((16, rest)) => {
            let units: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::new(),
    }
}

/// 12-byte timestamp: type and time zone, year, month, day, h, m, s, ...
fn timestamp(b: &[u8]) -> Option<Timestamp> {
    let tz = u16_at(b, 0);
    let year = u16_at(b, 2) as i16;
    let (mo, d, h, mi, s) = (b[4], b[5], b[6], b[7], b[8]);
    if year <= 0 || mo == 0 || mo > 12 || d == 0 || d > 31 {
        return None;
    }
    let secs = crate::time::secs_from_civil(i32::from(year), u32::from(mo), u32::from(d), u32::from(h), u32::from(mi), u32::from(s));
    // Type 1 with a time zone: local time at an offset from UTC (minutes).
    let offset = ((tz & 0x0FFF) as i16) << 4 >> 4;
    if tz >> 12 == 1 && offset != -2047 {
        Some(Timestamp::Utc(secs - i64::from(offset) * 60))
    } else {
        Some(Timestamp::Local(secs))
    }
}

enum Map {
    /// Blocks of a partition on the disc.
    Physical { start: u64 },
    /// Blocks of the metadata file, itself stored in a physical partition.
    Metadata { extents: Vec<(u64, u64)> },
}

struct Udf<'a, R> {
    r: &'a mut R,
    len: u64,
    bs: u64,
    maps: Vec<Map>,
    entries: Vec<Entry>,
    extents: Vec<Vec<(u64, u64)>>,
    seen: HashSet<u64>,
}

impl<R: Read + Seek> Udf<'_, R> {
    fn read_at(&mut self, at: u64, len: u64) -> Result<Vec<u8>> {
        if at.checked_add(len).is_none_or(|end| end > self.len) {
            return Err(bad("structure past the end of the image"));
        }
        let mut buf = vec![0u8; len as usize];
        self.r.seek(SeekFrom::Start(at))?;
        self.r.read_exact(&mut buf).map_err(Error::from_read)?;
        Ok(buf)
    }

    /// Byte offset on the disc of logical block `lbn` of partition `part`.
    fn locate(&self, part: u16, lbn: u32) -> Result<u64> {
        match self.maps.get(part as usize).ok_or_else(|| bad("unknown partition"))? {
            Map::Physical { start } => Ok(start + u64::from(lbn) * self.bs),
            Map::Metadata { extents } => {
                let mut off = u64::from(lbn) * self.bs;
                for &(at, len) in extents {
                    if off < len {
                        return Ok(at + off);
                    }
                    off -= len;
                }
                Err(bad("block outside the metadata file"))
            }
        }
    }

    /// Reads a File Entry or Extended File Entry: (type, size, mtime, extents).
    fn file_entry(&mut self, part: u16, lbn: u32) -> Result<FileInfo> {
        let at = self.locate(part, lbn)?;
        let fe = self.read_at(at, self.bs)?;
        let (info_len, mtime, ea_len, ad_len, base) = match tag(&fe) {
            Some(TAG_FILE_ENTRY) => (u64_at(&fe, 56), timestamp(&fe[84..96]), u32_at(&fe, 168), u32_at(&fe, 172), 176usize),
            Some(TAG_EXT_FILE_ENTRY) => (u64_at(&fe, 56), timestamp(&fe[92..104]), u32_at(&fe, 208), u32_at(&fe, 212), 216usize),
            _ => return Err(bad("bad file entry")),
        };
        let file_type = fe[27];
        let ad_start = base + ea_len as usize;
        let ad_end = ad_start.checked_add(ad_len as usize).filter(|&e| e <= fe.len()).ok_or_else(|| bad("bad file entry"))?;
        let mut extents = Vec::new();
        match u16_at(&fe, 34) & 7 {
            // Data embedded in the entry itself.
            3 => extents.push((at + ad_start as u64, u64::from(ad_len).min(info_len))),
            kind @ (0 | 1) => {
                let mut ads = fe[ad_start..ad_end].to_vec();
                let mut guard = 0;
                loop {
                    let step = if kind == 0 { 8 } else { 16 };
                    let mut next = None;
                    for ad in ads.chunks_exact(step) {
                        let raw = u32_at(ad, 0);
                        let (ext_type, len) = (raw >> 30, u64::from(raw & 0x3FFF_FFFF));
                        if len == 0 {
                            break;
                        }
                        let block = u32_at(ad, 4);
                        let ad_part = if kind == 0 { part } else { u16_at(ad, 8) };
                        match ext_type {
                            0 => extents.push((self.locate(ad_part, block)?, len)),
                            3 => {
                                next = Some((ad_part, block));
                                break;
                            }
                            _ => extents.push((ZEROS, len)),
                        }
                        if extents.len() > MAX_EXTENTS {
                            return Err(bad("too many extents"));
                        }
                    }
                    // A continuation: more descriptors in an extent of their own.
                    let Some((p, b)) = next else { break };
                    guard += 1;
                    if guard > 4096 {
                        return Err(bad("allocation descriptor loop"));
                    }
                    let at = self.locate(p, b)?;
                    let aed = self.read_at(at, self.bs)?;
                    if tag(&aed) != Some(TAG_AED) {
                        return Err(bad("bad allocation extent"));
                    }
                    let n = (u32_at(&aed, 20) as usize).min(aed.len() - 24);
                    ads = aed[24..24 + n].to_vec();
                }
                // Trim to the file's length (the last extent may be padded).
                let mut left = info_len;
                for e in &mut extents {
                    e.1 = e.1.min(left);
                    left -= e.1;
                }
                extents.retain(|e| e.1 > 0);
            }
            _ => return Err(Error::Unsupported("UDF extended allocation descriptors".into())),
        }
        Ok(FileInfo { file_type, size: info_len, modified: mtime, extents })
    }

    fn read_extents(&mut self, extents: &[(u64, u64)], cap: u64) -> Result<Vec<u8>> {
        let total: u64 = extents.iter().map(|e| e.1).sum();
        if total > cap {
            return Err(bad("directory too large"));
        }
        let mut out = Vec::with_capacity(total as usize);
        for &(at, len) in extents {
            if at == ZEROS {
                out.resize(out.len() + len as usize, 0);
            } else {
                out.extend_from_slice(&self.read_at(at, len)?);
            }
        }
        Ok(out)
    }

    fn dir(&mut self, prefix: &str, part: u16, lbn: u32, depth: usize) -> Result<()> {
        let key = self.locate(part, lbn)?;
        if depth > MAX_DEPTH || !self.seen.insert(key) {
            return Err(bad("directory loop or excessive nesting"));
        }
        let extents = self.file_entry(part, lbn)?.extents;
        let data = self.read_extents(&extents, MAX_DIR_BYTES)?;
        let mut p = 0usize;
        while p + 38 <= data.len() {
            let fid = &data[p..];
            if tag(fid) != Some(TAG_FID) {
                return Err(bad("bad directory record"));
            }
            let chars = fid[18];
            let name_len = fid[19] as usize;
            let iu_len = u16_at(fid, 36) as usize;
            let size = (38 + iu_len + name_len + 3) & !3;
            if p + 38 + iu_len + name_len > data.len() {
                return Err(bad("bad directory record"));
            }
            let (icb_lbn, icb_part) = (u32_at(fid, 24), u16_at(fid, 28));
            let name = cs0(&fid[38 + iu_len..38 + iu_len + name_len]);
            p += size;
            // Deleted entries and the parent link are not shown.
            if chars & 0x0C != 0 || name.is_empty() || name.contains(['/', '\0']) {
                continue;
            }
            let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            let FileInfo { file_type, size, modified: mtime, extents } = self.file_entry(icb_part, icb_lbn)?;
            if chars & 0x02 != 0 || file_type == 4 {
                let mut e = Entry::new(self.entries.len(), &path, EntryKind::Dir);
                e.modified = mtime;
                self.entries.push(e);
                self.extents.push(Vec::new());
                self.dir(&path, icb_part, icb_lbn, depth + 1)?;
            } else {
                for &(at, len) in &extents {
                    if at != ZEROS && at.checked_add(len).is_none_or(|end| end > self.len) {
                        return Err(bad("file data past the end of the image"));
                    }
                }
                let kind = if file_type == 12 { EntryKind::Link } else { EntryKind::File };
                let mut e = Entry::new(self.entries.len(), &path, kind);
                e.size = Some(size);
                e.modified = mtime;
                self.entries.push(e);
                self.extents.push(extents);
            }
            if self.entries.len() > MAX_ENTRIES {
                return Err(Error::Unsupported("too many files in the image".into()));
            }
        }
        Ok(())
    }
}

/// Reads the UDF tree of an image: entries and, per entry, the byte ranges
/// of its data ([`ZEROS`] for sparse ranges).
pub(crate) fn read<R: Read + Seek>(r: &mut R, len: u64) -> Result<Tree> {
    // Anchor at sector 256, for each plausible sector size.
    let mut found = None;
    for bs in [2048u64, 512, 4096] {
        let at = 256 * bs;
        if at + 512 > len {
            continue;
        }
        let mut a = vec![0u8; 512];
        r.seek(SeekFrom::Start(at))?;
        r.read_exact(&mut a).map_err(Error::from_read)?;
        if tag(&a) == Some(TAG_AVDP) {
            found = Some((bs, u64::from(u32_at(&a, 16)), u64::from(u32_at(&a, 20))));
            break;
        }
    }
    let (bs, vds_len, vds_at) = found.ok_or_else(|| bad("no anchor"))?;
    let mut u = Udf { r, len, bs, maps: Vec::new(), entries: Vec::new(), extents: Vec::new(), seen: HashSet::new() };

    // Volume descriptor sequence: partitions and the logical volume.
    let mut partitions: Vec<(u16, u64)> = Vec::new();
    let mut lvd = None;
    for i in 0..(vds_len / bs).min(256) {
        let d = u.read_at((vds_at + i) * bs, bs)?;
        match tag(&d) {
            Some(TAG_PARTITION) => partitions.push((u16_at(&d, 22), u64::from(u32_at(&d, 188)) * bs)),
            Some(TAG_LOGICAL_VOLUME) if lvd.is_none() => lvd = Some(d),
            Some(TAG_TERMINATOR) | None => break,
            _ => {}
        }
    }
    let lvd = lvd.ok_or_else(|| bad("no logical volume"))?;
    if u64::from(u32_at(&lvd, 212)) != bs {
        return Err(Error::Unsupported("UDF block size differs from the sector size".into()));
    }
    let start_of = |num: u16| partitions.iter().find(|p| p.0 == num).map(|p| p.1).ok_or_else(|| bad("missing partition"));
    let (n_maps, table_len) = (u32_at(&lvd, 268) as usize, u32_at(&lvd, 264) as usize);
    let table = lvd.get(440..440 + table_len).ok_or_else(|| bad("bad partition maps"))?;
    let mut metadata = Vec::new();
    let mut p = 0usize;
    for index in 0..n_maps {
        let (kind, mlen) = (*table.get(p).ok_or_else(|| bad("bad partition maps"))?, *table.get(p + 1).unwrap_or(&0) as usize);
        let m = table.get(p..p + mlen).filter(|_| mlen >= 6).ok_or_else(|| bad("bad partition maps"))?;
        match kind {
            1 => u.maps.push(Map::Physical { start: start_of(u16_at(m, 4))? }),
            2 if mlen >= 64 => {
                let ident = &m[5..28];
                let number = u16_at(m, 38);
                if ident.starts_with(b"*UDF Metadata Partition") {
                    // Resolved below, once the physical maps are known.
                    metadata.push((index, u32_at(m, 40)));
                    u.maps.push(Map::Physical { start: start_of(number)? });
                } else if ident.starts_with(b"*UDF Sparable Partition") {
                    u.maps.push(Map::Physical { start: start_of(number)? });
                } else {
                    return Err(Error::Unsupported("UDF virtual partition (CD-R)".into()));
                }
            }
            _ => return Err(bad("unknown partition map")),
        }
        p += mlen;
    }
    // The metadata file sits in the physical partition its map names (for
    // now, map `index` still points there).
    for (index, file_lbn) in metadata {
        let extents = u.file_entry(index as u16, file_lbn)?.extents;
        if extents.iter().any(|e| e.0 == ZEROS) {
            return Err(bad("sparse metadata file"));
        }
        u.maps[index] = Map::Metadata { extents };
    }

    // File set descriptor, then the root directory.
    let (fsd_lbn, fsd_part) = (u32_at(&lvd, 252), u16_at(&lvd, 256));
    let at = u.locate(fsd_part, fsd_lbn)?;
    let fsd = u.read_at(at, bs)?;
    if tag(&fsd) != Some(TAG_FSD) {
        return Err(bad("bad file set descriptor"));
    }
    let (root_lbn, root_part) = (u32_at(&fsd, 404), u16_at(&fsd, 408));
    u.dir("", root_part, root_lbn, 0)?;
    Ok((u.entries, u.extents))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_names_and_times() {
        assert_eq!(cs0(b"\x08abc"), "abc");
        assert_eq!(cs0(b"\x10\x00\xf1\x00o"), "ño");
        assert_eq!(cs0(b""), "");
        // 2024-07-07 21:11:37 at UTC+2 (type 1, offset 120 minutes).
        let t = [0x78, 0x10, 0xE8, 0x07, 7, 7, 21, 11, 37, 0, 0, 0];
        assert_eq!(timestamp(&t), Some(Timestamp::Utc(crate::time::secs_from_civil(2024, 7, 7, 19, 11, 37))));
    }
}
