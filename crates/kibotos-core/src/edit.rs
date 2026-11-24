//! Changing an existing archive: removing, renaming and replacing entries,
//! adding files and folders from disk, and creating empty folders.
//!
//! The archive is never modified in place. A new archive is written next to
//! it (`name.kibotos-part`), checked, and only then renamed over the
//! original, so a failure or a cancellation leaves the original untouched.
//!
//! - ZIP: entries that are kept are copied as they are (still compressed,
//!   still encrypted), so changing a large ZIP costs little more than
//!   copying it.
//! - tar (plain, gz, bz2, xz, zst): rewritten in one pass from the old
//!   stream to the new one; headers of kept entries (links, modes, owners)
//!   are copied.
//! - 7z: kept entries are extracted to a temporary folder next to the
//!   archive and everything is compressed again. Solid blocks cannot be
//!   cut, so this is what changing a 7z costs.
//!
//! RAR is never written (the unRAR license forbids building a RAR
//! compressor); ISO, CAB, MSI, single compressed files and split archives
//! are read-only.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::create::{self, CreateFormat, CreateOptions, CreateReport, Item, Level, SkipCause};
use crate::entry::normalize_path;
use crate::{Codec, Control, Entry, EntryId, EntryKind, Error, ExtractSink, Format, Result, Timestamp};

/// What to change. Paths inside the archive use `/` and no trailing `/`.
#[derive(Clone, Debug, Default)]
pub struct Changes {
    /// Entries to remove (a folder's content must be listed too).
    pub remove: Vec<EntryId>,
    /// Entries that move or get a new name: id and new path.
    pub rename: Vec<(EntryId, String)>,
    /// Entries whose content is replaced by a file on disk (an edited copy).
    pub replace: Vec<(EntryId, PathBuf)>,
    /// Files and folders on disk to add inside `add_into`. An entry with
    /// the same path as an added item is replaced by it.
    pub add: Vec<PathBuf>,
    /// Folder of the archive the added items go into ("" for the root).
    pub add_into: String,
    /// Empty folders to create (full paths inside the archive).
    pub new_dirs: Vec<String>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.remove.is_empty() && self.rename.is_empty() && self.replace.is_empty() && self.add.is_empty() && self.new_dirs.is_empty()
    }
}

#[derive(Clone, Debug, Default)]
pub struct UpdateOptions {
    /// Password of the archive. Needed for 7z archives with encrypted
    /// content (they are compressed again) and to encrypt added files.
    pub password: Option<String>,
    /// ZIP: encrypt added and replaced files with `password` (AES-256).
    /// 7z: the whole new archive is encrypted when `password` is set.
    pub encrypt_new: bool,
    /// Compression level for new content (and for the whole 7z or tar).
    pub level: Level,
    /// Worker threads; 0 means one per logical processor.
    pub threads: usize,
}

/// What a finished update did.
#[derive(Clone, Debug, Default)]
pub struct UpdateReport {
    pub removed: u64,
    pub renamed: u64,
    pub replaced: u64,
    /// Files and folders added (folders count with their content).
    pub added: u64,
    /// Size of the new archive.
    pub bytes_out: u64,
    /// Inputs that could not be added.
    pub skipped: Vec<(PathBuf, SkipCause)>,
}

/// Whether Kibotos can change archives of this format.
pub fn can_update(format: Format) -> bool {
    match format {
        Format::Zip | Format::SevenZip => true,
        Format::Tar(None) => true,
        Format::Tar(Some(c)) => CreateFormat::tar_codec_supported(c),
        _ => false,
    }
}

/// One item of the new archive, in order.
enum Out {
    /// An entry of the old archive, under its (possibly new) path.
    Keep(EntryId, String),
    /// An entry of the old archive whose content now comes from a file.
    Replace(EntryId, String, PathBuf),
    /// A file or folder from disk (or a new empty folder).
    Add(Item),
}

