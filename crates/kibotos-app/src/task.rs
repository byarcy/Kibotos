//! Runs a long operation (compress, update an archive, hash) on a worker
//! thread while this thread keeps its windows painted, with the progress
//! window. The caller waits for the result, like a modal dialog.

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use kibotos_core::{Control, Error};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, GetFocus, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::progress;

/// Runs `work` on a worker thread. The progress window appears after
/// 300 ms; `owner` is disabled meanwhile. Returns the result and how long
/// the work took. If the app is asked to quit meanwhile, the work is
/// cancelled and the quit is passed on afterwards.
pub fn run<T, F>(owner: Option<HWND>, title: &str, heading: &str, work: F) -> (kibotos_core::Result<T>, Duration)
where
    T: Send + 'static,
    F: FnOnce(&Control) -> kibotos_core::Result<T> + Send + 'static,
{
    let ctl = Arc::new(Control::new());
    progress::open(owner, title, heading, ctl.clone());
    let (tx, rx) = mpsc::channel();
    let ctl2 = ctl.clone();
    let tid = unsafe { GetCurrentThreadId() };
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let r = work(&ctl2);
        let _ = tx.send((r, t0.elapsed()));
        unsafe {
            let _ = PostThreadMessageW(tid, WM_NULL, WPARAM(0), LPARAM(0));
        }
    });
    // A disabled window loses the keyboard focus, and enabling it again
    // does not give it back: remember where it was.
    let focus = unsafe { GetFocus() };
    if let Some(o) = owner {
        unsafe {
            let _ = EnableWindow(o, false);
        }
    }
    // A thread timer wakes the loop even if the wake-up message is lost
    // (thread messages vanish while a modal loop runs).
    let timer = unsafe { SetTimer(None, 0, 100, None) };
    let mut quit = None;
    let result = loop {
        if let Ok(r) = rx.try_recv() {
            break r;
        }
        let mut msg = MSG::default();
        unsafe {
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                // The app is closing: stop and wait for the worker.
                quit = Some(msg.wParam.0 as i32);
                ctl.cancel();
                break rx.recv().unwrap_or((Err(Error::Cancelled), Duration::ZERO));
            }
            if let Some(p) = progress::current() {
                if IsDialogMessageW(p, &msg).as_bool() {
                    continue;
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    };
    unsafe {
        let _ = KillTimer(None, timer);
        if let Some(o) = owner {
            let _ = EnableWindow(o, true);
        }
    }
    progress::close();
    if !focus.is_invalid() && unsafe { IsWindow(Some(focus)) }.as_bool() {
        unsafe {
            let _ = SetFocus(Some(focus));
        }
    }
    if let Some(code) = quit {
        unsafe { PostQuitMessage(code) };
    }
    result
}
