//! One module per archive family. Each implements [`crate::ArchiveReader`].

pub mod cab;
pub mod iso;
pub mod msi;
pub mod rar;
pub mod sevenz;
pub mod single;
pub mod tar;
mod udf;
pub mod zip;

use std::io::BufReader;
use std::path::Path;

use crate::Result;
use crate::volume::Source;

/// Read buffer for archive files. Large enough to amortize syscalls, small
/// enough that opening an archive (index only) stays cheap.
pub(crate) const READ_BUF: usize = 128 * 1024;

/// Opens an archive file, joined with its other parts if it is split.
pub(crate) fn open_file(path: &Path) -> Result<(BufReader<Source>, u64)> {
    let source = Source::open(path)?;
    let len = source.len();
    Ok((BufReader::with_capacity(READ_BUF, source), len))
}
