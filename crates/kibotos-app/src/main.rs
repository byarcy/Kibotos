//! Kibotós: a fast, lightweight archive manager for Windows.

#![windows_subsystem = "windows"]

mod dialogs;
mod format;
mod icons;
mod progress;
mod strings;
mod timing;
mod window;

use std::path::PathBuf;

/// UTF-16, NUL-terminated, for Win32 calls.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn main() {
    timing::mark("main", "");
    // The first argument, if any, is an archive to open (Explorer passes
    // "%1"; dragging a file onto the .exe does the same).
    let archive = std::env::args_os().nth(1).map(PathBuf::from);
    window::run(archive);
}
