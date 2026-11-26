//! Kibotos for macOS: an archive manager built on `kibotos-core`, with an
//! eframe/egui window, the native menu bar (muda), native file dialogs
//! (rfd), drag and drop with Finder, and Finder's "open file" events.
//!
//! Everything is macOS-only; on other systems this binary is a stub so that
//! `cargo build` at the workspace root works everywhere.

#[cfg(target_os = "macos")]
mod app;
#[cfg(target_os = "macos")]
mod finder;
#[cfg(target_os = "macos")]
mod format;
#[cfg(target_os = "macos")]
mod jobs;
#[cfg(target_os = "macos")]
mod menu;

#[cfg(target_os = "macos")]
fn main() -> eframe::Result {
    finder::install_early();
    app::run()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("kibotos-mac is the macOS version of Kibotos; on Windows build kibotos-app.");
    std::process::exit(1);
}
