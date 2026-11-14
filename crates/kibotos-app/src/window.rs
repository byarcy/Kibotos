//! The main window: a virtual ListView (LVS_OWNERDATA) plus a status bar.
//!
//! The list holds no items. It only knows the row count and asks for the
//! text and icon of the rows it is about to paint (LVN_GETDISPINFOW), so
//! 100,000 entries cost the same as 100.
//!
//! Re-entrancy rule: state lives in a thread-local `RefCell`. Never hold the
//! borrow while sending a message to a child window, because the child may
//! synchronously send WM_NOTIFY back to `wndproc`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use kibotos_core::{ArchiveIndex, ArchiveReader, Entry, EntryKind, NodeId, OpenOptions};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    COLOR_WINDOW, CreateFontIndirectW, DeleteObject, HBRUSH, HFONT, HGDIOBJ, InvalidateRect, UpdateWindow,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::Shell::{DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, PWSTR, w};

use crate::icons::IconCache;
use crate::strings::{self, t};
use crate::{format, timing, wide};

const WM_APP_LOADED: u32 = WM_APP + 1;

const ID_OPEN: u16 = 101;
const ID_EXIT: u16 = 102;
const ID_ABOUT: u16 = 201;
const ID_LIST: usize = 1000;

const SEL_FOCUS: LIST_VIEW_ITEM_STATE_FLAGS = LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0);

/// Column titles, base widths at 96 DPI, right-aligned.
const COLUMNS: [(&str, i32, bool); 4] = [
    (t::COL_NAME, 420, false),
    (t::COL_SIZE, 110, true),
    (t::COL_PACKED, 110, true),
    (t::COL_MODIFIED, 140, false),
];

struct Loaded {
    path: PathBuf,
    reader: Box<dyn ArchiveReader>,
    index: ArchiveIndex,
    /// Nodes shown in the list, in display order.
    rows: Vec<NodeId>,
    open_ms: f64,
}

struct LoadMsg {
    generation: u64,
    path: PathBuf,
    result: kibotos_core::Result<Loaded>,
}

#[derive(Default)]
struct App {
    list: HWND,
    status: HWND,
    font: HFONT,
    archive: Option<Loaded>,
    /// Bumped on every open; results of older opens are ignored.
    generation: u64,
    icons: IconCache,
    image_list_set: bool,
}

thread_local! {
    static APP: RefCell<App> = RefCell::new(App::default());
}

