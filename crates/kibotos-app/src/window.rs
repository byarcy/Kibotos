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
use kibotos_core::{ArchiveIndex, ArchiveReader, Control, Entry, EntryId, EntryKind, Error, NodeId, OpenOptions};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    COLOR_WINDOW, CreateFontIndirectW, DeleteObject, HBRUSH, HFONT, HGDIOBJ, InvalidateRect, UpdateWindow,
};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{CF_UNICODETEXT, OleInitialize};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, SetFocus, VK_BACK, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_F5, VK_MENU, VK_NEXT, VK_PRIOR,
    VK_RETURN, VK_UP,
};
use windows::Win32::UI::Shell::{
    DefSubclassProc, DragFinish, DragQueryFileW, HDROP, SEE_MASK_INVOKEIDLIST, SHELLEXECUTEINFOW, SetWindowSubclass,
    ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, PWSTR, w};

use crate::dialogs;
use crate::drag;
use crate::fsview;
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
const ID_PATHBAR: u16 = 106;
const ID_REFRESH: u16 = 107;
const ID_ABOUT: u16 = 201;
const ID_INTEGRATE: u16 = 301;
const ID_UNINTEGRATE: u16 = 302;
const CM_OPEN: u32 = 401;
const CM_OPEN_WITH: u32 = 402;
const CM_EXTRACT: u32 = 403;
const CM_EXTRACT_NEXT: u32 = 404;
const CM_LOCATE: u32 = 405;
const CM_COPY_PATH: u32 = 406;
const CM_PROPERTIES: u32 = 407;
const CM_FS_EXTRACT_HERE: u32 = 408;
const CM_FS_EXTRACT_FOLDER: u32 = 409;
const CM_SHOW_IN_EXPLORER: u32 = 410;
const CM_REFRESH: u32 = 411;
const ID_LIST: usize = 1000;

/// Gives up waiting for the shell to copy the drag marker.
const TIMER_DROP: usize = 1;
const DROP_WAIT_MS: u32 = 60_000;
/// Tells Explorer about new files while an extraction to a folder runs.
const TIMER_SHELL: usize = 2;
const SHELL_REFRESH_MS: u32 = 500;
const ID_FILTER: usize = 1001;

const SEL_FOCUS: LIST_VIEW_ITEM_STATE_FLAGS = LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0);

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
    /// Ascending name order: the tie-breaker for other keys.
    names: Ordering,
    /// The order on screen.
    ord: Ordering,
    open_ms: f64,
    summary: String,
}

/// What a worker opened: an archive (and a path inside it to show), or a
/// disk listing (and a path to select).
enum Opened {
    Archive(Loaded, Option<String>),
    Folder(fsview::Listing, Option<PathBuf>),
}

struct LoadMsg {
    generation: u64,
    path: PathBuf,
    password: Option<String>,
    result: kibotos_core::Result<Opened>,
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
    /// The disk listing shown when no archive is open.
    fs: Option<fsview::Listing>,
    /// Archives waiting to be extracted (several chosen in the disk view).
    queue: std::collections::VecDeque<ExtractJob>,
    /// Rows in display order (`UP` for ".."). In the disk view, indices
    /// into `fs.items`.
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
    /// A drag dropped on the shell: waiting to learn the folder.
    drop_wait: Option<(drag::Watch, Pick)>,
    /// Folder an extraction is writing to, refreshed in Explorer.
    shell_dir: Option<PathBuf>,
    /// An extraction the UI waits for in a nested loop (drop on another
    /// program), and its result.
    blocking: bool,
    blocking_done: Option<ExtractDone>,
}

/// The entries a command acts on.
#[derive(Clone)]
struct Pick {
    archive: PathBuf,
    password: Option<String>,
    /// Selected (or implied) tree nodes.
    nodes: Vec<NodeId>,
    ids: Vec<EntryId>,
    bases: Vec<String>,
    encrypted: bool,
}

