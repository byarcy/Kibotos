//! Creating archives: default names, the "Comprimir" dialog, and running
//! the job with the progress window. Used by the disk view and by the
//! Explorer menu (`--compress-*`).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use kibotos_core::{Codec, Control, CreateFormat, CreateOptions, CreateReport, Error, Level};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::Shell::{FileSaveDialog, IFileSaveDialog, IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

use crate::dialogs::{self, centered, message_font, scale};
use crate::strings::{self, t};
use crate::{integrate, progress};

/// Formats offered, in menu order.
pub const FORMATS: [(CreateFormat, &str); 7] = [
    (CreateFormat::Zip, "ZIP"),
    (CreateFormat::SevenZip, "7z"),
    (CreateFormat::Tar(Some(Codec::Gzip)), "tar.gz"),
    (CreateFormat::Tar(Some(Codec::Xz)), "tar.xz"),
    (CreateFormat::Tar(Some(Codec::Zstd)), "tar.zst"),
    (CreateFormat::Tar(Some(Codec::Bzip2)), "tar.bz2"),
    (CreateFormat::Tar(None), "tar"),
];

const LEVELS: [(Level, &str); 4] =
    [(Level::Store, t::LEVEL_STORE), (Level::Fast, t::LEVEL_FAST), (Level::Normal, t::LEVEL_NORMAL), (Level::Maximum, t::LEVEL_MAX)];

/// Extensions recognised when the user edits the name (longest first).
const KNOWN_EXTS: [&str; 10] = [".tar.gz", ".tar.xz", ".tar.zst", ".tar.bz2", ".tgz", ".txz", ".tzst", ".zip", ".7z", ".tar"];

