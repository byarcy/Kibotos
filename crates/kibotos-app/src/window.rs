//! The main window: a path bar with the filter box, a virtual ListView
//! (LVS_OWNERDATA) and a status bar.
//!
//! The list holds no items. It only knows the row count and asks for the
//! text and icon of the rows it is about to paint (LVN_GETDISPINFOW), so
//! 100,000 entries cost the same as 100. Navigation, sorting and filtering
//! only rebuild a `Vec<NodeId>` of rows over the in-memory index.
//!
//! Re-entrancy rule: state lives in a thread-local `RefCell`. Never hold the
//! borrow while sending a message to a child window, because the child may
//! synchronously send WM_NOTIFY back to `wndproc`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::Instant;

use kibotos_core::view::{Ordering, Sort, SortKey, refine_rows};
use kibotos_core::{ArchiveIndex, ArchiveReader, Control, Entry, EntryKind, Error, NodeId, OpenOptions};
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
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, SetFocus, VK_BACK, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_NEXT, VK_PRIOR, VK_RETURN, VK_UP,
};
use windows::Win32::UI::Shell::{
    DefSubclassProc, DragFinish, DragQueryFileW, HDROP, SHELLEXECUTEINFOW, SetWindowSubclass, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, PWSTR, w};

use crate::dialogs;
use crate::icons::IconCache;
use crate::job::{self, Dest, ExtractDone, ExtractJob, WM_APP_CONFLICT, WM_APP_EXTRACTED};
use crate::progress;
use crate::temp;
use crate::strings::{self, t};
use crate::{format, timing, wide};

const WM_APP_LOADED: u32 = WM_APP + 1;
const WM_APP_ICONS: u32 = WM_APP + 2;

const ID_OPEN: u16 = 101;
const ID_EXIT: u16 = 102;
const ID_FIND: u16 = 103;
const ID_UP: u16 = 104;
const ID_EXTRACT: u16 = 105;
const ID_ABOUT: u16 = 201;
const ID_INTEGRATE: u16 = 301;
const ID_UNINTEGRATE: u16 = 302;
const ID_LIST: usize = 1000;
const ID_FILTER: usize = 1001;

const SEL_FOCUS: LIST_VIEW_ITEM_STATE_FLAGS = LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0);

// Static control styles (defined in a large windows-crate module we skip).
const SS_LEFTNOWORDWRAP: u32 = 0x000C;
const SS_NOPREFIX: u32 = 0x0080;
const SS_CENTERIMAGE: u32 = 0x0200;
const SS_PATHELLIPSIS: u32 = 0x8000;

/// Row value for the ".." entry at the top of a subfolder.
const UP: NodeId = NodeId::MAX;

/// Column titles, base widths at 96 DPI, right-aligned, sort key.
const COLUMNS: [(&str, i32, bool, SortKey); 4] = [
    (t::COL_NAME, 420, false, SortKey::Name),
    (t::COL_SIZE, 110, true, SortKey::Size),
    (t::COL_PACKED, 110, true, SortKey::Packed),
    (t::COL_MODIFIED, 140, false, SortKey::Modified),
];

struct Loaded {
    path: PathBuf,
    /// Password that opened the archive or last extracted it.
    password: Option<String>,
    reader: Box<dyn ArchiveReader>,
    index: ArchiveIndex,
    ord: Ordering,
    open_ms: f64,
    summary: String,
}

struct LoadMsg {
    generation: u64,
    path: PathBuf,
    password: Option<String>,
    result: kibotos_core::Result<Loaded>,
}

#[derive(Default)]
struct App {
    list: HWND,
    status: HWND,
    filter: HWND,
    path_label: HWND,
    font: HFONT,
    dpi: u32,
    archive: Option<Loaded>,
    /// Rows in display order (`UP` for "..").
    rows: Vec<NodeId>,
    folder: NodeId,
    /// Filter text as typed; empty means "show the current folder".
    query: String,
    /// The query `rows` was computed for, to narrow instead of rescanning.
    rows_query: String,
    sort: Sort,
    /// Set while the program itself changes the filter text.
    silent_filter: bool,
    /// Bumped on every open; results of older opens are ignored.
    generation: u64,
    icons: IconCache,
    image_list_set: bool,
    /// Texts last shown in the path bar and the two status parts.
    shown: (String, String, String),
    /// The running extraction, if any (one at a time).
    job: Option<Arc<Control>>,
    /// The user closed the window during an extraction: close once it has
    /// stopped and cleaned up its partial file.
    close_pending: bool,
}

