//! Kibotós: a fast, lightweight archive manager for Windows.

#![windows_subsystem = "windows"]

mod cli;
mod collect;
mod compress;
mod dialogs;
mod drag;
mod file_types;
mod format;
mod fsview;
mod icons;
mod install;
mod integrate;
mod job;
mod progress;
mod strings;
mod task;
mod temp;
mod timing;
mod update;
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

/// A panic aborts the process (panic = "abort"), which for a window app
/// means it just vanishes. Record what failed in `%TEMP%\Kibotos\crash.log`
/// and tell the user before that happens.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let place = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let what = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        let args: Vec<String> = std::env::args().skip(1).collect();
        let line = format!("v{} thread '{thread}' at {place}: {what}
  args: {args:?}
", env!("CARGO_PKG_VERSION"));
        let log = std::env::temp_dir().join("Kibotos").join("crash.log");
        let _ = std::fs::create_dir_all(log.parent().unwrap_or(&log));
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
            use std::io::Write;
            let _ = f.write_all(line.as_bytes());
        }
        let text = strings::crash_message(&format!("{what}
({place})"), &log.display().to_string());
        // On a thread of its own: a message box on the panicking thread can
        // return at once (e.g. when the panic happened while painting).
        let shown = std::thread::spawn(move || {
            use windows::Win32::UI::WindowsAndMessaging::*;
            unsafe {
                MessageBoxW(
                    None,
                    &windows::core::HSTRING::from(text),
                    &windows::core::HSTRING::from(strings::t::CRASH_TITLE),
                    MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
                );
            }
        });
        let _ = shown.join();
    }));
}

fn main() {
    install_panic_hook();
    timing::mark("main", "");
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    // "--extract-here" and friends come from the Explorer context menu and
    // never open the main window.
    if args.first().and_then(|a| a.to_str()).is_some_and(|a| a.starts_with("--")) {
        std::process::exit(cli::run(&args));
    }
    // Otherwise the first argument, if any, is an archive or folder to open
    // (Explorer passes "%1"; dragging a file onto the .exe does the same).
    // Without one, the window starts in "Este equipo".
    window::run(args.into_iter().next().filter(|a| !a.is_empty()).map(PathBuf::from));
}
