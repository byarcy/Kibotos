//! Kibotos core: archive format backends behind one common trait, the
//! in-memory folder index, and safe extraction.
//!
//! Design rules:
//! - Opening an archive reads only its index (central directory, headers).
//!   File data is touched only by [`ArchiveReader::extract`].
//! - Backends never write to disk themselves. They stream entry data into an
//!   [`ExtractSink`], and the shared extraction pump enforces cancellation,
//!   progress, and decompression-bomb limits for every format.
//! - Entry paths are untrusted. [`safety::safe_join`] validates them before
//!   anything is created on disk.
//!
//! This crate has no UI code so it can be tested and benchmarked alone.

mod control;
mod detect;
mod entry;
mod error;
mod formats;
mod index;
mod lzw;
mod pump;
pub mod safety;
mod sink;
mod time;
pub mod view;
mod volume;
mod xzmt;
pub mod zone;

use std::path::Path;

pub use control::{Control, Limits, Progress};
pub use detect::{Codec, Format, detect_path};
pub use entry::{Entry, EntryId, EntryKind, Timestamp};
pub use error::{Error, LimitKind, Result};
pub use index::{ArchiveIndex, NodeId};
pub use safety::UnsafeReason;
pub use volume::{first_part, is_part_name};
pub use sink::{Conflict, ConflictFn, DiskSink, ExtractReport, ExtractSink, MemorySink, Overwrite, SkipReason, Skipped};

/// Options used when opening an archive.
#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    /// Password for encrypted archives. Formats with encrypted headers
    /// (7z, RAR) need it to list; others need it only to extract.
    pub password: Option<String>,
}

/// A read-only view of an opened archive.
///
/// Writing will be a separate `ArchiveWriter` trait (planned for v0.2), so
/// readers never need to change when it is added.
pub trait ArchiveReader: Send {
    /// The detected format.
    fn format(&self) -> Format;

    /// All entries, in archive order. `entries()[i].id == i`.
    fn entries(&self) -> &[Entry];

    /// Replaces the password used for later extractions.
    fn set_password(&mut self, password: Option<&str>);

    /// Streams the selected entries into `sink`, in archive order.
    ///
    /// Directories are passed to the sink too, so it can create them.
    /// Unknown ids are ignored. Cancellation and limits are read from `ctl`.
    fn extract(&mut self, ids: &[EntryId], sink: &mut dyn ExtractSink, ctl: &Control) -> Result<()>;

    /// Extracts every entry.
    fn extract_all(&mut self, sink: &mut dyn ExtractSink, ctl: &Control) -> Result<()> {
        let ids: Vec<EntryId> = (0..self.entries().len() as EntryId).collect();
        self.extract(&ids, sink, ctl)
    }
}

/// Opens an archive, detecting its format from its content (not its extension).
///
/// Any part of a split archive (`.7z.001`, `.z01`, `.part2.rar`, ...) opens
/// the whole archive, starting from its first part.
pub fn open(path: &Path, opts: &OpenOptions) -> Result<Box<dyn ArchiveReader>> {
    if let Some(first) = volume::rar_first_volume(path) {
        if !first.is_file() {
            let name = first.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            return Err(Error::MissingVolume(name));
        }
        return open_as(&first, Format::Rar, opts);
    }
    let format = detect_path(path)?;
    open_as(path, format, opts).map_err(|e| match e {
        // A plain split cut short fails as a damaged archive: name the
        // part that is probably missing instead.
        Error::Corrupt(_) | Error::Io(_) => match volume::Source::open(path) {
            Ok(s) if s.is_split() => s.next_part().map(|n| Error::MissingVolume(n.to_owned())).unwrap_or(e),
            _ => e,
        },
        e => e,
    })
}

/// Opens an archive with an already known format.
pub fn open_as(path: &Path, format: Format, opts: &OpenOptions) -> Result<Box<dyn ArchiveReader>> {
    let password = opts.password.as_deref();
    Ok(match format {
        Format::Zip => Box::new(formats::zip::ZipBackend::open(path, password)?),
        Format::SevenZip => Box::new(formats::sevenz::SevenZipBackend::open(path, password)?),
        #[cfg(windows)]
        Format::Rar => Box::new(formats::rar::RarBackend::open(path, password)?),
        #[cfg(not(windows))]
        Format::Rar => return Err(Error::Unsupported("RAR is only supported on Windows".into())),
        Format::Tar(codec) => Box::new(formats::tar::TarBackend::open(path, codec)?),
        Format::Compressed(codec) => Box::new(formats::single::SingleBackend::open(path, codec)?),
        Format::Iso => Box::new(formats::iso::IsoBackend::open(path)?),
        Format::Cab => Box::new(formats::cab::CabBackend::open(path)?),
        Format::Msi => Box::new(formats::msi::MsiBackend::open(path)?),
    })
}