/// COM is needed by the shell (icons, dialogs) but not to show the window,
/// so it is initialized on first use.
fn ensure_com() {
    thread_local!(static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
    DONE.with(|d| {
        if !d.replace(true) {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
            }
        }
    });
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> R {
    APP.with(|a| f(&mut a.borrow_mut()))
}

fn scale(v: i32, dpi: u32) -> i32 {
    v * dpi as i32 / 96
}

fn loword(v: usize) -> u16 {
    (v & 0xFFFF) as u16
}

pub fn run(archive: Option<PathBuf>) {
    unsafe {
        // Without this, the first ShowWindow loads the text-services/IME
        // stack (~20 ms, a fifth of the startup budget). Spanish and other
        // Latin keyboards, dead keys and AltGr work without an IME; Chinese,
        // Japanese or Korean input methods are not available in Kibotós.
        let _ = windows::Win32::UI::Input::Ime::ImmDisableIME(u32::MAX);
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_BAR_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);

        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("KibotosMain");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as _),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let dpi = GetDpiForSystem();
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_ACCEPTFILES,
            class,
            &HSTRING::from(t::APP_NAME),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            scale(980, dpi),
            scale(640, dpi),
            None,
            Some(build_menu()),
            Some(hinst.into()),
            None,
        ) else {
            return;
        };

        // Start reading the archive before the window is painted, so disk
        // I/O overlaps with window creation.
        if let Some(path) = archive {
            start_load(hwnd, path);
        }

        let _ = ShowWindow(hwnd, SW_SHOWDEFAULT);
        let _ = UpdateWindow(hwnd);

        let accel = CreateAcceleratorTableW(&[ACCEL {
            fVirt: FCONTROL | FVIRTKEY,
            key: b'O' as u16,
            cmd: ID_OPEN,
        }])
        .unwrap_or_default();
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if TranslateAcceleratorW(hwnd, accel, &msg) == 0 {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

unsafe fn build_menu() -> HMENU {
    unsafe {
        let bar = CreateMenu().unwrap_or_default();
        let file = CreatePopupMenu().unwrap_or_default();
        let _ = AppendMenuW(file, MF_STRING, ID_OPEN as usize, &HSTRING::from(t::MENU_OPEN));
        let _ = AppendMenuW(file, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(file, MF_STRING, ID_EXIT as usize, &HSTRING::from(t::MENU_EXIT));
        let help = CreatePopupMenu().unwrap_or_default();
        let _ = AppendMenuW(help, MF_STRING, ID_ABOUT as usize, &HSTRING::from(t::MENU_ABOUT));
        let _ = AppendMenuW(bar, MF_POPUP, file.0 as usize, &HSTRING::from(t::MENU_FILE));
        let _ = AppendMenuW(bar, MF_POPUP, help.0 as usize, &HSTRING::from(t::MENU_HELP));
        bar
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    static PAINTED: AtomicBool = AtomicBool::new(false);
    unsafe {
        match msg {
            WM_CREATE => {
                on_create(hwnd);
                LRESULT(0)
            }
            WM_SIZE => {
                layout(hwnd);
                LRESULT(0)
            }
            WM_PAINT => {
                let r = DefWindowProcW(hwnd, msg, wp, lp);
                if !PAINTED.swap(true, Ordering::Relaxed) {
                    timing::mark("window visible", "");
                }
                r
            }
            WM_NOTIFY => on_notify(lp),
            WM_COMMAND => {
                on_command(hwnd, loword(wp.0));
                LRESULT(0)
            }
            WM_DROPFILES => {
                on_drop(hwnd, HDROP(wp.0 as _));
                LRESULT(0)
            }
            WM_APP_LOADED => {
                on_loaded(hwnd, lp);
                LRESULT(0)
            }
            WM_SETFOCUS => {
                let list = with_app(|a| a.list);
                let _ = SetFocus(Some(list));
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let r = &*(lp.0 as *const RECT);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                apply_dpi(hwnd, (wp.0 >> 16) as u32 & 0xFFFF);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe fn ui_font(dpi: u32) -> HFONT {
    unsafe {
        let mut ncm = NONCLIENTMETRICSW { cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
        let _ = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            ncm.cbSize,
            Some(&mut ncm as *mut _ as *mut _),
            0,
            dpi,
        );
        CreateFontIndirectW(&ncm.lfMessageFont)
    }
}

unsafe fn on_create(hwnd: HWND) {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let list_style = WS_CHILD
            | WS_VISIBLE
            | WS_TABSTOP
            | WINDOW_STYLE(LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS | LVS_SHAREIMAGELISTS);
        let list = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            WC_LISTVIEWW,
            PCWSTR::null(),
            list_style,
            0,
            0,
            0,
            0,
            Some(hwnd),
            Some(HMENU(ID_LIST as _)),
            Some(hinst.into()),
            None,
        )
        .unwrap_or_default();
        let status = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            STATUSCLASSNAMEW,
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(SBARS_SIZEGRIP),
            0,
            0,
            0,
            0,
            Some(hwnd),
            None,
            Some(hinst.into()),
            None,
        )
        .unwrap_or_default();

        let _ = SetWindowTheme(list, w!("Explorer"), PCWSTR::null());
        let ex = LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_HEADERDRAGDROP;
        SendMessageW(list, LVM_SETEXTENDEDLISTVIEWSTYLE, Some(WPARAM(ex as usize)), Some(LPARAM(ex as isize)));

        for (i, (title, _, right)) in COLUMNS.iter().enumerate() {
            let mut text = wide(title);
            let col = LVCOLUMNW {
                mask: LVCF_TEXT | LVCF_WIDTH | LVCF_FMT | LVCF_SUBITEM,
                fmt: if *right { LVCFMT_RIGHT } else { LVCFMT_LEFT },
                cx: 100,
                pszText: PWSTR(text.as_mut_ptr()),
                iSubItem: i as i32,
                ..Default::default()
            };
            SendMessageW(list, LVM_INSERTCOLUMNW, Some(WPARAM(i)), Some(LPARAM(&col as *const _ as isize)));
        }

        with_app(|a| {
            a.list = list;
            a.status = status;
        });
        apply_dpi(hwnd, GetDpiForWindow(hwnd));
        set_status(t::STATUS_READY);
    }
}

/// Fonts and column widths for the window's current DPI.
unsafe fn apply_dpi(_hwnd: HWND, dpi: u32) {
    unsafe {
        let font = ui_font(dpi);
        let (list, status, old) = with_app(|a| (a.list, a.status, std::mem::replace(&mut a.font, font)));
        for h in [list, status] {
            SendMessageW(h, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
        }
        if !old.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(old.0));
        }
        for (i, (_, width, _)) in COLUMNS.iter().enumerate() {
            SendMessageW(list, LVM_SETCOLUMNWIDTH, Some(WPARAM(i)), Some(LPARAM(scale(*width, dpi) as isize)));
        }
    }
}

unsafe fn layout(hwnd: HWND) {
    unsafe {
        let (list, status) = with_app(|a| (a.list, a.status));
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        SendMessageW(status, WM_SIZE, None, None);
        let mut src = RECT::default();
        let _ = GetWindowRect(status, &mut src);
        let sh = src.bottom - src.top;
        let _ = MoveWindow(list, 0, 0, rc.right, (rc.bottom - sh).max(0), true);
    }
}

fn set_status(text: &str) {
    let status = with_app(|a| a.status);
    let w = wide(text);
    unsafe {
        SendMessageW(status, SB_SETTEXTW, Some(WPARAM(0)), Some(LPARAM(w.as_ptr() as isize)));
    }
}

unsafe fn on_command(hwnd: HWND, id: u16) {
    unsafe {
        match id {
            ID_OPEN => {
                if let Some(path) = open_dialog(hwnd) {
                    start_load(hwnd, path);
                }
            }
            ID_EXIT => {
                let _ = DestroyWindow(hwnd);
            }
            ID_ABOUT => {
                MessageBoxW(
                    Some(hwnd),
                    &HSTRING::from(strings::about_text()),
                    &HSTRING::from(t::ABOUT_TITLE),
                    MB_OK | MB_ICONINFORMATION,
                );
            }
            _ => {}
        }
    }
}

unsafe fn open_dialog(hwnd: HWND) -> Option<PathBuf> {
    ensure_com();
    let patterns = "*.zip;*.jar;*.apk;*.cbz;*.7z;*.rar;*.cbr;*.tar;*.gz;*.tgz;*.bz2;*.tbz;*.tbz2;*.xz;*.txz;*.zst;*.tzst";
    let filter: Vec<u16> = format!("{}\0{patterns}\0{}\0*.*\0\0", t::FILTER_ARCHIVES, t::FILTER_ALL)
        .encode_utf16()
        .collect();
    let title = wide(t::OPEN_TITLE);
    let mut buf = vec![0u16; 32 * 1024];
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: hwnd,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrTitle: PCWSTR(title.as_ptr()),
        Flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY,
        ..Default::default()
    };
    if unsafe { GetOpenFileNameW(&mut ofn) }.as_bool() {
        let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..n])))
    } else {
        None
    }
}

