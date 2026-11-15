//! Explorer runs a context-menu command once per selected item: selecting
//! five files and choosing "Comprimir en ZIP" starts five processes. To
//! make one archive of them, the first process becomes a collector: the
//! others hand it their paths (WM_COPYDATA) and exit, and it goes on once
//! no new path has arrived for a short while.
//!
//! Processes meet through a named mutex and a message-only window whose
//! name includes the command and the folder (items selected together in
//! Explorer always share their folder).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, w};

/// How long to wait for more paths after the last one arrived.
const QUIET: Duration = Duration::from_millis(400);
/// Upper bound on the whole collection.
const LIMIT: Duration = Duration::from_secs(20);
/// How long a sender looks for the collector's window.
const FIND: Duration = Duration::from_secs(5);
const MAGIC: usize = 0x4B49_424F; // "KIBO"

thread_local! {
    static GOT: RefCell<(Vec<PathBuf>, Option<Instant>)> = const { RefCell::new((Vec::new(), None)) };
}

extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_COPYDATA {
            let cds = &*(lp.0 as *const COPYDATASTRUCT);
            if cds.dwData == MAGIC && !cds.lpData.is_null() {
                let units = std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2);
                GOT.with(|g| {
                    let mut g = g.borrow_mut();
                    for part in units.split(|&c| c == 0).filter(|p| !p.is_empty()) {
                        g.0.push(PathBuf::from(String::from_utf16_lossy(part)));
                    }
                    g.1 = Some(Instant::now());
                });
                return LRESULT(1);
            }
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

/// A name shared by the processes of one Explorer selection.
fn rendezvous(command: &str, first: &Path) -> String {
    let folder = first.parent().map(|p| p.to_string_lossy().to_lowercase()).unwrap_or_default();
    // FNV-1a: stable across processes (unlike `DefaultHasher` seeds).
    let hash = folder.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    format!("Kibotos.Collect.{command}.{hash:016x}")
}

fn send(window: HWND, paths: &[PathBuf]) -> bool {
    let mut data: Vec<u16> = Vec::new();
    for p in paths {
        data.extend(p.as_os_str().to_string_lossy().encode_utf16());
        data.push(0);
    }
    let cds = COPYDATASTRUCT { dwData: MAGIC, cbData: (data.len() * 2) as u32, lpData: data.as_ptr() as *mut _ };
    let mut result = 0usize;
    unsafe {
        SendMessageTimeoutW(window, WM_COPYDATA, WPARAM(0), LPARAM(&cds as *const _ as isize), SMTO_ABORTIFHUNG, 5000, Some(&mut result))
            .0
            != 0
            && result == 1
    }
}

/// Gathers the items of one Explorer selection. Returns all the paths in
/// the process that should do the work, and `None` in the others (they
/// handed their paths over and should exit).
pub fn gather(command: &str, mine: Vec<PathBuf>) -> Option<Vec<PathBuf>> {
    let Some(first) = mine.first().cloned() else { return Some(mine) };
    let name = rendezvous(command, &first);
    let hname = HSTRING::from(name.as_str());
    let class = w!("KibotosCollector");
    unsafe {
        let mutex = CreateMutexW(None, true, &hname).ok();
        let owner = mutex.is_some() && GetLastError() != ERROR_ALREADY_EXISTS;
        if !owner {
            // Someone else collects: find its window and hand the paths over.
            let start = Instant::now();
            while start.elapsed() < FIND {
                if let Ok(win) = FindWindowExW(Some(HWND_MESSAGE), None, class, &hname) {
                    if send(win, &mine) {
                        if let Some(m) = mutex {
                            let _ = CloseHandle(m);
                        }
                        return None;
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            // No collector answered: do this part alone.
            if let Some(m) = mutex {
                let _ = CloseHandle(m);
            }
            return Some(mine);
        }

        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc),
            hInstance: hinst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let win = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            &hname,
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinst.into()),
            None,
        );
        GOT.with(|g| *g.borrow_mut() = (mine, Some(Instant::now())));
        if let Ok(win) = win {
            let start = Instant::now();
            let timer = SetTimer(None, 0, 50, None);
            let mut msg = MSG::default();
            loop {
                let last = GOT.with(|g| g.borrow().1).unwrap_or(start);
                if last.elapsed() >= QUIET || start.elapsed() >= LIMIT {
                    break;
                }
                if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let _ = KillTimer(None, timer);
            let _ = DestroyWindow(win);
            // Late senders still in SendMessage get no answer and fall back
            // to working alone.
        }
        if let Some(m) = mutex {
            let _ = CloseHandle(m);
        }
        let mut all = GOT.with(|g| std::mem::take(&mut g.borrow_mut().0));
        all.sort();
        all.dedup();
        Some(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendezvous_depends_on_folder_not_file() {
        let a = rendezvous("zip", Path::new(r"C:\Fotos\a.jpg"));
        assert_eq!(a, rendezvous("zip", Path::new(r"c:\fotos\b.jpg")));
        assert_ne!(a, rendezvous("7z", Path::new(r"C:\Fotos\a.jpg")));
        assert_ne!(a, rendezvous("zip", Path::new(r"C:\Otra\a.jpg")));
    }
}
