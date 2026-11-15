//! Creating archives: ZIP, 7z and tar (plain, gzip, bzip2, xz, zstd).
//!
//! RAR is never written: the unRAR license forbids using its code to build
//! a RAR-compatible compressor, so [`CreateFormat`] has no RAR variant.
//!
//! The archive is written to a temporary file next to the destination and
//! renamed into place only when complete, so a cancelled or failed run never
//! leaves a half-written archive under the final name.

mod sevenz;
mod tar;
mod zip;

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{Codec, Control, Error, Result};

/// Formats Kibotos can create.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CreateFormat {
    Zip,
    SevenZip,
    /// A tar archive, optionally compressed (gzip, bzip2, xz or zstd).
    Tar(Option<Codec>),
}

impl CreateFormat {
    /// The usual file extension, without the dot ("zip", "tar.gz", ...).
    pub fn extension(self) -> &'static str {
        match self {
            CreateFormat::Zip => "zip",
            CreateFormat::SevenZip => "7z",
            CreateFormat::Tar(None) => "tar",
            CreateFormat::Tar(Some(Codec::Gzip)) => "tar.gz",
            CreateFormat::Tar(Some(Codec::Bzip2)) => "tar.bz2",
            CreateFormat::Tar(Some(Codec::Xz)) => "tar.xz",
            CreateFormat::Tar(Some(Codec::Zstd)) => "tar.zst",
            CreateFormat::Tar(Some(_)) => "tar",
        }
    }

    /// Whether the format can protect its content with a password.
    pub fn supports_password(self) -> bool {
        matches!(self, CreateFormat::Zip | CreateFormat::SevenZip)
    }

    /// Whether `codec` can wrap a tar archive Kibotos writes.
    pub fn tar_codec_supported(codec: Codec) -> bool {
        matches!(codec, Codec::Gzip | Codec::Bzip2 | Codec::Xz | Codec::Zstd)
    }
}

/// How hard to compress.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Level {
    /// No compression (ZIP "stored", 7z "copy").
    Store,
    Fast,
    #[default]
    Normal,
    Maximum,
}

#[derive(Clone, Debug)]
pub struct CreateOptions {
    pub format: CreateFormat,
    pub level: Level,
    /// ZIP: AES-256. 7z: AES-256 of the content (and of the names, see
    /// `encrypt_names`). Ignored by tar.
    pub password: Option<String>,
    /// 7z only: also encrypt the list of files, so the archive cannot even
    /// be listed without the password.
    pub encrypt_names: bool,
    /// Worker threads; 0 means one per logical processor.
    pub threads: usize,
}

impl Default for CreateOptions {
    fn default() -> Self {
        CreateOptions { format: CreateFormat::Zip, level: Level::Normal, password: None, encrypt_names: true, threads: 0 }
    }
}

/// What a finished run wrote.
#[derive(Clone, Debug, Default)]
pub struct CreateReport {
    pub files: u64,
    pub dirs: u64,
    /// Bytes read from the input files.
    pub bytes_in: u64,
    /// Size of the archive.
    pub bytes_out: u64,
    /// Inputs that could not be added.
    pub skipped: Vec<(PathBuf, SkipCause)>,
}

/// Why an input was left out.
#[derive(Clone, Debug)]
pub enum SkipCause {
    /// A symbolic link or junction: never followed.
    Link,
    /// It could not be read (the system's message).
    Unreadable(String),
}

/// One file or folder to add.
#[derive(Clone, Debug)]
pub(crate) struct Item {
    /// Path inside the archive, `/`-separated.
    pub name: String,
    pub path: PathBuf,
    pub dir: bool,
    pub size: u64,
    /// Seconds since 1970 (UTC).
    pub modified: Option<i64>,
    pub attributes: u32,
}

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
#[cfg(not(windows))]
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

fn attributes(meta: &std::fs::Metadata) -> u32 {
    #[cfg(windows)]
    {
        std::os::windows::fs::MetadataExt::file_attributes(meta)
    }
    #[cfg(not(windows))]
    {
        if meta.is_dir() { FILE_ATTRIBUTE_DIRECTORY } else { 0 }
    }
}

fn unix_secs(t: io::Result<SystemTime>) -> Option<i64> {
    let t = t.ok()?;
    Some(match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    })
}

/// Lowercase absolute form, to compare paths on a case-insensitive system.
fn path_key(p: &Path) -> String {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_owned());
    abs.to_string_lossy().to_lowercase()
}

/// Expands `inputs` (files and folders) into the list of items to add, in
/// order, each folder before its content. Links and junctions are not
/// followed (they could loop or leave the folder); they are reported as
/// skipped. `exclude` keeps the archive being written out of itself.
pub(crate) fn collect(inputs: &[PathBuf], exclude: &[PathBuf], ctl: &Control, skipped: &mut Vec<(PathBuf, SkipCause)>) -> Result<Vec<Item>> {
    let exclude: Vec<String> = exclude.iter().map(|p| path_key(p)).collect();
    let mut items = Vec::new();
    let mut roots = std::collections::HashSet::new();
    for input in inputs {
        let meta = std::fs::symlink_metadata(input)?;
        let base = input.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| {
            // A drive root ("D:\") has no file name: use its letter.
            input.to_string_lossy().trim_end_matches(['\\', '/', ':']).to_owned()
        });
        // Two inputs with the same name: keep both, "name (2)".
        let mut name = base.clone();
        let mut n = 2;
        while !roots.insert(name.to_lowercase()) {
            name = format!("{base} ({n})");
            n += 1;
        }
        walk(input, &meta, name, &exclude, ctl, &mut items, skipped, 0)?;
    }
    Ok(items)
}

