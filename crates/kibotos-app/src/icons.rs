//! Row icons from the system image list, cached per file extension (not
//! per row): 100,000 `.txt` files cost one shell lookup.

use std::collections::HashMap;

use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES};
use windows::Win32::UI::Controls::{HIMAGELIST, ILD_NORMAL, IImageList, ImageList_GetIcon};
use windows::Win32::UI::WindowsAndMessaging::DestroyIcon;
use windows::Win32::UI::Shell::{
    SHFILEINFOW, SHGFI_SMALLICON, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW, SHGetImageList, SHIL_SMALL,
};
use windows::core::{Interface, PCWSTR};

use crate::wide;

#[derive(Default)]
pub struct IconCache {
    by_ext: HashMap<String, i32>,
    folder: Option<i32>,
}

/// Icon index and image list for a name, or `None` if the shell failed.
/// It can fail while another thread is setting up the system image list,
/// so a failure must not be cached.
fn lookup(name: &str, attrs: FILE_FLAGS_AND_ATTRIBUTES) -> Option<(i32, HIMAGELIST)> {
    let w = wide(name);
    let mut info = SHFILEINFOW::default();
    // SAFETY: `w` is NUL-terminated; SHGFI_USEFILEATTRIBUTES means the file
    // does not need to exist, so this never touches the disk.
    let list = unsafe {
        SHGetFileInfoW(
            PCWSTR(w.as_ptr()),
            attrs,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_USEFILEATTRIBUTES | SHGFI_SYSICONINDEX | SHGFI_SMALLICON,
        )
    };
    (list != 0).then_some((info.iIcon, HIMAGELIST(list as _)))
}

impl IconCache {
    /// The system small-icon image list (shared, never destroyed).
    /// `None` if the shell could not provide it yet (try again later).
    pub fn system_list() -> Option<HIMAGELIST> {
        // SAFETY: SHGetImageList returns the process-wide list; it is kept
        // for the life of the process, so the reference is never released.
        if let Ok(list) = unsafe { SHGetImageList::<IImageList>(SHIL_SMALL as i32) } {
            let handle = HIMAGELIST(list.as_raw() as _);
            std::mem::forget(list);
            return Some(handle);
        }
        lookup("x", FILE_ATTRIBUTE_NORMAL).map(|(_, list)| list)
    }

    pub fn folder(&mut self) -> i32 {
        if let Some(i) = self.folder {
            return i;
        }
        let i = lookup("carpeta", FILE_ATTRIBUTE_DIRECTORY).map(|(i, _)| i);
        self.folder = i;
        i.unwrap_or(0)
    }

    pub fn file(&mut self, name: &str) -> i32 {
        let ext = extension(name);
        if let Some(&i) = self.by_ext.get(&ext) {
            return i;
        }
        match file_icon(&ext) {
            Some(i) => {
                self.by_ext.insert(ext, i);
                i
            }
            None => 0,
        }
    }

    /// Adds icons looked up in advance on another thread.
    pub fn warm(&mut self, folder: Option<i32>, by_ext: Vec<(String, i32)>) {
        if self.folder.is_none() {
            self.folder = folder;
        }
        for (ext, i) in by_ext {
            self.by_ext.entry(ext).or_insert(i);
        }
    }
}

/// Lowercase extension without the dot ("" if none).
pub fn extension(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i + 1 < name.len() => name[i + 1..].to_lowercase(),
        _ => String::new(),
    }
}

fn file_icon(ext: &str) -> Option<i32> {
    lookup(&format!("x.{ext}"), FILE_ATTRIBUTE_NORMAL).map(|(i, _)| i)
}

/// Looks up the folder icon and the icons for `exts`. Meant for a worker
/// thread: the first lookup of a file type can take tens of milliseconds
/// while the shell extracts its icon into the (process-wide) image list.
pub fn prefetch(exts: Vec<String>) -> (Option<i32>, Vec<(String, i32)>) {
    let Some((folder, list)) = lookup("carpeta", FILE_ATTRIBUTE_DIRECTORY) else {
        return (None, Vec::new());
    };
    warm_bitmap(list, folder);
    let icons = exts
        .into_iter()
        .filter_map(|e| {
            let i = file_icon(&e)?;
            warm_bitmap(list, i);
            Some((e, i))
        })
        .collect();
    (Some(folder), icons)
}

/// The system image list extracts an icon's bitmap lazily, the first time it
/// is drawn. Asking for the icon here does that work off the UI thread.
fn warm_bitmap(list: HIMAGELIST, i: i32) {
    // SAFETY: `list` is the system image list and `i` an index it returned.
    unsafe {
        let icon = ImageList_GetIcon(list, i, ILD_NORMAL);
        if !icon.is_invalid() {
            let _ = DestroyIcon(icon);
        }
    }
}
