//! Small modal dialogs: destination folder, password, overwrite question,
//! and the report of entries blocked for safety.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use kibotos_core::{Conflict, ExtractReport, Timestamp};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateFontIndirectW, DeleteObject, HFONT, HGDIOBJ};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TASKDIALOGCONFIG_0, TD_WARNING_ICON,
    TDF_ALLOW_DIALOG_CANCELLATION, TDF_POSITION_RELATIVE_TO_WINDOW, TaskDialogIndirect,
};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem,
    SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, HSTRING, PCWSTR, w};

use crate::strings::{self, t};
use crate::{format, wide};

fn scale(v: i32, dpi: u32) -> i32 {
    v * dpi as i32 / 96
}

// ------------------------------------------------------------ folder

/// Asks for a destination folder, starting at `initial`.
pub fn pick_folder(owner: HWND, initial: &Path) -> Option<PathBuf> {
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dlg.GetOptions().ok()?;
        dlg.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST).ok()?;
        let _ = dlg.SetTitle(&HSTRING::from(t::PICK_DEST_TITLE));
        let _ = dlg.SetOkButtonLabel(&HSTRING::from(t::PICK_DEST_OK));
        if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(initial.as_os_str()), None) {
            let _ = dlg.SetFolder(&item);
        }
        dlg.Show(Some(owner)).ok()?; // an error here means "cancelled"
        let item = dlg.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as _));
        path.map(PathBuf::from)
    }
}

// ---------------------------------------------------------- password

const IDOK_: i32 = IDOK.0;
const IDCANCEL_: i32 = IDCANCEL.0;

#[derive(Default)]
struct Prompt {
    edit: HWND,
    result: Option<String>,
    done: bool,
}

thread_local! {
    static PROMPT: RefCell<Prompt> = RefCell::new(Prompt::default());
}