unsafe fn on_drop(hwnd: HWND, drop: HDROP) {
    unsafe {
        let mut buf = vec![0u16; 32 * 1024];
        let n = DragQueryFileW(drop, 0, Some(&mut buf)) as usize;
        DragFinish(drop);
        if n > 0 {
            start_load(hwnd, PathBuf::from(String::from_utf16_lossy(&buf[..n])));
        }
    }
}

/// Folders first, then names, case-insensitive.
fn sorted_children(index: &ArchiveIndex, node: NodeId) -> Vec<NodeId> {
    let mut rows = index.children(node).to_vec();
    rows.sort_by(|&a, &b| {
        index.is_dir(b).cmp(&index.is_dir(a)).then_with(|| index.name_lower(a).cmp(index.name_lower(b)))
    });
    rows
}

/// Opens and indexes an archive on a worker thread. The UI thread stays
/// free; the result arrives as WM_APP_LOADED.
fn start_load(hwnd: HWND, path: PathBuf) {
    let generation = with_app(|a| {
        a.generation += 1;
        a.generation
    });
    set_status(t::STATUS_OPENING);
    let target = hwnd.0 as isize;
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let result = kibotos_core::open(&path, &OpenOptions::default()).map(|reader| {
            let index = ArchiveIndex::build(reader.entries());
            let rows = sorted_children(&index, ArchiveIndex::ROOT);
            Loaded { path: path.clone(), reader, index, rows, open_ms: 0.0 }
        });
        let result = result.map(|mut l| {
            l.open_ms = t0.elapsed().as_secs_f64() * 1000.0;
            l
        });
        let msg = Box::into_raw(Box::new(LoadMsg { generation, path, result }));
        // SAFETY: on success the UI thread takes ownership of `msg`.
        unsafe {
            if PostMessageW(Some(HWND(target as _)), WM_APP_LOADED, WPARAM(0), LPARAM(msg as isize)).is_err() {
                drop(Box::from_raw(msg));
            }
        }
    });
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

