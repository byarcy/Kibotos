//! Dragging rows out of the list to extract them.
//!
//! Windows does not tell a drag source where its data was dropped, which is
//! why archivers extract to a temporary folder first and let the target copy
//! the files again. Kibotós avoids that double write for the shell (Explorer
//! windows, the desktop, file dialogs):
//!
//! 1. The drag carries one tiny hidden *marker* file instead of the entries.
//! 2. When the shell copies the marker, it announces the new file through
//!    shell change notifications (`SHChangeNotifyRegister`). That reveals
//!    the exact folder, including a subfolder icon or a navigation-pane
//!    item the user dropped on.
//! 3. Kibotós deletes the marker and extracts straight into that folder,
//!    with its own progress, conflict questions and safety checks.
//!
//! Any other drop target (a mail client, an editor, another file manager)
//! gets real files: when the mouse button is released over it, the entries
//! are extracted to the session's temporary folder and handed over, like
//! other archivers do.

use std::cell::{Cell, RefCell};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, HANDLE, HGLOBAL, HWND, LPARAM, POINT, S_OK,
    WPARAM,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, DVASPECT_CONTENT, FORMATETC, IDataObject, STGMEDIUM, STGMEDIUM_0,
    TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CFSTR_PREFERREDDROPEFFECT, CLSID_DragDropHelper, CSIDL_DESKTOP, DROPFILES, IDragSourceHelper, ILFree,
    SHCNE_CREATE, SHCNE_DELETE, SHCNE_RENAMEITEM, SHCNE_UPDATEDIR, SHCNE_UPDATEITEM, SHCNF_PATHW, SHCNRF_NewDelivery,
    SHCNRF_ShellLevel, SHChangeNotification_Lock, SHChangeNotification_Unlock, SHChangeNotify, SHChangeNotifyDeregister,
    SHChangeNotifyEntry, SHChangeNotifyRegister, SHCreateDataObject, SHGetPathFromIDListW, SHGetSpecialFolderLocation,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_PARENT, GA_ROOT, GetAncestor, GetClassNameW, GetCursorPos, WM_APP, WindowFromPoint,
};
use windows::core::{BOOL, HRESULT, implement};

/// Shell change notifications arrive as this message.
pub const WM_APP_SHNOTIFY: u32 = WM_APP + 5;

/// Where the mouse button was released.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    /// Kibotós' own window: nothing to do.
    Own,
    /// A shell folder view, Explorer window or the desktop.
    Shell,
    /// Any other program.
    Other,
}

/// How a drag ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Cancelled, refused, or dropped on Kibotós itself.
    Nothing,
    /// Dropped on the shell: the marker shows up (or already did) through
    /// the [`Watch`].
    Shell,
    /// Dropped on another program, which received extracted copies.
    Handed,
}

/// Window classes that host shell folder views. A drop anywhere in these
/// (file list, navigation pane, address bar) is a copy by the shell.
const SHELL_CLASSES: &[&str] = &["SHELLDLL_DefView", "CabinetWClass", "ExploreWClass", "Progman", "WorkerW"];

fn class_name(h: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(h, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..n])
}

fn target_at_cursor(own: HWND) -> Target {
    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let under = WindowFromPoint(pt);
        if under.is_invalid() {
            return Target::Other;
        }
        if GetAncestor(under, GA_ROOT) == own {
            return Target::Own;
        }
        let mut h = under;
        while !h.is_invalid() {
            if SHELL_CLASSES.contains(&class_name(h).as_str()) {
                return Target::Shell;
            }
            h = GetAncestor(h, GA_PARENT);
        }
        Target::Other
    }
}

// ---------------------------------------------------------------- marker

/// The placeholder file carried by the drag.
pub struct Marker {
    /// Our copy, in the session's temporary folder.
    pub path: PathBuf,
    /// Unique file name, looked for in shell notifications.
    pub name: String,
}

