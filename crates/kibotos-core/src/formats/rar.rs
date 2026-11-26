//! RAR (v4 and v5), extraction only, through the official unRAR library.
//!
//! The unRAR license forbids using its code to build a RAR-compatible
//! compressor, so Kibotos never creates RAR archives
//! (see `licenses/unRAR-license.txt`).
//!
//! We drive the low-level API (`unrar_sys`) instead of letting unRAR write
//! files itself: data arrives through `UCM_PROCESSDATA` callbacks and goes
//! through the same pump as every other format, so path validation,
//! progress, cancellation and size limits apply to RAR too.

use std::ffi::{c_int, c_void};
use std::io;
use std::path::Path;
use std::ptr;

// The unRAR static library is built and linked by `unrar_sys`. We declare
// the API ourselves because unRAR's `dll.hpp` uses `#pragma pack(1)` and the
// `unrar_sys` structs are not packed: every field after the first pointer
// would be read at the wrong offset (modification time, link type...).
use unrar_sys as _;

use crate::pump::{Pump, Selection};
use crate::{ArchiveReader, Control, Entry, EntryId, EntryKind, Error, ExtractSink, Format, Result, Timestamp};

// unRAR uses the registry and CryptoAPI from advapi32.
#[cfg(windows)]
#[link(name = "advapi32")]
unsafe extern "system" {}

/// unRAR's `wchar_t`: UTF-16 on Windows, UTF-32 on macOS and Linux.
#[cfg(windows)]
type WChar = u16;
#[cfg(not(windows))]
type WChar = u32;

/// A string as NUL-terminated `wchar_t`s.
fn to_wide(s: &std::ffi::OsStr) -> Vec<WChar> {
    #[cfg(windows)]
    let mut w: Vec<WChar> = std::os::windows::ffi::OsStrExt::encode_wide(s).collect();
    #[cfg(not(windows))]
    let mut w: Vec<WChar> = s.to_string_lossy().chars().map(u32::from).collect();
    w.push(0);
    w
}

fn from_wide(w: &[WChar]) -> String {
    #[cfg(windows)]
    return <std::ffi::OsString as std::os::windows::ffi::OsStringExt>::from_wide(w).to_string_lossy().into_owned();
    #[cfg(not(windows))]
    return w.iter().map(|&c| char::from_u32(c).unwrap_or(char::REPLACEMENT_CHARACTER)).collect();
}

/// On Unix, unRAR turns wide file names into bytes with `wcstombs`, which
/// follows the process locale. A GUI app starts in the "C" locale, where
/// any non-ASCII archive path fails to open: switch the character type to
/// UTF-8 once (the rest of the locale is left alone).
#[cfg(unix)]
fn utf8_locale() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let current = libc::setlocale(libc::LC_CTYPE, ptr::null());
        let utf8 = !current.is_null() && {
            let name = std::ffi::CStr::from_ptr(current).to_string_lossy().to_ascii_uppercase();
            name.contains("UTF-8") || name.contains("UTF8")
        };
        if !utf8 {
            for candidate in [c"UTF-8", c"C.UTF-8", c"en_US.UTF-8"] {
                if !libc::setlocale(libc::LC_CTYPE, candidate.as_ptr()).is_null() {
                    break;
                }
            }
        }
    });
}

#[allow(dead_code)]
mod ffi {
    use std::ffi::{c_char, c_int, c_uint};

    use super::WChar;

    pub type Callback = extern "system" fn(c_uint, isize, isize, isize) -> c_int;

