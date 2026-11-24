//! Installing and updating from the main window: the offer to install a
//! downloaded copy, Herramientas › Instalar en este equipo, and the checks
//! for a newer version (daily if enabled, or Ayuda › Buscar
//! actualizaciones).
//!
//! Everything here runs from a timer a moment after the window appears, so
//! none of it adds to startup time.

use crate::install::{self, Offer};
use crate::update::{self, CheckError};

use super::*;

/// What the window shows now (archive or folder), to open it again in the
/// installed copy.
fn current_target() -> Option<PathBuf> {
    with_app(|a| {
        if let Some(arc) = &a.archive {
            Some(arc.path.clone())
        } else {
            a.fs.as_ref().map(|f| PathBuf::from(f.place.shown())).filter(|p| p.is_dir())
        }
    })
}

/// A moment after startup: clean up after an update, offer to install a
/// downloaded copy, or run the daily check for a new version.
pub(super) fn startup_checks(hwnd: HWND) {
    if install::is_installed_copy() {
        install::cleanup_old();
    }
    if let Some(offer) = install::offer() {
        offer_install(hwnd, Some(offer));
        return;
    }
    if update::due() {
        let raw = hwnd.0 as isize;
        std::thread::spawn(move || {
            let found = update::latest();
            update::mark_checked();
            if let Ok(tag) = found {
                if update::is_newer(&tag) && !update::skipped(&tag) {
                    let boxed = Box::into_raw(Box::new(tag));
                    unsafe {
                        if PostMessageW(Some(HWND(raw as _)), WM_APP_UPDATE, WPARAM(0), LPARAM(boxed as isize)).is_err() {
                            drop(Box::from_raw(boxed));
                        }
                    }
                }
            }
        });
    }
}

/// Asks to install (or update the installed copy) and does it. `offer` is
/// `None` from the menu (always offered, without "No volver a preguntar").
pub(super) fn offer_install(hwnd: HWND, offer: Option<Offer>) {
    const GO: i32 = 100;
    const NEVER: i32 = 101;
    let automatic = offer.is_some();
    let offer = offer.unwrap_or_else(|| match install::installed_version() {
        Some(v) if install::version_key(&v) < install::version_key(install::VERSION) => Offer::Update { installed: v },
        _ => Offer::Install,
    });
    let mut buttons = Vec::new();
    let (choice, updates) = match &offer {
        Offer::Install => {
            buttons.push((GO, t::INSTALL_BUTTON));
            buttons.push((IDCANCEL.0, t::NOT_NOW));
            if automatic {
                buttons.push((NEVER, t::NEVER_ASK));
            }
            dialogs::choose_check(hwnd, t::INSTALL_TITLE, t::INSTALL_MAIN, t::INSTALL_TEXT, &buttons, t::CHECK_UPDATES_BOX, true)
        }
        Offer::Update { installed } => {
            buttons.push((GO, t::UPDATE_INSTALLED_BUTTON));
            buttons.push((IDCANCEL.0, t::NOT_NOW));
            if automatic {
                buttons.push((NEVER, t::NEVER_ASK));
            }
            let main = strings::update_installed_main(installed);
            (dialogs::choose_info(hwnd, t::INSTALL_TITLE, &main, t::UPDATE_INSTALLED_TEXT, &buttons), update::enabled())
        }
    };
    match choice {
        GO => {}
        NEVER => {
            install::never_offer();
            return;
        }
        _ => return,
    }
    ensure_com();
    match install::install() {
        Ok(exe) => {
            update::set_enabled(updates);
            let dir = exe.parent().map(|d| d.display().to_string()).unwrap_or_default();
            dialogs::message(hwnd, t::INSTALL_TITLE, &strings::installed_done(&dir), MB_ICONINFORMATION);
            if install::is_installed_copy() {
                return;
            }
            // Continue in the installed copy, showing the same thing.
            let mut cmd = std::process::Command::new(&exe);
            if let Some(target) = current_target() {
                cmd.arg(target);
            }
            if cmd.spawn().is_ok() {
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
        }
        Err(e) => dialogs::message(hwnd, t::INSTALL_TITLE, &format!("{} {e}", t::INSTALL_FAILED), MB_ICONERROR),
    }
}

/// Ayuda › Buscar actualizaciones.
pub(super) fn manual_check(hwnd: HWND) {
    let (result, _) = crate::task::run(Some(hwnd), t::PROGRESS_CHECKING, "GitHub", |_| Ok(update::latest()));
    match result {
        Ok(Ok(tag)) if update::is_newer(&tag) => show_new_version(hwnd, &tag),
        Ok(Ok(_)) => dialogs::message(hwnd, t::NEW_VERSION_TITLE, &strings::up_to_date(), MB_ICONINFORMATION),
        Ok(Err(CheckError::NotFound)) => dialogs::message(hwnd, t::NEW_VERSION_TITLE, t::CHECK_NOT_FOUND, MB_ICONINFORMATION),
        Ok(Err(CheckError::Failed(e))) => {
            dialogs::message(hwnd, t::NEW_VERSION_TITLE, &format!("{} {e}", t::CHECK_FAILED), MB_ICONWARNING)
        }
        Err(_) => {}
    }
    update::mark_checked();
}

/// "Hay una versión nueva": open its page, later, or skip it.
pub(super) fn show_new_version(hwnd: HWND, tag: &str) {
    const OPEN: i32 = 100;
    const SKIP: i32 = 101;
    let choice = dialogs::choose_info(
        hwnd,
        t::NEW_VERSION_TITLE,
        &strings::new_version_main(tag),
        &strings::new_version_text(),
        &[(OPEN, t::NEW_VERSION_BUTTON), (IDCANCEL.0, t::NOT_NOW), (SKIP, t::SKIP_VERSION_BUTTON)],
    );
    match choice {
        OPEN => open_url(hwnd, &update::release_page(tag)),
        SKIP => update::skip(tag),
        _ => {}
    }
}

fn open_url(hwnd: HWND, url: &str) {
    ensure_com();
    let file = HSTRING::from(url);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        hwnd,
        lpVerb: w!("open"),
        lpFile: PCWSTR(file.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    let _ = unsafe { ShellExecuteExW(&mut info) };
}
