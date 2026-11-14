//! Kibotós: a fast, lightweight archive manager for Windows.

#![windows_subsystem = "windows"]

mod cli;
mod dialogs;
mod format;
mod icons;
mod integrate;
mod job;
mod progress;
mod strings;
mod temp;
mod timing;
mod window;

use std::path::PathBuf;

/// UTF-16, NUL-terminated, for Win32 calls.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The chest icon embedded by build.rs (resource id 1), at the system's
/// large or small icon size.
pub fn app_icon(small: bool) -> windows::Win32::UI::WindowsAndMessaging::HICON {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let (cx, cy) = if small {
            (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON))
        } else {
            (GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON))
        };
        LoadImageW(Some(hinst.into()), windows::core::PCWSTR(1 as _), IMAGE_ICON, cx, cy, LR_SHARED)
            .map(|h| HICON(h.0))
            .unwrap_or_default()
    }
}

fn main() {
    timing::mark("main", "");
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    // "--extract-here" and friends come from the Explorer context menu and
    // never open the main window.
    if args.first().and_then(|a| a.to_str()).is_some_and(|a| a.starts_with("--")) {
        std::process::exit(cli::run(&args));
    }
    // Otherwise the first argument, if any, is an archive to open (Explorer
    // passes "%1"; dragging a file onto the .exe does the same).
    window::run(args.into_iter().next().map(PathBuf::from));
}