    pub const ERAR_SUCCESS: c_int = 0;
    pub const ERAR_END_ARCHIVE: c_int = 10;
    pub const ERAR_NO_MEMORY: c_int = 11;
    pub const ERAR_BAD_DATA: c_int = 12;
    pub const ERAR_BAD_ARCHIVE: c_int = 13;
    pub const ERAR_UNKNOWN_FORMAT: c_int = 14;
    pub const ERAR_EOPEN: c_int = 15;
    pub const ERAR_ECREATE: c_int = 16;
    pub const ERAR_ECLOSE: c_int = 17;
    pub const ERAR_EREAD: c_int = 18;
    pub const ERAR_EWRITE: c_int = 19;
    pub const ERAR_SMALL_BUF: c_int = 20;
    pub const ERAR_UNKNOWN: c_int = 21;
    pub const ERAR_MISSING_PASSWORD: c_int = 22;
    pub const ERAR_EREFERENCE: c_int = 23;
    pub const ERAR_BAD_PASSWORD: c_int = 24;

    pub const RAR_OM_LIST: c_uint = 0;
    pub const RAR_OM_EXTRACT: c_uint = 1;
    pub const RAR_SKIP: c_int = 0;
    pub const RAR_TEST: c_int = 1;
    pub const RAR_VOL_ASK: isize = 0;
    pub const RAR_HASH_CRC32: c_uint = 1;

    pub const RHDF_ENCRYPTED: c_uint = 0x04;
    pub const RHDF_DIRECTORY: c_uint = 0x20;

    pub const UCM_PROCESSDATA: c_uint = 1;
    pub const UCM_CHANGEVOLUMEW: c_uint = 3;
    pub const UCM_NEEDPASSWORDW: c_uint = 4;
    /// Asks whether to allow a dictionary above the default limit. We
    /// return 0 (deny), so such archives fail instead of using many GB.
    pub const UCM_LARGEDICT: c_uint = 5;

    #[repr(C, packed)]
    pub struct OpenArchiveDataEx {
        pub arc_name: *const c_char,
        pub arc_name_w: *const WChar,
        pub open_mode: c_uint,
        pub open_result: c_uint,
        pub cmt_buf: *mut c_char,
        pub cmt_buf_size: c_uint,
        pub cmt_size: c_uint,
        pub cmt_state: c_uint,
        pub flags: c_uint,
        pub callback: Option<Callback>,
        pub user_data: isize,
        pub op_flags: c_uint,
        pub cmt_buf_w: *mut WChar,
        pub reserved: [c_uint; 25],
    }

    #[repr(C, packed)]
    pub struct HeaderDataEx {
        pub arc_name: [c_char; 1024],
        pub arc_name_w: [WChar; 1024],
        pub file_name: [c_char; 1024],
        pub file_name_w: [WChar; 1024],
        pub flags: c_uint,
        pub pack_size: c_uint,
        pub pack_size_high: c_uint,
        pub unp_size: c_uint,
        pub unp_size_high: c_uint,
        pub host_os: c_uint,
        pub file_crc: c_uint,
        pub file_time: c_uint,
        pub unp_ver: c_uint,
        pub method: c_uint,
        pub file_attr: c_uint,
        pub cmt_buf: *mut c_char,
        pub cmt_buf_size: c_uint,
        pub cmt_size: c_uint,
        pub cmt_state: c_uint,
        pub dict_size: c_uint,
        pub hash_type: c_uint,
        pub hash: [c_char; 32],
        pub redir_type: c_uint,
        pub redir_name: *mut WChar,
        pub redir_name_size: c_uint,
        pub dir_target: c_uint,
        pub mtime_low: c_uint,
        pub mtime_high: c_uint,
        pub ctime_low: c_uint,
        pub ctime_high: c_uint,
        pub atime_low: c_uint,
        pub atime_high: c_uint,
        pub arc_name_ex: *mut WChar,
        pub arc_name_ex_size: c_uint,
        pub file_name_ex: *mut WChar,
        pub file_name_ex_size: c_uint,
        pub reserved: [c_uint; 982],
    }

    #[repr(C)]
    pub struct Handle {
        _private: [u8; 0],
    }

    unsafe extern "system" {
        pub fn RAROpenArchiveEx(data: *mut OpenArchiveDataEx) -> *mut Handle;
        pub fn RARCloseArchive(h: *mut Handle) -> c_int;
        pub fn RARReadHeaderEx(h: *mut Handle, data: *mut HeaderDataEx) -> c_int;
        pub fn RARProcessFileW(h: *mut Handle, op: c_int, dest_path: *const WChar, dest_name: *const WChar) -> c_int;
    }