/// Checks a path given for the archive: normalized, relative, no `..`.
fn clean_name(raw: &str) -> Result<String> {
    let name = normalize_path(raw);
    let bad = name.is_empty()
        || name.starts_with('/')
        || name.split('/').any(|c| c == "." || c == ".." || c.contains(':'))
        || crate::safety::safe_join(Path::new("x"), &name).is_err();
    if bad {
        return Err(Error::Unsupported(format!("invalid name in archive: {raw}")));
    }
    Ok(name)
}

fn modified_secs(t: Option<Timestamp>) -> Option<i64> {
    match t? {
        Timestamp::Utc(s) => Some(s),
        Timestamp::Local(s) => Some(crate::time::local_to_utc(s)),
    }
}

/// Works out the new archive's content from the old entries and the changes.
/// `exclude`: files never added (the archive itself and its temporary file).
fn plan(
    entries: &[Entry],
    changes: &Changes,
    exclude: &[PathBuf],
    ctl: &Control,
    skipped: &mut Vec<(PathBuf, SkipCause)>,
) -> Result<(Vec<Out>, UpdateReport)> {
    let mut report = UpdateReport::default();
    let removed: HashSet<EntryId> = changes.remove.iter().copied().collect();
    let mut renamed: HashMap<EntryId, String> = HashMap::new();
    for (id, name) in &changes.rename {
        renamed.insert(*id, clean_name(name)?);
    }
    let replaced: HashMap<EntryId, &PathBuf> = changes.replace.iter().map(|(id, p)| (*id, p)).collect();

    // Items from disk, named inside `add_into`.
    let into = if changes.add_into.is_empty() { String::new() } else { clean_name(&changes.add_into)? };
    let mut added = create::collect(&changes.add, exclude, ctl, skipped)?;
    for item in &mut added {
        item.name = if into.is_empty() { item.name.clone() } else { format!("{into}/{}", item.name) };
    }
    for d in &changes.new_dirs {
        let name = clean_name(d)?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).ok();
        added.push(Item { name, path: PathBuf::new(), dir: true, size: 0, modified: now, attributes: 0x10 });
    }
    // Added items replace entries with the same path (Windows compares
    // names without case, so the old one would clash on extraction).
    let shadow: HashSet<String> = added.iter().map(|i| i.name.to_lowercase()).collect();

    let mut out = Vec::with_capacity(entries.len() + added.len());
    for e in entries {
        if removed.contains(&e.id) {
            report.removed += 1;
            continue;
        }
        let name = match renamed.get(&e.id) {
            Some(n) => {
                report.renamed += 1;
                n.clone()
            }
            None => e.path.to_string(),
        };
        if shadow.contains(&name.to_lowercase()) {
            // Replaced by the added item. An added folder takes the place
            // of the old folder entry; what was inside it is kept.
            continue;
        }
        match replaced.get(&e.id) {
            Some(p) if e.kind == EntryKind::File => {
                report.replaced += 1;
                out.push(Out::Replace(e.id, name, (*p).clone()));
            }
            _ => out.push(Out::Keep(e.id, name)),
        }
    }
    report.added = added.len() as u64;
    out.extend(added.into_iter().map(Out::Add));
    Ok((out, report))
}

/// Applies `changes` to `archive`. The original is replaced only when the
/// new archive is complete and opens correctly.
pub fn update(archive: &Path, changes: &Changes, opts: &UpdateOptions, ctl: &Control) -> Result<UpdateReport> {
    let format = crate::detect_path(archive)?;
    if !can_update(format) {
        return Err(Error::Unsupported(format!("changing {} archives", format.name())));
    }
    if crate::volume::Source::open(archive)?.is_split() {
        return Err(Error::Unsupported("changing split archives".into()));
    }
    let reader = crate::open_as(archive, format, &crate::OpenOptions { password: opts.password.clone() })?;
    let entries = reader.entries().to_vec();
    drop(reader);
    let mut skipped = Vec::new();
    let temp = create::temp_path(archive);
    let (out, mut report) = plan(&entries, changes, &[archive.to_owned(), temp.clone()], ctl, &mut skipped)?;

    let result = match format {
        Format::Zip => write_zip(archive, &entries, &out, opts, &temp, ctl, &mut skipped),
        Format::SevenZip => write_7z(archive, &entries, &out, opts, &temp, ctl, &mut skipped),
        Format::Tar(codec) => write_tar(archive, codec, &entries, &out, opts, &temp, ctl, &mut skipped),
        _ => unreachable!(),
    }
    .and_then(|()| verify(&temp, format, &out, opts, &skipped));
    if let Err(e) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(if ctl.is_cancelled() { Error::Cancelled } else { e });
    }
    replace_file(&temp, archive).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })?;
    report.bytes_out = std::fs::metadata(archive).map(|m| m.len()).unwrap_or(0);
    report.skipped = skipped;
    Ok(report)
}

