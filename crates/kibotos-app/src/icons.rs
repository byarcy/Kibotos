//! Row icons from the system image list, cached per file extension (not
//! per row): 100,000 `.txt` files cost one shell lookup.

use std::collections::HashMap;

use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES};
use windows::Win32::UI::Controls::HIMAGELIST;
use windows::Win32::UI::Shell::{SHFILEINFOW, SHGFI_SMALLICON, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW};
use windows::core::PCWSTR;

use crate::wide;

#[derive(Default)]
pub struct IconCache {
    by_ext: HashMap<String, i32>,
    folder: Option<i32>,
}

fn lookup(name: &str, attrs: FILE_FLAGS_AND_ATTRIBUTES) -> (i32, HIMAGELIST) {
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
    (info.iIcon, HIMAGELIST(list as _))
}

impl IconCache {
    /// The system small-icon image list (shared, never destroyed).
    pub fn system_list() -> HIMAGELIST {
        lookup("x", FILE_ATTRIBUTE_NORMAL).1
    }

    pub fn folder(&mut self) -> i32 {
        *self.folder.get_or_insert_with(|| lookup("carpeta", FILE_ATTRIBUTE_DIRECTORY).0)
    }

    pub fn file(&mut self, name: &str) -> i32 {
        let ext = match name.rfind('.') {
            Some(i) if i + 1 < name.len() => name[i + 1..].to_lowercase(),
            _ => String::new(),
        };
        if let Some(&i) = self.by_ext.get(&ext) {
            return i;
        }
        let i = lookup(&format!("x.{ext}"), FILE_ATTRIBUTE_NORMAL).0;
        self.by_ext.insert(ext, i);
        i
    }
}