    pub fn new_header() -> Box<HeaderDataEx> {
        // SAFETY: all-zero is a valid HeaderDataEx (integers and null pointers).
        Box::new(unsafe { std::mem::zeroed() })
    }
}

pub struct RarBackend {
    path_w: Vec<WChar>,
    entries: Vec<Entry>,
    password: Option<String>,
    len: u64,
}

/// State shared with the unRAR callback. Accessed only through a raw
/// pointer while unRAR is running.
struct CallbackState {
    password: Option<Vec<WChar>>,
    missing_volume: Option<String>,
    /// Points to a `&mut dyn FnMut(&[u8]) -> bool` during RARProcessFileW.
    on_data: *mut c_void,
}

type DataFn<'a> = &'a mut dyn FnMut(&[u8]) -> bool;

extern "system" fn callback(msg: u32, user: isize, p1: isize, p2: isize) -> c_int {
    if user == 0 {
        return -1;
    }
    // SAFETY: `user` is the CallbackState registered with this handle, which
    // outlives the handle.
    let st = unsafe { &mut *(user as *mut CallbackState) };
    match msg {
        ffi::UCM_CHANGEVOLUMEW => {
            if p2 == ffi::RAR_VOL_ASK {
                // The next volume does not exist. Record its name and stop.
                st.missing_volume = Some(unsafe { wide_to_string(p1 as *const WChar, 2048) });
                -1
            } else {
                1
            }
        }
        ffi::UCM_NEEDPASSWORDW => match &st.password {
            Some(pw) if p2 > 1 => {
                let n = pw.len().min(p2 as usize - 1);
                // SAFETY: unRAR gives a buffer of `p2` wide characters.
                unsafe {
                    ptr::copy_nonoverlapping(pw.as_ptr(), p1 as *mut WChar, n);
                    *(p1 as *mut WChar).add(n) = 0;
                }
                1
            }
            _ => -1,
        },
        ffi::UCM_PROCESSDATA => {
            if st.on_data.is_null() {
                return 1;
            }
            // SAFETY: `on_data` is set only for the duration of
            // RARProcessFileW and points to a live `DataFn`.
            let f = unsafe { &mut *(st.on_data as *mut DataFn<'_>) };
            let chunk = unsafe { std::slice::from_raw_parts(p1 as *const u8, p2 as usize) };
            if f(chunk) { 1 } else { -1 }
        }
        _ => 0,
    }
}

unsafe fn wide_to_string(p: *const WChar, max: usize) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while n < max && unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    from_wide(unsafe { std::slice::from_raw_parts(p, n) })
}

fn wide_field(buf: &[WChar]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    from_wide(&buf[..n])
}

/// An open unRAR handle; closed on drop.
struct Handle(*mut ffi::Handle);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { ffi::RARCloseArchive(self.0) };
    }
}

fn map_code(code: c_int, encrypted: bool, st: &CallbackState) -> Error {
    match code {
        ffi::ERAR_MISSING_PASSWORD => Error::PasswordRequired,
        ffi::ERAR_BAD_PASSWORD => Error::WrongPassword,
        ffi::ERAR_BAD_DATA if encrypted => Error::WrongPassword,
        ffi::ERAR_BAD_DATA => Error::Corrupt("checksum error".into()),
        ffi::ERAR_BAD_ARCHIVE => Error::Corrupt("not a valid RAR archive".into()),
        ffi::ERAR_UNKNOWN_FORMAT => Error::Unsupported("unknown RAR format version".into()),
        ffi::ERAR_NO_MEMORY => Error::Io(io::ErrorKind::OutOfMemory.into()),
        ffi::ERAR_EREFERENCE => Error::Unsupported("RAR file reference without its source".into()),
        ffi::ERAR_EOPEN | ffi::ERAR_EREAD if st.missing_volume.is_some() => {
            Error::MissingVolume(st.missing_volume.clone().unwrap_or_default())
        }
        ffi::ERAR_EOPEN => Error::Io(io::Error::other("cannot open RAR archive")),
        ffi::ERAR_EREAD => Error::Corrupt("read error (truncated archive?)".into()),
        ffi::ERAR_ECREATE | ffi::ERAR_EWRITE | ffi::ERAR_ECLOSE => Error::Io(io::Error::other("unRAR write error")),
        other => Error::Corrupt(format!("unRAR error {other}")),
    }
}

