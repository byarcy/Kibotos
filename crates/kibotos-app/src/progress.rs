//! The progress window for long operations.
//!
//! The worker thread never talks to this window: it only updates the
//! atomic counters in `Control`. A 100 ms timer here reads them, so
//! progress costs nothing per file or per chunk. The window appears only
//! if the operation is still running after 300 ms, so quick extractions do
//! not flash a window. Cancel, Esc or closing the window request
//! cancellation; the worker stops at its next chunk.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Instant;

use kibotos_core::Control;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateFontIndirectW, DeleteObject, HBRUSH, HFONT, HGDIOBJ};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{PBM_SETPOS, PBM_SETRANGE32, PROGRESS_CLASSW};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

use crate::strings::{self, t};

const TIMER: usize = 1;
const SHOW_AFTER_MS: u128 = 300;

struct State {
    hwnd: HWND,
    bar: HWND,
    detail: HWND,
    /// Elapsed and remaining time.
    times: HWND,
    button: HWND,
    font: HFONT,
    ctl: Arc<Control>,
    started: Instant,
    shown: bool,
    cancelling: bool,
}

thread_local! {
    static CURRENT: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn scale(v: i32, dpi: u32) -> i32 {
    v * dpi as i32 / 96
}

/// The open progress window, for `IsDialogMessage` in the message loop.
pub fn current() -> Option<HWND> {
    CURRENT.with(|c| c.borrow().as_ref().map(|s| s.hwnd))
}

extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                tick();
                LRESULT(0)
            }
            WM_COMMAND if (wp.0 & 0xFFFF) as i32 == IDCANCEL.0 => {
                request_cancel();
                LRESULT(0)
            }
            WM_CLOSE => {
                request_cancel();
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn request_cancel() {
    let Some((detail, button)) = CURRENT.with(|c| {
        let mut c = c.borrow_mut();
        let s = c.as_mut()?;
        s.ctl.cancel();
        s.cancelling = true;
        Some((s.detail, s.button))
    }) else {
        return;
    };
    unsafe {
        let _ = SetWindowTextW(detail, &HSTRING::from(t::PROGRESS_CANCELLING));
        let _ = EnableWindow(button, false);
    }
}

fn tick() {
    let Some((hwnd, bar, detail, times, show, pos, text, time_text)) = CURRENT.with(|c| {
        let mut c = c.borrow_mut();
        let s = c.as_mut()?;
        let elapsed = s.started.elapsed();
        let show = !s.shown && elapsed.as_millis() >= SHOW_AFTER_MS;
        s.shown |= show;
        let p = s.ctl.progress();
        // Bytes when sizes are known, otherwise entry count.
        let fraction = if p.bytes_total > 0 {
            p.bytes_done as f64 / p.bytes_total as f64
        } else if p.entries_total > 0 {
            p.entries_done as f64 / p.entries_total as f64
        } else {
            0.0
        };
        let fraction = fraction.clamp(0.0, 1.0);
        let pos = (fraction * 1000.0) as u64;
        // Average speed so far; too early or too little done says nothing.
        let secs = elapsed.as_secs_f64();
        let remaining = (secs >= 1.0 && fraction >= 0.01).then(|| (secs * (1.0 - fraction) / fraction).ceil() as u64);
        let time_text = strings::progress_times(elapsed.as_secs(), remaining);
        let rate = p.bytes_done as f64 / elapsed.as_secs_f64().max(0.001);
        let text = if s.cancelling {
            t::PROGRESS_CANCELLING.to_string()
        } else if p.entries_total == 0 {
            t::PROGRESS_PREPARING.to_string()
        } else {
            strings::progress_detail(p.entries_done, p.entries_total, p.bytes_done, p.bytes_total, rate)
        };
        Some((s.hwnd, s.bar, s.detail, s.times, show, pos, text, time_text))
    }) else {
        return;
    };
    unsafe {
        SendMessageW(bar, PBM_SETPOS, Some(WPARAM(pos as usize)), None);
        let _ = SetWindowTextW(detail, &HSTRING::from(text));
        let _ = SetWindowTextW(times, &HSTRING::from(time_text));
        if show {
            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        }
    }
}

/// Shows the window now (e.g. before asking a question on top of it) and
/// returns it.
pub fn show_now() -> Option<HWND> {
    let hwnd = CURRENT.with(|c| {
        let mut c = c.borrow_mut();
        let s = c.as_mut()?;
        s.shown = true;
        Some(s.hwnd)
    })?;
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
    }
    Some(hwnd)
}

