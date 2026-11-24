//! SHA-256 of entries, computed by decompressing them (archives store at
//! most a CRC-32, which only detects accidental damage).

use std::io::{self, Write};

use sha2::{Digest, Sha256};

use crate::{ArchiveReader, Control, Entry, EntryId, ExtractSink, Result};

struct HashSink {
    hasher: Sha256,
    out: Vec<(EntryId, [u8; 32])>,
}

struct Feed<'a>(&'a mut Sha256);

impl Write for Feed<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl ExtractSink for HashSink {
    fn begin(&mut self, entry: &Entry) -> Result<Option<Box<dyn Write + '_>>> {
        if entry.is_dir() {
            return Ok(None);
        }
        self.hasher = Sha256::new();
        Ok(Some(Box::new(Feed(&mut self.hasher))))
    }

    fn finish(&mut self, entry: &Entry) -> Result<()> {
        let digest: [u8; 32] = std::mem::take(&mut self.hasher).finalize().into();
        self.out.push((entry.id, digest));
        Ok(())
    }
}

/// SHA-256 of the selected files (folders and links are left out), in
/// archive order.
pub fn sha256(reader: &mut dyn ArchiveReader, ids: &[EntryId], ctl: &Control) -> Result<Vec<(EntryId, [u8; 32])>> {
    let files: Vec<EntryId> = ids
        .iter()
        .copied()
        .filter(|&id| reader.entries().get(id as usize).is_some_and(|e| e.kind == crate::EntryKind::File))
        .collect();
    let mut sink = HashSink { hasher: Sha256::new(), out: Vec::with_capacity(files.len()) };
    reader.extract(&files, &mut sink, ctl)?;
    Ok(sink.out)
}