impl RarBackend {
    pub fn open(path: &Path, password: Option<&str>) -> Result<RarBackend> {
        let len = std::fs::metadata(path)?.len();
        #[cfg(unix)]
        utf8_locale();
        let path_w = to_wide(path.as_os_str());
        let mut b = RarBackend { path_w, entries: Vec::new(), password: password.map(str::to_owned), len };
        b.entries = b.list()?;
        Ok(b)
    }

    fn state(&self) -> Box<CallbackState> {
        Box::new(CallbackState {
            password: self.password.as_ref().map(|p| {
                let mut w = to_wide(std::ffi::OsStr::new(p));
                w.pop(); // the callback adds its own terminator
                w
            }),
            missing_volume: None,
            on_data: ptr::null_mut(),
        })
    }

    fn open_handle(&self, mode: u32, st: *mut CallbackState) -> Result<Handle> {
        let mut data = ffi::OpenArchiveDataEx {
            arc_name: ptr::null(),
            arc_name_w: self.path_w.as_ptr(),
            open_mode: mode,
            open_result: 0,
            cmt_buf: ptr::null_mut(),
            cmt_buf_size: 0,
            cmt_size: 0,
            cmt_state: 0,
            flags: 0,
            callback: Some(callback),
            user_data: st as isize,
            op_flags: 0,
            cmt_buf_w: ptr::null_mut(),
            reserved: [0; 25],
        };
        // SAFETY: `data` is fully initialized and the path is NUL-terminated.
        let h = unsafe { ffi::RAROpenArchiveEx(&mut data) };
        if h.is_null() || data.open_result != 0 {
            if !h.is_null() {
                drop(Handle(h));
            }
            return Err(map_code(data.open_result as c_int, false, unsafe { &*st }));
        }
        Ok(Handle(h))
    }

    fn list(&self) -> Result<Vec<Entry>> {
        let mut st = self.state();
        let stp: *mut CallbackState = &mut *st;
        let h = self.open_handle(ffi::RAR_OM_LIST, stp)?;
        let mut hd = ffi::new_header();
        let mut entries = Vec::new();
        loop {
            let r = unsafe { ffi::RARReadHeaderEx(h.0, &mut *hd) };
            if r == ffi::ERAR_END_ARCHIVE {
                break;
            }
            if r != ffi::ERAR_SUCCESS {
                return Err(map_code(r, false, unsafe { &*stp }));
            }
            entries.push(convert(entries.len(), &hd));
            let r = unsafe { ffi::RARProcessFileW(h.0, ffi::RAR_SKIP, ptr::null(), ptr::null()) };
            if r != ffi::ERAR_SUCCESS {
                return Err(map_code(r, false, unsafe { &*stp }));
            }
        }
        drop(h);
        Ok(entries)
    }
}

