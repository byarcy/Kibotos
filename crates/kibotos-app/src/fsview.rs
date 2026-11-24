//! The disk side of the window: "Este equipo" (known folders and drives)
//! and folder listings. Listings are read on a worker thread; the UI only
//! sorts and filters the resulting `Vec<Item>`.

use std::cmp::Ordering;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use kibotos_core::Timestamp;
use kibotos_core::view::{Sort, SortKey};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM, FILE_FLAGS_AND_ATTRIBUTES, GetDiskFreeSpaceExW, GetLogicalDrives,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, KF_FLAG_DEFAULT, SHFILEINFOW, SHGFI_DISPLAYNAME,
    SHGFI_SMALLICON, SHGFI_SYSICONINDEX, SHGetFileInfoW, SHGetKnownFolderPath,
};
use windows::core::{GUID, HSTRING, PCWSTR};

use crate::strings::{self, t};

/// Where the window is, outside an archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// Known folders and drives.
    Computer,
    Folder(PathBuf),
}

impl Place {
    /// The place one level up (a drive root goes to `Computer`).
    pub fn parent(&self) -> Option<Place> {
        match self {
            Place::Computer => None,
            Place::Folder(p) => Some(p.parent().map_or(Place::Computer, |q| Place::Folder(q.to_path_buf()))),
        }
    }

    pub fn shown(&self) -> String {
        match self {
            Place::Computer => t::COMPUTER.to_string(),
            Place::Folder(p) => p.display().to_string(),
        }
    }

    /// Name for the window title.
    pub fn title(&self) -> String {
        match self {
            Place::Computer => t::COMPUTER.to_string(),
            Place::Folder(p) => p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A drive or known folder in "Este equipo".
    Place,
    Dir,
    File,
}

pub struct Item {
    pub name: String,
    lower: String,
    pub kind: Kind,
    pub path: PathBuf,
    pub size: Option<u64>,
    pub modified: Option<Timestamp>,
    /// Second column when it is not a byte count (a drive's capacity).
    pub size_text: String,
    /// Third column: the type, or a drive's free space.
    pub extra: String,
    /// System image list index, or -1 to look the icon up by extension.
    pub icon: i32,
    /// Opens inside Kibotós (by extension).
    pub archive: bool,
}

impl Item {
    pub fn is_container(&self) -> bool {
        self.kind != Kind::File
    }
}

pub struct Listing {
    pub place: Place,
    pub items: Vec<Item>,
    pub summary: String,
    /// Icons looked up on the worker: (folder icon, icons by extension).
    pub icons: (Option<i32>, Vec<(String, i32)>),
}

/// Extensions Kibotós opens itself (others open with their program).
pub fn is_archive_name(name: &str) -> bool {
    let ext = crate::icons::extension(name);
    crate::integrate::EXTENSIONS.contains(&ext.as_str()) || kibotos_core::is_part_name(name)
}

fn shell_info(path: &Path, flags: windows::Win32::UI::Shell::SHGFI_FLAGS) -> SHFILEINFOW {
    let w = HSTRING::from(path.as_os_str());
    let mut info = SHFILEINFOW::default();
    unsafe {
        SHGetFileInfoW(
            PCWSTR(w.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        );
    }
    info
}

fn display_name(info: &SHFILEINFOW) -> String {
    let n = info.szDisplayName.iter().position(|&c| c == 0).unwrap_or(info.szDisplayName.len());
    String::from_utf16_lossy(&info.szDisplayName[..n])
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let path = p.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(p.0 as _));
        path
    }
}

fn place_item(path: PathBuf, size_text: String, extra: String) -> Item {
    let info = shell_info(&path, SHGFI_DISPLAYNAME | SHGFI_SYSICONINDEX | SHGFI_SMALLICON);
    let mut name = display_name(&info);
    if name.is_empty() {
        name = path.display().to_string();
    }
    Item {
        lower: name.to_lowercase(),
        name,
        kind: Kind::Place,
        path,
        size: None,
        modified: None,
        size_text,
        extra,
        icon: info.iIcon,
        archive: false,
    }
}

fn list_computer() -> Vec<Item> {
    let mut items = Vec::new();
    for id in [&FOLDERID_Desktop, &FOLDERID_Downloads, &FOLDERID_Documents] {
        if let Some(p) = known_folder(id).filter(|p| p.is_dir()) {
            items.push(place_item(p, String::new(), t::KNOWN_FOLDER.to_string()));
        }
    }
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u8 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let root = PathBuf::from(format!("{}:\\", (b'A' + i) as char));
        let (mut free, mut total) = (0u64, 0u64);
        let known = unsafe {
            GetDiskFreeSpaceExW(&HSTRING::from(root.as_os_str()), Some(&mut free), Some(&mut total), None).is_ok()
        };
        let (size_text, extra) =
            if known { (strings::bytes(total), strings::drive_free(free)) } else { (String::new(), String::new()) };
        items.push(place_item(root, size_text, extra));
    }
    items
}

fn type_name(name: &str, is_dir: bool) -> String {
    if is_dir {
        return t::PROP_FOLDER.to_string();
    }
    if is_archive_name(name) {
        return t::TYPE_ARCHIVE.to_string();
    }
    match crate::icons::extension(name) {
        e if e.is_empty() => t::PROP_FILE.to_string(),
        e => format!("{} {}", t::PROP_FILE, e.to_uppercase()),
    }
}

/// Seconds since 1970 (UTC) from a FILETIME value.
fn filetime(ft: u64) -> Option<Timestamp> {
    (ft > 0).then(|| Timestamp::Utc((ft / 10_000_000) as i64 - 11_644_473_600))
}