#[allow(clippy::too_many_arguments)]
fn walk(
    path: &Path,
    meta: &std::fs::Metadata,
    name: String,
    exclude: &[String],
    ctl: &Control,
    items: &mut Vec<Item>,
    skipped: &mut Vec<(PathBuf, SkipCause)>,
    depth: usize,
) -> Result<()> {
    ctl.check()?;
    let attrs = attributes(meta);
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 || meta.file_type().is_symlink() {
        skipped.push((path.to_owned(), SkipCause::Link));
        return Ok(());
    }
    if exclude.contains(&path_key(path)) {
        return Ok(());
    }
    let item = Item {
        name: name.clone(),
        path: path.to_owned(),
        dir: meta.is_dir(),
        size: if meta.is_dir() { 0 } else { meta.len() },
        modified: unix_secs(meta.modified()),
        attributes: attrs,
    };
    let is_dir = item.dir;
    items.push(item);
    if !is_dir {
        return Ok(());
    }
    if depth > 256 {
        return Err(Error::Unsupported("folders nested too deeply".into()));
    }
    let mut children: Vec<_> = match std::fs::read_dir(path) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(e) => {
            skipped.push((path.to_owned(), SkipCause::Unreadable(e.to_string())));
            return Ok(());
        }
    };
    children.sort_by_key(|e| e.file_name());
    for child in children {
        let child_path = child.path();
        let meta = match std::fs::symlink_metadata(&child_path) {
            Ok(m) => m,
            Err(e) => {
                skipped.push((child_path, SkipCause::Unreadable(e.to_string())));
                continue;
            }
        };
        let child_name = format!("{name}/{}", child.file_name().to_string_lossy());
        walk(&child_path, &meta, child_name, exclude, ctl, items, skipped, depth + 1)?;
    }
    Ok(())
}

/// A file reader that reports progress and stops when cancelled.
pub(crate) struct Counted<'a, R> {
    pub inner: R,
    pub ctl: &'a Control,
}

impl<R: Read> Read for Counted<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.ctl.is_cancelled() {
            return Err(io::Error::other("cancelled"));
        }
        let n = self.inner.read(buf)?;
        self.ctl.add_bytes(n as u64);
        Ok(n)
    }
}

/// Maps an I/O error, reporting a cancellation as such.
pub(crate) fn io_err(e: io::Error, ctl: &Control) -> Error {
    if ctl.is_cancelled() { Error::Cancelled } else { Error::Io(e) }
}

/// Reads a whole (small) file, counting its bytes.
pub(crate) fn read_all(item: &Item, ctl: &Control) -> io::Result<Vec<u8>> {
    let mut data = Vec::with_capacity(item.size as usize);
    Counted { inner: File::open(&item.path)?, ctl }.read_to_end(&mut data)?;
    Ok(data)
}

pub(crate) fn threads(opts: &CreateOptions) -> usize {
    match opts.threads {
        0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
        n => n,
    }
}

/// The temporary file the archive is written to before it gets its name.
fn temp_path(output: &Path) -> PathBuf {
    let name = output.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    output.with_file_name(format!("{name}.kibotos-part"))
}

/// Creates `output` from `inputs` (files and folders on disk). Items are
/// stored under their own names: `D:\fotos` becomes `fotos/...`.
pub fn create(output: &Path, inputs: &[PathBuf], opts: &CreateOptions, ctl: &Control) -> Result<CreateReport> {
    if inputs.is_empty() {
        return Err(Error::Unsupported("nothing to compress".into()));
    }
    if let CreateFormat::Tar(Some(c)) = opts.format {
        if !CreateFormat::tar_codec_supported(c) {
            return Err(Error::Unsupported(format!("creating tar.{}", c.name())));
        }
    }
    let temp = temp_path(output);
    let mut report = CreateReport::default();
    let items = collect(inputs, &[output.to_owned(), temp.clone()], ctl, &mut report.skipped)?;
    let total: u64 = items.iter().map(|i| i.size).sum();
    let files = items.iter().filter(|i| !i.dir).count() as u64;
    ctl.set_totals(total, files);

    let result = match opts.format {
        CreateFormat::Zip => zip::write(&temp, &items, opts, ctl, &mut report),
        CreateFormat::SevenZip => sevenz::write(&temp, &items, opts, ctl, &mut report),
        CreateFormat::Tar(codec) => tar::write(&temp, &items, codec, opts, ctl, &mut report),
    };
    match result {
        Ok(()) => {
            std::fs::rename(&temp, output).inspect_err(|_| {
                let _ = std::fs::remove_file(&temp);
            })?;
            report.bytes_out = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
            Ok(report)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            Err(if ctl.is_cancelled() { Error::Cancelled } else { e })
        }
    }
}