fn convert(id: usize, hd: &ffi::HeaderDataEx) -> Entry {
    let kind = if hd.flags & ffi::RHDF_DIRECTORY != 0 {
        EntryKind::Dir
    } else if hd.redir_type != 0 {
        EntryKind::Link
    } else {
        EntryKind::File
    };
    let name_w = hd.file_name_w;
    let name = wide_field(&name_w);
    let mut e = Entry::new(id, &name, kind);
    if kind != EntryKind::Dir {
        e.size = Some(((hd.unp_size_high as u64) << 32) | hd.unp_size as u64);
        e.packed = Some(((hd.pack_size_high as u64) << 32) | hd.pack_size as u64);
        if hd.hash_type == ffi::RAR_HASH_CRC32 || hd.file_crc != 0 {
            e.crc = Some(hd.file_crc);
        }
    }
    let ft = ((hd.mtime_high as u64) << 32) | hd.mtime_low as u64;
    e.modified = crate::time::filetime_to_unix(ft).map(Timestamp::Utc).or_else(|| {
        crate::time::dos_to_secs((hd.file_time >> 16) as u16, hd.file_time as u16).map(Timestamp::Local)
    });
    e.encrypted = hd.flags & ffi::RHDF_ENCRYPTED != 0;
    e
}

impl ArchiveReader for RarBackend {
    fn format(&self) -> Format {
        Format::Rar
    }

    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn set_password(&mut self, password: Option<&str>) {
        self.password = password.map(str::to_owned);
    }

    fn extract(&mut self, ids: &[EntryId], sink: &mut dyn ExtractSink, ctl: &Control) -> Result<()> {
        let mut sel = Selection::new(ids, &self.entries, ctl)?;
        if sel.remaining == 0 {
            return Ok(());
        }
        let mut pump = Pump::new(ctl, self.len);
        let mut st = self.state();
        let stp: *mut CallbackState = &mut *st;
        let h = self.open_handle(ffi::RAR_OM_EXTRACT, stp)?;
        let mut hd = ffi::new_header();
        let skip = move |h: &Handle| -> Result<()> {
            let r = unsafe { ffi::RARProcessFileW(h.0, ffi::RAR_SKIP, ptr::null(), ptr::null()) };
            if r == ffi::ERAR_SUCCESS { Ok(()) } else { Err(map_code(r, false, unsafe { &*stp })) }
        };

        // Headers come in the same order as in `list` (both modes hide
        // split-file continuations), so the position is the entry id.
        let mut id = 0usize;
        loop {
            ctl.check()?;
            let r = unsafe { ffi::RARReadHeaderEx(h.0, &mut *hd) };
            if r == ffi::ERAR_END_ARCHIVE {
                break;
            }
            if r != ffi::ERAR_SUCCESS {
                return Err(map_code(r, false, unsafe { &*stp }));
            }
            let current = id;
            id += 1;
            if !sel.contains(current) {
                skip(&h)?;
                continue;
            }
            let entry = &self.entries[current];
            if entry.kind != EntryKind::File {
                pump.run(entry, &mut io::empty(), sink, &Error::from_read)?;
                skip(&h)?;
            } else {
                let Some(mut w) = pump.begin(entry, sink)? else {
                    skip(&h)?;
                    if sel.done(current) {
                        break;
                    }
                    continue;
                };
                let mut failure: Option<Error> = None;
                let r = {
                    let mut write_chunk = |chunk: &[u8]| match pump.write(entry, &mut *w, chunk) {
                        Ok(()) => true,
                        Err(e) => {
                            failure = Some(e);
                            false
                        }
                    };
                    let mut f: DataFn<'_> = &mut write_chunk;
                    unsafe { (*stp).on_data = &mut f as *mut DataFn<'_> as *mut c_void };
                    // RAR_TEST decodes and verifies without unRAR writing
                    // anything; the data reaches us through the callback.
                    let r = unsafe { ffi::RARProcessFileW(h.0, ffi::RAR_TEST, ptr::null(), ptr::null()) };
                    unsafe { (*stp).on_data = ptr::null_mut() };
                    r
                };
                drop(w);
                let outcome = match failure {
                    Some(e) => Err(e),
                    None if r != ffi::ERAR_SUCCESS => Err(map_code(r, entry.encrypted, unsafe { &*stp })),
                    None => Ok(()),
                };
                pump.end(entry, sink, outcome)?;
            }
            if sel.done(current) {
                break;
            }
        }
        drop(h);
        drop(st);
        Ok(())
    }
}
