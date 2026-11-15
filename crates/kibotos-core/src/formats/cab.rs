//! Windows cabinet files (`.cab`), also used inside MSI packages.
//!
//! Files in a cabinet live in "folders": one compressed stream each (stored,
//! MSZIP or LZX). Extraction decodes every needed folder once, from start
//! to end, handing out the requested files as their bytes go by, so a
//! cabinet with thousands of files is not decoded thousands of times.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::open_file;
use crate::pump::{Pump, Selection};
use crate::{ArchiveReader, Control, Entry, EntryId, EntryKind, Error, ExtractSink, Format, Result, Timestamp};

const SIGNATURE: &[u8; 4] = b"MSCF";
const FLAG_PREV: u16 = 1;
const FLAG_NEXT: u16 = 2;
const FLAG_RESERVE: u16 = 4;
const ATTR_UTF8: u16 = 0x80;
/// Folder indexes that mean "continued from / into another cabinet".
const FOLDER_CONTINUED: u16 = 0xFFFD;
/// Largest uncompressed data block (the format's limit is 32 KiB).
const MAX_BLOCK: usize = 32 * 1024;

pub(crate) struct CabFile {
    pub name: String,
    pub size: u32,
    offset: u32,
    folder: u16,
    date: u16,
    time: u16,
}

impl CabFile {
    pub fn modified(&self) -> Option<Timestamp> {
        crate::time::dos_to_secs(self.date, self.time).map(Timestamp::Local)
    }

    /// Whether the file's data is in another cabinet of a set.
    pub fn in_other_cabinet(&self) -> bool {
        self.folder >= FOLDER_CONTINUED
    }
}

struct CabFolder {
    data_start: u32,
    blocks: u16,
    method: u16,
}

/// The directory of one cabinet.
pub(crate) struct Cab {
    folders: Vec<CabFolder>,
    pub files: Vec<CabFile>,
    data_reserve: u8,
}

fn bad(msg: &str) -> Error {
    Error::Corrupt(format!("cabinet: {msg}"))
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// Reads a NUL-terminated string of at most 256 bytes.
fn read_cstr<R: Read>(r: &mut R) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut b = [0u8];
    loop {
        r.read_exact(&mut b).map_err(Error::from_read)?;
        if b[0] == 0 {
            return Ok(out);
        }
        out.push(b[0]);
        if out.len() > 1024 {
            return Err(bad("name too long"));
        }
    }
}

/// File names without the UTF-8 flag use the creator's code page; Latin-1
/// is the closest guess that never fails.
fn decode_name(raw: &[u8], utf8: bool) -> String {
    match std::str::from_utf8(raw) {
        Ok(s) if utf8 || s.is_ascii() => s.to_owned(),
        _ => raw.iter().map(|&b| b as char).collect(),
    }
}

impl Cab {
    /// Reads the header, folders and files of a cabinet starting at the
    /// reader's position 0.
    pub fn parse<R: Read + Seek>(r: &mut R) -> Result<Cab> {
        r.seek(SeekFrom::Start(0))?;
        let mut h = [0u8; 36];
        r.read_exact(&mut h).map_err(|_| bad("truncated header"))?;
        if &h[..4] != SIGNATURE {
            return Err(Error::UnknownFormat);
        }
        let files_at = u32_at(&h, 16);
        let (n_folders, n_files, flags) = (u16_at(&h, 26), u16_at(&h, 28), u16_at(&h, 30));
        let (mut folder_reserve, mut data_reserve) = (0u8, 0u8);
        if flags & FLAG_RESERVE != 0 {
            let mut x = [0u8; 4];
            r.read_exact(&mut x).map_err(Error::from_read)?;
            let header_reserve = u16_at(&x, 0) as i64;
            folder_reserve = x[2];
            data_reserve = x[3];
            r.seek(SeekFrom::Current(header_reserve))?;
        }
        if flags & FLAG_PREV != 0 {
            read_cstr(r)?;
            read_cstr(r)?;
        }
        if flags & FLAG_NEXT != 0 {
            read_cstr(r)?;
            read_cstr(r)?;
        }
        let mut folders = Vec::with_capacity(n_folders as usize);
        for _ in 0..n_folders {
            let mut f = [0u8; 8];
            r.read_exact(&mut f).map_err(Error::from_read)?;
            folders.push(CabFolder { data_start: u32_at(&f, 0), blocks: u16_at(&f, 4), method: u16_at(&f, 6) });
            r.seek(SeekFrom::Current(i64::from(folder_reserve)))?;
        }
        r.seek(SeekFrom::Start(u64::from(files_at)))?;
        let mut files = Vec::with_capacity(n_files as usize);
        for _ in 0..n_files {
            let mut f = [0u8; 16];
            r.read_exact(&mut f).map_err(Error::from_read)?;
            let attribs = u16_at(&f, 14);
            let raw = read_cstr(r)?;
            files.push(CabFile {
                name: decode_name(&raw, attribs & ATTR_UTF8 != 0),
                size: u32_at(&f, 0),
                offset: u32_at(&f, 4),
                folder: u16_at(&f, 8),
                date: u16_at(&f, 10),
                time: u16_at(&f, 12),
            });
        }
        Ok(Cab { folders, files, data_reserve })
    }