/// Opens the new archive and checks that it lists what was planned.
/// tar is not re-read: listing a compressed tar means decompressing it all.
fn verify(temp: &Path, format: Format, out: &[Out], opts: &UpdateOptions, skipped: &[(PathBuf, SkipCause)]) -> Result<()> {
    if matches!(format, Format::Tar(_)) {
        return Ok(());
    }
    let reader = crate::open_as(temp, format, &crate::OpenOptions { password: opts.password.clone() })?;
    let have: HashSet<String> = reader.entries().iter().map(|e| e.path.to_lowercase()).collect();
    for o in out {
        let name = match o {
            Out::Keep(_, n) | Out::Replace(_, n, _) => n,
            Out::Add(i) if skipped.iter().any(|(p, _)| *p == i.path) => continue,
            Out::Add(i) => &i.name,
        };
        if !have.contains(&name.to_lowercase()) {
            return Err(Error::Corrupt(format!("the new archive is missing {name}")));
        }
    }
    Ok(())
}

/// Renames `temp` over `target`. Retries for a moment: an antivirus or the
/// shell's preview may hold the archive briefly.
fn replace_file(temp: &Path, target: &Path) -> Result<()> {
    let mut last = None;
    for attempt in 0..10 {
        match std::fs::rename(temp, target) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(std::time::Duration::from_millis(100 * (attempt + 1)));
    }
    Err(Error::Io(last.unwrap_or_else(|| io::Error::other("rename failed"))))
}

// ------------------------------------------------------------------- ZIP

fn zip_err(e: zip::result::ZipError, ctl: &Control) -> Error {
    if ctl.is_cancelled() { Error::Cancelled } else { crate::formats::zip::map_err(e) }
}

fn zip_time(secs: Option<i64>) -> zip::DateTime {
    let (y, mo, d, h, mi, s) = crate::time::civil_from_secs(crate::time::utc_to_local(secs.unwrap_or(0)));
    zip::DateTime::from_date_and_time(y.clamp(1980, 2107) as u16, mo as u8, d as u8, h as u8, mi as u8, s as u8)
        .unwrap_or_default()
}

