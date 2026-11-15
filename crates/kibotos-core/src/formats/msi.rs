//! Windows Installer packages (`.msi`). Shown as the files they install,
//! with their real names and folders (from the File, Component and
//! Directory tables), not as the package's internal streams. The data is in
//! cabinets, embedded in the package or next to it.

use std::collections::HashMap;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

use msi::{Package, PackageType, Select};

use super::cab::Cab;
use super::open_file;
use crate::pump::{Pump, Selection};
use crate::volume::Source;
use crate::{ArchiveReader, Control, Entry, EntryId, EntryKind, Error, ExtractSink, Format, Result};

/// Deepest folder chain followed (the Directory table could hold a cycle).
const MAX_DEPTH: usize = 64;

struct Media {
    last_sequence: i64,
    /// `#name` for a stream in the package, `name` for a file beside it.
    cabinet: Option<String>,
}

pub struct MsiBackend {
    path: PathBuf,
    entries: Vec<Entry>,
    /// Per entry: the cabinet holding it (index into `media`) and its key
    /// (the file's name inside the cabinet).
    keys: Vec<(Option<usize>, String)>,
    media: Vec<Media>,
    len: u64,
}

fn map_err(e: std::io::Error) -> Error {
    Error::from_read(e)
}

/// "SHORT~1|Long name:source" -> Some("Long name"); "." -> None (same folder).
fn target_name(default_dir: &str) -> Option<String> {
    let target = default_dir.split(':').next().unwrap_or(default_dir);
    let long = target.rsplit('|').next().unwrap_or(target);
    (!long.is_empty() && long != ".").then(|| long.to_owned())
}

/// "SHORT~1.TXT|Long name.txt" -> "Long name.txt".
fn long_name(file_name: &str) -> &str {
    file_name.rsplit('|').next().unwrap_or(file_name)
}

fn text(row: &msi::Row, col: &str) -> Option<String> {
    row[col].as_str().map(str::to_owned)
}

impl MsiBackend {
    pub fn open(path: &Path) -> Result<MsiBackend> {
        let (file, len) = open_file(path)?;
        let mut pkg = Package::open(file).map_err(map_err)?;
        if pkg.package_type() != PackageType::Installer {
            return Err(Error::Unsupported("Windows Installer patch or transform".into()));
        }

        // Directory -> (parent, name in the target tree).
        let mut dirs: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
        if pkg.has_table("Directory") {
            for row in pkg.select_rows(Select::table("Directory")).map_err(map_err)? {
                let Some(id) = text(&row, "Directory") else { continue };
                let parent = text(&row, "Directory_Parent").filter(|p| *p != id);
                let name = if parent.is_none() { None } else { text(&row, "DefaultDir").and_then(|d| target_name(&d)) };
                dirs.insert(id, (parent, name));
            }
        }
        let mut dir_path: HashMap<String, String> = HashMap::new();
        let mut resolve = |id: &str| -> String {
            if let Some(p) = dir_path.get(id) {
                return p.clone();
            }
            let mut parts = Vec::new();
            let mut cur = Some(id.to_owned());
            for _ in 0..MAX_DEPTH {
                let Some(c) = cur else { break };
                let Some((parent, name)) = dirs.get(&c) else { break };
                if let Some(n) = name {
                    parts.push(n.clone());
                }
                cur = parent.clone();
            }
            parts.reverse();
            let p = parts.join("/");
            dir_path.insert(id.to_owned(), p.clone());
            p
        };

        let mut component_dir: HashMap<String, String> = HashMap::new();
        if pkg.has_table("Component") {
            for row in pkg.select_rows(Select::table("Component")).map_err(map_err)? {
                if let (Some(c), Some(d)) = (text(&row, "Component"), text(&row, "Directory_")) {
                    component_dir.insert(c, d);
                }
            }
        }

        let mut media = Vec::new();
        if pkg.has_table("Media") {
            for row in pkg.select_rows(Select::table("Media")).map_err(map_err)? {
                let last_sequence = row["LastSequence"].as_int().map_or(0, i64::from);
                media.push(Media { last_sequence, cabinet: text(&row, "Cabinet").filter(|c| !c.is_empty()) });
            }
        }
        media.sort_by_key(|m| m.last_sequence);

        let mut entries = Vec::new();
        let mut keys = Vec::new();
        if pkg.has_table("File") {
            for row in pkg.select_rows(Select::table("File")).map_err(map_err)? {
                let (Some(key), Some(name)) = (text(&row, "File"), text(&row, "FileName")) else { continue };
                let folder = text(&row, "Component_").and_then(|c| component_dir.get(&c).cloned());
                let folder = folder.map(|d| resolve(&d)).unwrap_or_default();
                let path = if folder.is_empty() { long_name(&name).to_owned() } else { format!("{folder}/{}", long_name(&name)) };
                let sequence = row["Sequence"].as_int().map_or(0, i64::from);
                let cabinet = media.iter().position(|m| m.last_sequence >= sequence).filter(|&m| media[m].cabinet.is_some());
                let mut e = Entry::new(entries.len(), &path, EntryKind::File);
                e.size = row["FileSize"].as_int().map(|s| u64::from(s as u32));
                entries.push(e);
                keys.push((cabinet, key));
            }
        }
        Ok(MsiBackend { path: path.to_owned(), entries, keys, media, len })
    }

