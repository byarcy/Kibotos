//! One module per archive family. Each implements [`crate::ArchiveReader`].

#[cfg(windows)]
pub mod rar;
pub mod sevenz;
pub mod single;
pub mod tar;
pub mod zip;

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use crate::Result;

/// Read buffer for archive files. Large enough to amortize syscalls, small
/// enough that opening an archive (index only) stays cheap.
pub(crate) const READ_BUF: usize = 128 * 1024;

pub(crate) fn open_file(path: &Path) -> Result<(BufReader<File>, u64)> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    Ok((BufReader::with_capacity(READ_BUF, file), len))
}
