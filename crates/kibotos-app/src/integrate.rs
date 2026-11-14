//! Windows integration: "Abrir con", the Explorer context menu and Default
//! Apps. Everything is written under HKEY_CURRENT_USER (no administrator
//! rights) and `remove` deletes exactly what `install` wrote.
//!
//! Windows protects the user's default-app choice, so Kibotós never sets
//! itself as the default program: it registers as an option (OpenWithProgids
//! and Capabilities) and the user picks it once through "Abrir con".
//!
//! The context menu uses registry verbs under SystemFileAssociations, so it
//! appears on every archive whatever program opens it by default. On
//! Windows 11 these appear under "Mostrar más opciones"; the modern menu
//! needs an IExplorerCommand handler in a signed package (a later version).

use std::io;
use std::path::Path;

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_NONE, REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE,
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteTreeW, RegDeleteValueW, RegOpenKeyExW, RegQueryInfoKeyW,
    RegQueryValueExW, RegSetValueExW,
};
use windows::core::HSTRING;

use crate::strings::t;

pub const PROGID: &str = "Kibotos.Archive";
const MENU: &str = "Kibotos.Menu";
const APP: &str = "Kibotos";

/// Extensions Kibotós offers to open. `.jar` and `.apk` get the context
/// menu and "Abrir con", which never changes how they run by default.
pub const EXTENSIONS: &[&str] = &[
    "zip", "jar", "apk", "cbz", "7z", "rar", "cbr", "tar", "gz", "tgz", "bz2", "tbz", "tbz2", "xz", "txz", "zst", "tzst",
];

fn check(e: WIN32_ERROR) -> io::Result<()> {
    if e.0 == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(e.0 as i32)) }
}

/// An open registry key under HKEY_CURRENT_USER.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

impl Key {
    fn create(path: &str) -> io::Result<Key> {
        let mut h = HKEY::default();
        check(unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                &HSTRING::from(path),
                None,
                &HSTRING::new(),
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_WRITE,
                None,
                &mut h,
                None,
            )
        })?;
        Ok(Key(h))
    }

    fn open(path: &str) -> Option<Key> {
        let mut h = HKEY::default();
        let e = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, &HSTRING::from(path), None, KEY_READ, &mut h) };
        (e.0 == 0).then_some(Key(h))
    }

    fn set_raw(&self, name: &str, kind: REG_VALUE_TYPE, data: &[u8]) -> io::Result<()> {
        check(unsafe { RegSetValueExW(self.0, &HSTRING::from(name), None, kind, Some(data)) })
    }

    /// A string value; `""` is the key's default value.
    fn set(&self, name: &str, value: &str) -> io::Result<()> {
        let bytes: Vec<u8> = value.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect();
        self.set_raw(name, REG_SZ, &bytes)
    }

    fn get(&self, name: &str) -> Option<String> {
        let mut len = 0u32;
        let name = HSTRING::from(name);
        unsafe {
            if RegQueryValueExW(self.0, &name, None, None, None, Some(&mut len)).0 != 0 || len < 2 {
                return None;
            }
            let mut buf = vec![0u16; (len as usize).div_ceil(2)];
            if RegQueryValueExW(self.0, &name, None, None, Some(buf.as_mut_ptr() as *mut u8), Some(&mut len)).0 != 0 {
                return None;
            }
            let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Some(String::from_utf16_lossy(&buf[..n]))
        }
    }

    fn is_empty(&self) -> bool {
        let (mut subkeys, mut values) = (0u32, 0u32);
        let ok = unsafe {
            RegQueryInfoKeyW(
                self.0,
                None,
                None,
                None,
                Some(&mut subkeys),
                None,
                None,
                Some(&mut values),
                None,
                None,
                None,
                None,
            )
        };
        ok.0 == 0 && subkeys == 0 && values == 0
    }
}

fn delete_tree(path: &str) -> io::Result<()> {
    let e = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(path)) };
    if e == ERROR_FILE_NOT_FOUND { Ok(()) } else { check(e) }
}

fn delete_value(path: &str, name: &str) -> io::Result<()> {
    let Some(k) = Key::open_rw(path) else { return Ok(()) };
    let e = unsafe { RegDeleteValueW(k.0, &HSTRING::from(name)) };
    if e == ERROR_FILE_NOT_FOUND { Ok(()) } else { check(e) }
}

/// Deletes `path` only if nothing (ours or anyone else's) is left in it.
fn remove_if_empty(path: &str) {
    if Key::open(path).is_some_and(|k| k.is_empty()) {
        unsafe {
            let _ = RegDeleteKeyW(HKEY_CURRENT_USER, &HSTRING::from(path));
        }
    }
}