fn write_zip(
    archive: &Path,
    entries: &[Entry],
    out: &[Out],
    opts: &UpdateOptions,
    temp: &Path,
    ctl: &Control,
    skipped: &mut Vec<(PathBuf, SkipCause)>,
) -> Result<()> {
    use zip::write::SimpleFileOptions;
    let mut src = zip::ZipArchive::new(BufReader::with_capacity(1 << 20, File::open(archive)?)).map_err(|e| zip_err(e, ctl))?;
    if src.offset() > 0 {
        // Data before the archive (a self-extracting .exe stub) would be lost.
        return Err(Error::Unsupported("changing self-extracting archives".into()));
    }
    let total: u64 = out
        .iter()
        .map(|o| match o {
            Out::Keep(id, _) => entries[*id as usize].packed.unwrap_or(0),
            Out::Replace(_, _, p) => std::fs::metadata(p).map(|m| m.len()).unwrap_or(0),
            Out::Add(i) => i.size,
        })
        .sum();
    ctl.set_totals(total, out.len() as u64);

    let mut w = zip::ZipWriter::new(BufWriter::with_capacity(1 << 20, File::create(temp)?));
    w.set_raw_comment(src.comment().to_vec().into_boxed_slice()).map_err(|e| zip_err(e, ctl))?;
    let password = opts.password.as_deref().filter(|p| opts.encrypt_new && !p.is_empty()).map(crate::codepage::zip_password_for_writing);
    let file_options = |modified: Option<i64>, size: u64| {
        let base = SimpleFileOptions::default().last_modified_time(zip_time(modified)).large_file(size > 0xF000_0000);
        let base = match create::zip::deflate_level(opts.level) {
            Some(l) => base.compression_method(zip::CompressionMethod::Deflated).compression_level(Some(i64::from(l))),
            None => base.compression_method(zip::CompressionMethod::Stored),
        };
        match &password {
            Some(pw) => base.with_aes_encryption_bytes(zip::AesMode::Aes256, pw),
            None => base,
        }
    };
    let add_file = |w: &mut zip::ZipWriter<BufWriter<File>>, name: &str, file: File, modified: Option<i64>| -> Result<()> {
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        w.start_file(name, file_options(modified, size)).map_err(|e| zip_err(e, ctl))?;
        io::copy(&mut create::Counted { inner: BufReader::with_capacity(1 << 20, file), ctl }, w)
            .map_err(|e| create::io_err(e, ctl))?;
        Ok(())
    };

    for o in out {
        ctl.check()?;
        match o {
            Out::Keep(id, name) => {
                let e = &entries[*id as usize];
                let f = src.by_index_raw(*id as usize).map_err(|e| zip_err(e, ctl))?;
                let new_name = if e.is_dir() { format!("{name}/") } else { name.clone() };
                if f.name_raw() == new_name.as_bytes() {
                    w.raw_copy_file(f).map_err(|e| zip_err(e, ctl))?;
                } else {
                    w.raw_copy_file_rename(f, new_name).map_err(|e| zip_err(e, ctl))?;
                }
                ctl.add_bytes(e.packed.unwrap_or(0));
            }
            Out::Replace(_, name, path) => {
                let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|t| {
                    t.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs() as i64)
                });
                add_file(&mut w, name, File::open(path)?, modified)?;
            }
            Out::Add(item) if item.dir => {
                w.add_directory(format!("{}/", item.name), file_options(item.modified, 0)).map_err(|e| zip_err(e, ctl))?;
            }
            Out::Add(item) => match File::open(&item.path) {
                Ok(f) => add_file(&mut w, &item.name, f, item.modified)?,
                // A file that vanished or is locked is left out and reported.
                Err(e) => {
                    skipped.push((item.path.clone(), SkipCause::Unreadable(e.to_string())));
                    continue;
                }
            },
        }
        ctl.add_entry();
    }
    w.finish().map_err(|e| zip_err(e, ctl))?.flush()?;
    Ok(())
}

// ------------------------------------------------------------------- 7z

/// Writes the extracted entries to their new paths in a staging folder.
struct Staging<'a> {
    root: &'a Path,
    /// Entry id -> file in the staging folder.
    targets: &'a HashMap<EntryId, PathBuf>,
}

impl ExtractSink for Staging<'_> {
    fn begin(&mut self, entry: &Entry) -> Result<Option<Box<dyn Write + '_>>> {
        let Some(target) = self.targets.get(&entry.id) else { return Ok(None) };
        if entry.is_dir() {
            std::fs::create_dir_all(target)?;
            return Ok(None);
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        debug_assert!(target.starts_with(self.root));
        Ok(Some(Box::new(BufWriter::with_capacity(1 << 20, File::create(target)?))))
    }

    fn finish(&mut self, entry: &Entry) -> Result<()> {
        if let (Some(target), Some(secs)) = (self.targets.get(&entry.id), modified_secs(entry.modified)) {
            if let Ok(f) = File::options().write(true).open(target) {
                let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs.max(0) as u64);
                let _ = f.set_modified(when);
            }
        }
        Ok(())
    }

    fn abort(&mut self, entry: &Entry) {
        if let Some(target) = self.targets.get(&entry.id) {
            let _ = std::fs::remove_file(target);
        }
    }
}

