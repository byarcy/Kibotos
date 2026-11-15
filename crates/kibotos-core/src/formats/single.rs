//! A single compressed file (`.gz`, `.bz2`, `.xz`, `.zst` that is not a tar).
//! Shown as an archive with one entry named after the file.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::open_file;
use crate::volume::Source;
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
fn known_size(file: &mut Source, codec: Codec) -> Option<u64> {
    match codec {
        Codec::Xz => lzma_rust2::xz_uncompressed_size(&mut *file).ok(),
        Codec::Zstd => {
            let mut head = [0u8; 18];
            file.seek(SeekFrom::Start(0)).ok()?;
            let n = file.read(&mut head).ok()?;
            zstd::zstd_safe::get_frame_content_size(&head[..n]).ok().flatten()
        }
        Codec::Lzma => {
            let mut head = [0u8; 13];
            file.seek(SeekFrom::Start(0)).ok()?;
            file.read_exact(&mut head).ok()?;
            let size = u64::from_le_bytes(head[5..13].try_into().ok()?);
            (size != u64::MAX).then_some(size)
        }
        Codec::Lzip => {
            // A one-member file ends with: CRC32, data size, member size.
            let len = file.seek(SeekFrom::End(0)).ok()?;
            let mut trailer = [0u8; 20];
            file.seek(SeekFrom::Start(len.checked_sub(20)?)).ok()?;
            file.read_exact(&mut trailer).ok()?;
            let member = u64::from_le_bytes(trailer[12..20].try_into().ok()?);
            (member == len).then(|| u64::from_le_bytes(trailer[4..12].try_into().unwrap()))
        }
        // gzip stores the size modulo 4 GiB of the *last* member only;
        // bzip2, .Z and lz4 frames (usually) store nothing.
        Codec::Gzip | Codec::Bzip2 | Codec::Compress | Codec::Lz4 => None,
    }
}

impl SingleBackend {
    pub fn open(path: &Path, codec: Codec) -> Result<SingleBackend> {
        let mut file = Source::open(path)?;
        let meta = std::fs::metadata(path)?;
        let len = file.len();
        // "notes.txt.gz.001" names the same entry as "notes.txt.gz".
        let shown = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) if file.is_split() => path.with_file_name(&n[..n.rfind('.').unwrap_or(n.len())]),
            _ => path.to_owned(),
        };
        let mut e = Entry::new(0, &inner_name(&shown, codec), EntryKind::File);
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
