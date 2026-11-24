//! Installing Kibotós for the current user, without administrator rights.
//!
//! The downloaded .exe installs itself: it is copied to
//! `%LOCALAPPDATA%\Programs\Kibotos\kibotos.exe` (Windows' folder for
//! per-user programs, on the system drive), integrated with Explorer, given
//! a Start menu shortcut and an entry in Configuración › Aplicaciones whose
//! "Desinstalar" runs `kibotos.exe --uninstall`.
//!
//! Installing a newer version over a running one works: a running .exe
//! cannot be overwritten but can be renamed, so the old file is moved aside
//! (`kibotos.exe.old`) and removed the next time the installed copy starts.
//!
//! The offer to install appears only for a copy downloaded from the
//! internet (it carries the Mark of the Web), so copies built or copied
//! locally never ask.

use std::io;
use std::path::{Path, PathBuf};

use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::core::{HSTRING, Interface};

use crate::integrate::{self, Key};
use crate::strings::t;

const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Kibotos";
const EXE_NAME: &str = "kibotos.exe";
/// Setting: "never" when the user asked not to be offered installing.
const OFFER_SETTING: &str = "InstallOffer";

/// `%LOCALAPPDATA%\Programs\Kibotos`.
pub fn install_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Programs").join("Kibotos"))
}

fn installed_path() -> Option<PathBuf> {
    install_dir().map(|d| d.join(EXE_NAME))
}

fn same_file(a: &Path, b: &Path) -> bool {
    let key = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_owned()).to_string_lossy().to_lowercase();
    key(a) == key(b)
}

/// Whether this process is the installed copy.
pub fn is_installed_copy() -> bool {
    match (std::env::current_exe(), installed_path()) {
        (Ok(me), Some(inst)) => same_file(&me, &inst),
        _ => false,
    }
}

/// Version of the installed copy, if Kibotós is installed.
pub fn installed_version() -> Option<String> {
    installed_path().filter(|p| p.is_file())?;
    Key::open(UNINSTALL_KEY)?.get("DisplayVersion")
}

/// Whether this .exe came from the internet (Mark of the Web).
fn downloaded() -> bool {
    std::env::current_exe().is_ok_and(|me| {
        let mut stream = me.into_os_string();
        stream.push(":Zone.Identifier");
        std::fs::metadata(stream).is_ok()
    })
}

/// "0.3.0" or "v0.3" as numbers, to compare versions.
pub fn version_key(v: &str) -> (u32, u32, u32) {
    let mut parts = v.trim().trim_start_matches(['v', 'V']).split('.').map(|p| {
        p.chars().take_while(char::is_ascii_digit).collect::<String>().parse::<u32>().unwrap_or(0)
    });
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What to offer when a downloaded copy starts.
pub enum Offer {
    Install,
    /// An older version is installed.
    Update { installed: String },
}

pub fn offer() -> Option<Offer> {
    if is_installed_copy() || !downloaded() || integrate::read_setting(OFFER_SETTING).as_deref() == Some("never") {
        return None;
    }
    match installed_version() {
        None => Some(Offer::Install),
        Some(v) if version_key(&v) < version_key(VERSION) => Some(Offer::Update { installed: v }),
        Some(_) => None,
    }
}

pub fn never_offer() {
    integrate::write_setting(OFFER_SETTING, "never");
}

/// Installs (or updates) this .exe for the current user. Returns the
/// installed .exe.
pub fn install() -> io::Result<PathBuf> {
    let me = std::env::current_exe()?;
    let dir = install_dir().ok_or_else(|| io::Error::other("LOCALAPPDATA is not set"))?;
    let target = dir.join(EXE_NAME);
    std::fs::create_dir_all(&dir)?;
    if !same_file(&me, &target) {
        let fresh = dir.join(format!("{EXE_NAME}.new"));
        std::fs::copy(&me, &fresh)?;
        // The copy keeps the download's Mark of the Web: an installed
        // program does not need it (the user chose to install it).
        let mut stream = fresh.clone().into_os_string();
        stream.push(":Zone.Identifier");
        let _ = std::fs::remove_file(stream);
        if target.exists() {
            // A running copy can be renamed, not replaced.
            let old = dir.join(format!("{EXE_NAME}.old"));
            let _ = std::fs::remove_file(&old);
            std::fs::rename(&target, &old)?;
        }
        std::fs::rename(&fresh, &target)?;
    }
    integrate::install_exe(&target)?;
    shortcut(&target)?;
    register_uninstall(&target, &dir)?;
    Ok(target)
}

/// Removes the file left by an update over a running copy.
pub fn cleanup_old() {
    if let Some(dir) = install_dir() {
        let _ = std::fs::remove_file(dir.join(format!("{EXE_NAME}.old")));
        let _ = std::fs::remove_file(dir.join(format!("{EXE_NAME}.new")));
    }
}

fn shortcut_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(|d| PathBuf::from(d).join(r"Microsoft\Windows\Start Menu\Programs").join(format!("{}.lnk", t::APP_NAME)))
}