/// Icons whose look depends on the file itself, not only its extension.
const OWN_ICON: &[&str] = &["exe", "lnk", "ico", "url", "msi", "scr", "cpl"];

fn list_folder(dir: &Path) -> std::io::Result<Vec<Item>> {
    let mut items = Vec::new();
    let mut own_icons = 0;
    for e in std::fs::read_dir(dir)? {
        let Ok(e) = e else { continue };
        // On Windows this comes from the directory listing: no extra I/O.
        let Ok(md) = e.metadata() else { continue };
        if md.file_attributes() & (FILE_ATTRIBUTE_HIDDEN.0 | FILE_ATTRIBUTE_SYSTEM.0) != 0 {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let is_dir = md.is_dir();
        let path = e.path();
        let mut icon = -1;
        if !is_dir && own_icons < 500 && OWN_ICON.contains(&crate::icons::extension(&name).as_str()) {
            own_icons += 1;
            icon = shell_info(&path, SHGFI_SYSICONINDEX | SHGFI_SMALLICON).iIcon;
        }
        items.push(Item {
            lower: name.to_lowercase(),
            size_text: String::new(),
            extra: type_name(&name, is_dir),
            archive: !is_dir && is_archive_name(&name),
            kind: if is_dir { Kind::Dir } else { Kind::File },
            size: (!is_dir).then(|| md.file_size()),
            modified: filetime(md.last_write_time()),
            name,
            path,
            icon,
        });
    }
    Ok(items)
}

/// Reads a place. Runs on a worker thread (with COM initialized).
pub fn list(place: Place) -> std::io::Result<Listing> {
    let items = match &place {
        Place::Computer => list_computer(),
        Place::Folder(dir) => list_folder(dir)?,
    };
    let mut exts: Vec<String> =
        items.iter().filter(|i| i.kind == Kind::File && i.icon < 0).map(|i| crate::icons::extension(&i.name)).collect();
    exts.sort_unstable();
    exts.dedup();
    exts.truncate(256);
    let icons = crate::icons::prefetch(exts);
    let files = items.iter().filter(|i| i.kind == Kind::File).count();
    let dirs = items.len() - files;
    let bytes: u64 = items.iter().filter_map(|i| i.size).sum();
    let summary = match place {
        Place::Computer => String::new(),
        Place::Folder(_) => format!("{}  ·  {}", strings::items_summary(files, dirs), strings::bytes(bytes)),
    };
    Ok(Listing { place, items, summary, icons })
}

fn compare(a: &Item, b: &Item, key: SortKey) -> Ordering {
    let by_name = || natural_cmp(&a.lower, &b.lower);
    match key {
        // Files on disk have no stored CRC.
        SortKey::Name | SortKey::Crc => by_name(),
        SortKey::Size => a.size.cmp(&b.size).then_with(by_name),
        SortKey::Packed => a.extra.to_lowercase().cmp(&b.extra.to_lowercase()).then_with(by_name),
        SortKey::Modified => {
            a.modified.map(|t| t.sort_key()).cmp(&b.modified.map(|t| t.sort_key())).then_with(by_name)
        }
    }
}

/// Compares names with digit runs as numbers: "foto2" < "foto10".
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let mut n = String::new();
                while let Some(c) = x.peek().filter(|c| c.is_ascii_digit()) {
                    n.push(*c);
                    x.next();
                }
                let mut m = String::new();
                while let Some(d) = y.peek().filter(|d| d.is_ascii_digit()) {
                    m.push(*d);
                    y.next();
                }
                let (n2, m2) = (n.trim_start_matches('0'), m.trim_start_matches('0'));
                let o = n2.len().cmp(&m2.len()).then_with(|| n2.cmp(m2)).then_with(|| n.len().cmp(&m.len()));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(c), Some(d)) => {
                if c != d {
                    return c.cmp(&d);
                }
                x.next();
                y.next();
            }
        }
    }
}

/// Indices of `items` in display order: drives and folders first, then
/// files, each group sorted by `sort`; only names containing `query`.
pub fn rows(items: &[Item], sort: Sort, query: &str, out: &mut Vec<u32>) {
    let q = query.to_lowercase();
    out.clear();
    out.extend((0..items.len() as u32).filter(|&i| q.is_empty() || items[i as usize].lower.contains(&q)));
    // "Este equipo" keeps the shell's order (known folders, then drives).
    if items.iter().all(|i| i.kind == Kind::Place) {
        return;
    }
    out.sort_by(|&i, &j| {
        let (a, b) = (&items[i as usize], &items[j as usize]);
        b.is_container().cmp(&a.is_container()).then_with(|| {
            let o = compare(a, b, sort.key);
            if sort.descending { o.reverse() } else { o }
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        let mut v = vec!["foto10", "foto2", "foto1", "Foto02", "a", "foto"];
        v.sort_by(|a, b| natural_cmp(&a.to_lowercase(), &b.to_lowercase()));
        assert_eq!(v, ["a", "foto", "foto1", "foto2", "Foto02", "foto10"]);
    }

    #[test]
    fn parents() {
        let p = Place::Folder(PathBuf::from(r"C:\x\y"));
        assert_eq!(p.parent(), Some(Place::Folder(PathBuf::from(r"C:\x"))));
        assert_eq!(Place::Folder(PathBuf::from(r"C:\")).parent(), Some(Place::Computer));
        assert_eq!(Place::Computer.parent(), None);
    }

    #[test]
    fn archive_names() {
        assert!(is_archive_name("a.ZIP"));
        assert!(is_archive_name("b.tar.gz"));
        assert!(!is_archive_name("c.txt"));
    }
}
