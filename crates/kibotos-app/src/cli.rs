//! Context-menu modes: extract without opening the main window.
//!
//!   kibotos.exe --extract-here      <archive>...
//!   kibotos.exe --extract-to-folder <archive>...
//!   kibotos.exe --extract-dialog    <archive>...
//!
//!   kibotos.exe --compress-zip      <file or folder>...
//!   kibotos.exe --compress-7z       <file or folder>...
//!   kibotos.exe --compress-dialog   <file or folder>...
//!
//! Only the small progress window appears (after 300 ms), plus any question
//! the operation needs: password, overwrite, blocked entries, errors.
//! The process exit code is 0 on success, 1 on error, 2 if cancelled.

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kibotos_core::{Control, Error};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    ICC_PROGRESS_CLASS, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

use crate::job::{self, Dest, ExtractDone, ExtractJob, WM_APP_CONFLICT, WM_APP_EXTRACTED};
use crate::strings::{self, t};
use crate::{collect, compress, dialogs, progress};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Here,
    ToFolder,
    Dialog,
}

impl Mode {
    pub fn from_flag(flag: &str) -> Option<Mode> {
        match flag {
            "--extract-here" => Some(Mode::Here),
            "--extract-to-folder" => Some(Mode::ToFolder),
            "--extract-dialog" => Some(Mode::Dialog),
            _ => None,
        }
    }
}

const OK: i32 = 0;
const FAILED: i32 = 1;
const CANCELLED: i32 = 2;

thread_local! {
    static DONE: RefCell<Option<ExtractDone>> = const { RefCell::new(None) };
}

extern "system" fn sink_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_APP_EXTRACTED => {
                let done = job::take_done(lp);
                DONE.with(|d| *d.borrow_mut() = Some(done));
                LRESULT(0)
            }
            WM_APP_CONFLICT => LRESULT(job::answer_conflict(HWND::default(), lp)),
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// A hidden message-only window that receives the worker's messages.
fn message_window() -> HWND {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(sink_proc),
            hInstance: hinst.into(),
            lpszClassName: w!("KibotosCli"),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("KibotosCli"),
            w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinst.into()),
            None,
        )
        .unwrap_or_default()
    }
}

/// Pumps messages until the worker reports back.
fn wait_for_done() -> Option<ExtractDone> {
    let mut msg = MSG::default();
    loop {
        if let Some(done) = DONE.with(|d| d.borrow_mut().take()) {
            return Some(done);
        }
        unsafe {
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                return None;
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

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn usage() -> i32 {
    dialogs::message(HWND::default(), t::APP_NAME, t::CLI_USAGE, MB_ICONINFORMATION);
    FAILED
}

/// "Integrar con Windows" / "Quitar integración", from the menu or from
/// `--integrate` / `--unintegrate`. Shows the result and returns the exit
/// code.
pub fn integration(owner: HWND, install: bool) -> i32 {
    let result = if install {
        crate::integrate::install().map(|exe| strings::integrated(&exe.display().to_string()))
    } else {
        crate::integrate::remove().map(|()| strings::unintegrated())
    };
    match result {
        Ok(text) => {
            dialogs::message(owner, t::INTEGRATE_TITLE, &text, MB_ICONINFORMATION);
            OK
        }
        Err(e) => {
            dialogs::message(owner, t::INTEGRATE_TITLE, &format!("{} {e}", t::INTEGRATE_FAILED), MB_ICONERROR);
            FAILED
        }
    }
}

/// `--install`: installs this .exe for the current user.
fn install_cmd() -> i32 {
    init_ui();
    match crate::install::install() {
        Ok(exe) => {
            let dir = exe.parent().map(|d| d.display().to_string()).unwrap_or_default();
            dialogs::message(HWND::default(), t::INSTALL_TITLE, &strings::installed_done(&dir), MB_ICONINFORMATION);
            OK
        }
        Err(e) => {
            dialogs::message(HWND::default(), t::INSTALL_TITLE, &format!("{} {e}", t::INSTALL_FAILED), MB_ICONERROR);
            FAILED
        }
    }
}

/// `--uninstall` (Configuración › Aplicaciones › Desinstalar). `--quiet`
/// asks nothing and shows nothing.
fn uninstall_cmd(quiet: bool) -> i32 {
    init_ui();
    if !quiet {
        const GO: i32 = 100;
        let choice = dialogs::choose(
            HWND::default(),
            t::UNINSTALL_TITLE,
            t::UNINSTALL_MAIN,
            t::UNINSTALL_TEXT,
            &[(GO, t::UNINSTALL_BUTTON), (IDCANCEL.0, t::CANCEL)],
        );
        if choice != GO {
            return CANCELLED;
        }
    }
    match crate::install::uninstall() {
        Ok(()) => {
            if !quiet {
                dialogs::message(HWND::default(), t::UNINSTALL_TITLE, t::UNINSTALLED, MB_ICONINFORMATION);
            }
            OK
        }
        Err(e) => {
            if !quiet {
                dialogs::message(HWND::default(), t::UNINSTALL_TITLE, &format!("{} {e}", t::UNINSTALL_FAILED), MB_ICONERROR);
            }
            FAILED
        }
    }
}

fn init_ui() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_PROGRESS_CLASS | ICC_STANDARD_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);
    }
}