/// The Start menu entry.
fn shortcut(exe: &Path) -> io::Result<()> {
    let Some(lnk) = shortcut_path() else { return Ok(()) };
    let err = |e: windows::core::Error| io::Error::other(e.message());
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(err)?;
        link.SetPath(&HSTRING::from(exe.as_os_str())).map_err(err)?;
        if let Some(dir) = exe.parent() {
            link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str())).map_err(err)?;
        }
        link.SetDescription(&HSTRING::from(t::ABOUT_TAGLINE)).map_err(err)?;
        let file: IPersistFile = link.cast().map_err(err)?;
        file.Save(&HSTRING::from(lnk.as_os_str()), true).map_err(err)?;
    }
    Ok(())
}

/// Configuración › Aplicaciones: name, version, size and "Desinstalar".
fn register_uninstall(exe: &Path, dir: &Path) -> io::Result<()> {
    let k = Key::create(UNINSTALL_KEY)?;
    let exe_s = exe.display().to_string();
    k.set("DisplayName", t::APP_NAME)?;
    k.set("DisplayVersion", VERSION)?;
    k.set("Publisher", "byarcy")?;
    k.set("DisplayIcon", &format!("\"{exe_s}\",0"))?;
    k.set("InstallLocation", &dir.display().to_string())?;
    k.set("UninstallString", &format!("\"{exe_s}\" --uninstall"))?;
    k.set("QuietUninstallString", &format!("\"{exe_s}\" --uninstall --quiet"))?;
    k.set("URLInfoAbout", "https://github.com/byarcy/Kibotos")?;
    let today = {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let days = secs.div_euclid(86_400);
        // Civil date from days since 1970 (Howard Hinnant's algorithm).
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!("{y:04}{m:02}{d:02}")
    };
    k.set("InstallDate", &today)?;
    k.set_dword("NoModify", 1)?;
    k.set_dword("NoRepair", 1)?;
    let kb = std::fs::metadata(exe).map_or(0, |m| m.len().div_ceil(1024)) as u32;
    k.set_dword("EstimatedSize", kb)?;
    Ok(())
}

/// Removes the installation: Explorer integration, settings, shortcut, the
/// Configuración entry and the folder. The running .exe cannot delete
/// itself, so a hidden command removes the folder once this process has
/// ended.
pub fn uninstall() -> io::Result<()> {
    integrate::remove_all()?;
    if let Some(lnk) = shortcut_path() {
        let _ = std::fs::remove_file(lnk);
    }
    integrate::delete_key(UNINSTALL_KEY)?;
    let Some(dir) = install_dir() else { return Ok(()) };
    if !dir.exists() {
        return Ok(());
    }
    if is_installed_copy() {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let script = format!("ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"", dir.display());
        std::process::Command::new("cmd.exe").args(["/d", "/c"]).raw_arg(&script).creation_flags(CREATE_NO_WINDOW).spawn()?;
    } else {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(version_key("v0.4") > version_key("0.3.0"));
        assert!(version_key("0.3.1") > version_key("v0.3"));
        assert_eq!(version_key("v0.3"), version_key("0.3.0"));
        assert!(version_key("1.0.0-beta") > version_key("0.9.9"));
        assert!(version_key("v0.10") > version_key("v0.9"));
    }
}