thread_local! {
    static APP: RefCell<App> = RefCell::new(App::default());
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> R {
    APP.with(|a| f(&mut a.borrow_mut()))
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

fn scale(v: i32, dpi: u32) -> i32 {
    v * dpi as i32 / 96
}

fn send(h: HWND, msg: u32, wp: usize, lp: isize) -> LRESULT {
    unsafe { SendMessageW(h, msg, Some(WPARAM(wp)), Some(LPARAM(lp))) }
}

// ------------------------------------------------------------------ setup

pub fn run(archive: Option<PathBuf>) {
    unsafe {
        // Without this, the first ShowWindow loads the text-services/IME
        // stack (~20 ms, a fifth of the startup budget). Spanish and other
        // Latin keyboards, dead keys and AltGr work without an IME; Chinese,
        // Japanese or Korean input methods are not available in Kibotós.
        let _ = windows::Win32::UI::Input::Ime::ImmDisableIME(u32::MAX);
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_BAR_CLASSES | ICC_STANDARD_CLASSES | ICC_PROGRESS_CLASS,
        };
        let _ = InitCommonControlsEx(&icc);

        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("KibotosMain");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: crate::app_icon(false),
            hIconSm: crate::app_icon(true),
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
            start_load(hwnd, path, None);
        }

        let _ = ShowWindow(hwnd, SW_SHOWDEFAULT);
        let _ = UpdateWindow(hwnd);
        std::thread::spawn(temp::clean_stale);

        let accel = CreateAcceleratorTableW(&[
            ACCEL { fVirt: FCONTROL | FVIRTKEY, key: b'O' as u16, cmd: ID_OPEN },
            ACCEL { fVirt: FCONTROL | FVIRTKEY, key: b'F' as u16, cmd: ID_FIND },
            ACCEL { fVirt: FCONTROL | FVIRTKEY, key: b'E' as u16, cmd: ID_EXTRACT },
            ACCEL { fVirt: FALT | FVIRTKEY, key: VK_UP.0, cmd: ID_UP },
        ])
        .unwrap_or_default();
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // Esc / Enter / Tab in the progress window.
            if let Some(p) = progress::current() {
                if IsDialogMessageW(p, &msg).as_bool() {
                    continue;
                }
            }
            if TranslateAcceleratorW(hwnd, accel, &msg) == 0 {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        temp::cleanup();
    }
}

unsafe fn build_menu() -> HMENU {
    unsafe {
        let bar = CreateMenu().unwrap_or_default();
        let file = CreatePopupMenu().unwrap_or_default();
        let _ = AppendMenuW(file, MF_STRING, ID_OPEN as usize, &HSTRING::from(t::MENU_OPEN));
        let _ = AppendMenuW(file, MF_STRING, ID_EXTRACT as usize, &HSTRING::from(t::MENU_EXTRACT));
        let _ = AppendMenuW(file, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(file, MF_STRING, ID_EXIT as usize, &HSTRING::from(t::MENU_EXIT));
        let tools = CreatePopupMenu().unwrap_or_default();
        let _ = AppendMenuW(tools, MF_STRING, ID_INTEGRATE as usize, &HSTRING::from(t::MENU_INTEGRATE));
        let _ = AppendMenuW(tools, MF_STRING, ID_UNINTEGRATE as usize, &HSTRING::from(t::MENU_UNINTEGRATE));
        let help = CreatePopupMenu().unwrap_or_default();
        let _ = AppendMenuW(help, MF_STRING, ID_ABOUT as usize, &HSTRING::from(t::MENU_ABOUT));
        let _ = AppendMenuW(bar, MF_POPUP, file.0 as usize, &HSTRING::from(t::MENU_FILE));
        let _ = AppendMenuW(bar, MF_POPUP, tools.0 as usize, &HSTRING::from(t::MENU_TOOLS));
        let _ = AppendMenuW(bar, MF_POPUP, help.0 as usize, &HSTRING::from(t::MENU_HELP));
        bar
    }
}

unsafe fn child(parent: HWND, class: PCWSTR, style: WINDOW_STYLE, ex: WINDOW_EX_STYLE, id: usize) -> HWND {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        CreateWindowExW(
            ex,
            class,
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | style,
            0,
            0,
            0,
            0,
            Some(parent),
            Some(HMENU(id as _)),
            Some(hinst.into()),
            None,
        )
        .unwrap_or_default()
    }
}

unsafe fn on_create(hwnd: HWND) {
    unsafe {
        let path_label = child(
            hwnd,
            w!("STATIC"),
            WINDOW_STYLE(SS_LEFTNOWORDWRAP | SS_PATHELLIPSIS | SS_CENTERIMAGE | SS_NOPREFIX),
            WINDOW_EX_STYLE(0),
            0,
        );
        let filter = child(
            hwnd,
            w!("EDIT"),
            WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            WS_EX_CLIENTEDGE,
            ID_FILTER,
        );
        let list = child(
            hwnd,
            WC_LISTVIEWW,
            WS_TABSTOP | WINDOW_STYLE(LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS | LVS_SHAREIMAGELISTS),
            WINDOW_EX_STYLE(0),
            ID_LIST,
        );
        let status = child(hwnd, STATUSCLASSNAMEW, WINDOW_STYLE(SBARS_SIZEGRIP), WINDOW_EX_STYLE(0), 0);

        let cue = wide(t::FILTER_CUE);
        send(filter, EM_SETCUEBANNER, 1, cue.as_ptr() as isize);
        let _ = SetWindowSubclass(list, Some(list_proc), 1, 0);
        let _ = SetWindowSubclass(filter, Some(filter_proc), 2, 0);

        let _ = SetWindowTheme(list, w!("Explorer"), PCWSTR::null());
        // Double buffering also paints faster here (measured ~9 vs ~12 ms).
        let ex = LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_HEADERDRAGDROP;
        send(list, LVM_SETEXTENDEDLISTVIEWSTYLE, ex as usize, ex as isize);

        for (i, (title, _, right, _)) in COLUMNS.iter().enumerate() {
            let mut text = wide(title);
            let col = LVCOLUMNW {
                mask: LVCF_TEXT | LVCF_WIDTH | LVCF_FMT | LVCF_SUBITEM,
                fmt: if *right { LVCFMT_RIGHT } else { LVCFMT_LEFT },
                cx: 100,
                pszText: PWSTR(text.as_mut_ptr()),
                iSubItem: i as i32,
                ..Default::default()
            };
            send(list, LVM_INSERTCOLUMNW, i, &col as *const _ as isize);
        }

        with_app(|a| {
            a.list = list;
            a.status = status;
            a.filter = filter;
            a.path_label = path_label;
        });
        apply_dpi(hwnd, GetDpiForWindow(hwnd));
        update_sort_arrows();
        set_status(t::STATUS_READY, "");
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

/// Fonts, column widths and layout for the window's current DPI.
unsafe fn apply_dpi(hwnd: HWND, dpi: u32) {
    unsafe {
        let font = ui_font(dpi);
        let (handles, old) = with_app(|a| {
            a.dpi = dpi;
            ([a.list, a.status, a.filter, a.path_label], std::mem::replace(&mut a.font, font))
        });
        for h in handles {
            send(h, WM_SETFONT, font.0 as usize, 1);
        }
        if !old.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(old.0));
        }
        for (i, (_, width, _, _)) in COLUMNS.iter().enumerate() {
            send(handles[0], LVM_SETCOLUMNWIDTH, i, scale(*width, dpi) as isize);
        }
        layout(hwnd);
    }
}

unsafe fn layout(hwnd: HWND) {
    unsafe {
        let (list, status, filter, label, dpi) = with_app(|a| (a.list, a.status, a.filter, a.path_label, a.dpi));
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let s = |v| scale(v, dpi.max(96));

        send(status, WM_SIZE, 0, 0);
        let mut src = RECT::default();
        let _ = GetWindowRect(status, &mut src);
        let sh = src.bottom - src.top;
        let parts = [rc.right * 45 / 100, -1];
        send(status, SB_SETPARTS, 2, parts.as_ptr() as isize);

        let bar = s(30);
        let pad = s(4);
        let fw = s(280).min(rc.right / 2);
        let _ = MoveWindow(label, s(8), 0, (rc.right - fw - s(20)).max(0), bar, true);
        let _ = MoveWindow(filter, rc.right - fw - pad, pad, fw, bar - 2 * pad, true);
        let _ = MoveWindow(list, 0, bar, rc.right, (rc.bottom - sh - bar).max(0), true);
    }
}

fn set_status(left: &str, right: &str) {
    let status = with_app(|a| {
        a.shown.1 = left.to_owned();
        a.shown.2 = right.to_owned();
        a.status
    });
    for (i, text) in [left, right].into_iter().enumerate() {
        let w = wide(text);
        send(status, SB_SETTEXTW, i, w.as_ptr() as isize);
    }
}

// ------------------------------------------------------------- messages

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
                if !PAINTED.swap(true, AtomicOrdering::Relaxed) {
                    timing::mark("window visible", "");
                }
                r
            }
            WM_NOTIFY => on_notify(lp),
            WM_COMMAND => {
                let id = (wp.0 & 0xFFFF) as u16;
                let code = ((wp.0 >> 16) & 0xFFFF) as u32;
                if id as usize == ID_FILTER && code == EN_CHANGE {
                    on_filter_changed();
                } else {
                    on_command(hwnd, id);
                }
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
            WM_APP_EXTRACTED => {
                on_extracted(hwnd, lp);
                LRESULT(0)
            }
            WM_APP_CONFLICT => LRESULT(job::answer_conflict(hwnd, lp)),
            WM_APP_ICONS => {
                let (folder, icons) = *Box::from_raw(lp.0 as *mut (i32, Vec<(String, i32)>));
                with_app(|a| a.icons.warm(folder, icons));
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
            WM_CLOSE => {
                if confirm_close(hwnd) {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_INITMENUPOPUP => {
                // Show the current integration state in Herramientas.
                let menu = HMENU(wp.0 as _);
                let integrated = crate::integrate::installed_exe().is_some();
                let check = if integrated { MF_CHECKED } else { MF_UNCHECKED };
                let enable = if integrated { MF_ENABLED } else { MF_GRAYED };
                CheckMenuItem(menu, ID_INTEGRATE as u32, (MF_BYCOMMAND | check).0);
                let _ = EnableMenuItem(menu, ID_UNINTEGRATE as u32, MF_BYCOMMAND | enable);
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

unsafe fn on_command(hwnd: HWND, id: u16) {
    unsafe {
        match id {
            ID_OPEN => {
                if let Some(path) = open_dialog(hwnd) {
                    start_load(hwnd, path, None);
                }
            }
            ID_EXIT => {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            ID_FIND => focus_filter(),
            ID_EXTRACT => on_extract(hwnd),
            ID_INTEGRATE => {
                crate::cli::integration(hwnd, true);
            }
            ID_UNINTEGRATE => {
                crate::cli::integration(hwnd, false);
            }
            ID_UP => go_up(),
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

unsafe fn on_notify(lp: LPARAM) -> LRESULT {
    unsafe {
        let hdr = &*(lp.0 as *const NMHDR);
        match hdr.code {
            LVN_GETDISPINFOW => fill_row(&mut *(lp.0 as *mut NMLVDISPINFOW)),
            NM_DBLCLK => {
                let item = (*(lp.0 as *const NMITEMACTIVATE)).iItem;
                if item >= 0 {
                    activate(item as usize, false);
                }
            }
            LVN_COLUMNCLICK => {
                let col = (*(lp.0 as *const NMLISTVIEW)).iSubItem;
                if let Some(c) = COLUMNS.get(col as usize) {
                    sort_by(c.3);
                }
            }
            _ => {}
        }
        LRESULT(0)
    }
}

/// Keyboard in the list: Enter opens, Backspace goes up, Esc clears the
/// filter, and typing a character starts filtering.
unsafe extern "system" fn list_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    unsafe {
        match msg {
            WM_KEYDOWN => {
                let vk = wp.0 as u16;
                if vk == VK_RETURN.0 {
                    if let Some(i) = focused_row() {
                        activate(i, ctrl_down());
                    }
                    return LRESULT(0);
                }
                if vk == VK_BACK.0 {
                    go_up();
                    return LRESULT(0);
                }
                if vk == VK_ESCAPE.0 {
                    clear_filter();
                    return LRESULT(0);
                }
            }
            WM_CHAR => {
                let c = wp.0 as u32;
                if c == 0x0D || c == 0x08 || c == 0x1B {
                    return LRESULT(0); // handled in WM_KEYDOWN; avoid the beep
                }
                if c >= 0x20 && c != 0x7F {
                    let filter = with_app(|a| a.filter);
                    let _ = SetFocus(Some(filter));
                    send(filter, EM_SETSEL, usize::MAX, -1);
                    send(filter, WM_CHAR, wp.0, lp.0);
                    return LRESULT(0);
                }
            }
            _ => {}
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}

/// Keyboard in the filter box: arrows move into the list, Enter opens the
/// selected row, Esc clears the filter.
unsafe extern "system" fn filter_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    unsafe {
        match msg {
            WM_KEYDOWN => {
                let vk = wp.0 as u16;
                let list = with_app(|a| a.list);
                if [VK_DOWN.0, VK_UP.0, VK_NEXT.0, VK_PRIOR.0].contains(&vk) {
                    let _ = SetFocus(Some(list));
                    send(list, WM_KEYDOWN, wp.0, lp.0);
                    return LRESULT(0);
                }
                if vk == VK_RETURN.0 {
                    if let Some(i) = focused_row() {
                        activate(i, ctrl_down());
                    }
                    return LRESULT(0);
                }
                if vk == VK_ESCAPE.0 {
                    clear_filter();
                    return LRESULT(0);
                }
            }
            WM_CHAR if matches!(wp.0, 0x0D | 0x1B | 0x7F) => return LRESULT(0),
            _ => {}
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}

// ---------------------------------------------------------- open / load

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
            start_load(hwnd, PathBuf::from(String::from_utf16_lossy(&buf[..n])), None);
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Opens, indexes and sorts an archive on a worker thread. The UI thread
/// stays free; the result arrives as WM_APP_LOADED.
fn start_load(hwnd: HWND, path: PathBuf, password: Option<String>) {
    let (generation, sort) = with_app(|a| {
        a.generation += 1;
        (a.generation, a.sort)
    });
    set_status(t::STATUS_OPENING, "");
    let target = hwnd.0 as isize;
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let opts = OpenOptions { password: password.clone() };
        let result = kibotos_core::open(&path, &opts).map(|reader| {
            let index = ArchiveIndex::build(reader.entries());
            prefetch_icons(target, &index);
            let ord = Ordering::new(&index, reader.entries(), sort);
            let open_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let entries = reader.entries();
            let files = entries.iter().filter(|e: &&Entry| e.kind == EntryKind::File).count();
            let dirs = (1..index.len() as NodeId).filter(|&n| index.is_dir(n)).count();
            let summary = format!(
                "{}  ·  {}",
                strings::opened_in(reader.format().name(), entries.len(), open_ms),
                strings::items_summary(files, dirs)
            );
            Loaded { path: path.clone(), password: password.clone(), reader, index, ord, open_ms, summary }
        });
        let msg = Box::into_raw(Box::new(LoadMsg { generation, path, password, result }));
        // SAFETY: on success the UI thread takes ownership of `msg`.
        unsafe {
            if PostMessageW(Some(HWND(target as _)), WM_APP_LOADED, WPARAM(0), LPARAM(msg as isize)).is_err() {
                drop(Box::from_raw(msg));
            }
        }
    });
}

/// Looks up the icons of every file type in the archive on another thread,
/// so the UI thread never waits for the shell to extract an icon.
fn prefetch_icons(target: isize, index: &ArchiveIndex) {
    let mut seen = std::collections::HashSet::new();
    let mut exts = Vec::new();
    for n in 1..index.len() as NodeId {
        if !index.is_dir(n) && seen.len() < 256 {
            let e = crate::icons::extension(index.name(n));
            if seen.insert(e.clone()) {
                exts.push(e);
            }
        }
    }
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let icons = Box::into_raw(Box::new(crate::icons::prefetch(exts)));
        if PostMessageW(Some(HWND(target as _)), WM_APP_ICONS, WPARAM(0), LPARAM(icons as isize)).is_err() {
            drop(Box::from_raw(icons));
        }
    });
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
            // Encrypted file names (7z, RAR): ask and open again.
            Err(e @ (Error::PasswordRequired | Error::WrongPassword)) => {
                let wrong = matches!(e, Error::WrongPassword) || msg.password.is_some();
                set_status(t::STATUS_READY, "");
                if let Some(pw) = dialogs::ask_password(hwnd, &file_name(&msg.path), wrong) {
                    start_load(hwnd, msg.path, Some(pw));
                }
                return;
            }
            Err(e) => {
                set_status(t::STATUS_READY, "");
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
            send(list, LVM_SETIMAGELIST, LVSIL_SMALL as usize, images.0);
            with_app(|a| a.image_list_set = true);
        }

        let title = format!("{} - {}", file_name(&loaded.path), t::APP_NAME);
        let open_ms = loaded.open_ms;
        with_app(|a| {
            a.archive = Some(loaded);
            a.folder = ArchiveIndex::ROOT;
        });
        set_filter_text("");
        let _ = SetWindowTextW(hwnd, &HSTRING::from(title));
        refresh(None);
        let _ = SetFocus(Some(list));
        let _ = UpdateWindow(list);
        let count = with_app(|a| a.rows.len());
        timing::mark("archive visible", &format!("(rows={count}, open+index+sort={open_ms:.1} ms)"));
    }
}

// ------------------------------------------------------- view changes

/// Recomputes the rows for the current folder / filter, updates the list,
/// path bar and status, and selects `select` (or the first row).
fn refresh(select: Option<NodeId>) {
    let Some((list, label, count, sel, path_text, left, right)) = with_app(|a| {
        let arc = a.archive.as_ref()?;
        let filtering = !a.query.is_empty();
        if filtering {
            let narrows = !a.rows_query.is_empty() && a.query.to_lowercase().contains(&a.rows_query.to_lowercase());
            if narrows {
                refine_rows(&arc.index, &a.query, &mut a.rows);
            } else {
                arc.ord.filter_rows(&arc.index, &a.query, &mut a.rows);
            }
            a.rows_query = a.query.clone();
        } else {
            arc.ord.folder_rows(&arc.index, a.folder, &mut a.rows);
            if a.folder != ArchiveIndex::ROOT {
                a.rows.insert(0, UP);
            }
            a.rows_query.clear();
        }
        // By default select the first real item, not "..", so Enter keeps
        // going down and Backspace goes up.
        let first = usize::from(a.rows.len() > 1 && a.rows[0] == UP);
        let sel = select.and_then(|n| a.rows.iter().position(|&r| r == n)).unwrap_or(first);

        let mut path_text = file_name(&arc.path);
        let folder_path = arc.index.path(a.folder);
        if !folder_path.is_empty() {
            path_text.push('\\');
            path_text.push_str(&folder_path.replace('/', "\\"));
        }
        let shown = a.rows.iter().filter(|&&r| r != UP).count();
        let left = if filtering { strings::matches(shown, &a.query) } else { strings::folder_items(shown) };
        Some((a.list, a.path_label, a.rows.len(), sel, path_text, left, arc.summary.clone()))
    }) else {
        return;
    };

    send(list, LVM_SETITEMCOUNT, count, 0);
    let none = LVITEMW { stateMask: SEL_FOCUS, ..Default::default() };
    send(list, LVM_SETITEMSTATE, usize::MAX, &none as *const _ as isize);
    if count > 0 {
        // Focus only, no selection: Enter acts on the focused row, and
        // Ctrl+E with nothing selected extracts the whole folder.
        let item = LVITEMW { state: LVIS_FOCUSED, stateMask: SEL_FOCUS, ..Default::default() };
        send(list, LVM_SETITEMSTATE, sel, &item as *const _ as isize);
        send(list, LVM_ENSUREVISIBLE, sel, 0);
    }
    unsafe {
        let _ = InvalidateRect(Some(list), None, false);
    }
    // Repainting unchanged labels costs frame time while typing.
    let (path_changed, status_changed) = with_app(|a| {
        let c = (a.shown.0 != path_text, a.shown.1 != left || a.shown.2 != right);
        a.shown.0 = path_text.clone();
        c
    });
    if path_changed {
        unsafe {
            let _ = SetWindowTextW(label, &HSTRING::from(path_text));
        }
    }
    if status_changed {
        set_status(&left, &right);
    }
}

fn focused_row() -> Option<usize> {
    let list = with_app(|a| a.list);
    let i = send(list, LVM_GETNEXTITEM, usize::MAX, LVNI_FOCUSED as isize).0;
    (i >= 0).then_some(i as usize)
}

fn ctrl_down() -> bool {
    unsafe { GetKeyState(VK_CONTROL.0 as i32) < 0 }
}

/// Enter or double-click on a row. Folders open; files open with their
/// program. `locate` (Ctrl+Enter) shows a filter result in its folder.
fn activate(row: usize, locate: bool) {
    let Some((node, is_dir, parent, filtering)) = with_app(|a| {
        let arc = a.archive.as_ref()?;
        let node = *a.rows.get(row)?;
        if node == UP {
            return Some((UP, true, None, false));
        }
        Some((node, arc.index.is_dir(node), arc.index.parent(node), !a.query.is_empty()))
    }) else {
        return;
    };
    if node == UP {
        go_up();
    } else if locate && filtering {
        navigate(parent.unwrap_or(ArchiveIndex::ROOT), Some(node));
    } else if is_dir {
        navigate(node, None);
    } else {
        let hwnd = with_app(|a| unsafe { GetParent(a.list) }.unwrap_or_default());
        open_entry(hwnd, node);
    }
}

/// Shows `folder`, leaving filter mode.
fn navigate(folder: NodeId, select: Option<NodeId>) {
    with_app(|a| {
        a.folder = folder;
        a.query.clear();
    });
    set_filter_text("");
    refresh(select);
    let list = with_app(|a| a.list);
    unsafe {
        let _ = SetFocus(Some(list));
    }
}

/// Backspace / Alt+Up: leaves filter mode, or goes to the parent folder and
/// selects the folder we came from.
fn go_up() {
    let target = with_app(|a| {
        let arc = a.archive.as_ref()?;
        if !a.query.is_empty() {
            return Some((a.folder, None));
        }
        let parent = arc.index.parent(a.folder)?;
        Some((parent, Some(a.folder)))
    });
    if let Some((folder, select)) = target {
        navigate(folder, select);
    }
}

fn clear_filter() {
    let filtering = with_app(|a| !a.query.is_empty());
    if filtering {
        let folder = with_app(|a| a.folder);
        navigate(folder, None);
    } else {
        let list = with_app(|a| a.list);
        unsafe {
            let _ = SetFocus(Some(list));
        }
    }
}

fn focus_filter() {
    let filter = with_app(|a| a.filter);
    unsafe {
        let _ = SetFocus(Some(filter));
    }
    send(filter, EM_SETSEL, 0, -1);
}

/// Sets the filter box text without treating it as user input.
fn set_filter_text(text: &str) {
    let filter = with_app(|a| {
        a.silent_filter = true;
        a.filter
    });
    unsafe {
        let _ = SetWindowTextW(filter, &HSTRING::from(text));
    }
    with_app(|a| a.silent_filter = false);
}

/// The user typed in the filter box.
fn on_filter_changed() {
    let t0 = Instant::now();
    let (filter, silent) = with_app(|a| (a.filter, a.silent_filter));
    if silent {
        return;
    }
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetWindowTextW(filter, &mut buf) } as usize;
    let text = String::from_utf16_lossy(&buf[..n]);
    // An empty query shows the current folder again.
    with_app(|a| a.query = text.clone());
    refresh(None);
    if timing::enabled() {
        let list = with_app(|a| a.list);
        let tp = Instant::now();
        unsafe {
            let _ = UpdateWindow(list);
        }
        let paint_ms = tp.elapsed().as_secs_f64() * 1000.0;
        let rows = with_app(|a| a.rows.len());
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        timing::mark("filter", &format!("'{text}' rows={rows} keystroke={ms:.2} ms (paint {paint_ms:.2} ms)"));
    }
    // Keep typing in the box even if the list took focus.
    unsafe {
        if GetFocus() != filter {
            let _ = SetFocus(Some(filter));
        }
    }
}

/// Column header click: same column toggles direction.
fn sort_by(key: SortKey) {
    let t0 = Instant::now();
    let selected = focused_row().and_then(|i| with_app(|a| a.rows.get(i).copied()));
    with_app(|a| {
        a.sort = if a.sort.key == key {
            Sort { key, descending: !a.sort.descending }
        } else {
            Sort { key, descending: false }
        };
        let sort = a.sort;
        if let Some(arc) = a.archive.as_mut() {
            arc.ord = Ordering::new(&arc.index, arc.reader.entries(), sort);
        }
        a.rows_query.clear();
    });
    update_sort_arrows();
    refresh(selected.filter(|&n| n != UP));
    timing::mark("sort", &format!("{:.1} ms", t0.elapsed().as_secs_f64() * 1000.0));
}

fn update_sort_arrows() {
    let (list, sort) = with_app(|a| (a.list, a.sort));
    let header = HWND(send(list, LVM_GETHEADER, 0, 0).0 as _);
    for (i, (_, _, _, key)) in COLUMNS.iter().enumerate() {
        let mut item = HDITEMW { mask: HDI_FORMAT, ..Default::default() };
        send(header, HDM_GETITEMW, i, &mut item as *mut _ as isize);
        let mut fmt = item.fmt.0 & !(HDF_SORTUP.0 | HDF_SORTDOWN.0);
        if *key == sort.key {
            fmt |= if sort.descending { HDF_SORTDOWN.0 } else { HDF_SORTUP.0 };
        }
        item.fmt = HEADER_CONTROL_FORMAT_FLAGS(fmt);
        send(header, HDM_SETITEMW, i, &item as *const _ as isize);
    }
}

// ------------------------------------------------------------ extract

fn selected_rows(list: HWND) -> Vec<usize> {
    let mut rows = Vec::new();
    let mut i = -1isize;
    loop {
        i = send(list, LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize).0;
        if i < 0 {
            return rows;
        }
        rows.push(i as usize);
    }
}

/// Ctrl+E: the selected rows, or everything in view when nothing is
/// selected (the current folder, or all filter results).
fn on_extract(hwnd: HWND) {
    let list = with_app(|a| a.list);
    let selected = selected_rows(list);
    let Some((archive, password, ids, base, encrypted)) = with_app(|a| {
        if a.job.is_some() {
            return None;
        }
        let arc = a.archive.as_ref()?;
        let filtering = !a.query.is_empty();
        let mut nodes: Vec<NodeId> = selected.iter().filter_map(|&i| a.rows.get(i).copied()).filter(|&n| n != UP).collect();
        if nodes.is_empty() {
            nodes = if filtering { a.rows.clone() } else { vec![a.folder] };
        }
        let mut ids = Vec::new();
        for n in nodes {
            arc.index.entries_under(n, &mut ids);
        }
        ids.sort_unstable();
        ids.dedup();
        let base = if filtering { String::new() } else { arc.index.path(a.folder) };
        let entries = arc.reader.entries();
        let encrypted = ids.iter().any(|&id| entries[id as usize].encrypted);
        Some((arc.path.clone(), arc.password.clone(), ids, base, encrypted))
    }) else {
        return;
    };
    if ids.is_empty() {
        return;
    }
    let initial = archive.parent().map(Path::to_path_buf).unwrap_or_default();
    ensure_com();
    let Some(dest) = dialogs::pick_folder(hwnd, &initial) else { return };
    let mut password = password;
    if encrypted && password.is_none() {
        let Some(pw) = dialogs::ask_password(hwnd, &file_name(&archive), false) else { return };
        password = Some(pw);
    }
    start_extract(
        hwnd,
        ExtractJob { archive, password, ids: Some(ids), base, dest: Dest::Folder(dest), verify: encrypted, open_after: None },
    );
}

/// Extracts one file to a fresh temporary folder and opens it.
fn open_entry(hwnd: HWND, node: NodeId) {
    let Some((archive, password, id, path, encrypted)) = with_app(|a| {
        if a.job.is_some() {
            return None;
        }
        let arc = a.archive.as_ref()?;
        let id = arc.index.entry(node)?;
        let e = &arc.reader.entries()[id as usize];
        Some((arc.path.clone(), arc.password.clone(), id, e.path.to_string(), e.encrypted))
    }) else {
        return;
    };
    let (base, name) = path.rsplit_once('/').unwrap_or(("", &path));
    let slot = match temp::new_slot() {
        Ok(d) => d,
        Err(e) => {
            dialogs::message(hwnd, t::OPEN_FAILED, &e.to_string(), MB_ICONERROR);
            return;
        }
    };
    // The name may still be unsafe (e.g. "CON"); then nothing is opened and
    // the extraction reports it as blocked.
    let target = kibotos_core::safety::safe_join(&slot, name).ok();
    let mut password = password;
    if encrypted && password.is_none() {
        let Some(pw) = dialogs::ask_password(hwnd, &file_name(&archive), false) else { return };
        password = Some(pw);
    }
    start_extract(
        hwnd,
        ExtractJob {
            archive,
            password,
            ids: Some(vec![id]),
            base: base.to_string(),
            dest: Dest::Folder(slot),
            verify: encrypted,
            open_after: Some(target.unwrap_or_default()),
        },
    );
}

fn shell_open(hwnd: HWND, file: &Path) {
    ensure_com();
    let path = HSTRING::from(file.as_os_str());
    let dir = HSTRING::from(file.parent().unwrap_or(file).as_os_str());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        hwnd,
        lpFile: PCWSTR(path.as_ptr()),
        lpDirectory: PCWSTR(dir.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // Without a registered program Windows shows its "Open with" dialog.
    if let Err(e) = unsafe { ShellExecuteExW(&mut info) } {
        const ERROR_CANCELLED: i32 = 1223;
        if e.code().0 & 0xFFFF != ERROR_CANCELLED {
            dialogs::message(hwnd, t::OPEN_FAILED, &format!("{}\n\n{}", file.display(), e.message()), MB_ICONERROR);
        }
    }
}

/// Before closing: files edited after being opened from the archive would
/// be lost with the temporary folder, so offer to keep copies.
fn confirm_close(hwnd: HWND) -> bool {
    // Never exit in the middle of a file: cancel, and close when the worker
    // reports back (it removes the partial file first).
    let running = with_app(|a| {
        let job = a.job.clone();
        if let Some(ctl) = &job {
            ctl.cancel();
            a.close_pending = true;
        }
        job.is_some()
    });
    if running {
        return false;
    }
    let modified = temp::modified();
    if modified.is_empty() {
        return true;
    }
    const SAVE: i32 = 100;
    const DISCARD: i32 = 101;
    let mut content = format!("{}\n", t::MODIFIED_TEXT);
    for (f, _) in &modified {
        content.push_str(&format!("\n•  {}", file_name(f)));
    }
    let choice = dialogs::choose(
        hwnd,
        t::MODIFIED_TITLE,
        t::MODIFIED_MAIN,
        &content,
        &[(SAVE, t::SAVE_COPIES), (DISCARD, t::DISCARD), (IDCANCEL.0, t::CANCEL)],
    );
    match choice {
        SAVE => {
            let mut saved = Vec::new();
            for (f, dir) in &modified {
                match temp::save_copy(f, dir) {
                    Ok(p) => saved.push(p.display().to_string()),
                    Err(e) => {
                        let text = format!("{} «{}»:\n{e}", t::COPY_FAILED, file_name(f));
                        dialogs::message(hwnd, t::MODIFIED_TITLE, &text, MB_ICONERROR);
                        return false;
                    }
                }
            }
            dialogs::message(hwnd, t::COPIES_SAVED, &saved.join("\n"), MB_ICONINFORMATION);
            true
        }
        DISCARD => true,
        _ => false,
    }
}

fn start_extract(hwnd: HWND, job: ExtractJob) {
    let ctl = Arc::new(Control::new());
    with_app(|a| a.job = Some(ctl.clone()));
    let name = match &job.open_after {
        Some(target) => file_name(target),
        None => file_name(&job.archive),
    };
    let verb = if job.open_after.is_some() { t::PROGRESS_OPENING } else { t::PROGRESS_EXTRACTING };
    let title = format!("{verb} {name}");
    let heading = format!("{name}  →  {}", job.dest.shown().display());
    progress::open(Some(hwnd), &title, &heading, ctl.clone());
    set_status(verb, "");
    job::spawn(hwnd, job, ctl);
}

unsafe fn on_extracted(hwnd: HWND, lp: LPARAM) {
    let ExtractDone { mut job, dest, result, secs } = unsafe { job::take_done(lp) };
    progress::close();
    with_app(|a| a.job = None);
    timing::mark("extract", &format!("{} in {secs:.2} s", dest.display()));
    if with_app(|a| std::mem::take(&mut a.close_pending)) {
        let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        return;
    }
    match result {
        Ok(report) => {
            let summary = with_app(|a| a.shown.2.clone());
            match &job.open_after {
                Some(target) if report.files > 0 && target.is_file() => {
                    temp::track(target, &job.archive);
                    set_status(&strings::opened(&file_name(target)), &summary);
                    shell_open(hwnd, target);
                }
                Some(_) => refresh_status(),
                None => {
                    let text = strings::extracted(report.files, report.bytes, secs, &dest.display().to_string());
                    set_status(&text, &summary);
                }
            }
            dialogs::show_blocked(hwnd, &report);
        }
        Err(Error::Cancelled) => {
            let summary = with_app(|a| a.shown.2.clone());
            set_status(t::EXTRACT_CANCELLED, &summary);
        }
        Err(e @ (Error::PasswordRequired | Error::WrongPassword)) => {
            let wrong = matches!(e, Error::WrongPassword) || job.password.is_some();
            refresh_status();
            if let Some(pw) = dialogs::ask_password(hwnd, &file_name(&job.archive), wrong) {
                with_app(|a| {
                    if let Some(arc) = a.archive.as_mut() {
                        arc.password = Some(pw.clone());
                    }
                });
                job.password = Some(pw);
                job.verify = true;
                start_extract(hwnd, job);
            }
            return;
        }
        Err(e) => {
            refresh_status();
            dialogs::message(hwnd, t::EXTRACT_ERROR_TITLE, &strings::error_message(&e), MB_ICONERROR);
            return;
        }
    }
    // Remember a password that worked for the next extraction.
    if let Some(pw) = job.password {
        with_app(|a| {
            if let Some(arc) = a.archive.as_mut() {
                arc.password.get_or_insert(pw);
            }
        });
    }
}

/// Puts the normal folder / filter text back in the status bar.
fn refresh_status() {
    let Some((left, right)) = with_app(|a| {
        let arc = a.archive.as_ref()?;
        let shown = a.rows.iter().filter(|&&r| r != UP).count();
        let left = if a.query.is_empty() { strings::folder_items(shown) } else { strings::matches(shown, &a.query) };
        Some((left, arc.summary.clone()))
    }) else {
        return;
    };
    set_status(&left, &right);
}

// ------------------------------------------------------------ painting

/// Supplies text and icons for the rows the ListView is painting.
fn fill_row(di: &mut NMLVDISPINFOW) {
    APP.with(|cell| {
        let Ok(mut guard) = cell.try_borrow_mut() else { return };
        let app = &mut *guard;
        let Some(arc) = &app.archive else { return };
        let Some(&node) = app.rows.get(di.item.iItem as usize) else { return };
        let wants_image = di.item.mask.0 & LVIF_IMAGE.0 != 0;
        let wants_text = di.item.mask.0 & LVIF_TEXT.0 != 0;

        if node == UP {
            if wants_image {
                di.item.iImage = app.icons.folder();
            }
            if wants_text {
                copy_text(if di.item.iSubItem == 0 { t::UP_ROW } else { "" }, di.item.pszText, di.item.cchTextMax);
            }
            return;
        }

        let index = &arc.index;
        let is_dir = index.is_dir(node);
        let entry = index.entry(node).map(|id| &arc.reader.entries()[id as usize]);
        if wants_image {
            di.item.iImage = if is_dir { app.icons.folder() } else { app.icons.file(index.name(node)) };
        }
        if wants_text {
            let text = match di.item.iSubItem {
                // Filter results come from any folder: show where they are.
                0 if !app.query.is_empty() => index.path(node).replace('/', "\\"),
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