impl Key {
    fn open_rw(path: &str) -> Option<Key> {
        let mut h = HKEY::default();
        let e = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, &HSTRING::from(path), None, KEY_READ | KEY_WRITE, &mut h) };
        (e.0 == 0).then_some(Key(h))
    }
}

/// Registry layout, relative to HKEY_CURRENT_USER. `software` is normally
/// "Software"; tests use a scratch key instead.
struct Layout {
    software: String,
}

impl Layout {
    fn classes(&self) -> String {
        format!(r"{}\Classes", self.software)
    }
    fn app(&self) -> String {
        format!(r"{}\{APP}", self.software)
    }
    fn capabilities(&self) -> String {
        format!(r"{}\{APP}\Capabilities", self.software)
    }
}

/// Writes the integration for `exe`. Running it again updates the path.
fn install_at(l: &Layout, exe: &Path) -> io::Result<()> {
    let exe_s = exe.display().to_string();
    let exe_name = exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "kibotos.exe".into());
    let icon = format!("\"{exe_s}\",0");
    let open = format!("\"{exe_s}\" \"%1\"");
    let classes = l.classes();

    // ProgID: what "Kibotós" means for an archive.
    let progid = format!(r"{classes}\{PROGID}");
    Key::create(&progid)?.set("", t::REG_TYPE_NAME)?;
    Key::create(&format!(r"{progid}\DefaultIcon"))?.set("", &icon)?;
    Key::create(&format!(r"{progid}\shell"))?.set("", "open")?;
    Key::create(&format!(r"{progid}\shell\open"))?.set("", t::REG_OPEN)?;
    Key::create(&format!(r"{progid}\shell\open\command"))?.set("", &open)?;

    // The submenu's verbs (ordered by key name).
    let menu = format!(r"{classes}\{MENU}\shell");
    for (key, label, args) in [
        ("1open", t::REG_OPEN, String::new()),
        ("2here", t::REG_EXTRACT_HERE, "--extract-here ".to_string()),
        ("3folder", t::REG_EXTRACT_FOLDER, "--extract-to-folder ".to_string()),
        ("4dialog", t::REG_EXTRACT_DIALOG, "--extract-dialog ".to_string()),
    ] {
        let verb = Key::create(&format!(r"{menu}\{key}"))?;
        verb.set("MUIVerb", label)?;
        Key::create(&format!(r"{menu}\{key}\command"))?.set("", &format!("\"{exe_s}\" {args}\"%1\""))?;
    }

    // "Abrir con" entry for the program itself.
    let app = format!(r"{classes}\Applications\{exe_name}");
    Key::create(&app)?.set("FriendlyAppName", t::APP_NAME)?;
    Key::create(&format!(r"{app}\DefaultIcon"))?.set("", &icon)?;
    Key::create(&format!(r"{app}\shell\open\command"))?.set("", &open)?;
    let supported = Key::create(&format!(r"{app}\SupportedTypes"))?;

    // Default Apps (Settings > Apps > Default apps).
    let caps = l.capabilities();
    let c = Key::create(&caps)?;
    c.set("ApplicationName", t::APP_NAME)?;
    c.set("ApplicationDescription", t::ABOUT_TAGLINE)?;
    c.set("ApplicationIcon", &icon)?;
    let assoc = Key::create(&format!(r"{caps}\FileAssociations"))?;

    for ext in EXTENSIONS {
        let dot = format!(".{ext}");
        // Offered in "Abrir con"; the default program is left alone.
        Key::create(&format!(r"{classes}\{dot}\OpenWithProgids"))?.set_raw(PROGID, REG_NONE, &[])?;
        supported.set(&dot, "")?;
        assoc.set(&dot, PROGID)?;
        // The cascading "Kibotós" entry in the context menu.
        let entry = Key::create(&format!(r"{classes}\SystemFileAssociations\{dot}\shell\{APP}"))?;
        entry.set("MUIVerb", t::APP_NAME)?;
        entry.set("Icon", &icon)?;
        entry.set("ExtendedSubCommandsKey", MENU)?;
    }

    Key::create(&format!(r"{}\RegisteredApplications", l.software))?.set(APP, &format!(r"{}\{APP}\Capabilities", l.software))?;
    // Remember what to remove later, even if the .exe is renamed.
    let app_key = Key::create(&l.app())?;
    app_key.set("Exe", &exe_s)?;
    app_key.set("ExeName", &exe_name)?;
    Ok(())
}