    /// Streams the files at `wanted` (indexes into `files`) to `each`, in
    /// folder order. `each` receives the file index and a reader limited to
    /// the file's bytes.
    pub fn extract<R: Read + Seek>(
        &self,
        r: &mut R,
        wanted: &[usize],
        mut each: impl FnMut(usize, &mut dyn Read) -> Result<()>,
    ) -> Result<()> {
        let mut order: Vec<usize> = wanted.to_vec();
        order.sort_by_key(|&i| (self.files[i].folder, self.files[i].offset));
        let mut stream: Option<(u16, FolderStream)> = None;
        for i in order {
            let f = &self.files[i];
            if f.in_other_cabinet() {
                return Err(Error::Unsupported("file continued in another cabinet".into()));
            }
            let folder = self.folders.get(f.folder as usize).ok_or_else(|| bad("bad folder index"))?;
            let reuse = matches!(&stream, Some((n, s)) if *n == f.folder && s.pos <= u64::from(f.offset));
            if !reuse {
                stream = Some((f.folder, FolderStream::new(folder, self.data_reserve)?));
            }
            let (_, s) = stream.as_mut().unwrap();
            let skip = u64::from(f.offset) - s.pos;
            let skipped = io::copy(&mut Reading { s: &mut *s, r: &mut *r }.take(skip), &mut io::sink())?;
            if skipped != skip {
                return Err(bad("data ends early"));
            }
            let mut file = Reading { s: &mut *s, r: &mut *r }.take(u64::from(f.size));
            each(i, &mut file)?;
        }
        Ok(())
    }
}

/// The decoded bytes of one folder, block by block.
struct FolderStream {
    next_block: u64,
    blocks_left: u16,
    method: u16,
    data_reserve: u8,
    buf: Vec<u8>,
    at: usize,
    /// Uncompressed position of `buf[at]` within the folder.
    pos: u64,
    inflate: flate2::Decompress,
    history: Vec<u8>,
    lzx: Option<lzxd::Lzxd>,
}

impl FolderStream {
    fn new(folder: &CabFolder, data_reserve: u8) -> Result<FolderStream> {
        let lzx = match folder.method & 0x0F {
            0 | 1 => None,
            3 => {
                use lzxd::WindowSize as W;
                let window = match (folder.method >> 8) & 0x1F {
                    15 => W::KB32,
                    16 => W::KB64,
                    17 => W::KB128,
                    18 => W::KB256,
                    19 => W::KB512,
                    20 => W::MB1,
                    21 => W::MB2,
                    _ => return Err(bad("bad LZX window")),
                };
                Some(lzxd::Lzxd::new(window))
            }
            2 => return Err(Error::Unsupported("Quantum-compressed cabinet".into())),
            m => return Err(Error::Unsupported(format!("cabinet compression {m}"))),
        };
        Ok(FolderStream {
            next_block: u64::from(folder.data_start),
            blocks_left: folder.blocks,
            method: folder.method & 0x0F,
            data_reserve,
            buf: Vec::new(),
            at: 0,
            pos: 0,
            inflate: flate2::Decompress::new(false),
            history: Vec::new(),
            lzx,
        })
    }