extern "system" fn prompt_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let id = (wp.0 & 0xFFFF) as i32;
                if id == IDOK_ {
                    let edit = PROMPT.with(|p| p.borrow().edit);
                    let mut buf = vec![0u16; 1024];
                    let n = GetWindowTextW(edit, &mut buf) as usize;
                    let text = String::from_utf16_lossy(&buf[..n]);
                    PROMPT.with(|p| {
                        let mut p = p.borrow_mut();
                        p.result = Some(text);
                        p.done = true;
                    });
                } else if id == IDCANCEL_ {
                    PROMPT.with(|p| p.borrow_mut().done = true);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                PROMPT.with(|p| p.borrow_mut().done = true);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn message_font(dpi: u32) -> HFONT {
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

/// Centers a window of size (w, h) over `owner` (or the screen).
fn centered(owner: HWND, w: i32, h: i32) -> (i32, i32) {
    let mut r = RECT::default();
    unsafe {
        if owner.is_invalid() || GetWindowRect(owner, &mut r).is_err() {
            r = RECT { left: 0, top: 0, right: GetSystemMetrics(SM_CXSCREEN), bottom: GetSystemMetrics(SM_CYSCREEN) };
        }
    }
    (r.left + (r.right - r.left - w) / 2, r.top + (r.bottom - r.top - h) / 2)
}

/// Asks for a password. Returns `None` if the user cancels.
pub fn ask_password(owner: HWND, archive_name: &str, wrong: bool) -> Option<String> {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("KibotosPrompt");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(prompt_proc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH((windows::Win32::Graphics::Gdi::COLOR_BTNFACE.0 + 1) as _),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc); // fails harmlessly if already registered

        let dpi = if owner.is_invalid() { 96 } else { GetDpiForWindow(owner).max(96) };
        let s = |v| scale(v, dpi);
        let (w, h) = (s(420), s(170));
        let (x, y) = centered(owner, w, h);
        let dlg = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT,
            class,
            &HSTRING::from(t::PASSWORD_TITLE),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            x,
            y,
            w,
            h,
            Some(owner),
            None,
            Some(hinst.into()),
            None,
        )
        .ok()?;
        let font = message_font(dpi);
        let mut rc = RECT::default();
        let _ = GetClientRect(dlg, &mut rc);
        let cw = rc.right;
        let mk = |class: PCWSTR, text: &str, style: u32, ex: WINDOW_EX_STYLE, x, y, w, h, id: i32| {
            let hw = CreateWindowExW(
                ex,
                class,
                &HSTRING::from(text),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
                x,
                y,
                w,
                h,
                Some(dlg),
                Some(HMENU(id as _)),
                Some(hinst.into()),
                None,
            )
            .unwrap_or_default();
            SendMessageW(hw, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
            hw
        };
        let prompt = strings::password_prompt(archive_name, wrong);
        mk(w!("STATIC"), &prompt, 0, WINDOW_EX_STYLE(0), s(12), s(10), cw - s(24), s(36), 0);
        const ES_PASSWORD: u32 = 0x20;
        const ES_AUTOHSCROLL: u32 = 0x80;
        let edit = mk(
            w!("EDIT"),
            "",
            ES_PASSWORD | ES_AUTOHSCROLL | WS_TABSTOP.0,
            WS_EX_CLIENTEDGE,
            s(12),
            s(50),
            cw - s(24),
            s(24),
            100,
        );
        const BS_DEFPUSHBUTTON: u32 = 1;
        let bw = s(90);
        mk(w!("BUTTON"), t::OK, BS_DEFPUSHBUTTON | WS_TABSTOP.0, WINDOW_EX_STYLE(0), cw - 2 * bw - s(20), s(88), bw, s(28), IDOK_);
        mk(w!("BUTTON"), t::CANCEL, WS_TABSTOP.0, WINDOW_EX_STYLE(0), cw - bw - s(12), s(88), bw, s(28), IDCANCEL_);

        PROMPT.with(|p| *p.borrow_mut() = Prompt { edit, ..Default::default() });
        if !owner.is_invalid() {
            let _ = EnableWindow(owner, false);
        }
        let _ = ShowWindow(dlg, SW_SHOW);
        let _ = SetFocus(Some(edit));

        let mut msg = MSG::default();
        while !PROMPT.with(|p| p.borrow().done) {
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                PostQuitMessage(msg.wParam.0 as i32);
                break;
            }
            // Tab, Enter (default button) and Esc (IDCANCEL) like a dialog.
            if !IsDialogMessageW(dlg, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if !owner.is_invalid() {
            let _ = EnableWindow(owner, true);
        }
        let _ = DestroyWindow(dlg);
        let _ = DeleteObject(HGDIOBJ(font.0));
        PROMPT.with(|p| p.borrow_mut().result.take())
    }
}

// --------------------------------------------------------- conflicts

fn describe(size: Option<u64>, modified: Option<Timestamp>) -> String {
    let size = size.map(strings::bytes).unwrap_or_default();
    let date = format::date(modified);
    match (size.is_empty(), date.is_empty()) {
        (false, false) => format!("{size}, {date}"),
        (false, true) => size,
        _ => date,
    }
}

/// "This file already exists" with Replace / Skip / Cancel and a
/// "do the same for the rest" check box.
pub fn ask_conflict(owner: HWND, target: &Path, new_size: Option<u64>, new_modified: Option<Timestamp>) -> Conflict {
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let existing = std::fs::metadata(target).ok();
    let ex_size = existing.as_ref().map(|m| m.len());
    let ex_time = existing
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| Timestamp::Utc(d.as_secs() as i64));
    let content = format!(
        "{} {}\n{} {}",
        t::CONFLICT_EXISTING,
        describe(ex_size, ex_time),
        t::CONFLICT_NEW,
        describe(new_size, new_modified)
    );

    let title = wide(t::CONFLICT_TITLE);
    let main = wide(&strings::conflict_question(&name));
    let body = wide(&content);
    let verify = wide(t::CONFLICT_ALL);
    let replace = wide(t::CONFLICT_REPLACE);
    let skip = wide(t::CONFLICT_SKIP);
    // Our own Cancel button: the built-in one is labelled in the Windows
    // display language, not in Spanish.
    let cancel = wide(t::CANCEL);
    const REPLACE: i32 = 100;
    const SKIP: i32 = 101;
    let buttons = [
        TASKDIALOG_BUTTON { nButtonID: REPLACE, pszButtonText: PCWSTR(replace.as_ptr()) },
        TASKDIALOG_BUTTON { nButtonID: SKIP, pszButtonText: PCWSTR(skip.as_ptr()) },
        TASKDIALOG_BUTTON { nButtonID: IDCANCEL_, pszButtonText: PCWSTR(cancel.as_ptr()) },
    ];
    let cfg = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: owner,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_POSITION_RELATIVE_TO_WINDOW,
        pszWindowTitle: PCWSTR(title.as_ptr()),
        Anonymous1: TASKDIALOGCONFIG_0 { pszMainIcon: TD_WARNING_ICON },
        pszMainInstruction: PCWSTR(main.as_ptr()),
        pszContent: PCWSTR(body.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: REPLACE,
        pszVerificationText: PCWSTR(verify.as_ptr()),
        ..Default::default()
    };
    let mut button = 0i32;
    let mut all = BOOL(0);
    if unsafe { TaskDialogIndirect(&cfg, Some(&mut button), None, Some(&mut all)) }.is_err() {
        return Conflict::Cancel;
    }
    match (button, all.as_bool()) {
        (REPLACE, false) => Conflict::Replace,
        (REPLACE, true) => Conflict::ReplaceAll,
        (SKIP, false) => Conflict::Skip,
        (SKIP, true) => Conflict::SkipAll,
        _ => Conflict::Cancel,
    }
}

// ----------------------------------------------------------- reports

/// Lists the entries that were refused for safety, if any.
pub fn show_blocked(owner: HWND, report: &ExtractReport) {
    let blocked: Vec<_> = report.blocked().collect();
    if blocked.is_empty() {
        return;
    }
    const SHOWN: usize = 12;
    let mut text = format!("{}\n\n", t::BLOCKED_INTRO);
    for s in blocked.iter().take(SHOWN) {
        text.push_str(&format!("•  {}  —  {}\n", s.path, strings::blocked_reason(&s.reason)));
    }
    if blocked.len() > SHOWN {
        text.push_str(&format!("…  {} {}\n", t::BLOCKED_MORE, strings::number((blocked.len() - SHOWN) as u64)));
    }
    message(owner, t::BLOCKED_TITLE, &text, MB_ICONWARNING);
}

pub fn message(owner: HWND, title: &str, text: &str, icon: MESSAGEBOX_STYLE) {
    unsafe {
        MessageBoxW(Some(owner), &HSTRING::from(text), &HSTRING::from(title), MB_OK | icon);
    }
}
