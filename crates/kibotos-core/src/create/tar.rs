//! tar writer, plain or wrapped in gzip, bzip2, xz or zstd. xz and zstd
//! compress on several threads (independent blocks); gzip and bzip2 are
//! single streams.

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;

use super::{Counted, CreateOptions, CreateReport, Item, Level, SkipCause, io_err, threads};
use crate::{Codec, Control, Error, Result};

/// The output stream: the file, possibly behind a compressor.
enum Out {
    Plain(BufWriter<File>),
    Gzip(flate2::write::GzEncoder<BufWriter<File>>),
    Bzip2(bzip2::write::BzEncoder<BufWriter<File>>),
    Xz(lzma_rust2::XzWriter<BufWriter<File>>),
    XzMt(lzma_rust2::XzWriterMt<BufWriter<File>>),
    Zstd(zstd::stream::write::Encoder<'static, BufWriter<File>>),
}

impl Write for Out {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Out::Plain(w) => w.write(buf),
            Out::Gzip(w) => w.write(buf),
            Out::Bzip2(w) => w.write(buf),
            Out::Xz(w) => w.write(buf),
            Out::XzMt(w) => w.write(buf),
            Out::Zstd(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Out::Plain(w) => w.flush(),
            Out::Gzip(w) => w.flush(),
            Out::Bzip2(w) => w.flush(),
            Out::Xz(w) => w.flush(),
            Out::XzMt(w) => w.flush(),
            Out::Zstd(w) => w.flush(),
        }
    }
}

impl Out {
    fn finish(self) -> io::Result<()> {
        let mut file = match self {
            Out::Plain(w) => w,
            Out::Gzip(w) => w.finish()?,
            Out::Bzip2(w) => w.finish()?,
            Out::Xz(w) => w.finish()?,
            Out::XzMt(w) => w.finish()?,
            Out::Zstd(w) => w.finish()?,
        };
        file.flush()
    }
}

fn open_out(temp: &Path, codec: Option<Codec>, level: Level, threads: usize) -> Result<Out> {
    let file = BufWriter::with_capacity(1 << 20, File::create(temp)?);
    let pick = |store: u32, fast: u32, normal: u32, max: u32| match level {
        Level::Store => store,
        Level::Fast => fast,
        Level::Normal => normal,
        Level::Maximum => max,
    };
    Ok(match codec {
        None => Out::Plain(file),
        Some(Codec::Gzip) => Out::Gzip(flate2::write::GzEncoder::new(file, flate2::Compression::new(pick(0, 1, 6, 9)))),
        Some(Codec::Bzip2) => Out::Bzip2(bzip2::write::BzEncoder::new(file, bzip2::Compression::new(pick(1, 1, 9, 9)))),
        Some(Codec::Xz) => {
            let mut opts = lzma_rust2::XzOptions::with_preset(pick(0, 1, 6, 9));
            if threads > 1 {
                // Independent blocks of 3x the dictionary, as `xz -T` does.
                let block = (u64::from(opts.lzma_options.dict_size) * 3).max(1 << 20);
                opts.block_size = std::num::NonZeroU64::new(block);
                Out::XzMt(lzma_rust2::XzWriterMt::new(file, opts, threads as u32)?)
            } else {
                Out::Xz(lzma_rust2::XzWriter::new(file, opts)?)
            }
        }
        Some(Codec::Zstd) => {
            let mut enc = zstd::stream::write::Encoder::new(file, pick(1, 1, 3, 19) as i32)?;
            if threads > 1 {
                enc.multithread(threads as u32)?;
            }
            enc.include_checksum(true)?;
            Out::Zstd(enc)
        }
        Some(c) => return Err(Error::Unsupported(format!("creating tar.{}", c.name()))),
    })
}

fn header(item: &Item) -> tar::Header {
    let mut h = tar::Header::new_gnu();
    h.set_mtime(item.modified.unwrap_or(0).max(0) as u64);
    if item.dir {
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mode(0o755);
        h.set_size(0);
    } else {
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(0o644);
        h.set_size(item.size);
    }
    h
}

pub(super) fn write(
    temp: &Path,
    items: &[Item],
    codec: Option<Codec>,
    opts: &CreateOptions,
    ctl: &Control,
    report: &mut CreateReport,
) -> Result<()> {
    let out = open_out(temp, codec, opts.level, threads(opts))?;
    let mut b = tar::Builder::new(out);
    for item in items {
        ctl.check()?;
        if item.dir {
            b.append_data(&mut header(item), format!("{}/", item.name), io::empty()).map_err(|e| io_err(e, ctl))?;
            report.dirs += 1;
            continue;
        }
        let file = match File::open(&item.path) {
            Ok(f) => f,
            Err(e) => {
                report.skipped.push((item.path.clone(), SkipCause::Unreadable(e.to_string())));
                continue;
            }
        };
        // tar records the size before the data: if the file changed size
        // since it was listed, store what was listed (padding or cutting).
        let reader = Counted { inner: io::BufReader::with_capacity(1 << 20, file), ctl };
        let data = reader.take(item.size).chain(io::repeat(0)).take(item.size);
        b.append_data(&mut header(item), &item.name, data).map_err(|e| io_err(e, ctl))?;
        ctl.add_entry();
        report.files += 1;
        report.bytes_in += item.size;
    }
    let out = b.into_inner().map_err(|e| io_err(e, ctl))?;
    out.finish().map_err(|e| io_err(e, ctl))?;
    Ok(())
}