impl Marker {
    fn create() -> io::Result<Marker> {
        let slot = crate::temp::new_slot()?;
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let name = format!("~kibotos-{}-{nanos:x}.tmp", unsafe { GetCurrentProcessId() });
        let path = slot.join(&name);
        std::fs::write(&path, b"Kibotos drag-and-drop marker. It is deleted right after the drop.\r\n")?;
        // A hidden copy does not flash in the target folder.
        unsafe {
            let _ = SetFileAttributesW(&windows::core::HSTRING::from(path.as_os_str()), FILE_ATTRIBUTE_HIDDEN);
        }
        Ok(Marker { path, name })
    }
}

// ------------------------------------------------------- data object

fn wide_path(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().collect()
}

unsafe fn global_with(bytes: &[u8]) -> windows::core::Result<HGLOBAL> {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len())?;
        let p = GlobalLock(h) as *mut u8;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(h);
        Ok(h)
    }
}

unsafe fn set_global(data: &IDataObject, format: u16, bytes: &[u8]) -> windows::core::Result<()> {
    unsafe {
        let fmt = FORMATETC {
            cfFormat: format,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        let medium = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: global_with(bytes)? },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        };
        // fRelease: the data object owns the memory from now on.
        data.SetData(&fmt, &medium, true)
    }
}

/// Puts a file list (CF_HDROP) on the data object, replacing any previous.
unsafe fn set_files(data: &IDataObject, files: &[PathBuf]) -> windows::core::Result<()> {
    let header = DROPFILES { pFiles: std::mem::size_of::<DROPFILES>() as u32, fWide: true.into(), ..Default::default() };
    let mut bytes = unsafe {
        std::slice::from_raw_parts(&header as *const _ as *const u8, std::mem::size_of::<DROPFILES>()).to_vec()
    };
    for f in files {
        for unit in wide_path(f).into_iter().chain([0]) {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&[0, 0]);
    unsafe { set_global(data, CF_HDROP.0, &bytes) }
}

unsafe fn set_preferred_effect(data: &IDataObject, effect: DROPEFFECT) -> windows::core::Result<()> {
    unsafe {
        let cf = RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT) as u16;
        set_global(data, cf, &effect.0.to_le_bytes())
    }
}

// ------------------------------------------------------------ source

#[implement(IDropSource)]
struct Source {
    own: HWND,
    data: IDataObject,
    /// Extracts the dragged entries to temporary files for a non-shell
    /// target. Runs when the button is released over it.
    extract_now: RefCell<Box<dyn FnMut() -> Option<Vec<PathBuf>>>>,
    target: Rc<Cell<Option<Target>>>,
}

impl IDropSource_Impl for Source_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            return DRAGDROP_S_CANCEL;
        }
        if keys.0 & MK_LBUTTON.0 != 0 {
            return S_OK; // still dragging
        }
        let target = target_at_cursor(self.own);
        self.target.set(Some(target));
        match target {
            Target::Own => DRAGDROP_S_CANCEL,
            Target::Shell => DRAGDROP_S_DROP,
            Target::Other => {
                let files = (self.extract_now.borrow_mut())();
                match files {
                    Some(files) if unsafe { set_files(&self.data, &files) }.is_ok() => DRAGDROP_S_DROP,
                    _ => DRAGDROP_S_CANCEL,
                }
            }
        }
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Runs the drag loop (blocking until the drop or cancel), carrying the
/// marker of `watch`. `list` provides the drag image; `pt` is the cursor in
/// its client coordinates.
///
/// The shell copies the marker while this is still running, so `watch`
/// must already be where `WM_APP_SHNOTIFY` handling can see it.
pub fn run(own: HWND, list: HWND, pt: POINT, marker: &Path, extract_now: Box<dyn FnMut() -> Option<Vec<PathBuf>>>) -> Outcome {
    unsafe {
        let _ = OleInitialize(None);
        let Ok(data) = SHCreateDataObject::<_, IDataObject>(None, None, None) else { return Outcome::Nothing };
        if set_files(&data, &[marker.to_path_buf()]).is_err() {
            return Outcome::Nothing;
        }
        let _ = set_preferred_effect(&data, DROPEFFECT_COPY);
        // The drag image: the selected rows, as the list draws them.
        if let Ok(helper) = CoCreateInstance::<_, IDragSourceHelper>(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) {
            let _ = helper.InitializeFromWindow(Some(list), Some(&pt), &data);
        }
        let target = Rc::new(Cell::new(None));
        let source: IDropSource =
            Source { own, data: data.clone(), extract_now: RefCell::new(extract_now), target: target.clone() }.into();
        let mut effect = DROPEFFECT_NONE;
        let hr = DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect);
        crate::timing::mark("drag done", &format!("hr={hr:?} effect={}", effect.0));
        drop(source);
        match (hr, target.get()) {
            (DRAGDROP_S_DROP, Some(Target::Shell)) => Outcome::Shell,
            (DRAGDROP_S_DROP, Some(Target::Other)) => Outcome::Handed,
            _ => Outcome::Nothing,
        }
    }
}

