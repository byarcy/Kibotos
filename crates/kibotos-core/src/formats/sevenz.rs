//! 7z. Opening reads only the header. Solid blocks must be decoded in
//! order, so extraction walks blocks and skips every block that contains
//! no requested entry.

use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

use sevenz_rust2::{Archive, ArchiveEntry, ArchiveReader as SzReader, EntryDecision, Password};

use super::open_file;
use crate::volume::Source;
use crate::pump::{Pump, Selection};
use crate::{ArchiveReader, Control, Entry, EntryId, EntryKind, Error, ExtractSink, Format, Result, Timestamp};

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const UNIX_EXTENSION: u32 = 0x8000;
const S_IFMT: u32 = 0o170000;
const S_IFLNK: u32 = 0o120000;

pub struct SevenZipBackend {
    path: PathBuf,
    reader: SzReader<BufReader<Source>>,
    entries: Vec<Entry>,
    /// 7z file index -> entry id (anti-items have no entry).
    to_entry: Vec<Option<EntryId>>,
    len: u64,
}

fn password(p: Option<&str>) -> Password {
    p.map(Password::from).unwrap_or_else(Password::empty)
}

impl SevenZipBackend {
    pub fn open(path: &Path, pw: Option<&str>) -> Result<SevenZipBackend> {
        let (mut file, len) = open_file(path)?;
        let archive = Archive::read(&mut file, &password(pw)).map_err(map_err)?;
        let mut entries = Vec::with_capacity(archive.files.len());
        let mut to_entry = Vec::with_capacity(archive.files.len());
        for (i, f) in archive.files.iter().enumerate() {
            if f.is_anti_item {
                to_entry.push(None);
                continue;
            }
            let id = entries.len();
            to_entry.push(Some(id as EntryId));
            entries.push(convert(id, f, archive.is_file_encrypted(i)));
        }
        let reader = SzReader::from_archive(archive, file, password(pw));
        Ok(SevenZipBackend { path: path.to_owned(), reader, entries, to_entry, len })
    }
}

fn convert(id: usize, f: &ArchiveEntry, encrypted: bool) -> Entry {
    let attr = f.windows_attributes;
    let unix_link = attr & UNIX_EXTENSION != 0 && (attr >> 16) & S_IFMT == S_IFLNK;
    let kind = if f.is_directory {
        EntryKind::Dir
    } else if f.has_windows_attributes && (attr & FILE_ATTRIBUTE_REPARSE_POINT != 0 || unix_link) {
        EntryKind::Link
    } else {
        EntryKind::File
    };
    let mut e = Entry::new(id, &f.name, kind);
    if kind != EntryKind::Dir {
        e.size = Some(f.size);
        e.packed = (f.compressed_size > 0).then_some(f.compressed_size);
        e.crc = f.has_crc.then_some(f.crc as u32);
    }
    if f.has_last_modified_date {
        e.modified = crate::time::filetime_to_unix(u64::from(f.last_modified_date)).map(Timestamp::Utc);
    }
    e.encrypted = encrypted;
    e
}

impl ArchiveReader for SevenZipBackend {
    fn format(&self) -> Format {
        Format::SevenZip
    }

    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn set_password(&mut self, pw: Option<&str>) {
        // The reader owns the password; rebuild it around the parsed header.
        if let Ok((file, _)) = open_file(&self.path) {
            let archive = self.reader.archive().clone();
            self.reader = SzReader::from_archive(archive, file, password(pw));
        }
    }

    fn extract(&mut self, ids: &[EntryId], sink: &mut dyn ExtractSink, ctl: &Control) -> Result<()> {
        let mut sel = Selection::new(ids, &self.entries, ctl)?;
        if sel.remaining == 0 {
            return Ok(());
        }
        let mut pump = Pump::new(ctl, self.len);

        // How many requested entries each block holds, so blocks with none
        // are skipped without decoding.
        let archive = self.reader.archive();
        let mut block_wanted = vec![0usize; archive.blocks.len()];
        for (fi, block) in archive.stream_map.file_block_index.iter().enumerate() {
            if let (Some(b), Some(id)) = (block, self.to_entry[fi]) {
                if sel.contains(id as usize) {
                    block_wanted[*b] += 1;
                }
            }
        }
        let block_of: Vec<Option<usize>> = archive.stream_map.file_block_index.clone();
        let files_base = archive.files.as_ptr() as usize;
        let file_count = archive.files.len();

        let entries = &self.entries;
        let to_entry = &self.to_entry;
        let mut failure: Option<Error> = None;

        let result = self.reader.for_each_entries_with_decision(|f, data| {
            // The reader passes references into `archive.files`, so the
            // file index is the element offset.
            let offset = (f as *const ArchiveEntry as usize).wrapping_sub(files_base);
            let fi = offset / std::mem::size_of::<ArchiveEntry>();
            if fi >= file_count {
                failure = Some(Error::Corrupt("unexpected 7z entry".into()));
                return Ok(EntryDecision::Stop);
            }
            if ctl.is_cancelled() {
                failure = Some(Error::Cancelled);
                return Ok(EntryDecision::Stop);
            }
            let block = block_of[fi];
            let id = match to_entry[fi] {
                Some(id) if sel.contains(id as usize) => id as usize,
                _ => {
                    let none_left = block.is_some_and(|b| block_wanted[b] == 0);
                    return Ok(if none_left { EntryDecision::SkipBlock } else { EntryDecision::Continue });
                }
            };
            let entry = &entries[id];
            let encrypted = entry.encrypted;
            let res = pump.run(entry, data, sink, &|e| {
                if encrypted && e.kind() == io::ErrorKind::InvalidData {
                    Error::WrongPassword
                } else {
                    Error::from_read(e)
                }
            });
            if let Some(b) = block {
                block_wanted[b] -= 1;
            }
            match res {
                Err(e) => {
                    failure = Some(e);
                    Ok(EntryDecision::Stop)
                }
                Ok(()) if sel.done(id) => Ok(EntryDecision::Stop),
                Ok(()) => Ok(EntryDecision::Continue),
            }
        });
        if let Some(e) = failure {
            return Err(e);
        }
        result.map_err(map_err)
    }
}

fn map_err(e: sevenz_rust2::Error) -> Error {
    use sevenz_rust2::Error as E;
    match e {
        E::PasswordRequired => Error::PasswordRequired,
        E::MaybeBadPassword(_) => Error::WrongPassword,
        E::Io(e, _) => Error::from_read(e),
        E::FileOpen(e, _) => Error::Io(e),
        E::UnsupportedCompressionMethod(m) => Error::Unsupported(format!("7z method {m}")),
        E::Unsupported(s) => Error::Unsupported(s.into_owned()),
        E::UnsupportedVersion { major, minor } => Error::Unsupported(format!("7z version {major}.{minor}")),
        E::ExternalUnsupported => Error::Unsupported("external 7z header".into()),
        E::MaxMemLimited { .. } => Error::Unsupported("7z block needs too much memory".into()),
        other => Error::Corrupt(other.to_string()),
    }
}