/// What to create and how.
#[derive(Clone, Debug)]
pub struct Request {
    pub output: PathBuf,
    pub options: CreateOptions,
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The name an archive of `inputs` gets: the folder's or file's own name
/// ("informe.docx" -> "informe"), or the parent folder's for several items.
pub fn base_name(inputs: &[PathBuf]) -> String {
    let name = match inputs {
        [one] if one.is_dir() => file_name(one),
        [one] => one.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        [first, ..] => first.parent().map(file_name).unwrap_or_default(),
        [] => String::new(),
    };
    let name = name.trim_end_matches(['.', ' ']).to_owned();
    if name.is_empty() { t::REG_TYPE_NAME.to_owned() } else { name }
}

/// `path`, or "name (2).ext", "name (3).ext"... if it exists.
pub fn unique(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let name = file_name(&path);
    let lower = name.to_lowercase();
    let ext = KNOWN_EXTS.iter().find(|e| lower.ends_with(*e)).map_or_else(
        || path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default(),
        |e| name[name.len() - e.len()..].to_owned(),
    );
    let stem = &name[..name.len() - ext.len()];
    (2..)
        .map(|n| path.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap_or(path)
}

/// Where a quick "Comprimir en ZIP/7z" writes: next to the first input.
pub fn default_output(inputs: &[PathBuf], format: CreateFormat) -> PathBuf {
    let parent = inputs.first().and_then(|p| p.parent()).map(Path::to_path_buf).unwrap_or_default();
    unique(parent.join(format!("{}.{}", base_name(inputs), format.extension())))
}

/// Replaces a known archive extension of `name` with the one of `format`.
fn with_format(name: &str, format: CreateFormat) -> String {
    let lower = name.to_lowercase();
    let stem = KNOWN_EXTS.iter().find(|e| lower.ends_with(*e)).map_or(name, |e| &name[..name.len() - e.len()]);
    format!("{stem}.{}", format.extension())
}

/// The format a name asks for, if its extension is one Kibotos writes.
fn format_of(name: &str) -> Option<CreateFormat> {
    let lower = name.to_lowercase();
    let map = [
        (".tar.gz", CreateFormat::Tar(Some(Codec::Gzip))),
        (".tgz", CreateFormat::Tar(Some(Codec::Gzip))),
        (".tar.xz", CreateFormat::Tar(Some(Codec::Xz))),
        (".txz", CreateFormat::Tar(Some(Codec::Xz))),
        (".tar.zst", CreateFormat::Tar(Some(Codec::Zstd))),
        (".tzst", CreateFormat::Tar(Some(Codec::Zstd))),
        (".tar.bz2", CreateFormat::Tar(Some(Codec::Bzip2))),
        (".zip", CreateFormat::Zip),
        (".7z", CreateFormat::SevenZip),
        (".tar", CreateFormat::Tar(None)),
    ];
    map.iter().find(|(e, _)| lower.ends_with(e)).map(|(_, f)| *f)
}

// ---------------------------------------------------------- settings

/// The last format and level used in the dialog (ZIP, Normal at first).
pub fn remembered() -> (CreateFormat, Level) {
    let format = integrate::read_setting("CompressFormat")
        .and_then(|n| FORMATS.iter().find(|(_, label)| *label == n).map(|(f, _)| *f))
        .unwrap_or(CreateFormat::Zip);
    let level = integrate::read_setting("CompressLevel")
        .and_then(|n| LEVELS.iter().find(|(l, _)| format!("{l:?}") == n).map(|(l, _)| *l))
        .unwrap_or_default();
    (format, level)
}

fn remember(format: CreateFormat, level: Level) {
    if let Some((_, label)) = FORMATS.iter().find(|(f, _)| *f == format) {
        integrate::write_setting("CompressFormat", label);
    }
    integrate::write_setting("CompressLevel", &format!("{level:?}"));
}

// ------------------------------------------------------------ dialog

const ID_NAME: i32 = 200;
const ID_BROWSE: i32 = 201;
const ID_FORMAT: i32 = 202;
const ID_LEVEL: i32 = 203;
const ID_PW: i32 = 204;
const ID_PW2: i32 = 205;
const ID_NAMES: i32 = 206;
const ID_NOTE: i32 = 207;
const IDOK_: i32 = IDOK.0;
const IDCANCEL_: i32 = IDCANCEL.0;
const CBN_SELCHANGE: u32 = 1;
const EN_CHANGE: u32 = 0x0300;
const CB_ADDSTRING: u32 = 0x0143;
const CB_GETCURSEL: u32 = 0x0147;
const CB_SETCURSEL: u32 = 0x014E;
const BM_GETCHECK: u32 = 0x00F0;
const BM_SETCHECK: u32 = 0x00F1;

#[derive(Default)]
struct Dialog {
    folder: PathBuf,
    result: Option<Request>,
    done: bool,
}

thread_local! {
    static DIALOG: RefCell<Dialog> = RefCell::new(Dialog::default());
}

fn item(dlg: HWND, id: i32) -> HWND {
    unsafe { GetDlgItem(Some(dlg), id).unwrap_or_default() }
}

fn text_of(hwnd: HWND) -> String {
    unsafe {
        let n = GetWindowTextLengthW(hwnd).max(0) as usize;
        let mut buf = vec![0u16; n + 1];
        let got = GetWindowTextW(hwnd, &mut buf) as usize;
        String::from_utf16_lossy(&buf[..got])
    }
}

fn set_text(hwnd: HWND, s: &str) {
    unsafe {
        let _ = SetWindowTextW(hwnd, &HSTRING::from(s));
    }
}

fn selection(combo: HWND) -> usize {
    unsafe { SendMessageW(combo, CB_GETCURSEL, None, None).0.max(0) as usize }
}

fn chosen_format(dlg: HWND) -> CreateFormat {
    FORMATS[selection(item(dlg, ID_FORMAT)).min(FORMATS.len() - 1)].0
}

/// Enables the password fields for formats that support them, and shows
/// the note that applies (no password in tar, accents in ZIP passwords).
fn refresh(dlg: HWND) {
    let format = chosen_format(dlg);
    let pw = text_of(item(dlg, ID_PW));
    unsafe {
        let _ = EnableWindow(item(dlg, ID_PW), format.supports_password());
        let _ = EnableWindow(item(dlg, ID_PW2), format.supports_password());
        let _ = EnableWindow(item(dlg, ID_NAMES), format == CreateFormat::SevenZip && !pw.is_empty());
    }
    let note = if !format.supports_password() {
        t::COMPRESS_NO_PASSWORD
    } else if format == CreateFormat::Zip && !pw.is_ascii() {
        t::COMPRESS_ZIP_ACCENTS
    } else {
        ""
    };
    set_text(item(dlg, ID_NOTE), note);
}

/// The output path typed in the dialog (relative names go to the folder
/// of the inputs).
fn output_path(dlg: HWND) -> PathBuf {
    let text = text_of(item(dlg, ID_NAME)).trim().to_owned();
    let p = PathBuf::from(&text);
    if p.is_absolute() { p } else { DIALOG.with(|d| d.borrow().folder.join(p)) }
}

fn browse(dlg: HWND) {
    let current = output_path(dlg);
    unsafe {
        let Ok(fd) = CoCreateInstance::<_, IFileSaveDialog>(&FileSaveDialog, None, CLSCTX_INPROC_SERVER) else { return };
        let _ = fd.SetTitle(&HSTRING::from(t::SAVE_ARCHIVE_TITLE));
        let _ = fd.SetFileName(&HSTRING::from(file_name(&current)));
        if let Some(dir) = current.parent() {
            if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(dir.as_os_str()), None) {
                let _ = fd.SetFolder(&item);
            }
        }
        if fd.Show(Some(dlg)).is_err() {
            return;
        }
        let Ok(item) = fd.GetResult() else { return };
        let Ok(name) = item.GetDisplayName(SIGDN_FILESYSPATH) else { return };
        let path = name.to_string().unwrap_or_default();
        CoTaskMemFree(Some(name.0 as _));
        if let Some(f) = format_of(&path) {
            if let Some(i) = FORMATS.iter().position(|(x, _)| *x == f) {
                SendMessageW(self::item(dlg, ID_FORMAT), CB_SETCURSEL, Some(WPARAM(i)), None);
            }
            set_text(self::item(dlg, ID_NAME), &path);
        } else {
            set_text(self::item(dlg, ID_NAME), &with_format(&path, chosen_format(dlg)));
        }
        refresh(dlg);
    }
}

fn accept(dlg: HWND) {
    let output = output_path(dlg);
    if file_name(&output).is_empty() {
        set_text(item(dlg, ID_NOTE), t::COMPRESS_NO_NAME);
        unsafe {
            let _ = SetFocus(Some(item(dlg, ID_NAME)));
        }
        return;
    }
    let format = chosen_format(dlg);
    let level = LEVELS[selection(item(dlg, ID_LEVEL)).min(LEVELS.len() - 1)].0;
    let (pw, pw2) = (text_of(item(dlg, ID_PW)), text_of(item(dlg, ID_PW2)));
    let password = if format.supports_password() && !pw.is_empty() {
        if pw != pw2 {
            set_text(item(dlg, ID_NOTE), t::COMPRESS_MISMATCH);
            unsafe {
                let _ = SetFocus(Some(item(dlg, ID_PW2)));
            }
            return;
        }
        Some(pw)
    } else {
        None
    };
    let encrypt_names = unsafe { SendMessageW(item(dlg, ID_NAMES), BM_GETCHECK, None, None).0 == 1 };
    if output.exists() {
        let text = strings::compress_exists(&file_name(&output));
        let answer = dialogs::choose(dlg, t::COMPRESS_TITLE, &text, "", &[(IDOK_, t::REPLACE), (IDCANCEL_, t::CANCEL)]);
        if answer != IDOK_ {
            return;
        }
    }
    remember(format, level);
    let options = CreateOptions { format, level, password, encrypt_names, threads: 0 };
    DIALOG.with(|d| {
        let mut d = d.borrow_mut();
        d.result = Some(Request { output, options });
        d.done = true;
    });
}

extern "system" fn dialog_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                let (id, code) = ((wp.0 & 0xFFFF) as i32, ((wp.0 >> 16) & 0xFFFF) as u32);
                match (id, code) {
                    (IDOK_, _) => accept(hwnd),
                    (IDCANCEL_, _) => DIALOG.with(|d| d.borrow_mut().done = true),
                    (ID_BROWSE, _) => browse(hwnd),
                    (ID_FORMAT, CBN_SELCHANGE) => {
                        let name = item(hwnd, ID_NAME);
                        set_text(name, &with_format(&text_of(name), chosen_format(hwnd)));
                        refresh(hwnd);
                    }
                    (ID_PW, EN_CHANGE) => refresh(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                DIALOG.with(|d| d.borrow_mut().done = true);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// The "Comprimir" dialog. `None` if cancelled.
pub fn ask(owner: HWND, inputs: &[PathBuf]) -> Option<Request> {
    let (format, level) = remembered();
    let folder = inputs.first().and_then(|p| p.parent()).map(Path::to_path_buf).unwrap_or_default();
    let initial = default_output(inputs, format);
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("KibotosCompress");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(dialog_proc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: crate::app_icon(false),
            hIconSm: crate::app_icon(true),
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH((windows::Win32::Graphics::Gdi::COLOR_BTNFACE.0 + 1) as _),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let dpi = if owner.is_invalid() { windows::Win32::UI::HiDpi::GetDpiForSystem().max(96) } else { GetDpiForWindow(owner).max(96) };
        let s = |v| scale(v, dpi);
        let (w, h) = (s(540), s(330));
        let (x, y) = centered(owner, w, h);
        let ex = if owner.is_invalid() { WS_EX_DLGMODALFRAME | WS_EX_APPWINDOW } else { WS_EX_DLGMODALFRAME };
        let title = format!("{} — {}", t::COMPRESS_TITLE, base_name(inputs));
        let dlg = CreateWindowExW(
            ex | WS_EX_CONTROLPARENT,
            class,
            &HSTRING::from(title),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            x,
            y,
            w,
            h,
            if owner.is_invalid() { None } else { Some(owner) },
            None,
            Some(hinst.into()),
            None,
        )
        .ok()?;
        DIALOG.with(|d| *d.borrow_mut() = Dialog { folder: folder.clone(), result: None, done: false });
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
        const ES_AUTOHSCROLL: u32 = 0x80;
        const ES_PASSWORD: u32 = 0x20;
        const CBS_DROPDOWNLIST: u32 = 3;
        const BS_AUTOCHECKBOX: u32 = 3;
        const BS_DEFPUSHBUTTON: u32 = 1;
        let tab = WS_TABSTOP.0;
        let none = WINDOW_EX_STYLE(0);
        let (m, lw) = (s(12), s(110));
        let fw = cw - m - lw - m;

        mk(w!("STATIC"), t::COMPRESS_FILE, 0, none, m, s(16), lw, s(20), 0);
        let shown = if initial.parent() == Some(folder.as_path()) { file_name(&initial) } else { initial.display().to_string() };
        let name = mk(w!("EDIT"), &shown, ES_AUTOHSCROLL | tab, WS_EX_CLIENTEDGE, m + lw, s(12), fw - s(104), s(24), ID_NAME);
        mk(w!("BUTTON"), t::COMPRESS_BROWSE, tab, none, cw - m - s(96), s(11), s(96), s(26), ID_BROWSE);

        mk(w!("STATIC"), t::COMPRESS_FORMAT, 0, none, m, s(56), lw, s(20), 0);
        let fcombo = mk(w!("COMBOBOX"), "", CBS_DROPDOWNLIST | WS_VSCROLL.0 | tab, none, m + lw, s(52), s(140), s(200), ID_FORMAT);
        mk(w!("STATIC"), t::COMPRESS_LEVEL, 0, none, m + lw + s(160), s(56), s(60), s(20), 0);
        let lcombo = mk(w!("COMBOBOX"), "", CBS_DROPDOWNLIST | WS_VSCROLL.0 | tab, none, m + lw + s(220), s(52), fw - s(220), s(200), ID_LEVEL);
        for (_, label) in FORMATS {
            let wl = HSTRING::from(label);
            SendMessageW(fcombo, CB_ADDSTRING, None, Some(LPARAM(wl.as_ptr() as isize)));
        }
        for (_, label) in LEVELS {
            let wl = HSTRING::from(label);
            SendMessageW(lcombo, CB_ADDSTRING, None, Some(LPARAM(wl.as_ptr() as isize)));
        }
        let fi = FORMATS.iter().position(|(f, _)| *f == format).unwrap_or(0);
        let li = LEVELS.iter().position(|(l, _)| *l == level).unwrap_or(2);
        SendMessageW(fcombo, CB_SETCURSEL, Some(WPARAM(fi)), None);
        SendMessageW(lcombo, CB_SETCURSEL, Some(WPARAM(li)), None);

        mk(w!("STATIC"), t::COMPRESS_PASSWORD, 0, none, m, s(100), lw, s(20), 0);
        mk(w!("EDIT"), "", ES_PASSWORD | ES_AUTOHSCROLL | tab, WS_EX_CLIENTEDGE, m + lw, s(96), fw, s(24), ID_PW);
        mk(w!("STATIC"), t::COMPRESS_PASSWORD2, 0, none, m, s(134), lw, s(20), 0);
        mk(w!("EDIT"), "", ES_PASSWORD | ES_AUTOHSCROLL | tab, WS_EX_CLIENTEDGE, m + lw, s(130), fw, s(24), ID_PW2);
        let names = mk(w!("BUTTON"), t::COMPRESS_ENCRYPT_NAMES, BS_AUTOCHECKBOX | tab, none, m + lw, s(162), fw, s(22), ID_NAMES);
        SendMessageW(names, BM_SETCHECK, Some(WPARAM(1)), None);
        mk(w!("STATIC"), "", 0, none, m, s(194), cw - 2 * m, s(44), ID_NOTE);

        let bw = s(100);
        let by = h - s(84);
        mk(w!("BUTTON"), t::COMPRESS_OK, BS_DEFPUSHBUTTON | tab, none, cw - 2 * bw - s(20), by, bw, s(28), IDOK_);
        mk(w!("BUTTON"), t::CANCEL, tab, none, cw - bw - m, by, bw, s(28), IDCANCEL_);
        refresh(dlg);

        if !owner.is_invalid() {
            let _ = EnableWindow(owner, false);
        }
        let _ = ShowWindow(dlg, SW_SHOW);
        let _ = SetForegroundWindow(dlg);
        let _ = SetFocus(Some(name));
        // Select the name without the extension, ready to be typed over.
        let stem_len = file_name(&initial).len() - format.extension().len() - 1;
        if shown == file_name(&initial) {
            const EM_SETSEL: u32 = 0x00B1;
            SendMessageW(name, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(shown[..stem_len].encode_utf16().count() as isize)));
        }

        let mut msg = MSG::default();
        while !DIALOG.with(|d| d.borrow().done) {
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                PostQuitMessage(msg.wParam.0 as i32);
                break;
            }
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
        DIALOG.with(|d| d.borrow_mut().result.take())
    }
}

// --------------------------------------------------------------- run

/// How a compression ended.
pub enum Outcome {
    Done(CreateReport, Duration),
    Cancelled,
    Failed(Error),
}

/// Creates the archive on a worker thread, showing the progress window
/// (after 300 ms) and keeping this thread's windows responsive.
pub fn run(owner: Option<HWND>, inputs: &[PathBuf], req: &Request) -> Outcome {
    let ctl = Arc::new(Control::new());
    let name = file_name(&req.output);
    let title = format!("{} {name}", t::PROGRESS_COMPRESSING);
    let heading = format!("{}  →  {name}", base_name(inputs));
    progress::open(owner, &title, &heading, ctl.clone());
    let (tx, rx) = mpsc::channel();
    let (inputs, req2, ctl2) = (inputs.to_vec(), req.clone(), ctl.clone());
    let tid = unsafe { GetCurrentThreadId() };
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let r = kibotos_core::create(&req2.output, &inputs, &req2.options, &ctl2);
        let _ = tx.send((r, t0.elapsed()));
        unsafe {
            let _ = PostThreadMessageW(tid, WM_NULL, WPARAM(0), LPARAM(0));
        }
    });
    if let Some(o) = owner {
        unsafe {
            let _ = EnableWindow(o, false);
        }
    }
    // A thread timer wakes the loop even if the wake-up message is lost
    // (thread messages vanish while a modal loop runs).
    let timer = unsafe { SetTimer(None, 0, 100, None) };
    let mut quit = None;
    let (result, took) = loop {
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
    if let Some(code) = quit {
        unsafe { PostQuitMessage(code) };
    }
    match result {
        Ok(report) => Outcome::Done(report, took),
        Err(Error::Cancelled) => Outcome::Cancelled,
        Err(e) => Outcome::Failed(e),
    }
}

/// Tells the user how it went: the archive made (and any items left
/// out), or the error.
/// Returns the status line for a successful run.
pub fn report(owner: HWND, req: &Request, outcome: &Outcome) -> Option<String> {
    match outcome {
        Outcome::Done(r, took) => {
            let done = strings::compress_done(&file_name(&req.output), r.files, r.bytes_in, r.bytes_out, took.as_secs_f64());
            if r.skipped.is_empty() {
                dialogs::message(owner, t::COMPRESS_DONE_TITLE, &done, MB_ICONINFORMATION | MB_SETFOREGROUND);
            } else {
                let text = format!("{done}

{}
{}", t::COMPRESS_SKIPPED_NOTE, strings::skipped_list(&r.skipped));
                dialogs::message(owner, t::COMPRESS_SKIPPED_TITLE, &text, MB_ICONWARNING | MB_SETFOREGROUND);
            }
            Some(strings::compressed(&file_name(&req.output), r.files, r.bytes_in, r.bytes_out, took.as_secs_f64()))
        }
        Outcome::Cancelled => None,
        Outcome::Failed(e) => {
            let text = format!("{}\n\n{}", file_name(&req.output), strings::error_message(e));
            dialogs::message(owner, t::COMPRESS_ERROR_TITLE, &text, MB_ICONERROR);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let dir = std::env::temp_dir().join(format!("kibotos-names-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("fotos")).unwrap();
        std::fs::write(dir.join("informe.docx"), b"x").unwrap();
        assert_eq!(base_name(&[dir.join("fotos")]), "fotos");
        assert_eq!(base_name(&[dir.join("informe.docx")]), "informe");
        assert_eq!(base_name(&[dir.join("fotos"), dir.join("informe.docx")]), file_name(&dir));
        let out = default_output(&[dir.join("fotos")], CreateFormat::SevenZip);
        assert_eq!(out, dir.join("fotos.7z"));
        std::fs::write(&out, b"").unwrap();
        assert_eq!(default_output(&[dir.join("fotos")], CreateFormat::SevenZip), dir.join("fotos (2).7z"));
        std::fs::write(dir.join("a.tar.gz"), b"").unwrap();
        assert_eq!(unique(dir.join("a.tar.gz")), dir.join("a (2).tar.gz"));
        assert_eq!(with_format("copia.tar.gz", CreateFormat::Zip), "copia.zip");
        assert_eq!(with_format("copia", CreateFormat::Tar(Some(Codec::Xz))), "copia.tar.xz");
        assert_eq!(format_of("x.TGZ"), Some(CreateFormat::Tar(Some(Codec::Gzip))));
        let _ = std::fs::remove_dir_all(dir);
    }
}