thread_local! {
    static APP: RefCell<App> = RefCell::new(App::default());
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> R {
    APP.with(|a| f(&mut a.borrow_mut()))
}

/// COM/OLE is needed by the shell (icons, dialogs, drag and drop) but not
/// to show the window, so it is initialized on first use.
fn ensure_com() {
    thread_local!(static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
    DONE.with(|d| {
        if !d.replace(true) {
            unsafe {
                let _ = OleInitialize(None);
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
        match archive {
            Some(path) => start_load(hwnd, path, None),
            None => open_place(hwnd, fsview::Place::Computer, None),
        }

        let _ = ShowWindow(hwnd, SW_SHOWDEFAULT);
        let _ = UpdateWindow(hwnd);
        std::thread::spawn(temp::clean_stale);

        let accel = CreateAcceleratorTableW(&[
            ACCEL { fVirt: FCONTROL | FVIRTKEY, key: b'O' as u16, cmd: ID_OPEN },
            ACCEL { fVirt: FCONTROL | FVIRTKEY, key: b'F' as u16, cmd: ID_FIND },
            ACCEL { fVirt: FCONTROL | FVIRTKEY, key: b'E' as u16, cmd: ID_EXTRACT },
            ACCEL { fVirt: FALT | FVIRTKEY, key: VK_UP.0, cmd: ID_UP },
            ACCEL { fVirt: FCONTROL | FVIRTKEY, key: b'L' as u16, cmd: ID_PATHBAR },
            ACCEL { fVirt: FALT | FVIRTKEY, key: b'D' as u16, cmd: ID_PATHBAR },
            ACCEL { fVirt: FVIRTKEY, key: VK_F5.0, cmd: ID_REFRESH },
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
            w!("EDIT"),
            WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            WS_EX_CLIENTEDGE,
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
        let _ = SetWindowSubclass(path_label, Some(path_proc), 3, 0);

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
        let _ = MoveWindow(label, pad, pad, (rc.right - fw - 3 * pad).max(0), bar - 2 * pad, true);
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
            WM_CONTEXTMENU => {
                let list = with_app(|a| a.list);
                if HWND(wp.0 as _) == list {
                    on_context_menu(hwnd, lp);
                    LRESULT(0)
                } else {
                    DefWindowProcW(hwnd, msg, wp, lp)
                }
            }
            drag::WM_APP_SHNOTIFY => {
                on_shell_notify(hwnd, wp, lp);
                LRESULT(0)
            }
            WM_TIMER if wp.0 == TIMER_SHELL => {
                if let Some(dir) = with_app(|a| a.shell_dir.clone()) {
                    drag::shell_refresh(&dir);
                }
                LRESULT(0)
            }
            WM_TIMER if wp.0 == TIMER_DROP => {
                let _ = KillTimer(Some(hwnd), TIMER_DROP);
                // The drop never reached a folder (e.g. the recycle bin).
                with_app(|a| a.drop_wait = None);
                LRESULT(0)
            }
            WM_APP_ICONS => {
                let (folder, icons) = *Box::from_raw(lp.0 as *mut (Option<i32>, Vec<(String, i32)>));
                with_app(|a| a.icons.warm(folder, icons));
                // If the image list could not be set at load time, the icons
                // are ready now: try again.
                let (list, set) = with_app(|a| (a.list, a.image_list_set));
                if !set && set_image_list(list) {
                    let _ = InvalidateRect(Some(list), None, false);
                }
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
                // Closing from inside a menu's modal loop would leave the
                // loop running without a window: close the menu first.
                if MENU_OPEN.with(|m| m.get()) {
                    let _ = EndMenu();
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                    return LRESULT(0);
                }
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
            ID_PATHBAR => focus_path_bar(),
            ID_REFRESH => reload(hwnd),
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
            LVN_BEGINDRAG => {
                let pt = (*(lp.0 as *const NMLISTVIEW)).ptAction;
                begin_drag(hdr.hwndFrom, pt);
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
                if vk == b'A' as u16 && ctrl_down() {
                    let all = LVITEMW { state: LVIS_SELECTED, stateMask: LVIS_SELECTED, ..Default::default() };
                    send(hwnd, LVM_SETITEMSTATE, usize::MAX, &all as *const _ as isize);
                    return LRESULT(0);
                }
            }
            WM_SYSKEYDOWN if wp.0 as u16 == VK_RETURN.0 && GetKeyState(VK_MENU.0 as i32) < 0 => {
                show_properties(GetParent(hwnd).unwrap_or_default());
                return LRESULT(0);
            }
            WM_SYSCHAR if wp.0 == 0x0D => return LRESULT(0),
            WM_CHAR => {
                let c = wp.0 as u32;
                if c == 0x0D || c == 0x08 || c == 0x1B || c == 0x01 {
                    return LRESULT(0); // handled in WM_KEYDOWN; avoid the beep
                }
                if c > 0x20 && c != 0x7F && !ctrl_down() {
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

/// Opens an archive or a folder (see [`start_open`]).
fn start_load(hwnd: HWND, path: PathBuf, password: Option<String>) {
    start_open(hwnd, Target::Path(path), password, None);
}

/// What to show next.
enum Target {
    /// A folder, an archive, or a path inside an archive
    /// (`D:\x\fotos.zip\2026`), decided on the worker thread.
    Path(PathBuf),
    Place(fsview::Place),
}

/// Splits `D:\x\fotos.zip\2026\a.jpg` into the archive file and the path
/// inside it (`2026/a.jpg`). Runs on the worker (touches the disk).
fn split_archive_path(path: &Path) -> Option<(PathBuf, String)> {
    let mut base = path.to_path_buf();
    let mut inner = Vec::new();
    while !base.exists() {
        inner.push(base.file_name()?.to_string_lossy().into_owned());
        base = base.parent()?.to_path_buf();
    }
    if !base.is_file() {
        return None;
    }
    inner.reverse();
    Some((base, inner.join("/")))
}

/// Opens a folder, an archive or a path inside an archive on a worker
/// thread. The UI thread stays free; the result arrives as WM_APP_LOADED.
/// `select` is a disk path to select in a folder listing.
fn start_open(hwnd: HWND, target: Target, password: Option<String>, select: Option<PathBuf>) {
    let (generation, sort) = with_app(|a| {
        a.generation += 1;
        (a.generation, a.sort)
    });
    timing::mark("open start", "");
    let archive_like = matches!(&target, Target::Path(p) if fsview::is_archive_name(&p.to_string_lossy()));
    if archive_like {
        set_status(t::STATUS_OPENING, "");
    }
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let shown_path;
        let result = match target {
            Target::Place(place) => {
                shown_path = PathBuf::from(place.shown());
                list_place(place, select)
            }
            Target::Path(path) if path.is_dir() => {
                shown_path = path.clone();
                list_place(fsview::Place::Folder(path), select)
            }
            Target::Path(path) => {
                let (archive, inner) = split_archive_path(&path).unwrap_or((path.clone(), String::new()));
                shown_path = archive.clone();
                open_archive(hwnd_raw, archive, password.clone(), sort, t0)
                    .map(|l| Opened::Archive(l, (!inner.is_empty()).then_some(inner)))
            }
        };
        let msg = Box::into_raw(Box::new(LoadMsg { generation, path: shown_path, password, result }));
        // SAFETY: on success the UI thread takes ownership of `msg`.
        unsafe {
            if PostMessageW(Some(HWND(hwnd_raw as _)), WM_APP_LOADED, WPARAM(0), LPARAM(msg as isize)).is_err() {
                drop(Box::from_raw(msg));
            }
        }
    });
}

fn list_place(place: fsview::Place, select: Option<PathBuf>) -> kibotos_core::Result<Opened> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
    }
    fsview::list(place).map(|l| Opened::Folder(l, select)).map_err(Error::Io)
}

fn open_archive(target: isize, path: PathBuf, password: Option<String>, sort: Sort, t0: Instant) -> kibotos_core::Result<Loaded> {
    let opts = OpenOptions { password: password.clone() };
    kibotos_core::open(&path, &opts).map(|reader| {
        let index = ArchiveIndex::build(reader.entries());
        prefetch_icons(target, &index);
        let names = Ordering::by_name(&index);
        let asc = Ordering::by_key(&index, reader.entries(), sort.key, &names);
        let ord = if sort.descending { asc.reversed(&index) } else { asc };
        let open_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let entries = reader.entries();
        let files = entries.iter().filter(|e: &&Entry| e.kind == EntryKind::File).count();
        let dirs = (1..index.len() as NodeId).filter(|&n| index.is_dir(n)).count();
        let summary = format!(
            "{}  ·  {}",
            strings::opened_in(reader.format().name(), entries.len(), open_ms),
            strings::items_summary(files, dirs)
        );
        Loaded { path, password, reader, index, names, ord, open_ms, summary }
    })
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
        let opened = match msg.result {
            Ok(o) => o,
            // Encrypted file names (7z, RAR): ask and open again.
            Err(e @ (Error::PasswordRequired | Error::WrongPassword)) => {
                let wrong = matches!(e, Error::WrongPassword) || msg.password.is_some();
                refresh_status();
                if let Some(pw) = dialogs::ask_password(hwnd, &file_name(&msg.path), wrong) {
                    start_load(hwnd, msg.path, Some(pw));
                }
                return;
            }
            Err(e) => {
                refresh_status();
                let title = if msg.path.is_dir() { t::FOLDER_ERROR_TITLE } else { t::ERROR_TITLE };
                let text = format!("{}\n\n{}", msg.path.display(), strings::error_message(&e));
                MessageBoxW(Some(hwnd), &HSTRING::from(text), &HSTRING::from(title), MB_OK | MB_ICONERROR);
                return;
            }
        };

        if !has_images {
            // First use of the shell icon list happens after the window is
            // already visible, so it does not delay startup.
            set_image_list(list);
        }

        let title = match opened {
            Opened::Archive(loaded, inner) => {
                let title = file_name(&loaded.path);
                let open_ms = loaded.open_ms;
                let mut select = None;
                with_app(|a| {
                    a.folder = ArchiveIndex::ROOT;
                    if let Some(node) = inner.as_deref().and_then(|p| loaded.index.find(p)) {
                        if loaded.index.is_dir(node) {
                            a.folder = node;
                        } else {
                            a.folder = loaded.index.parent(node).unwrap_or(ArchiveIndex::ROOT);
                            select = Some(node);
                        }
                    }
                    a.archive = Some(loaded);
                    a.fs = None;
                    a.query.clear();
                    // The rows still index the previous view; the list may
                    // repaint before refresh() below rebuilds them.
                    a.rows.clear();
                    a.rows_query.clear();
                });
                set_filter_text("");
                set_mode(true);
                refresh(select);
                timing::mark("archive visible", &format!("(open+index+sort={open_ms:.1} ms)"));
                title
            }
            Opened::Folder(mut listing, select) => {
                let icons = std::mem::take(&mut listing.icons);
                let title = listing.place.title();
                with_app(|a| {
                    a.icons.warm(icons.0, icons.1);
                    a.fs = Some(listing);
                    a.archive = None;
                    a.query.clear();
                    a.rows.clear();
                    a.rows_query.clear();
                });
                set_filter_text("");
                set_mode(false);
                let sel = select.and_then(|p| with_app(|a| a.fs.as_ref()?.items.iter().position(|i| i.path == p)));
                refresh(sel.map(|i| i as NodeId));
                timing::mark("folder visible", &title);
                title
            }
        };
        let _ = SetWindowTextW(hwnd, &HSTRING::from(format!("{title} - {}", t::APP_NAME)));
        if GetFocus() != with_app(|a| a.path_label) {
            let _ = SetFocus(Some(list));
        }
        let _ = UpdateWindow(list);
    }
}

/// Gives the list the system icons. The shell can fail to hand out its
/// image list while a worker thread is looking up icons at the same moment,
/// so this is retried until it succeeds.
fn set_image_list(list: HWND) -> bool {
    ensure_com();
    let Some(images) = IconCache::system_list() else { return false };
    send(list, LVM_SETIMAGELIST, LVSIL_SMALL as usize, images.0);
    with_app(|a| a.image_list_set = true);
    true
}

/// Column titles and filter hint for archive or disk views.
fn set_mode(archive: bool) {
    let (list, filter) = with_app(|a| (a.list, a.filter));
    // Third column: packed size (numbers, right) or type (text, left).
    let dpi = with_app(|a| a.dpi.max(96));
    let mut title = wide(if archive { t::COL_PACKED } else { t::COL_TYPE });
    let col = LVCOLUMNW {
        mask: LVCF_TEXT | LVCF_FMT | LVCF_WIDTH,
        fmt: if archive { LVCFMT_RIGHT } else { LVCFMT_LEFT },
        cx: scale(if archive { COLUMNS[2].1 } else { 160 }, dpi),
        pszText: PWSTR(title.as_mut_ptr()),
        ..Default::default()
    };
    send(list, LVM_SETCOLUMNW, 2, &col as *const _ as isize);
    let cue = wide(if archive { t::FILTER_CUE } else { t::FILTER_CUE_FOLDER });
    send(filter, EM_SETCUEBANNER, 1, cue.as_ptr() as isize);
}

// ------------------------------------------------------- view changes

/// Recomputes the rows for the current folder / filter, updates the list,
/// path bar and status, and selects `select` (or the first row).
fn refresh(select: Option<NodeId>) {
    let Some((list, label, count, sel, path_text, left, right)) = with_app(|a| {
        let filtering = !a.query.is_empty();
        let (path_text, right) = if let Some(fs) = a.fs.as_ref() {
            fsview::rows(&fs.items, a.sort, &a.query, &mut a.rows);
            if !filtering && fs.place.parent().is_some() {
                a.rows.insert(0, UP);
            }
            a.rows_query.clear();
            (fs.place.shown(), fs.summary.clone())
        } else {
            let arc = a.archive.as_ref()?;
            if filtering {
                let narrows =
                    !a.rows_query.is_empty() && a.query.to_lowercase().contains(&a.rows_query.to_lowercase());
                if narrows {
                    refine_rows(&arc.index, &a.query, &mut a.rows);
                } else {
                    arc.ord.filter_rows(&arc.index, &a.query, &mut a.rows);
                }
                a.rows_query = a.query.clone();
            } else {
                arc.ord.folder_rows(&arc.index, a.folder, &mut a.rows);
                // At the root ".." leads to the folder holding the archive.
                a.rows.insert(0, UP);
                a.rows_query.clear();
            }
            // The full path, so it can be copied or edited in the path bar.
            let mut path_text = arc.path.display().to_string();
            let folder_path = arc.index.path(a.folder);
            if !folder_path.is_empty() {
                path_text.push('\\');
                path_text.push_str(&folder_path.replace('/', "\\"));
            }
            (path_text, arc.summary.clone())
        };
        // By default select the first real item, not "..", so Enter keeps
        // going down and Backspace goes up.
        let first = usize::from(a.rows.len() > 1 && a.rows[0] == UP);
        let sel = select.and_then(|n| a.rows.iter().position(|&r| r == n)).unwrap_or(first);
        let shown = a.rows.iter().filter(|&&r| r != UP).count();
        let left = if filtering { strings::matches(shown, &a.query) } else { strings::folder_items(shown) };
        Some((a.list, a.path_label, a.rows.len(), sel, path_text, left, right))
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
    let hwnd = with_app(|a| unsafe { GetParent(a.list) }.unwrap_or_default());
    // Disk view: folders and drives open here, archives open in Kibotós,
    // other files with their program.
    let fs_item = with_app(|a| {
        let fs = a.fs.as_ref()?;
        let node = *a.rows.get(row)?;
        if node == UP {
            return Some(None);
        }
        let it = &fs.items[node as usize];
        Some(Some((it.path.clone(), it.is_container(), it.archive)))
    });
    match fs_item {
        Some(None) => return go_up(),
        Some(Some((path, true, _))) => return open_place(hwnd, fsview::Place::Folder(path), None),
        Some(Some((path, false, true))) => return start_load(hwnd, path, None),
        Some(Some((path, false, false))) => return shell_open(hwnd, &path, false),
        None => {}
    }
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
        open_entry(hwnd, node, false);
    }
}

/// Lists a disk place, selecting `select` (a path) if given.
fn open_place(hwnd: HWND, place: fsview::Place, select: Option<PathBuf>) {
    start_open(hwnd, Target::Place(place), None, select);
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
    enum Up {
        Folder(NodeId, Option<NodeId>),
        LeaveFilter,
        Place(fsview::Place, Option<PathBuf>),
    }
    let target = with_app(|a| {
        if !a.query.is_empty() {
            return Some(Up::LeaveFilter);
        }
        if let Some(arc) = a.archive.as_ref() {
            return Some(match arc.index.parent(a.folder) {
                Some(parent) => Up::Folder(parent, Some(a.folder)),
                // At the archive root: the folder that holds the archive.
                None => Up::Place(
                    arc.path.parent().map_or(fsview::Place::Computer, |p| fsview::Place::Folder(p.to_path_buf())),
                    Some(arc.path.clone()),
                ),
            });
        }
        let fs = a.fs.as_ref()?;
        let select = match &fs.place {
            fsview::Place::Folder(p) => Some(p.clone()),
            fsview::Place::Computer => None,
        };
        Some(Up::Place(fs.place.parent()?, select))
    });
    let hwnd = with_app(|a| unsafe { GetParent(a.list) }.unwrap_or_default());
    match target {
        Some(Up::Folder(folder, select)) => navigate(folder, select),
        Some(Up::LeaveFilter) => clear_filter(),
        Some(Up::Place(place, select)) => open_place(hwnd, place, select),
        None => {}
    }
}

fn clear_filter() {
    let filtering = with_app(|a| !a.query.is_empty());
    if filtering {
        if with_app(|a| a.fs.is_some()) {
            with_app(|a| a.query.clear());
            set_filter_text("");
            refresh(None);
        } else {
            let folder = with_app(|a| a.folder);
            navigate(folder, None);
            return;
        }
    }
    let list = with_app(|a| a.list);
    unsafe {
        let _ = SetFocus(Some(list));
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
            // Same column: reverse the current order; another: integer keys
            // with the name order as tie-breaker. No string sorting here.
            arc.ord = if arc.ord.sort.key == sort.key {
                arc.ord.reversed(&arc.index)
            } else {
                Ordering::by_key(&arc.index, arc.reader.entries(), sort.key, &arc.names)
            };
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

/// The selected rows (never ".."). With `whole_view`, an empty selection
/// means everything in view: the current folder, or all filter results.
///
/// Paths are made relative to the folder being viewed, so items land in
/// the destination by name. Filter results come from many folders: with
/// `by_name` each still lands by its own name (dragging), otherwise their
/// archive paths are kept (Ctrl+E on results).
fn pick(whole_view: bool, by_name: bool) -> Option<Pick> {
    let list = with_app(|a| a.list);
    let selected = selected_rows(list);
    with_app(|a| {
        let arc = a.archive.as_ref()?;
        let index = &arc.index;
        let filtering = !a.query.is_empty();
        let mut nodes: Vec<NodeId> = selected.iter().filter_map(|&i| a.rows.get(i).copied()).filter(|&n| n != UP).collect();
        let mut bases = Vec::new();
        if nodes.is_empty() {
            if !whole_view {
                return None;
            }
            if filtering {
                nodes = a.rows.clone();
            } else {
                nodes = vec![a.folder];
                bases.push(index.path(a.folder));
            }
        } else if !filtering {
            bases.push(index.path(a.folder));
        }
        if filtering && by_name {
            for &n in &nodes {
                bases.push(index.parent(n).map(|p| index.path(p)).unwrap_or_default());
            }
        }
        let mut ids = Vec::new();
        for &n in &nodes {
            index.entries_under(n, &mut ids);
        }
        ids.sort_unstable();
        ids.dedup();
        let entries = arc.reader.entries();
        let encrypted = ids.iter().any(|&id| entries[id as usize].encrypted);
        Some(Pick { archive: arc.path.clone(), password: arc.password.clone(), nodes, ids, bases, encrypted })
    })
}

/// Ctrl+E: the selected rows, or everything in view when nothing is
/// selected, into a folder chosen by the user.
fn on_extract(hwnd: HWND) {
    if with_app(|a| a.job.is_some()) {
        return;
    }
    if with_app(|a| a.fs.is_some()) {
        let archives = fs_selection().into_iter().filter(|s| s.2).map(|s| s.0).collect();
        return fs_extract(hwnd, archives, crate::cli::Mode::Dialog);
    }
    let Some(p) = pick(true, false) else { return };
    if p.ids.is_empty() {
        return;
    }
    let initial = p.archive.parent().map(Path::to_path_buf).unwrap_or_default();
    ensure_com();
    let Some(dest) = dialogs::pick_folder(hwnd, &initial) else { return };
    extract_to(hwnd, p, dest);
}

/// Asks for the password if needed and starts extracting `p` into `dest`.
fn extract_to(hwnd: HWND, p: Pick, dest: PathBuf) {
    if p.ids.is_empty() || with_app(|a| a.job.is_some()) {
        return;
    }
    let mut password = p.password;
    if p.encrypted && password.is_none() {
        let Some(pw) = dialogs::ask_password(hwnd, &file_name(&p.archive), false) else { return };
        password = Some(pw);
    }
    start_extract(
        hwnd,
        ExtractJob {
            archive: p.archive,
            password,
            ids: Some(p.ids),
            bases: p.bases,
            dest: Dest::Folder(dest),
            verify: p.encrypted,
            open_after: None,
            open_with: false,
        },
    );
}

// --------------------------------------------------------- context menu

/// Right-click, the Menu key or Shift+F10 on the list.
fn on_context_menu(hwnd: HWND, lp: LPARAM) {
    let list = with_app(|a| a.list);
    let mut pt = POINT { x: (lp.0 & 0xFFFF) as i16 as i32, y: ((lp.0 >> 16) & 0xFFFF) as i16 as i32 };
    if lp.0 == -1 {
        // From the keyboard: below the focused row.
        let mut rc = RECT { left: LVIR_LABEL as i32, ..Default::default() };
        let row = focused_row().unwrap_or(0);
        if send(list, LVM_GETITEMRECT, row, &mut rc as *mut _ as isize).0 == 0 {
            rc = RECT::default();
        }
        pt = POINT { x: rc.left, y: rc.bottom };
        unsafe {
            let _ = ClientToScreen(list, &mut pt);
        }
        // Rows are focused, not selected, after navigation: from the
        // keyboard the menu is for the focused row.
        if selected_rows(list).is_empty() && with_app(|a| a.rows.get(row).is_some_and(|&n| n != UP)) {
            let item = LVITEMW { state: LVIS_SELECTED, stateMask: LVIS_SELECTED, ..Default::default() };
            send(list, LVM_SETITEMSTATE, row, &item as *const _ as isize);
        }
    }
    if with_app(|a| a.fs.is_some()) {
        return fs_context_menu(hwnd, pt);
    }
    let Some((single, is_dir, count, filtering)) = with_app(|a| {
        let arc = a.archive.as_ref()?;
        let rows = selected_rows(a.list);
        let nodes: Vec<NodeId> = rows.iter().filter_map(|&i| a.rows.get(i).copied()).filter(|&n| n != UP).collect();
        let single = (nodes.len() == 1).then(|| nodes[0]);
        let is_dir = single.is_some_and(|n| arc.index.is_dir(n));
        Some((single, is_dir, nodes.len(), !a.query.is_empty()))
    }) else {
        return;
    };
    let busy = with_app(|a| a.job.is_some());
    unsafe {
        let menu = CreatePopupMenu().unwrap_or_default();
        let add = |id: u32, text: &str, enabled: bool| {
            let flags = if enabled { MF_STRING } else { MF_STRING | MF_GRAYED };
            let _ = AppendMenuW(menu, flags, id as usize, &HSTRING::from(text));
        };
        let sep = || {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        };
        if single.is_some() {
            add(CM_OPEN, t::CTX_OPEN, !busy || is_dir);
            if !is_dir {
                add(CM_OPEN_WITH, t::CTX_OPEN_WITH, !busy);
            }
            let _ = SetMenuDefaultItem(menu, CM_OPEN, 0);
            sep();
        }
        let extract_text = if count == 0 { t::CTX_EXTRACT_ALL } else { t::CTX_EXTRACT };
        add(CM_EXTRACT, extract_text, !busy);
        add(CM_EXTRACT_NEXT, t::CTX_EXTRACT_NEXT, !busy);
        if count > 0 {
            sep();
            if single.is_some() && filtering {
                add(CM_LOCATE, t::CTX_LOCATE, true);
            }
            add(CM_COPY_PATH, t::CTX_COPY_PATH, true);
            add(CM_PROPERTIES, t::CTX_PROPERTIES, true);
        }
        let cmd = track_menu(menu, hwnd, pt);
        let _ = DestroyMenu(menu);
        match cmd {
            CM_OPEN => {
                if let Some(i) = focused_or_selected() {
                    activate(i, false);
                }
            }
            CM_OPEN_WITH => {
                if let Some(node) = single {
                    open_entry(hwnd, node, true);
                }
            }
            CM_EXTRACT => on_extract(hwnd),
            CM_EXTRACT_NEXT => {
                if let Some(p) = pick(true, false) {
                    let dest = p.archive.parent().map(Path::to_path_buf).unwrap_or_default();
                    extract_to(hwnd, p, dest);
                }
            }
            CM_LOCATE => {
                if let Some(i) = focused_or_selected() {
                    activate(i, true);
                }
            }
            CM_COPY_PATH => copy_paths(hwnd),
            CM_PROPERTIES => show_properties(hwnd),
            _ => {}
        }
    }
}

thread_local!(static MENU_OPEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });

/// Shows a context menu and returns the chosen command (0 if none).
fn track_menu(menu: HMENU, hwnd: HWND, pt: POINT) -> u32 {
    MENU_OPEN.with(|m| m.set(true));
    let cmd = unsafe { TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, None, hwnd, None) };
    MENU_OPEN.with(|m| m.set(false));
    cmd.0 as u32
}

/// The focused row if it is selected, else the first selected row.
fn focused_or_selected() -> Option<usize> {
    let list = with_app(|a| a.list);
    let sel = selected_rows(list);
    match focused_row() {
        Some(f) if sel.contains(&f) => Some(f),
        _ => sel.first().copied().or_else(focused_row),
    }
}

/// Full paths of the selected rows: "D:\x\fotos.zip\2026\a.jpg".
fn copy_paths(hwnd: HWND) {
    let Some(p) = pick(false, false) else { return };
    let text = with_app(|a| {
        let arc = a.archive.as_ref()?;
        let lines: Vec<String> = p
            .nodes
            .iter()
            .map(|&n| format!("{}\\{}", arc.path.display(), arc.index.path(n).replace('/', "\\")))
            .collect();
        Some(lines.join("\r\n"))
    });
    if let Some(text) = text {
        if set_clipboard_text(hwnd, &text) {
            let summary = with_app(|a| a.shown.2.clone());
            set_status(t::STATUS_PATH_COPIED, &summary);
        }
    }
}

fn set_clipboard_text(hwnd: HWND, text: &str) -> bool {
    let units: Vec<u16> = text.encode_utf16().chain([0]).collect();
    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return false;
        }
        let _ = EmptyClipboard();
        let mut ok = false;
        if let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2) {
            let dst = GlobalLock(h) as *mut u16;
            if !dst.is_null() {
                std::ptr::copy_nonoverlapping(units.as_ptr(), dst, units.len());
                let _ = GlobalUnlock(h);
                // The clipboard owns the memory once this succeeds.
                ok = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(h.0))).is_ok();
            }
        }
        let _ = CloseClipboard();
        ok
    }
}

/// Alt+Enter: name, sizes, date, CRC and encryption of the selection.
fn show_properties(hwnd: HWND) {
    if with_app(|a| a.fs.is_some()) {
        if let [(path, _, _)] = &fs_selection()[..] {
            shell_properties(hwnd, path);
        }
        return;
    }
    let Some(p) = pick(false, false) else { return };
    let Some(text) = with_app(|a| {
        let arc = a.archive.as_ref()?;
        let index = &arc.index;
        let entries = arc.reader.entries();
        let mut size = 0u64;
        let mut packed = 0u64;
        let mut unknown = false;
        let (mut files, mut dirs) = (0usize, 0usize);
        for &id in &p.ids {
            let e = &entries[id as usize];
            match e.kind {
                EntryKind::Dir => dirs += 1,
                _ => {
                    files += 1;
                    match e.size {
                        Some(s) => size += s,
                        None => unknown = true,
                    }
                    packed += e.packed.unwrap_or(0);
                }
            }
        }
        let size_text = if unknown { t::PROP_UNKNOWN.to_string() } else { strings::bytes_exact(size) };
        let packed_text = format!("{}{}", strings::bytes_exact(packed), strings::ratio(packed, size));
        let mut lines = Vec::new();
        if let [node] = p.nodes[..] {
            let location = index.parent(node).map(|q| index.path(q)).unwrap_or_default().replace('/', "\\");
            let entry = index.entry(node).map(|id| &entries[id as usize]);
            let is_dir = index.is_dir(node);
            let kind = match entry.map(|e| e.kind) {
                Some(EntryKind::Link) => t::PROP_LINK,
                _ if is_dir => t::PROP_FOLDER,
                _ => t::PROP_FILE,
            };
            lines.push(format!("{}  {}", t::PROP_NAME, index.name(node)));
            let mut loc = file_name(&arc.path);
            if !location.is_empty() {
                loc = format!("{loc}\\{location}");
            }
            lines.push(format!("{}  {loc}", t::PROP_LOCATION));
            lines.push(format!("{}  {kind}", t::PROP_TYPE));
            if is_dir {
                let inner_dirs = dirs.saturating_sub(usize::from(entry.is_some()));
                lines.push(format!("{}  {}", t::PROP_CONTAINS, strings::items_summary(files, inner_dirs)));
            }
            lines.push(String::new());
            lines.push(format!("{}  {size_text}", t::PROP_SIZE));
            lines.push(format!("{}  {packed_text}", t::PROP_PACKED));
            if let Some(e) = entry {
                lines.push(format!("{}  {}", t::PROP_MODIFIED, format::date(e.modified)));
                if let Some(crc) = e.crc {
                    lines.push(format!("CRC32:  {crc:08X}"));
                }
                lines.push(format!("{}  {}", t::PROP_ENCRYPTED, if e.encrypted { t::YES } else { t::NO }));
            }
        } else {
            lines.push(format!("{}  {}", t::PROP_SELECTION, strings::selection_count(p.nodes.len())));
            lines.push(format!("{}  {}", t::PROP_CONTAINS, strings::items_summary(files, dirs)));
            lines.push(String::new());
            lines.push(format!("{}  {size_text}", t::PROP_SIZE));
            lines.push(format!("{}  {packed_text}", t::PROP_PACKED));
            if p.encrypted {
                lines.push(format!("{}  {}", t::PROP_ENCRYPTED, t::YES));
            }
        }
        Some(lines.join("\n"))
    }) else {
        return;
    };
    dialogs::message(hwnd, t::PROP_TITLE, &text, MB_ICONINFORMATION);
}

// ------------------------------------------------------------- drag out

/// LVN_BEGINDRAG: drag the selected rows out of the window.
fn begin_drag(list: HWND, pt: POINT) {
    if with_app(|a| a.job.is_some() || a.drop_wait.is_some()) {
        return;
    }
    let hwnd = unsafe { GetParent(list) }.unwrap_or_default();
    if with_app(|a| a.fs.is_some()) {
        // Real files: a normal shell drag (copies).
        let files: Vec<PathBuf> =
            fs_selection().into_iter().filter(|s| s.1 != fsview::Kind::Place).map(|s| s.0).collect();
        if !files.is_empty() {
            ensure_com();
            drag::files(hwnd, list, pt, &files);
        }
        return;
    }
    let Some(p) = pick(false, true) else { return };
    if p.ids.is_empty() {
        return;
    }
    ensure_com();
    let Some(watch) = drag::Watch::start(hwnd) else { return };
    let marker = watch.marker().to_path_buf();
    // The shell may copy the marker before the drag loop returns, so the
    // watch must be in place (and its result kept) during the drag.
    with_app(|a| a.drop_wait = Some((watch, p.clone())));
    // For a program other than the shell: extract to temporary files when
    // the button is released over it.
    let mut once = Some(p);
    let extract_now = Box::new(move || once.take().and_then(|p| extract_for_drop(hwnd, p)));
    let outcome = drag::run(hwnd, list, pt, &marker, extract_now);
    if with_app(|a| a.drop_wait.is_none()) {
        return; // the marker already landed and extraction has started
    }
    match outcome {
        drag::Outcome::Shell => unsafe {
            SetTimer(Some(hwnd), TIMER_DROP, DROP_WAIT_MS, None);
        },
        _ => with_app(|a| a.drop_wait = None),
    }
}

/// The shell reported a change: is it our marker landing in a folder?
fn on_shell_notify(hwnd: HWND, wp: WPARAM, lp: LPARAM) {
    let folder = with_app(|a| a.drop_wait.as_ref().and_then(|(w, _)| w.check(wp, lp)));
    let Some(folder) = folder else { return };
    // Explorer copies the marker about 0.1 s after the drop but keeps the
    // drag loop busy for another ~0.35 s finishing its copy: start now.
    drop_into(hwnd, folder);
}

/// Extracts the dragged entries into the folder the shell dropped them on.
fn drop_into(hwnd: HWND, folder: PathBuf) {
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_DROP);
    }
    let Some((watch, p)) = with_app(|a| a.drop_wait.take()) else { return };
    drop(watch);
    timing::mark("drop", &folder.display().to_string());
    extract_to(hwnd, p, folder);
}

/// Extracts the dragged entries to a temporary folder while the drop is in
/// progress (a modal wait with the progress window) and returns the paths
/// of the dragged items there.
fn extract_for_drop(hwnd: HWND, p: Pick) -> Option<Vec<PathBuf>> {
    let mut password = p.password.clone();
    if p.encrypted && password.is_none() {
        password = Some(dialogs::ask_password(hwnd, &file_name(&p.archive), false)?);
    }
    let slot = temp::new_slot().ok()?;
    let names: Vec<String> = with_app(|a| {
        let arc = a.archive.as_ref()?;
        Some(p.nodes.iter().map(|&n| arc.index.name(n).to_string()).collect())
    })?;
    with_app(|a| {
        a.blocking = true;
        a.blocking_done = None;
    });
    start_extract(
        hwnd,
        ExtractJob {
            archive: p.archive.clone(),
            password,
            ids: Some(p.ids),
            bases: p.bases,
            dest: Dest::Folder(slot.clone()),
            verify: p.encrypted,
            open_after: None,
            open_with: false,
        },
    );
    let done = unsafe { wait_blocking() };
    with_app(|a| a.blocking = false);
    let done = done?;
    match &done.result {
        Ok(report) => dialogs::show_blocked(hwnd, report),
        Err(Error::Cancelled) => {
            refresh_status();
            return None;
        }
        Err(e) => {
            refresh_status();
            dialogs::message(hwnd, t::EXTRACT_ERROR_TITLE, &strings::error_message(e), MB_ICONERROR);
            return None;
        }
    }
    refresh_status();
    let files: Vec<PathBuf> = names
        .iter()
        .filter_map(|n| kibotos_core::safety::safe_join(&slot, n).ok())
        .filter(|f| f.exists())
        .collect();
    (!files.is_empty()).then_some(files)
}

/// Pumps messages until the blocking extraction reports back. Esc cancels
/// (the mouse is still captured by the drag, so the button may not work).
unsafe fn wait_blocking() -> Option<ExtractDone> {
    let mut msg = MSG::default();
    loop {
        if let Some(done) = with_app(|a| a.blocking_done.take()) {
            return Some(done);
        }
        unsafe {
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                PostQuitMessage(msg.wParam.0 as i32);
                return None;
            }
            if msg.message == WM_KEYDOWN && msg.wParam.0 as u16 == VK_ESCAPE.0 {
                if let Some(ctl) = with_app(|a| a.job.clone()) {
                    ctl.cancel();
                }
                continue;
            }
            if let Some(p) = progress::current() {
                if IsDialogMessageW(p, &msg).as_bool() {
                    continue;
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Extracts one file to a fresh temporary folder and opens it.
fn open_entry(hwnd: HWND, node: NodeId, with: bool) {
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
            bases: vec![base.to_string()],
            dest: Dest::Folder(slot),
            verify: encrypted,
            open_after: Some(target.unwrap_or_default()),
            open_with: with,
        },
    );
}

fn shell_open(hwnd: HWND, file: &Path, with: bool) {
    ensure_com();
    let path = HSTRING::from(file.as_os_str());
    let dir = HSTRING::from(file.parent().unwrap_or(file).as_os_str());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        hwnd,
        lpFile: PCWSTR(path.as_ptr()),
        lpDirectory: PCWSTR(dir.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        lpVerb: if with { w!("openas") } else { PCWSTR::null() },
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
    if job.open_after.is_none() && !with_app(|a| a.blocking) {
        with_app(|a| a.shell_dir = Some(job.dest.shown().to_path_buf()));
        unsafe {
            SetTimer(Some(hwnd), TIMER_SHELL, SHELL_REFRESH_MS, None);
        }
    }
    job::spawn(hwnd, job, ctl);
}

unsafe fn on_extracted(hwnd: HWND, lp: LPARAM) {
    let done = unsafe { job::take_done(lp) };
    progress::close();
    with_app(|a| a.job = None);
    if with_app(|a| a.shell_dir.take()).is_some() {
        unsafe {
            let _ = KillTimer(Some(hwnd), TIMER_SHELL);
        }
        drag::shell_refresh(&done.dest);
    }
    if with_app(|a| a.blocking) {
        with_app(|a| a.blocking_done = Some(done));
        return;
    }
    let ExtractDone { mut job, dest, result, secs } = done;
    // Start the next queued archive once this one is fully handled.
    struct Next(HWND);
    impl Drop for Next {
        fn drop(&mut self) {
            if with_app(|a| a.job.is_some()) {
                return; // a retry (password) is running
            }
            match with_app(|a| a.queue.pop_front()) {
                Some(j) => start_extract(self.0, j),
                None => {
                    if with_app(|a| a.fs.is_some()) {
                        reload(self.0);
                    }
                }
            }
        }
    }
    let _next = Next(hwnd);
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
                    shell_open(hwnd, target, job.open_with);
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
            with_app(|a| a.queue.clear());
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
            let text = format!("{}\n\n{}", file_name(&job.archive), strings::error_message(&e));
            dialogs::message(hwnd, t::EXTRACT_ERROR_TITLE, &text, MB_ICONERROR);
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
        let right = match (&a.fs, &a.archive) {
            (Some(fs), _) => fs.summary.clone(),
            (None, Some(arc)) => arc.summary.clone(),
            (None, None) => return Some((t::STATUS_READY.to_string(), String::new())),
        };
        let shown = a.rows.iter().filter(|&&r| r != UP).count();
        let left = if a.query.is_empty() { strings::folder_items(shown) } else { strings::matches(shown, &a.query) };
        Some((left, right))
    }) else {
        return;
    };
    set_status(&left, &right);
}

// ------------------------------------------------------------ disk view

/// Selected disk items (or the focused one when nothing is selected).
fn fs_selection() -> Vec<(PathBuf, fsview::Kind, bool)> {
    let list = with_app(|a| a.list);
    let mut rows = selected_rows(list);
    if rows.is_empty() {
        rows.extend(focused_row());
    }
    with_app(|a| {
        let Some(fs) = a.fs.as_ref() else { return Vec::new() };
        rows.iter()
            .filter_map(|&i| a.rows.get(i).copied())
            .filter(|&n| n != UP)
            .filter_map(|n| fs.items.get(n as usize))
            .map(|it| (it.path.clone(), it.kind, it.archive))
            .collect()
    })
}

/// Context menu for disk items.
fn fs_context_menu(hwnd: HWND, pt: POINT) {
    let list = with_app(|a| a.list);
    let sel: Vec<_> = {
        let rows = selected_rows(list);
        if rows.is_empty() { Vec::new() } else { fs_selection() }
    };
    let archives: Vec<PathBuf> = sel.iter().filter(|s| s.2).map(|s| s.0.clone()).collect();
    let busy = with_app(|a| a.job.is_some());
    unsafe {
        let menu = CreatePopupMenu().unwrap_or_default();
        let add = |id: u32, text: &str, enabled: bool| {
            let flags = if enabled { MF_STRING } else { MF_STRING | MF_GRAYED };
            let _ = AppendMenuW(menu, flags, id as usize, &HSTRING::from(text));
        };
        let sep = || {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        };
        if let [(_, kind, archive)] = sel[..] {
            add(CM_OPEN, if archive { t::CTX_OPEN_ARCHIVE } else { t::CTX_OPEN }, true);
            if kind == fsview::Kind::File {
                add(CM_OPEN_WITH, t::CTX_OPEN_WITH, true);
            }
            let _ = SetMenuDefaultItem(menu, CM_OPEN, 0);
        }
        if !archives.is_empty() {
            sep();
            add(CM_FS_EXTRACT_HERE, t::CTX_EXTRACT_HERE, !busy);
            let folder_label = match &archives[..] {
                [one] => strings::extract_to_folder(&job::folder_name(one)),
                _ => t::CTX_EXTRACT_EACH.to_string(),
            };
            add(CM_FS_EXTRACT_FOLDER, &folder_label, !busy);
            add(CM_EXTRACT, t::CTX_EXTRACT, !busy);
        }
        if !sel.is_empty() {
            sep();
            add(CM_SHOW_IN_EXPLORER, t::CTX_SHOW_IN_EXPLORER, sel.len() == 1);
            add(CM_COPY_PATH, t::CTX_COPY_PATH, true);
            add(CM_PROPERTIES, t::CTX_PROPERTIES, sel.len() == 1);
        } else {
            add(CM_REFRESH, t::CTX_REFRESH, true);
        }
        let cmd = track_menu(menu, hwnd, pt);
        let _ = DestroyMenu(menu);
        match cmd {
            CM_OPEN => {
                if let Some(i) = focused_or_selected() {
                    activate(i, false);
                }
            }
            CM_OPEN_WITH => {
                if let [(path, _, _)] = &sel[..] {
                    shell_open(hwnd, path, true);
                }
            }
            CM_FS_EXTRACT_HERE => fs_extract(hwnd, archives, crate::cli::Mode::Here),
            CM_FS_EXTRACT_FOLDER => fs_extract(hwnd, archives, crate::cli::Mode::ToFolder),
            CM_EXTRACT => fs_extract(hwnd, archives, crate::cli::Mode::Dialog),
            CM_SHOW_IN_EXPLORER => {
                if let [(path, kind, _)] = &sel[..] {
                    show_in_explorer(hwnd, path, *kind);
                }
            }
            CM_COPY_PATH => {
                let text: Vec<String> = sel.iter().map(|s| s.0.display().to_string()).collect();
                if set_clipboard_text(hwnd, &text.join("\r\n")) {
                    let summary = with_app(|a| a.shown.2.clone());
                    set_status(t::STATUS_PATH_COPIED, &summary);
                }
            }
            CM_PROPERTIES => {
                if let [(path, _, _)] = &sel[..] {
                    shell_properties(hwnd, path);
                }
            }
            CM_REFRESH => reload(hwnd),
            _ => {}
        }
    }
}

/// Extracts whole archives from the disk view, one after another, like
/// the Explorer context menu does.
fn fs_extract(hwnd: HWND, archives: Vec<PathBuf>, mode: crate::cli::Mode) {
    use crate::cli::Mode;
    if archives.is_empty() || with_app(|a| a.job.is_some()) {
        return;
    }
    let chosen = if mode == Mode::Dialog {
        let initial = archives[0].parent().map(Path::to_path_buf).unwrap_or_default();
        ensure_com();
        let Some(dest) = dialogs::pick_folder(hwnd, &initial) else { return };
        Some(dest)
    } else {
        None
    };
    let several = archives.len() > 1;
    let mut jobs: std::collections::VecDeque<ExtractJob> = archives
        .into_iter()
        .map(|archive| {
            let parent = archive.parent().map(Path::to_path_buf).unwrap_or_default();
            let name = job::folder_name(&archive);
            let dest = match (&chosen, mode) {
                (Some(d), _) if several => Dest::Folder(d.join(&name)),
                (Some(d), _) => Dest::Folder(d.clone()),
                (None, Mode::Here) => Dest::Smart { parent, folder_name: name },
                (None, _) => Dest::Folder(parent.join(name)),
            };
            ExtractJob {
                archive,
                password: None,
                ids: None,
                bases: Vec::new(),
                dest,
                verify: true,
                open_after: None,
                open_with: false,
            }
        })
        .collect();
    let first = jobs.pop_front();
    with_app(|a| a.queue = jobs);
    if let Some(j) = first {
        start_extract(hwnd, j);
    }
}

/// F5: reads the folder or archive again, keeping the focused row.
fn reload(hwnd: HWND) {
    let focused = focused_row();
    let (place, archive) = with_app(|a| {
        let place = a.fs.as_ref().map(|f| f.place.clone());
        let archive = a.archive.as_ref().map(|l| (l.path.clone(), l.password.clone()));
        (place, archive)
    });
    if let Some(place) = place {
        let select = focused.and_then(|i| {
            with_app(|a| {
                let n = *a.rows.get(i)?;
                a.fs.as_ref()?.items.get(n as usize).map(|it| it.path.clone())
            })
        });
        open_place(hwnd, place, select);
    } else if let Some((path, password)) = archive {
        let folder = with_app(|a| a.archive.as_ref().map(|l| l.index.path(a.folder))).unwrap_or_default();
        let target = if folder.is_empty() { path } else { path.join(folder.replace('/', "\\")) };
        start_open(hwnd, Target::Path(target), password, None);
    }
}

fn show_in_explorer(hwnd: HWND, path: &Path, kind: fsview::Kind) {
    if kind == fsview::Kind::Place {
        return shell_open(hwnd, path, false);
    }
    ensure_com();
    let args = HSTRING::from(format!("/select,\"{}\"", path.display()));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        hwnd,
        lpFile: w!("explorer.exe"),
        lpParameters: PCWSTR(args.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    let _ = unsafe { ShellExecuteExW(&mut info) };
}

/// The shell's own properties dialog for a disk item.
fn shell_properties(hwnd: HWND, path: &Path) {
    ensure_com();
    let file = HSTRING::from(path.as_os_str());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_INVOKEIDLIST,
        hwnd,
        lpVerb: w!("properties"),
        lpFile: PCWSTR(file.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    let _ = unsafe { ShellExecuteExW(&mut info) };
}

// ------------------------------------------------------------ path bar

/// Keyboard in the path bar: Enter goes to the typed path (a folder, an
/// archive, or a folder inside an archive), Esc puts the current one back.
unsafe extern "system" fn path_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    unsafe {
        match msg {
            WM_KEYDOWN if wp.0 as u16 == VK_RETURN.0 => {
                let mut buf = vec![0u16; 2048];
                let n = GetWindowTextW(hwnd, &mut buf) as usize;
                let text = String::from_utf16_lossy(&buf[..n]);
                let text = text.trim().trim_matches('"');
                let main = GetParent(hwnd).unwrap_or_default();
                let list = with_app(|a| a.list);
                if !text.is_empty() {
                    let target = if text.eq_ignore_ascii_case(t::COMPUTER) {
                        Target::Place(fsview::Place::Computer)
                    } else {
                        Target::Path(PathBuf::from(text))
                    };
                    with_app(|a| a.shown.0.clear());
                    start_open(main, target, None, None);
                }
                let _ = SetFocus(Some(list));
                return LRESULT(0);
            }
            WM_KEYDOWN if wp.0 as u16 == VK_ESCAPE.0 => {
                let (list, shown) = with_app(|a| (a.list, a.shown.0.clone()));
                let _ = SetWindowTextW(hwnd, &HSTRING::from(shown));
                let _ = SetFocus(Some(list));
                return LRESULT(0);
            }
            WM_CHAR if matches!(wp.0, 0x0D | 0x1B) => return LRESULT(0),
            _ => {}
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}

fn focus_path_bar() {
    let bar = with_app(|a| a.path_label);
    unsafe {
        let _ = SetFocus(Some(bar));
    }
    send(bar, EM_SETSEL, 0, -1);
}

// ------------------------------------------------------------ painting

/// Supplies text and icons for the rows the ListView is painting.
fn fill_row(di: &mut NMLVDISPINFOW) {
    APP.with(|cell| {
        let Ok(mut guard) = cell.try_borrow_mut() else { return };
        let app = &mut *guard;
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

        if let Some(fs) = &app.fs {
            let Some(it) = fs.items.get(node as usize) else { return };
            if wants_image {
                di.item.iImage = if it.icon >= 0 {
                    it.icon
                } else if it.is_container() {
                    app.icons.folder()
                } else {
                    app.icons.file(&it.name)
                };
            }
            if wants_text {
                let text = match di.item.iSubItem {
                    0 => it.name.clone(),
                    1 if !it.size_text.is_empty() => it.size_text.clone(),
                    1 => format::size(it.size),
                    2 => it.extra.clone(),
                    3 => format::date(it.modified),
                    _ => String::new(),
                };
                copy_text(&text, di.item.pszText, di.item.cchTextMax);
            }
            return;
        }

        let Some(arc) = &app.archive else { return };
        let index = &arc.index;
        if node as usize >= index.len() {
            return; // a row of another view, about to be replaced
        }
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