unsafe fn on_loaded(hwnd: HWND, lp: LPARAM) {
    unsafe {
        let msg = Box::from_raw(lp.0 as *mut LoadMsg);
        let (list, current, has_images) = with_app(|a| (a.list, a.generation, a.image_list_set));
        if msg.generation != current {
            return;
        }
        let loaded = match msg.result {
            Ok(l) => l,
            Err(e) => {
                set_status(t::STATUS_READY);
                let text = format!("{}\n\n{}", file_name(&msg.path), strings::error_message(&e));
                MessageBoxW(Some(hwnd), &HSTRING::from(text), &HSTRING::from(t::ERROR_TITLE), MB_OK | MB_ICONERROR);
                return;
            }
        };

        if !has_images {
            // First use of the shell icon list happens after the window is
            // already visible, so it does not delay startup.
            ensure_com();
            let images = IconCache::system_list();
            SendMessageW(list, LVM_SETIMAGELIST, Some(WPARAM(LVSIL_SMALL as usize)), Some(LPARAM(images.0)));
            with_app(|a| a.image_list_set = true);
        }

        let entries = loaded.reader.entries();
        let files = entries.iter().filter(|e: &&Entry| e.kind == EntryKind::File).count();
        let dirs = loaded.index.len() - 1 - files.min(loaded.index.len() - 1);
        let status = format!(
            "{}  ·  {}",
            strings::opened_in(loaded.reader.format().name(), entries.len(), loaded.open_ms),
            strings::items_summary(files, dirs)
        );
        let title = format!("{} - {}", file_name(&loaded.path), t::APP_NAME);
        let count = loaded.rows.len();
        let open_ms = loaded.open_ms;

        with_app(|a| a.archive = Some(loaded));
        SendMessageW(list, LVM_SETITEMCOUNT, Some(WPARAM(count)), Some(LPARAM(0)));
        let none = LVITEMW { stateMask: SEL_FOCUS, ..Default::default() };
        SendMessageW(list, LVM_SETITEMSTATE, Some(WPARAM(usize::MAX)), Some(LPARAM(&none as *const _ as isize)));
        if count > 0 {
            let first = LVITEMW {
                state: SEL_FOCUS,
                stateMask: SEL_FOCUS,
                ..Default::default()
            };
            SendMessageW(list, LVM_SETITEMSTATE, Some(WPARAM(0)), Some(LPARAM(&first as *const _ as isize)));
            SendMessageW(list, LVM_ENSUREVISIBLE, Some(WPARAM(0)), Some(LPARAM(0)));
        }
        let _ = SetWindowTextW(hwnd, &HSTRING::from(title));
        set_status(&status);
        let _ = SetFocus(Some(list));
        let _ = InvalidateRect(Some(list), None, true);
        let _ = UpdateWindow(list);
        timing::mark("archive visible", &format!("(entries={count}, open+index={open_ms:.1} ms)"));
    }
}

/// Supplies text and icons for the rows the ListView is painting.
unsafe fn on_notify(lp: LPARAM) -> LRESULT {
    unsafe {
        let hdr = &*(lp.0 as *const NMHDR);
        if hdr.code == LVN_GETDISPINFOW {
            fill_row(&mut *(lp.0 as *mut NMLVDISPINFOW));
        }
        LRESULT(0)
    }
}

fn fill_row(di: &mut NMLVDISPINFOW) {
    APP.with(|cell| {
        let Ok(mut guard) = cell.try_borrow_mut() else { return };
        let app = &mut *guard;
        let Some(arc) = &app.archive else { return };
        let Some(&node) = arc.rows.get(di.item.iItem as usize) else { return };
        let index = &arc.index;
        let is_dir = index.is_dir(node);
        let entry = index.entry(node).map(|id| &arc.reader.entries()[id as usize]);

        if di.item.mask.0 & LVIF_IMAGE.0 != 0 {
            di.item.iImage = if is_dir { app.icons.folder() } else { app.icons.file(index.name(node)) };
        }
        if di.item.mask.0 & LVIF_TEXT.0 != 0 {
            let text = match di.item.iSubItem {
                0 => index.name(node).to_owned(),
                1 if !is_dir => format::size(entry.and_then(|e| e.size)),
                2 if !is_dir => format::size(entry.and_then(|e| e.packed)),
                3 => format::date(entry.and_then(|e| e.modified)),
                _ => String::new(),
            };
            copy_text(&text, di.item.pszText, di.item.cchTextMax);
        }
    });
}

fn copy_text(text: &str, dst: PWSTR, cap: i32) {
    if dst.is_null() || cap <= 0 {
        return;
    }
    let cap = cap as usize;
    let mut n = 0;
    for unit in text.encode_utf16() {
        if n + 1 >= cap {
            break;
        }
        // SAFETY: the ListView provides a buffer of `cap` UTF-16 units.
        unsafe { *dst.0.add(n) = unit };
        n += 1;
    }
    unsafe { *dst.0.add(n) = 0 };
}
