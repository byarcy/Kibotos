//! The native menu bar: Kibotos (About, Hide, Quit), File, Edit, View,
//! Window and Help. Menu events are queued for the UI, which reads them on
//! its next frame.

use std::sync::{Arc, Mutex, OnceLock};

use eframe::egui;
use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{AboutMetadata, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

/// What a menu item asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Open,
    NewArchive,
    ExtractAll,
    ExtractSelected,
    Close,
    SelectAll,
    Find,
    GoUp,
    Help,
}

const COMMANDS: [(&str, Command); 9] = [
    ("open", Command::Open),
    ("new", Command::NewArchive),
    ("extract-all", Command::ExtractAll),
    ("extract-selected", Command::ExtractSelected),
    ("close", Command::Close),
    ("select-all", Command::SelectAll),
    ("find", Command::Find),
    ("go-up", Command::GoUp),
    ("help", Command::Help),
];

static QUEUE: Mutex<Vec<Command>> = Mutex::new(Vec::new());
static CTX: OnceLock<egui::Context> = OnceLock::new();

pub const UNRAR_NOTICE: &str = "RAR extraction uses the unRAR library by Alexander Roshal. \
    Its license forbids using it to create RAR archives, so Kibotos never does.";

/// Items whose availability changes with the open archive.
pub struct AppMenu {
    _menu: Menu,
    extract_all: MenuItem,
    extract_selected: MenuItem,
    close: MenuItem,
    state: (bool, bool),
}

fn item(id: &str, text: &str, mods: Option<Modifiers>, key: Option<Code>) -> MenuItem {
    let accel = match (mods, key) {
        (Some(m), Some(k)) => Some(Accelerator::new(m, k)),
        _ => None,
    };
    MenuItem::with_id(id, text, true, accel)
}

impl AppMenu {
    /// Builds the menu bar and makes it the app's. Call on the main thread
    /// after launch.
    pub fn install(ctx: &egui::Context) -> AppMenu {
        let _ = CTX.set(ctx.clone());
        let cmd = Some(Modifiers::SUPER);
        let cmd_shift = Some(Modifiers::SUPER | Modifiers::SHIFT);

        let about = PredefinedMenuItem::about(
            Some("About Kibotos"),
            Some(AboutMetadata {
                name: Some("Kibotos".into()),
                version: Some(env!("CARGO_PKG_VERSION").into()),
                copyright: Some("byarcy 2026".into()),
                comments: Some("A fast, lightweight archive manager.".into()),
                credits: Some(UNRAR_NOTICE.into()),
                ..Default::default()
            }),
        );
        let sep = || PredefinedMenuItem::separator();
        let app_menu = Submenu::with_items(
            "Kibotos",
            true,
            &[
                &about,
                &sep(),
                &PredefinedMenuItem::services(None),
                &sep(),
                &PredefinedMenuItem::hide(None),
                &PredefinedMenuItem::hide_others(None),
                &PredefinedMenuItem::show_all(None),
                &sep(),
                &PredefinedMenuItem::quit(None),
            ],
        )
        .expect("app menu");

        let open = item("open", "Open…", cmd, Some(Code::KeyO));
        let new = item("new", "New Archive…", cmd, Some(Code::KeyN));
        let extract_all = item("extract-all", "Extract All…", cmd, Some(Code::KeyE));
        let extract_selected = item("extract-selected", "Extract Selected…", cmd_shift, Some(Code::KeyE));
        let close = item("close", "Close Archive", cmd, Some(Code::KeyW));
        let file = Submenu::with_items(
            "File",
            true,
            &[&open, &new, &sep(), &extract_all, &extract_selected, &sep(), &close],
        )
        .expect("file menu");

        // ⌘A is left to the list and to text fields (a menu shortcut would
        // take it from both).
        let select_all = item("select-all", "Select All", None, None);
        let find = item("find", "Find…", cmd, Some(Code::KeyF));
        let edit = Submenu::with_items("Edit", true, &[&select_all, &find]).expect("edit menu");

        let go_up = item("go-up", "Enclosing Folder", cmd, Some(Code::ArrowUp));
        let view = Submenu::with_items("View", true, &[&go_up]).expect("view menu");

        let window = Submenu::with_items(
            "Window",
            true,
            &[&PredefinedMenuItem::minimize(None), &PredefinedMenuItem::fullscreen(None)],
        )
        .expect("window menu");

        let help_item = item("help", "Kibotos Help", None, None);
        let help = Submenu::with_items("Help", true, &[&help_item]).expect("help menu");

        let menu = Menu::new();
        let _ = menu.append_items(&[&app_menu, &file, &edit, &view, &window, &help]);
        menu.init_for_nsapp();
        window.set_as_windows_menu_for_nsapp();
        help.set_as_help_menu_for_nsapp();

        let handler: Arc<dyn Fn(MenuEvent) + Send + Sync> = Arc::new(|event: MenuEvent| {
            if let Some((_, c)) = COMMANDS.iter().find(|(id, _)| event.id == *id) {
                if let Ok(mut q) = QUEUE.lock() {
                    q.push(*c);
                }
                if let Some(ctx) = CTX.get() {
                    ctx.request_repaint();
                }
            }
        });
        MenuEvent::set_event_handler(Some(handler));

        AppMenu { _menu: menu, extract_all, extract_selected, close, state: (true, true) }
    }

    /// Enables the archive items when an archive is open, and "Extract
    /// Selected" when something is selected. Only touches AppKit on change.
    pub fn update(&mut self, has_archive: bool, has_selection: bool) {
        if self.state == (has_archive, has_selection) {
            return;
        }
        self.state = (has_archive, has_selection);
        self.extract_all.set_enabled(has_archive);
        self.close.set_enabled(has_archive);
        self.extract_selected.set_enabled(has_archive && has_selection);
    }
}

/// Commands chosen since the last call.
pub fn take() -> Vec<Command> {
    QUEUE.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
}
