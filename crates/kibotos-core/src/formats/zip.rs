//! ZIP and ZIP-based files (jar, apk, cbz, ...). Opening reads only the
//! central directory; entry metadata never touches local headers.

use std::io::{self, BufReader};
use std::path::Path;

use zip::result::ZipError;
use zip::{ZipArchive, ZipReadOptions};

use super::open_file;
use crate::volume::Source;
use crate::pump::{Pump, Selection};
use crate::{ArchiveReader, Control, Entry, EntryId, EntryKind, Error, ExtractSink, Format, Result, Timestamp};

pub struct ZipBackend {
    archive: ZipArchive<BufReader<Source>>,
    entries: Vec<Entry>,
    password: Option<String>,
    len: u64,
}

impl ZipBackend {
    pub fn open(path: &Path, password: Option<&str>) -> Result<ZipBackend> {
        let (reader, len) = open_file(path)?;
        let archive = ZipArchive::new(reader).map_err(map_err)?;
        let mut entries = Vec::with_capacity(archive.len());
        for i in 0..archive.len() {
            let ze = archive.by_index_data(i).map_err(map_err)?;
            let name = match ze.name() {
                Ok(n) => n,
                Err(_) => String::from_utf8_lossy(ze.name_raw()).into_owned().into(),
            };
            let kind = if ze.is_dir() {
                EntryKind::Dir
            } else if ze.is_symlink() {
                EntryKind::Link
            } else {
                EntryKind::File
            };
            let mut e = Entry::new(i, &name, kind);
            if kind != EntryKind::Dir {
                e.size = Some(ze.size());
                e.packed = Some(ze.compressed_size());
                e.crc = Some(ze.crc32());
            }
            e.encrypted = ze.encrypted();
            e.modified = ze
                .last_modified()
                .and_then(|dt| crate::time::dos_to_secs(dt.datepart(), dt.timepart()))
                .map(Timestamp::Local);
            entries.push(e);
        }
        Ok(ZipBackend { archive, entries, password: password.map(str::to_owned), len })
    }
}

impl ArchiveReader for ZipBackend {
    fn format(&self) -> Format {
        Format::Zip
    }

    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn set_password(&mut self, password: Option<&str>) {
        self.password = password.map(str::to_owned);
    }

    fn extract(&mut self, ids: &[EntryId], sink: &mut dyn ExtractSink, ctl: &Control) -> Result<()> {
        let mut sel = Selection::new(ids, &self.entries, ctl)?;
        let mut pump = Pump::new(ctl, self.len);
        let password = self.password.as_deref().map(str::as_bytes);
        for i in 0..self.entries.len() {
            if sel.remaining == 0 {
                break;
            }
            if !sel.contains(i) {
                continue;
            }
            let entry = &self.entries[i];
            if entry.kind == EntryKind::File {
                let encrypted = entry.encrypted;
                let opts = ZipReadOptions::new().password(if encrypted { password } else { None });
                let mut zf = self.archive.by_index_with_options(i, opts).map_err(map_err)?;
                pump.run(entry, &mut zf, sink, &|e| {
                    // ZipCrypto has a weak password check: a wrong password
                    // often passes it and fails later as a CRC error.
                    if encrypted && e.kind() == io::ErrorKind::InvalidData {
                        Error::WrongPassword
                    } else {
                        Error::from_read(e)
                    }
                })?;
            } else {
                pump.run(entry, &mut io::empty(), sink, &Error::from_read)?;
            }
            sel.done(i);
        }
        Ok(())
    }
}

fn map_err(e: ZipError) -> Error {
    match e {
        ZipError::Io(e) => Error::from_read(e),
        ZipError::UnsupportedArchive(ZipError::PASSWORD_REQUIRED) => Error::PasswordRequired,
        ZipError::InvalidPassword => Error::WrongPassword,
        ZipError::UnsupportedArchive(s) => Error::Unsupported(s.to_string()),
        ZipError::CompressionMethodNotSupported(m) => Error::Unsupported(format!("ZIP compression method {m}")),
        ZipError::InvalidArchive(s) => Error::Corrupt(s.into_owned()),
        other => Error::Corrupt(other.to_string()),
    }
}