// ------------------------------------------------------------- watch

/// Waits for the shell to copy the marker somewhere.
pub struct Watch {
    id: u32,
    marker: Marker,
}

impl Watch {
    /// Creates the marker and starts listening for shell changes.
    pub fn start(hwnd: HWND) -> Option<Watch> {
        let _ = unsafe { OleInitialize(None) };
        let marker = Marker::create().ok()?;
        unsafe {
            let id = match SHGetSpecialFolderLocation(None, CSIDL_DESKTOP as i32) {
                Ok(desktop) => {
                    let entry = SHChangeNotifyEntry { pidl: desktop, fRecursive: true.into() };
                    let events = SHCNE_CREATE.0 | SHCNE_RENAMEITEM.0 | SHCNE_UPDATEDIR.0 | SHCNE_UPDATEITEM.0;
                    let id = SHChangeNotifyRegister(
                        hwnd,
                        SHCNRF_ShellLevel | SHCNRF_NewDelivery,
                        events as i32,
                        WM_APP_SHNOTIFY,
                        1,
                        &entry,
                    );
                    ILFree(Some(desktop));
                    id
                }
                Err(_) => 0,
            };
            Some(Watch { id, marker })
        }
    }

    pub fn marker(&self) -> &Path {
        &self.marker.path
    }

    /// Handles one `WM_APP_SHNOTIFY`. Returns the folder the marker was
    /// copied into, after deleting that copy.
    pub fn check(&self, wp: WPARAM, lp: LPARAM) -> Option<PathBuf> {
        let mut found = None;
        for p in notified_paths(wp, lp) {
            if p.starts_with(crate::temp::session_dir()) {
                continue; // our own copy
            }
            let candidate = if p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(&self.marker.name)) {
                p
            } else {
                p.join(&self.marker.name)
            };
            if candidate.is_file() {
                found = Some(candidate);
                break;
            }
        }
        let copy = found?;
        let folder = copy.parent()?.to_path_buf();
        let _ = std::fs::remove_file(&copy);
        unsafe {
            let w: Vec<u16> = wide_path(&copy).into_iter().chain([0]).collect();
            SHChangeNotify(SHCNE_DELETE, SHCNF_PATHW, Some(w.as_ptr() as _), None);
        }
        Some(folder)
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        if self.id != 0 {
            unsafe {
                let _ = SHChangeNotifyDeregister(self.id);
            }
        }
        let _ = std::fs::remove_file(&self.marker.path);
    }
}

fn notified_paths(wp: WPARAM, lp: LPARAM) -> Vec<PathBuf> {
    let mut out = Vec::new();
    unsafe {
        let mut pidls: *mut *mut ITEMIDLIST = std::ptr::null_mut();
        let mut event = 0i32;
        let lock = SHChangeNotification_Lock(HANDLE(wp.0 as _), lp.0 as u32, Some(&mut pidls), Some(&mut event));
        if lock.is_invalid() || pidls.is_null() {
            return out;
        }
        for i in 0..2 {
            let pidl = *pidls.add(i);
            if pidl.is_null() {
                continue;
            }
            let mut buf = [0u16; 260];
            if SHGetPathFromIDListW(pidl, &mut buf).as_bool() {
                let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                if n > 0 {
                    out.push(PathBuf::from(String::from_utf16_lossy(&buf[..n])));
                }
            }
        }
        let _ = SHChangeNotification_Unlock(lock);
    }
    out
}