/// Removes everything `install_at` wrote, leaving other programs' entries.
fn remove_at(l: &Layout) -> io::Result<()> {
    let classes = l.classes();
    let current = std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    let exe_name = Key::open(&l.app()).and_then(|k| k.get("ExeName")).or(current).unwrap_or_else(|| "kibotos.exe".into());

    delete_tree(&format!(r"{classes}\{PROGID}"))?;
    delete_tree(&format!(r"{classes}\{MENU}"))?;
    delete_tree(&format!(r"{classes}\Applications\{exe_name}"))?;
    for ext in EXTENSIONS {
        let dot = format!(".{ext}");
        delete_value(&format!(r"{classes}\{dot}\OpenWithProgids"), PROGID)?;
        remove_if_empty(&format!(r"{classes}\{dot}\OpenWithProgids"));
        remove_if_empty(&format!(r"{classes}\{dot}"));
        delete_tree(&format!(r"{classes}\SystemFileAssociations\{dot}\shell\{APP}"))?;
        remove_if_empty(&format!(r"{classes}\SystemFileAssociations\{dot}\shell"));
        remove_if_empty(&format!(r"{classes}\SystemFileAssociations\{dot}"));
    }
    delete_value(&format!(r"{}\RegisteredApplications", l.software), APP)?;
    delete_tree(&l.app())?;
    Ok(())
}

fn notify_shell() {
    use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

/// "Integrar con Windows" for the running .exe.
pub fn install() -> io::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    // A previous integration may point to another copy or name.
    remove_at(&Layout { software: "Software".into() })?;
    install_at(&Layout { software: "Software".into() }, &exe)?;
    notify_shell();
    Ok(exe)
}

/// "Quitar integración".
pub fn remove() -> io::Result<()> {
    remove_at(&Layout { software: "Software".into() })?;
    notify_shell();
    Ok(())
}

/// Whether Kibotós is currently integrated, and with which .exe.
pub fn installed_exe() -> Option<String> {
    Key::open(&format!(r"Software\{APP}")).and_then(|k| k.get("Exe"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes the full layout under a scratch key, checks it, removes it and
    /// checks that nothing is left. Never touches the real associations.
    #[test]
    fn install_then_remove_leaves_nothing() {
        let scratch = format!(r"Software\KibotosTest-{}", std::process::id());
        let l = Layout { software: scratch.clone() };
        let exe = Path::new(r"C:\Programas\Kibotos\kibotos-v0.1.exe");
        // Another program's entry under one of our extensions must survive.
        Key::create(&format!(r"{scratch}\Classes\.zip\OpenWithProgids")).unwrap().set_raw("Otro.Zip", REG_NONE, &[]).unwrap();

        install_at(&l, exe).unwrap();
        let classes = l.classes();
        let get = |path: &str, name: &str| Key::open(path).and_then(|k| k.get(name));
        assert_eq!(get(&format!(r"{classes}\{PROGID}\shell\open\command"), "").unwrap(), format!("\"{}\" \"%1\"", exe.display()));
        assert_eq!(
            get(&format!(r"{classes}\{MENU}\shell\2here\command"), "").unwrap(),
            format!("\"{}\" --extract-here \"%1\"", exe.display())
        );
        assert_eq!(get(&format!(r"{classes}\SystemFileAssociations\.7z\shell\{APP}"), "ExtendedSubCommandsKey").unwrap(), MENU);
        assert_eq!(get(&format!(r"{}\FileAssociations", l.capabilities()), ".rar").unwrap(), PROGID);
        assert!(Key::open(&format!(r"{classes}\Applications\kibotos-v0.1.exe")).is_some());
        // The default program of .zip was not touched.
        assert!(get(&format!(r"{classes}\.zip"), "").is_none());

        remove_at(&l).unwrap();
        assert!(Key::open(&format!(r"{classes}\{PROGID}")).is_none());
        assert!(Key::open(&format!(r"{classes}\.7z")).is_none(), "empty keys we created are removed");
        assert!(Key::open(&format!(r"{classes}\SystemFileAssociations")).is_none_or(|k| k.is_empty()));
        assert!(Key::open(&l.app()).is_none());
        let other = Key::open(&format!(r"{classes}\.zip\OpenWithProgids")).expect("other program's key kept");
        let mut kind = REG_VALUE_TYPE::default();
        let found = unsafe { RegQueryValueExW(other.0, &HSTRING::from("Otro.Zip"), None, Some(&mut kind), None, None) };
        assert_eq!(found.0, 0, "other program's value kept");
        drop(other);
        delete_tree(&scratch).unwrap();
    }
}
