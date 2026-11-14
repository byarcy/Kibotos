//! A single compressed file (`.gz`, `.bz2`, `.xz`, `.zst` that is not a tar).
//! Shown as an archive with one entry named after the file.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::open_file;
use crate::pump::{Pump, Selection};
use crate::{ArchiveReader, Codec, Control, Entry, EntryId, EntryKind, Error, ExtractSink, Format, Result, Timestamp};

pub struct SingleBackend {
    path: PathBuf,
    codec: Codec,
    entries: Vec<Entry>,
    len: u64,
}

/// "notes.txt.gz" -> "notes.txt", "image.svgz" -> "image.svg".
fn inner_name(path: &Path, codec: Codec) -> String {
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (stem, ext) = match file_name.rfind('.') {
        Some(i) if i > 0 => (&file_name[..i], file_name[i + 1..].to_ascii_lowercase()),
        _ => return format!("{file_name}.out"),
    };
    if codec.extensions().contains(&ext.as_str()) {
        stem.to_owned()
    } else if ext == "svgz" {
        format!("{stem}.svg")
    } else {
        stem.to_owned()
    }
}

/// Uncompressed size, when it can be read cheaply without decompressing.
fn known_size(file: &mut File, codec: Codec) -> Option<u64> {
    match codec {
        Codec::Xz => lzma_rust2::xz_uncompressed_size(&mut *file).ok(),
        Codec::Zstd => {
            let mut head = [0u8; 18];
            file.seek(SeekFrom::Start(0)).ok()?;
            let n = file.read(&mut head).ok()?;
            zstd::zstd_safe::get_frame_content_size(&head[..n]).ok().flatten()
        }
        // gzip stores the size modulo 4 GiB of the *last* member only, and
        // bzip2 stores nothing: both are unknown until decompressed.
        Codec::Gzip | Codec::Bzip2 => None,
    }
}

impl SingleBackend {
    pub fn open(path: &Path, codec: Codec) -> Result<SingleBackend> {
        let mut file = File::open(path)?;
        let meta = file.metadata()?;
        let len = meta.len();
        let mut e = Entry::new(0, &inner_name(path, codec), EntryKind::File);
        e.size = known_size(&mut file, codec);
        e.packed = Some(len);
        e.modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| Timestamp::Utc(d.as_secs() as i64));
        Ok(SingleBackend { path: path.to_owned(), codec, entries: vec![e], len })
    }
}

impl ArchiveReader for SingleBackend {
    fn format(&self) -> Format {
        Format::Compressed(self.codec)
    }

    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn set_password(&mut self, _password: Option<&str>) {}

    fn extract(&mut self, ids: &[EntryId], sink: &mut dyn ExtractSink, ctl: &Control) -> Result<()> {
        let sel = Selection::new(ids, &self.entries, ctl)?;
        if !sel.contains(0) {
            return Ok(());
        }
        let mut pump = Pump::new(ctl, self.len);
        let (file, _) = open_file(&self.path)?;
        let mut dec = self.codec.file_decoder(file).map_err(Error::from_read)?;
        pump.run(&self.entries[0], &mut dec, sink, &Error::from_read)
    }
}
