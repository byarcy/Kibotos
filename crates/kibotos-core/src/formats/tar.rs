//! tar, plain or compressed. Tar has no central index: listing a plain tar
//! seeks from header to header, but a compressed tar must be decompressed
//! from start to end to be listed. This affects every archive manager.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use tar::EntryType;

use super::open_file;
use crate::pump::{Pump, Selection};
use crate::{ArchiveReader, Codec, Control, Entry, EntryId, EntryKind, Error, ExtractSink, Format, Result, Timestamp};

pub struct TarBackend {
    path: PathBuf,
    codec: Option<Codec>,
    entries: Vec<Entry>,
    /// Entry id -> position in the tar entry stream.
    ordinals: Vec<u32>,
    len: u64,
}

fn map_tar(e: io::Error) -> Error {
    match e.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied | io::ErrorKind::OutOfMemory => Error::Io(e),
        _ => Error::Corrupt(e.to_string()),
    }
}

/// Visits tar entries in order until `f` returns `false`.
fn walk<R: Read>(
    entries: tar::Entries<'_, R>,
    mut f: impl FnMut(u32, &mut tar::Entry<'_, R>) -> Result<bool>,
) -> Result<()> {
    for (ordinal, item) in entries.enumerate() {
        let mut item = item.map_err(map_tar)?;
        if !f(ordinal as u32, &mut item)? {
            break;
        }
    }
    Ok(())
}

impl TarBackend {
    pub fn open(path: &Path, codec: Option<Codec>) -> Result<TarBackend> {
        let mut b = TarBackend { path: path.to_owned(), codec, entries: Vec::new(), ordinals: Vec::new(), len: 0 };
        let (file, len) = open_file(path)?;
        b.len = len;
        let mut list = |ordinal: u32, item: &mut dyn TarItem| -> Result<bool> {
            if let Some(e) = item.to_entry(b.entries.len(), codec.is_none()) {
                b.entries.push(e);
                b.ordinals.push(ordinal);
            }
            Ok(true)
        };
        match codec {
            None => {
                let mut ar = tar::Archive::new(file);
                walk(ar.entries_with_seek().map_err(map_tar)?, |o, it| list(o, it))?;
            }
            Some(c) => {
                let mut ar = tar::Archive::new(c.file_decoder(file)?);
                walk(ar.entries().map_err(map_tar)?, |o, it| list(o, it))?;
            }
        }
        Ok(b)
    }
}

/// Object-safe view of `tar::Entry<R>` for any reader type.
trait TarItem: Read {
    fn to_entry(&self, id: usize, plain: bool) -> Option<Entry>;
}

impl<R: Read> TarItem for tar::Entry<'_, R> {
    fn to_entry(&self, id: usize, plain: bool) -> Option<Entry> {
        let header = self.header();
        let kind = match header.entry_type() {
            EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse => EntryKind::File,
            EntryType::Directory => EntryKind::Dir,
            EntryType::Symlink | EntryType::Link => EntryKind::Link,
            // Devices, FIFOs and metadata records are not files.
            _ => return None,
        };
        let path = String::from_utf8_lossy(&self.path_bytes()).into_owned();
        let mut e = Entry::new(id, &path, kind);
        if kind == EntryKind::File {
            e.size = Some(self.size());
            e.packed = plain.then(|| self.size());
        }
        e.modified = header.mtime().ok().map(|t| Timestamp::Utc(t as i64));
        if kind == EntryKind::Link {
            e.link_target = self.link_name_bytes().map(|b| String::from_utf8_lossy(&b).into());
        }
        Some(e)
    }
}

impl ArchiveReader for TarBackend {
    fn format(&self) -> Format {
        Format::Tar(self.codec)
    }

    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn set_password(&mut self, _password: Option<&str>) {}

    fn extract(&mut self, ids: &[EntryId], sink: &mut dyn ExtractSink, ctl: &Control) -> Result<()> {
        let mut sel = Selection::new(ids, &self.entries, ctl)?;
        if sel.remaining == 0 {
            return Ok(());
        }
        let mut pump = Pump::new(ctl, self.len);
        // Ordinal -> entry id for the requested entries.
        let mut by_ordinal = std::collections::HashMap::new();
        for (id, &ord) in self.ordinals.iter().enumerate() {
            if sel.contains(id) {
                by_ordinal.insert(ord, id);
            }
        }
        let entries = &self.entries;
        let mut visit = |ordinal: u32, item: &mut dyn TarItem| -> Result<bool> {
            ctl.check()?;
            let Some(&id) = by_ordinal.get(&ordinal) else { return Ok(true) };
            pump.run(&entries[id], item, sink, &map_tar)?;
            Ok(!sel.done(id))
        };
        let (file, _) = open_file(&self.path)?;
        match self.codec {
            None => {
                let mut ar = tar::Archive::new(file);
                walk(ar.entries_with_seek().map_err(map_tar)?, |o, it| visit(o, it))
            }
            Some(c) => {
                let mut ar = tar::Archive::new(c.file_decoder(file)?);
                walk(ar.entries().map_err(map_tar)?, |o, it| visit(o, it))
            }
        }
    }
}