/// Removes the staging folder when the update ends, however it ends.
struct StagingDir(PathBuf);

impl Drop for StagingDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_7z(
    archive: &Path,
    entries: &[Entry],
    out: &[Out],
    opts: &UpdateOptions,
    temp: &Path,
    ctl: &Control,
    skipped: &mut Vec<(PathBuf, SkipCause)>,
) -> Result<()> {
    if out.iter().any(|o| matches!(o, Out::Keep(id, _) if entries[*id as usize].kind == EntryKind::Link)) {
        return Err(Error::Unsupported("changing 7z archives that contain links".into()));
    }
    // Encrypted names stay encrypted: the header could not be read without
    // the password.
    let names_encrypted = matches!(crate::formats::sevenz::SevenZipBackend::open(archive, None), Err(Error::PasswordRequired));
    let password = opts.password.clone().filter(|p| !p.is_empty());
    let encrypted = entries.iter().any(|e| e.encrypted);
    if encrypted && password.is_none() {
        return Err(Error::PasswordRequired);
    }

    // Kept entries are extracted next to the archive (the same disk as the
    // result, not the system drive).
    let name = archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let root = archive.with_file_name(format!(".{name}.kibotos-edit"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root)?;
    let _guard = StagingDir(root.clone());

    let mut targets = HashMap::new();
    for o in out {
        if let Out::Keep(id, name) = o {
            let target = crate::safety::safe_join(&root, name).map_err(|r| Error::Unsupported(format!("{name}: {r:?}")))?;
            targets.insert(*id, target);
        }
    }
    if !targets.is_empty() {
        let mut reader = crate::formats::sevenz::SevenZipBackend::open(archive, password.as_deref())?;
        let ids: Vec<EntryId> = targets.keys().copied().collect();
        crate::ArchiveReader::extract(&mut reader, &ids, &mut Staging { root: &root, targets: &targets }, ctl)?;
    }

    // Everything is compressed again, kept entries from the staging folder.
    let mut items = Vec::with_capacity(out.len());
    for o in out {
        match o {
            Out::Keep(id, name) => {
                let e = &entries[*id as usize];
                let path = targets[id].clone();
                let size = if e.is_dir() { 0 } else { std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) };
                let attributes = if e.is_dir() { 0x10 } else { 0 };
                items.push(Item { name: name.clone(), path, dir: e.is_dir(), size, modified: modified_secs(e.modified), attributes });
            }
            Out::Replace(_, name, path) => {
                let meta = std::fs::metadata(path)?;
                let modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64);
                items.push(Item { name: name.clone(), path: path.clone(), dir: false, size: meta.len(), modified, attributes: 0 });
            }
            Out::Add(item) => items.push(item.clone()),
        }
    }
    ctl.restart();
    ctl.set_totals(items.iter().map(|i| i.size).sum(), items.iter().filter(|i| !i.dir).count() as u64);
    let copts = CreateOptions {
        format: CreateFormat::SevenZip,
        level: opts.level,
        password: if encrypted || opts.encrypt_new { password } else { None },
        encrypt_names: names_encrypted,
        threads: opts.threads,
    };
    let mut report = CreateReport::default();
    create::sevenz::write(temp, &items, &copts, ctl, &mut report)?;
    skipped.extend(report.skipped);
    Ok(())
}

// ------------------------------------------------------------------- tar