    /// Extracts the wanted entries held by one cabinet.
    fn extract_cabinet<R: Read + Seek>(
        &self,
        mut reader: R,
        wanted: &[usize],
        pump: &mut Pump,
        sink: &mut dyn ExtractSink,
    ) -> Result<()> {
        let cab = Cab::parse(&mut reader)?;
        let by_name: HashMap<&str, usize> = cab.files.iter().enumerate().map(|(i, f)| (f.name.as_str(), i)).collect();
        let mut to_entry = HashMap::new();
        let mut files = Vec::with_capacity(wanted.len());
        for &id in wanted {
            let key = self.keys[id].1.as_str();
            let i = by_name
                .get(key)
                .copied()
                .or_else(|| cab.files.iter().position(|f| f.name.eq_ignore_ascii_case(key)))
                .ok_or_else(|| Error::Corrupt(format!("{key} is missing from its cabinet")))?;
            to_entry.insert(i, id);
            files.push(i);
        }
        let entries = &self.entries;
        cab.extract(&mut reader, &files, |i, data| pump.run(&entries[to_entry[&i]], data, sink, &Error::from_read))
    }
}

impl ArchiveReader for MsiBackend {
    fn format(&self) -> Format {
        Format::Msi
    }

    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn set_password(&mut self, _password: Option<&str>) {}

    fn extract(&mut self, ids: &[EntryId], sink: &mut dyn ExtractSink, ctl: &Control) -> Result<()> {
        let sel = Selection::new(ids, &self.entries, ctl)?;
        let mut by_cabinet: Vec<Vec<usize>> = vec![Vec::new(); self.media.len()];
        for id in (0..self.entries.len()).filter(|&i| sel.contains(i)) {
            match self.keys[id].0 {
                Some(m) => by_cabinet[m].push(id),
                None => return Err(Error::Unsupported("file not stored in the package".into())),
            }
        }
        let mut pump = Pump::new(ctl, self.len);
        let mut pkg: Option<Package<std::io::BufReader<Source>>> = None;
        for (m, wanted) in by_cabinet.iter().enumerate().filter(|(_, w)| !w.is_empty()) {
            let name = self.media[m].cabinet.as_deref().unwrap_or_default();
            if let Some(stream) = name.strip_prefix('#') {
                if pkg.is_none() {
                    pkg = Some(Package::open(open_file(&self.path)?.0).map_err(map_err)?);
                }
                let reader = pkg.as_mut().unwrap().read_stream(stream).map_err(map_err)?;
                self.extract_cabinet(reader, wanted, &mut pump, sink)?;
            } else {
                let beside = self.path.with_file_name(name);
                if !beside.is_file() {
                    return Err(Error::MissingVolume(name.to_owned()));
                }
                let (file, _) = open_file(&beside)?;
                self.extract_cabinet(file, wanted, &mut pump, sink)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(target_name("PROGRA~1|Program Files:PFiles").as_deref(), Some("Program Files"));
        assert_eq!(target_name(".:Src"), None);
        assert_eq!(target_name("bin").as_deref(), Some("bin"));
        assert_eq!(long_name("README~1.TXT|Read me.txt"), "Read me.txt");
        assert_eq!(long_name("a.dll"), "a.dll");
    }
}