    /// Decodes the next data block into `buf`; false at the end of the folder.
    fn load<R: Read + Seek>(&mut self, r: &mut R) -> io::Result<bool> {
        if self.blocks_left == 0 {
            return Ok(false);
        }
        self.blocks_left -= 1;
        r.seek(SeekFrom::Start(self.next_block))?;
        let mut h = [0u8; 8];
        r.read_exact(&mut h)?;
        let (packed, size) = (u16_at(&h, 4) as usize, u16_at(&h, 6) as usize);
        if size == 0 || size > MAX_BLOCK || packed > MAX_BLOCK + 6144 {
            // Size 0 means the block continues in the next cabinet.
            return Err(io::Error::new(io::ErrorKind::InvalidData, "cabinet block spans cabinets or is damaged"));
        }
        r.seek(SeekFrom::Current(i64::from(self.data_reserve)))?;
        let mut data = vec![0u8; packed];
        r.read_exact(&mut data)?;
        self.next_block += 8 + u64::from(self.data_reserve) + packed as u64;
        self.buf = match self.method {
            0 => data,
            1 => self.mszip(&data, size)?,
            _ => {
                let lzx = self.lzx.as_mut().unwrap();
                lzx.decompress_next(&data, size)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("LZX: {e}")))?
                    .to_vec()
            }
        };
        if self.buf.len() != size {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "cabinet block has the wrong size"));
        }
        self.at = 0;
        Ok(true)
    }

    /// MSZIP: each block is a raw deflate stream that may refer back into
    /// the previous block's output, fed first as a stored block.
    fn mszip(&mut self, data: &[u8], size: usize) -> io::Result<Vec<u8>> {
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "MSZIP block is damaged");
        let body = data.strip_prefix(b"CK").ok_or_else(bad)?;
        self.inflate.reset(false);
        if !self.history.is_empty() {
            let n = self.history.len() as u16;
            let mut pre = Vec::with_capacity(5 + self.history.len());
            pre.push(0);
            pre.extend_from_slice(&n.to_le_bytes());
            pre.extend_from_slice(&(!n).to_le_bytes());
            pre.extend_from_slice(&self.history);
            let mut discard = Vec::with_capacity(self.history.len());
            self.inflate.decompress_vec(&pre, &mut discard, flate2::FlushDecompress::Sync).map_err(|_| bad())?;
        }
        let mut out = Vec::with_capacity(size);
        self.inflate.decompress_vec(body, &mut out, flate2::FlushDecompress::Finish).map_err(|_| bad())?;
        self.history.clear();
        self.history.extend_from_slice(&out);
        Ok(out)
    }
}

/// A folder stream paired with the reader it decodes from.
struct Reading<'a, R> {
    s: &'a mut FolderStream,
    r: &'a mut R,
}

impl<R: Read + Seek> Read for Reading<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.s.at == self.s.buf.len() && !self.s.load(self.r)? {
            return Ok(0);
        }
        let n = (self.s.buf.len() - self.s.at).min(buf.len());
        buf[..n].copy_from_slice(&self.s.buf[self.s.at..self.s.at + n]);
        self.s.at += n;
        self.s.pos += n as u64;
        Ok(n)
    }
}

/// A standalone `.cab` file.
pub struct CabBackend {
    path: PathBuf,
    cab: Cab,
    entries: Vec<Entry>,
    len: u64,
}

impl CabBackend {
    pub fn open(path: &Path) -> Result<CabBackend> {
        let (mut file, len) = open_file(path)?;
        let cab = Cab::parse(&mut file)?;
        let entries = cab
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let mut e = Entry::new(i, &f.name, EntryKind::File);
                e.size = Some(u64::from(f.size));
                e.modified = f.modified();
                e
            })
            .collect();
        Ok(CabBackend { path: path.to_owned(), cab, entries, len })
    }
}

impl ArchiveReader for CabBackend {
    fn format(&self) -> Format {
        Format::Cab
    }

    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn set_password(&mut self, _password: Option<&str>) {}

    fn extract(&mut self, ids: &[EntryId], sink: &mut dyn ExtractSink, ctl: &Control) -> Result<()> {
        let sel = Selection::new(ids, &self.entries, ctl)?;
        let wanted: Vec<usize> = (0..self.entries.len()).filter(|&i| sel.contains(i)).collect();
        if wanted.is_empty() {
            return Ok(());
        }
        let mut pump = Pump::new(ctl, self.len);
        let (mut file, _) = open_file(&self.path)?;
        let entries = &self.entries;
        self.cab.extract(&mut file, &wanted, |i, data| pump.run(&entries[i], data, sink, &Error::from_read))
    }
}