/// Creates the (initially hidden) progress window. With no owner it is a
/// normal top-level window with a taskbar button (context-menu modes).
pub fn open(owner: Option<HWND>, title: &str, heading: &str, ctl: Arc<Control>) {
    close();
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("KibotosProgress");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: crate::app_icon(false),
            hIconSm: crate::app_icon(true),
            hbrBackground: HBRUSH((windows::Win32::Graphics::Gdi::COLOR_BTNFACE.0 + 1) as _),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let dpi = match owner {
            Some(o) => GetDpiForWindow(o).max(96),
            None => GetDpiForSystem().max(96),
        };
        let s = |v| scale(v, dpi);
        let (w, h) = (s(480), s(198));
        let mut r = RECT::default();
        let have_owner = owner.is_some_and(|o| GetWindowRect(o, &mut r).is_ok());
        if !have_owner {
            r = RECT { left: 0, top: 0, right: GetSystemMetrics(SM_CXSCREEN), bottom: GetSystemMetrics(SM_CYSCREEN) };
        }
        let (x, y) = (r.left + (r.right - r.left - w) / 2, r.top + (r.bottom - r.top - h) / 2);
        let ex = if owner.is_some() { WS_EX_DLGMODALFRAME } else { WS_EX_DLGMODALFRAME | WS_EX_APPWINDOW };
        let Ok(hwnd) = CreateWindowExW(
            ex | WS_EX_CONTROLPARENT,
            class,
            &HSTRING::from(title),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            x,
            y,
            w,
            h,
            owner,
            None,
            Some(hinst.into()),
            None,
        ) else {
            return;
        };

        let mut ncm = NONCLIENTMETRICSW { cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
        let _ = SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS.0, ncm.cbSize, Some(&mut ncm as *mut _ as *mut _), 0, dpi);
        let font = CreateFontIndirectW(&ncm.lfMessageFont);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let cw = rc.right;
        let mk = |class: PCWSTR, text: &str, style: u32, x, y, w, h, id: i32| {
            let c = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                &HSTRING::from(text),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
                x,
                y,
                w,
                h,
                Some(hwnd),
                Some(HMENU(id as _)),
                Some(hinst.into()),
                None,
            )
            .unwrap_or_default();
            SendMessageW(c, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
            c
        };
        const SS_ENDELLIPSIS: u32 = 0x4000;
        const SS_PATHELLIPSIS: u32 = 0x8000;
        const PBS_SMOOTH: u32 = 0x01;
        mk(w!("STATIC"), heading, SS_PATHELLIPSIS, s(14), s(12), cw - s(28), s(20), 0);
        let bar = mk(PROGRESS_CLASSW, "", PBS_SMOOTH, s(14), s(40), cw - s(28), s(18), 0);
        let detail = mk(w!("STATIC"), t::PROGRESS_PREPARING, SS_ENDELLIPSIS, s(14), s(68), cw - s(28), s(20), 0);
        let times = mk(w!("STATIC"), "", SS_ENDELLIPSIS, s(14), s(90), cw - s(28), s(20), 0);
        let bw = s(100);
        let button = mk(w!("BUTTON"), t::CANCEL, WS_TABSTOP.0, cw - bw - s(14), s(122), bw, s(28), IDCANCEL.0);
        SendMessageW(bar, PBM_SETRANGE32, Some(WPARAM(0)), Some(LPARAM(1000)));
        SetTimer(Some(hwnd), TIMER, 100, None);

        CURRENT.with(|c| {
            *c.borrow_mut() = Some(State {
                hwnd,
                bar,
                detail,
                times,
                button,
                font,
                ctl,
                started: Instant::now(),
                shown: false,
                cancelling: false,
            })
        });
    }
}

/// Closes the progress window, if any.
pub fn close() {
    if let Some(s) = CURRENT.with(|c| c.borrow_mut().take()) {
        unsafe {
            let _ = KillTimer(Some(s.hwnd), TIMER);
            let _ = DestroyWindow(s.hwnd);
            let _ = DeleteObject(HGDIOBJ(s.font.0));
        }
    }
}