#[allow(clippy::too_many_arguments)]
fn write_tar(
    archive: &Path,
    codec: Option<Codec>,
    entries: &[Entry],
    out: &[Out],
    opts: &UpdateOptions,
    temp: &Path,
    ctl: &Control,
    skipped: &mut Vec<(PathBuf, SkipCause)>,
) -> Result<()> {
    // Kept and replaced entries are written where they were; added items
    // go at the end.
    let mut kept: HashMap<EntryId, &Out> = HashMap::new();
    let mut adds = Vec::new();
    for o in out {
        match o {
            Out::Keep(id, _) | Out::Replace(id, _, _) => {
                kept.insert(*id, o);
            }
            Out::Add(item) => adds.push(item),
        }
    }
    let threads = match opts.threads {
        0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
        n => n,
    };
    let total = std::fs::metadata(archive).map(|m| m.len()).unwrap_or(0) + adds.iter().map(|i| i.size).sum::<u64>();
    ctl.set_totals(total, out.len() as u64);
    let mut b = tar::Builder::new(create::tar::open_out(temp, codec, opts.level, threads)?);
    b.mode(tar::HeaderMode::Complete);

    let (file, _) = crate::formats::open_file(archive)?;
    let counted = create::Counted { inner: file, ctl };
    let input: Box<dyn Read> = match codec {
        None => Box::new(counted),
        Some(c) => c.decoder(counted).map_err(Error::Io)?,
    };
    let mut src = tar::Archive::new(input);
    let mut next_id: EntryId = 0;
    let map = |e: io::Error| if ctl.is_cancelled() { Error::Cancelled } else { crate::formats::tar::map_tar(e) };
    for item in src.entries().map_err(map)? {
        ctl.check()?;
        let mut item = item.map_err(map)?;
        let Some(kind) = crate::formats::tar::kind_of(item.header().entry_type()) else {
            // Devices and other special records have no entry: copied as
            // they are.
            let mut h = item.header().clone();
            let path = item.path_bytes().into_owned();
            b.append_data(&mut h, String::from_utf8_lossy(&path).as_ref(), &mut item).map_err(map)?;
            continue;
        };
        let id = next_id;
        next_id += 1;
        let Some(o) = kept.get(&id) else {
            io::copy(&mut item, &mut io::sink()).map_err(map)?;
            continue;
        };
        debug_assert_eq!(entries.get(id as usize).map(|e| e.kind), Some(kind));
        let mut h = item.header().clone();
        match o {
            Out::Keep(_, name) => match kind {
                EntryKind::Link => {
                    let target = item.link_name_bytes().map(|t| String::from_utf8_lossy(&t).into_owned()).unwrap_or_default();
                    b.append_link(&mut h, name, target).map_err(map)?;
                }
                EntryKind::Dir => b.append_data(&mut h, format!("{name}/"), io::empty()).map_err(map)?,
                EntryKind::File => {
                    if h.entry_type() == tar::EntryType::GNUSparse {
                        // The sparse map is not copied: store the file whole.
                        h.set_entry_type(tar::EntryType::Regular);
                        h.set_size(item.size());
                    }
                    b.append_data(&mut h, name, &mut item).map_err(map)?;
                }
            },
            Out::Replace(_, name, path) => {
                io::copy(&mut item, &mut io::sink()).map_err(map)?;
                let file = File::open(path)?;
                let meta = file.metadata()?;
                h.set_entry_type(tar::EntryType::Regular);
                h.set_size(meta.len());
                if let Some(t) = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()) {
                    h.set_mtime(t.as_secs());
                }
                let data = create::Counted { inner: BufReader::with_capacity(1 << 20, file), ctl }.take(meta.len());
                b.append_data(&mut h, name, data).map_err(map)?;
            }
            Out::Add(_) => unreachable!(),
        }
        ctl.add_entry();
    }
    let mut report = CreateReport::default();
    for item in adds {
        ctl.check()?;
        create::tar::append_item(&mut b, item, ctl, &mut report)?;
    }
    skipped.extend(report.skipped);
    let out = b.into_inner().map_err(map)?;
    out.finish().map_err(map)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_checked() {
        assert_eq!(clean_name("a\\b/c").unwrap(), "a/b/c");
        assert!(clean_name("../x").is_err());
        assert!(clean_name("/etc/x").is_err());
        assert!(clean_name("C:/x").is_err());
        assert!(clean_name("").is_err());
        assert!(clean_name("a/CON").is_err());
    }
}