/// `--compress-zip`, `--compress-7z`, `--compress-dialog`: one archive of
/// everything selected in Explorer (see `collect`).
fn compress_cmd(kind: &str, rest: &[OsString]) -> i32 {
    use kibotos_core::{CreateFormat, CreateOptions};
    let mine: Vec<PathBuf> = rest.iter().map(PathBuf::from).collect();
    if mine.is_empty() || !matches!(kind, "zip" | "7z" | "dialog") {
        return usage();
    }
    let Some(inputs) = collect::gather(kind, mine) else { return OK };
    init_ui();
    let req = if kind == "dialog" {
        match compress::ask(HWND::default(), &inputs) {
            Some(r) => r,
            None => return CANCELLED,
        }
    } else {
        let format = if kind == "7z" { CreateFormat::SevenZip } else { CreateFormat::Zip };
        let (_, level) = compress::remembered();
        let output = compress::default_output(&inputs, format);
        compress::Request { output, options: CreateOptions { format, level, ..Default::default() } }
    };
    let outcome = compress::run(None, &inputs, &req);
    compress::report(HWND::default(), &req, &outcome);
    match outcome {
        compress::Outcome::Done(..) => {
            if let Some(dir) = req.output.parent() {
                crate::drag::shell_refresh(dir);
            }
            OK
        }
        compress::Outcome::Cancelled => CANCELLED,
        compress::Outcome::Failed(_) => FAILED,
    }
}

/// Entry point for `kibotos.exe --<mode> <archive>...`.
pub fn run(args: &[OsString]) -> i32 {
    match args.first().and_then(|a| a.to_str()) {
        Some("--integrate") => return integration(HWND::default(), true),
        Some("--unintegrate") => return integration(HWND::default(), false),
        Some("--install") => return install_cmd(),
        Some("--uninstall") => return uninstall_cmd(args.iter().any(|a| a == "--quiet")),
        Some(flag) if flag.starts_with("--compress-") => return compress_cmd(&flag["--compress-".len()..], &args[1..]),
        _ => {}
    }
    let Some(mode) = args.first().and_then(|a| a.to_str()).and_then(Mode::from_flag) else {
        return usage();
    };
    let archives: Vec<PathBuf> = args[1..].iter().map(PathBuf::from).collect();
    if archives.is_empty() {
        return usage();
    }
    init_ui();
    let target = message_window();
    let mut code = OK;
    for archive in archives {
        let c = extract_one(target, mode, &archive);
        code = code.max(c);
        if c == CANCELLED {
            break;
        }
    }
    code
}

fn extract_one(target: HWND, mode: Mode, archive: &Path) -> i32 {
    let name = file_name(archive);
    let parent = archive.parent().map(Path::to_path_buf).unwrap_or_default();
    let dest = match mode {
        Mode::Here => Dest::Smart { parent, folder_name: job::folder_name(archive) },
        Mode::ToFolder => Dest::Folder(parent.join(job::folder_name(archive))),
        Mode::Dialog => match dialogs::pick_folder(HWND::default(), &parent) {
            Some(folder) => Dest::Folder(folder),
            None => return CANCELLED,
        },
    };

    let mut password: Option<String> = None;
    loop {
        let job = ExtractJob {
            archive: archive.to_path_buf(),
            password: password.clone(),
            ids: None,
            bases: Vec::new(),
            dest: dest.clone(),
            verify: true,
            open_after: None,
            open_with: false,
        };
        let ctl = Arc::new(Control::new());
        let title = format!("{} {name}", t::PROGRESS_EXTRACTING);
        let heading = format!("{name}  →  {}", dest.shown().display());
        progress::open(None, &title, &heading, ctl.clone());
        job::spawn(target, job, ctl);
        let done = wait_for_done();
        progress::close();
        let Some(done) = done else { return CANCELLED };

        match done.result {
            Ok(report) => {
                dialogs::show_blocked(HWND::default(), &report);
                return OK;
            }
            Err(Error::Cancelled) => return CANCELLED,
            Err(e @ (Error::PasswordRequired | Error::WrongPassword)) => {
                let wrong = matches!(e, Error::WrongPassword) || password.is_some();
                match dialogs::ask_password(HWND::default(), &name, wrong) {
                    Some(pw) => password = Some(pw),
                    None => return CANCELLED,
                }
            }
            Err(e) => {
                let text = format!("{name}\n\n{}", strings::error_message(&e));
                dialogs::message(HWND::default(), t::EXTRACT_ERROR_TITLE, &text, MB_ICONERROR);
                return FAILED;
            }
        }
    }
}
