//! The shared extraction loop. Every backend moves data through a [`Pump`],
//! so cancellation, progress and decompression-bomb limits behave the same
//! for all formats.

use std::io::{self, Read, Write};

use crate::{Control, Entry, EntryId, Error, ExtractSink, LimitKind, Result};

const CHUNK: usize = 256 * 1024;

/// Which entries of an archive were requested.
pub(crate) struct Selection {
    wanted: Vec<bool>,
    pub remaining: usize,
}

impl Selection {
    /// Builds the selection, sets progress totals and rejects requests whose
    /// declared size already exceeds the output limit.
    pub fn new(ids: &[EntryId], entries: &[Entry], ctl: &Control) -> Result<Selection> {
        let mut wanted = vec![false; entries.len()];
        let mut remaining = 0;
        let mut bytes = 0u64;
        for &id in ids {
            if let Some(w) = wanted.get_mut(id as usize) {
                if !*w {
                    *w = true;
                    remaining += 1;
                    bytes = bytes.saturating_add(entries[id as usize].size.unwrap_or(0));
                }
            }
        }
        if bytes > ctl.limits.max_total_bytes {
            return Err(Error::LimitExceeded(LimitKind::TotalSize));
        }
        ctl.set_totals(bytes, remaining as u64);
        Ok(Selection { wanted, remaining })
    }

    pub fn contains(&self, id: usize) -> bool {
        self.wanted.get(id).copied().unwrap_or(false)
    }

    /// Marks an entry as handled; returns true when nothing is left.
    pub fn done(&mut self, id: usize) -> bool {
        if let Some(w) = self.wanted.get_mut(id) {
            if *w {
                *w = false;
                self.remaining -= 1;
            }
        }
        self.remaining == 0
    }
}

pub(crate) struct Pump<'c> {
    ctl: &'c Control,
    archive_len: u64,
    entry_written: u64,
    buf: Vec<u8>,
}

impl<'c> Pump<'c> {
    pub fn new(ctl: &'c Control, archive_len: u64) -> Pump<'c> {
        Pump { ctl, archive_len: archive_len.max(1), entry_written: 0, buf: Vec::new() }
    }

    /// Pull model: copies `reader` into the sink. `map_read` converts errors
    /// raised by the decoder (as opposed to errors writing the output).
    pub fn run(
        &mut self,
        entry: &Entry,
        reader: &mut dyn Read,
        sink: &mut dyn ExtractSink,
        map_read: &dyn Fn(io::Error) -> Error,
    ) -> Result<()> {
        self.ctl.check()?;
        let mut buf = std::mem::take(&mut self.buf);
        buf.resize(CHUNK, 0);
        let result = (|| {
            let Some(mut w) = sink.begin(entry)? else {
                self.ctl.add_entry();
                return Ok(());
            };
            self.entry_written = 0;
            let copied = loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) => break Ok(()),
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => break Err(map_read(e)),
                };
                if let Err(e) = self.write(entry, &mut *w, &buf[..n]) {
                    break Err(e);
                }
            };
            drop(w);
            self.end(entry, sink, copied)
        })();
        self.buf = buf;
        result
    }

    /// Push model, step 1: asks the sink for a writer.
    pub fn begin<'s>(&mut self, entry: &Entry, sink: &'s mut dyn ExtractSink) -> Result<Option<Box<dyn Write + 's>>> {
        self.ctl.check()?;
        self.entry_written = 0;
        let w = sink.begin(entry)?;
        if w.is_none() {
            self.ctl.add_entry();
        }
        Ok(w)
    }

    /// Push model, step 2: writes one chunk after checking cancel and limits.
    pub fn write(&mut self, entry: &Entry, w: &mut dyn Write, data: &[u8]) -> Result<()> {
        self.ctl.check()?;
        let n = data.len() as u64;
        self.entry_written += n;
        if let Some(size) = entry.size {
            if self.entry_written > size {
                return Err(Error::LimitExceeded(LimitKind::EntrySize));
            }
        }
        let total = self.ctl.add_bytes(n);
        let limits = &self.ctl.limits;
        if total > limits.max_total_bytes {
            return Err(Error::LimitExceeded(LimitKind::TotalSize));
        }
        if total > limits.ratio_threshold && total / self.archive_len > limits.max_ratio {
            return Err(Error::LimitExceeded(LimitKind::Ratio));
        }
        w.write_all(data)?;
        Ok(())
    }

    /// Push model, step 3: finishes or aborts the entry depending on `outcome`.
    /// The writer returned by `begin` must already be dropped.
    pub fn end(&mut self, entry: &Entry, sink: &mut dyn ExtractSink, outcome: Result<()>) -> Result<()> {
        match outcome.and_then(|()| sink.finish(entry)) {
            Ok(()) => {
                self.ctl.add_entry();
                Ok(())
            }
            Err(e) => {
                sink.abort(entry);
                Err(e)
            }
        }
    }
}
