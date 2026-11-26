//! The main window: a toolbar, the folder tree of the open archive, the
//! file list, and the dialogs (password, create, progress, results).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::time::Duration;

use eframe::egui::{self, Align, Key, Layout, Modal, Sense};
use egui_extras::{Column, TableBuilder};
use kibotos_core::view::{Ordering, Sort, SortKey};
use kibotos_core::zone::{self, Zone, ZonePolicy};
use kibotos_core::{
    ArchiveIndex, Codec, Conflict, Control, CreateFormat, CreateOptions, DiskSink, EntryId, Error, Level, NodeId,
    OpenOptions, Overwrite,
};

use crate::jobs::{self, ConflictQuestion, Job, Loaded, Outcome};
use crate::menu::{AppMenu, Command};
use crate::{finder, format, menu};

/// Shown under the cursor while dragging files out to Finder.
const DRAG_IMAGE: &[u8] = include_bytes!("../assets/drag.png");
const HELP_URL: &str = "https://github.com/byarcy/Kibotos#macos";

const FORMATS: [(&str, CreateFormat); 7] = [
    ("ZIP", CreateFormat::Zip),
    ("7z", CreateFormat::SevenZip),
    ("tar.gz", CreateFormat::Tar(Some(Codec::Gzip))),
    ("tar.xz", CreateFormat::Tar(Some(Codec::Xz))),
    ("tar.zst", CreateFormat::Tar(Some(Codec::Zstd))),
    ("tar.bz2", CreateFormat::Tar(Some(Codec::Bzip2))),
    ("tar", CreateFormat::Tar(None)),
];

const LEVELS: [(&str, Level); 4] =
    [("Store (no compression)", Level::Store), ("Fast", Level::Fast), ("Normal", Level::Normal), ("Maximum", Level::Maximum)];

pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Kibotos")
            .with_inner_size([980.0, 640.0])
            .with_min_inner_size([620.0, 380.0])
            .with_drag_and_drop(true)
            // An empty icon: eframe then leaves the app bundle's icon in the
            // Dock instead of replacing it with its own.
            .with_icon(std::sync::Arc::new(egui::IconData::default())),
        ..Default::default()
    };
    eframe::run_native("Kibotos", options, Box::new(|cc| Ok(Box::new(KibotosApp::new(cc)))))
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

/// What an extraction does.
#[derive(Clone)]
struct ExtractRequest {
    archive: PathBuf,
    /// `None` extracts everything.
    ids: Option<Vec<EntryId>>,
    /// Archive folders stripped from paths (see `DiskSink::bases`).
    bases: Vec<String>,
    dest: PathBuf,
    /// A file to open when done (double click on a file).
    open: Option<PathBuf>,
    policy: ZonePolicy,
}

/// What to do again once the user has typed a password.
#[derive(Clone)]
enum Retry {
    Open(PathBuf),
    Extract(ExtractRequest),
    None,
}

struct CreateForm {
    inputs: Vec<PathBuf>,
    format: usize,
    level: usize,
    password: String,
    encrypt_names: bool,
}

impl CreateForm {
    fn new(inputs: Vec<PathBuf>) -> CreateForm {
        CreateForm { inputs, format: 0, level: 2, password: String::new(), encrypt_names: true }
    }
}

enum Dialog {
    Password { retry: Retry, name: String, text: String, wrong: bool, focused: bool },
    Message { title: String, text: String, reveal: Option<PathBuf> },
    Create(CreateForm),
}

pub struct KibotosApp {
    menu: AppMenu,
    archive: Option<Loaded>,
    folder: NodeId,
    ord: Option<Ordering>,
    sort: Sort,
    rows: Vec<NodeId>,
    selected: BTreeSet<NodeId>,
    anchor: Option<usize>,
    filter: String,
    focus_filter: bool,
    job: Option<(Job, Retry)>,
    question: Option<ConflictQuestion>,
    dialog: Option<Dialog>,
    status: String,
    title: String,
    temp_root: PathBuf,
    temp_next: u32,
}

impl KibotosApp {
    fn new(cc: &eframe::CreationContext<'_>) -> KibotosApp {
        finder::install(&cc.egui_ctx);
        let menu = AppMenu::install(&cc.egui_ctx);
        let mut app = KibotosApp {
            menu,
            archive: None,
            folder: ArchiveIndex::ROOT,
            ord: None,
            sort: Sort::default(),
            rows: Vec::new(),
            selected: BTreeSet::new(),
            anchor: None,
            filter: String::new(),
            focus_filter: false,
            job: None,
            question: None,
            dialog: None,
            status: String::new(),
            title: String::new(),
            temp_root: std::env::temp_dir().join("Kibotos").join(std::process::id().to_string()),
            temp_next: 0,
        };
        // Started from a terminal with a file (Finder uses Apple events).
        if let Some(path) = std::env::args_os().skip(1).map(PathBuf::from).find(|p| !p.to_string_lossy().starts_with("-psn")) {
            app.open(&cc.egui_ctx, path, None);
        }
        app
    }

    // ------------------------------------------------------------ actions

    fn open(&mut self, ctx: &egui::Context, path: PathBuf, password: Option<String>) {
        if self.job.is_some() {
            return;
        }
        let p = path.clone();
        let job = Job::spawn(ctx, format!("Opening {}", file_name(&path)), move |_, _| jobs::open(&p, password));
        self.job = Some((job, Retry::Open(path)));
    }

    fn extract(&mut self, ctx: &egui::Context, req: ExtractRequest, password: Option<String>) {
        if self.job.is_some() {
            return;
        }
        let title = match &req.open {
            Some(f) => format!("Opening {}", file_name(f)),
            None => format!("Extracting {}", file_name(&req.archive)),
        };
        let r = req.clone();
        let job = Job::spawn(ctx, title, move |ctl, asker| {
            let mut reader = kibotos_core::open(&r.archive, &OpenOptions { password })?;
            let mark = zone::read(&r.archive).map(|data| Zone { data, policy: r.policy });
            let mut sink = DiskSink::new(&r.dest)
                .overwrite(if r.open.is_some() { Overwrite::Always } else { Overwrite::Ask })
                .on_conflict(Box::new(move |path: &Path, entry: &kibotos_core::Entry| asker.ask(path, entry)))
                .zone(mark)
                .bases(r.bases.clone());
            match &r.ids {
                Some(ids) => reader.extract(ids, &mut sink, ctl)?,
                None => reader.extract_all(&mut sink, ctl)?,
            }
            let report = sink.close()?;
            Ok(Outcome::Extracted { report, dest: r.dest, open: r.open })
        });
        self.job = Some((job, Retry::Extract(req)));
    }

    fn create(&mut self, ctx: &egui::Context, form: &CreateForm) {
        let format = FORMATS[form.format].1;
        let first = &form.inputs[0];
        let dir = first.parent().map(Path::to_path_buf).unwrap_or_default();
        let base = if form.inputs.len() == 1 {
            let n = file_name(first);
            if first.is_dir() { n } else { first.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or(n) }
        } else {
            dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Archive".into())
        };
        let Some(output) = rfd::FileDialog::new()
            .set_title("Save Archive")
            .set_directory(&dir)
            .set_file_name(format!("{base}.{}", format.extension()))
            .save_file()
        else {
            return;
        };
        let password = (format.supports_password() && !form.password.is_empty()).then(|| form.password.clone());
        let options = CreateOptions {
            format,
            level: LEVELS[form.level].1,
            encrypt_names: form.encrypt_names,
            password,
            ..Default::default()
        };
        let inputs = form.inputs.clone();
        let out = output.clone();
        let job = Job::spawn(ctx, format!("Compressing {}", file_name(&output)), move |ctl, _| {
            let report = kibotos_core::create(&out, &inputs, &options, ctl)?;
            Ok(Outcome::Created { report, output: out })
        });
        self.job = Some((job, Retry::None));
    }

    fn finish(&mut self, ctx: &egui::Context, retry: Retry, result: kibotos_core::Result<Outcome>) {
        match result {
            Ok(Outcome::Opened(loaded)) => {
                self.status = format!("{} · {} items", loaded.reader.format().name(), loaded.reader.entries().len());
                self.archive = Some(loaded);
                self.ord = None;
                self.filter.clear();
                self.go(ctx, ArchiveIndex::ROOT);
            }
            Ok(Outcome::Extracted { report, dest, open }) => {
                if let Some(file) = open {
                    let _ = Process::new("/usr/bin/open").arg(&file).spawn();
                    return;
                }
                let blocked = report.blocked().count();
                let mut text = format!("{} files extracted to “{}”.", report.files, dest.display());
                if blocked > 0 {
                    text.push_str(&format!(
                        "\n\n{blocked} items were not extracted because their paths could write outside the destination or they are links."
                    ));
                }
                self.status = format!("Extracted {} files", report.files);
                self.dialog = Some(Dialog::Message { title: "Extraction finished".into(), text, reveal: Some(dest) });
            }
            Ok(Outcome::Created { report, output }) => {
                let mut text = format!(
                    "“{}”: {} files, {} → {}.",
                    file_name(&output),
                    report.files,
                    format::size(Some(report.bytes_in)),
                    format::size(Some(report.bytes_out))
                );
                if !report.skipped.is_empty() {
                    text.push_str(&format!("\n\n{} items could not be added (links or unreadable files).", report.skipped.len()));
                }
                self.dialog = Some(Dialog::Message { title: "Archive created".into(), text, reveal: Some(output) });
            }
            Err(Error::Cancelled) => self.status = "Cancelled".into(),
            Err(e @ (Error::PasswordRequired | Error::WrongPassword)) => {
                let name = match &retry {
                    Retry::Open(p) => file_name(p),
                    Retry::Extract(r) => file_name(&r.archive),
                    Retry::None => String::new(),
                };
                let wrong = matches!(e, Error::WrongPassword);
                self.dialog = Some(Dialog::Password { retry, name, text: String::new(), wrong, focused: false });
            }
            Err(e) => {
                let what = match &retry {
                    Retry::Open(p) => format!("Could not open “{}”", file_name(p)),
                    Retry::Extract(r) => format!("Could not extract from “{}”", file_name(&r.archive)),
                    Retry::None => "Could not create the archive".into(),
                };
                self.dialog = Some(Dialog::Message { title: what, text: e.to_string(), reveal: None });
            }
        }
    }

    // --------------------------------------------------------- the list

    fn refresh(&mut self) {
        self.rows.clear();
        let Some(a) = &self.archive else { return };
        let ord = self.ord.get_or_insert_with(|| Ordering::new(&a.index, a.reader.entries(), self.sort));
        let query = self.filter.trim().to_lowercase();
        if query.is_empty() {
            ord.folder_rows(&a.index, self.folder, &mut self.rows);
        } else {
            ord.filter_rows(&a.index, &query, &mut self.rows);
        }
        let rows: BTreeSet<NodeId> = self.rows.iter().copied().collect();
        self.selected.retain(|n| rows.contains(n));
        self.anchor = None;
    }

    /// Shows `folder`, and opens its branch in the tree.
    fn go(&mut self, ctx: &egui::Context, folder: NodeId) {
        self.folder = folder;
        self.filter.clear();
        self.selected.clear();
        if let Some(a) = &self.archive {
            let mut n = Some(folder);
            while let Some(node) = n {
                let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ctx, tree_id(node), false);
                state.set_open(true);
                state.store(ctx);
                n = a.index.parent(node);
            }
        }
        self.refresh();
    }

    fn go_up(&mut self, ctx: &egui::Context) {
        if let Some(parent) = self.archive.as_ref().and_then(|a| a.index.parent(self.folder)) {
            self.go(ctx, parent);
        }
    }

    /// The selected entries, and the folders to strip so they land by name.
    fn selection(&self) -> Option<(Vec<EntryId>, Vec<String>)> {
        let a = self.archive.as_ref()?;
        let mut ids = Vec::new();
        let mut bases = Vec::new();
        for &n in &self.selected {
            a.index.entries_under(n, &mut ids);
            let base = a.index.parent(n).map(|p| a.index.path(p)).unwrap_or_default();
            if !bases.contains(&base) {
                bases.push(base);
            }
        }
        ids.sort_unstable();
        ids.dedup();
        (!ids.is_empty()).then_some((ids, bases))
    }

    fn new_temp_dir(&mut self) -> std::io::Result<PathBuf> {
        self.temp_next += 1;
        let dir = self.temp_root.join(self.temp_next.to_string());
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Double click on a file: extract it to a temporary folder and open it.
    fn open_entry(&mut self, ctx: &egui::Context, node: NodeId) {
        let Some((path, id, name, base, password)) = self.archive.as_ref().and_then(|a| {
            let id = a.index.entry(node)?;
            let base = a.index.parent(node).map(|p| a.index.path(p)).unwrap_or_default();
            Some((a.path.clone(), id, a.index.name(node).to_owned(), base, a.password.clone()))
        }) else {
            return;
        };
        let Ok(dir) = self.new_temp_dir() else { return };
        let req = ExtractRequest {
            archive: path,
            ids: Some(vec![id]),
            bases: vec![base],
            dest: dir.clone(),
            open: Some(dir.join(name)),
            policy: ZonePolicy::All,
        };
        self.extract(ctx, req, password);
    }

    fn activate(&mut self, ctx: &egui::Context, node: NodeId) {
        let is_dir = self.archive.as_ref().is_some_and(|a| a.index.is_dir(node));
        if is_dir { self.go(ctx, node) } else { self.open_entry(ctx, node) }
    }

    /// Dragging rows out: the selection is extracted to a temporary folder
    /// (on this thread, so the drag starts while the button is still down)
    /// and those files are handed to the drag.
    fn drag_out(&mut self, frame: &eframe::Frame) {
        let Some((ids, bases)) = self.selection() else { return };
        let Ok(dir) = self.new_temp_dir() else { return };
        let Some(a) = self.archive.as_mut() else { return };
        let mark = zone::read(&a.path).map(|data| Zone { data, policy: ZonePolicy::All });
        let mut sink = DiskSink::new(&dir).overwrite(Overwrite::Always).zone(mark).bases(bases);
        let ctl = Control::new();
        if let Err(e) = a.reader.extract(&ids, &mut sink, &ctl) {
            self.status = match e {
                Error::PasswordRequired | Error::WrongPassword => "Extract once with the password before dragging".into(),
                e => e.to_string(),
            };
            return;
        }
        if sink.close().is_err() {
            return;
        }
        let paths: Vec<PathBuf> =
            self.selected.iter().map(|&n| dir.join(a.index.name(n))).filter(|p| p.exists()).collect();
        if paths.is_empty() {
            return;
        }
        let image = drag::Image::Raw(DRAG_IMAGE.to_vec());
        if let Err(e) = drag::start_drag(frame, drag::DragItem::Files(paths), image, |_, _| {}, drag::Options::default()) {
            self.status = format!("Drag failed: {e}");
        }
    }

    // ------------------------------------------------------------ commands

    fn command(&mut self, ctx: &egui::Context, c: Command) {
        if self.job.is_some() || self.dialog.is_some() {
            return;
        }
        match c {
            Command::Open => {
                if let Some(p) = rfd::FileDialog::new().set_title("Open Archive").pick_file() {
                    self.open(ctx, p, None);
                }
            }
            Command::NewArchive => {
                if let Some(inputs) = rfd::FileDialog::new().set_title("Choose Files and Folders to Compress").pick_files_or_folders() {
                    if !inputs.is_empty() {
                        self.dialog = Some(Dialog::Create(CreateForm::new(inputs)));
                    }
                }
            }
            Command::ExtractAll | Command::ExtractSelected => {
                let Some(a) = &self.archive else { return };
                let (ids, bases) = if c == Command::ExtractAll {
                    (None, Vec::new())
                } else {
                    match self.selection() {
                        Some((ids, bases)) => (Some(ids), bases),
                        None => return,
                    }
                };
                let start = a.path.parent().map(Path::to_path_buf).unwrap_or_default();
                let Some(dest) = rfd::FileDialog::new().set_title("Extract To").set_directory(start).pick_folder() else {
                    return;
                };
                let req = ExtractRequest {
                    archive: a.path.clone(),
                    ids,
                    bases,
                    dest,
                    open: None,
                    policy: ZonePolicy::Risky,
                };
                let password = a.password.clone();
                self.extract(ctx, req, password);
            }
            Command::Close => {
                self.archive = None;
                self.ord = None;
                self.rows.clear();
                self.selected.clear();
                self.filter.clear();
                self.status.clear();
            }
            Command::SelectAll => {
                self.selected = self.rows.iter().copied().collect();
            }
            Command::Find => self.focus_filter = true,
            Command::GoUp => self.go_up(ctx),
            Command::Help => {
                let _ = Process::new("/usr/bin/open").arg(HELP_URL).spawn();
            }
        }
    }

    // ---------------------------------------------------------------- UI

    fn toolbar(&mut self, ui: &mut egui::Ui, commands: &mut Vec<Command>) {
        let has_archive = self.archive.is_some();
        ui.horizontal(|ui| {
            if ui.button("Open…").clicked() {
                commands.push(Command::Open);
            }
            if ui.button("New Archive…").clicked() {
                commands.push(Command::NewArchive);
            }
            if ui.add_enabled(has_archive, egui::Button::new("Extract All…")).clicked() {
                commands.push(Command::ExtractAll);
            }
            if ui.add_enabled(!self.selected.is_empty(), egui::Button::new("Extract Selected…")).clicked() {
                commands.push(Command::ExtractSelected);
            }
            ui.separator();
            let can_up = self.archive.as_ref().is_some_and(|a| a.index.parent(self.folder).is_some());
            if ui.add_enabled(can_up, egui::Button::new("⬆")).on_hover_text("Enclosing folder (⌘↑)").clicked() {
                commands.push(Command::GoUp);
            }
            if let Some(a) = &self.archive {
                let inner = a.index.path(self.folder);
                let shown = if inner.is_empty() { file_name(&a.path) } else { format!("{} › {}", file_name(&a.path), inner.replace('/', " › ")) };
                ui.label(shown);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let edit = egui::TextEdit::singleline(&mut self.filter).hint_text("Search in archive").desired_width(200.0);
                let r = ui.add_enabled(has_archive, edit);
                if self.focus_filter {
                    r.request_focus();
                    self.focus_filter = false;
                }
                if r.changed() {
                    self.selected.clear();
                    self.refresh();
                }
            });
        });
    }

    fn tree(&self, ui: &mut egui::Ui, goto: &mut Option<NodeId>) {
        let Some(a) = &self.archive else {
            ui.weak("No archive open");
            return;
        };
        egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
            tree_node(ui, a, ArchiveIndex::ROOT, &file_name(&a.path), self.folder, goto);
        });
    }

    fn list(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, frame: &eframe::Frame) {
        let Some(a) = &self.archive else {
            ui.centered_and_justified(|ui| {
                ui.label("Open an archive (⌘O) or drop one here.\nDrop files or folders here to compress them.");
            });
            return;
        };
        let filtering = !self.filter.trim().is_empty();
        let entries = a.reader.entries();
        let mut clicked: Option<usize> = None;
        let mut activated: Option<NodeId> = None;
        let mut dragged: Option<usize> = None;
        let mut sort_by: Option<SortKey> = None;
        let columns = [("Name", SortKey::Name), ("Size", SortKey::Size), ("Packed", SortKey::Packed), ("Modified", SortKey::Modified), ("CRC32", SortKey::Crc)];
        TableBuilder::new(ui)
            .striped(true)
            .sense(Sense::click_and_drag())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::remainder().at_least(200.0).clip(true))
            .column(Column::exact(90.0))
            .column(Column::exact(90.0))
            .column(Column::exact(130.0))
            .column(Column::exact(80.0))
            .header(22.0, |mut header| {
                for (title, key) in columns {
                    header.col(|ui| {
                        let arrow = if self.sort.key == key { if self.sort.descending { " ⏷" } else { " ⏶" } } else { "" };
                        if ui.add(egui::Label::new(egui::RichText::new(format!("{title}{arrow}")).strong()).sense(Sense::click())).clicked() {
                            sort_by = Some(key);
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, self.rows.len(), |mut row| {
                    let i = row.index();
                    let node = self.rows[i];
                    let is_dir = a.index.is_dir(node);
                    let entry = a.index.entry(node).map(|id| &entries[id as usize]);
                    row.set_selected(self.selected.contains(&node));
                    row.col(|ui| {
                        let name = if filtering { a.index.path(node) } else { a.index.name(node).to_owned() };
                        let lock = if entry.is_some_and(|e| e.encrypted) { " 🔒" } else { "" };
                        ui.label(format!("{} {name}{lock}", if is_dir { "📁" } else { "📄" }));
                    });
                    row.col(|ui| {
                        if !is_dir {
                            ui.label(format::size(entry.and_then(|e| e.size)));
                        }
                    });
                    row.col(|ui| {
                        if !is_dir {
                            ui.label(format::size(entry.and_then(|e| e.packed)));
                        }
                    });
                    row.col(|ui| {
                        ui.label(format::date(entry.and_then(|e| e.modified)));
                    });
                    row.col(|ui| {
                        if let Some(c) = entry.and_then(|e| e.crc).filter(|_| !is_dir) {
                            ui.monospace(format!("{c:08X}"));
                        }
                    });
                    let r = row.response();
                    if r.double_clicked() {
                        activated = Some(node);
                    } else if r.clicked() {
                        clicked = Some(i);
                    }
                    if r.drag_started() {
                        dragged = Some(i);
                    }
                });
            });

        if let Some(key) = sort_by {
            self.sort = if self.sort.key == key { Sort { key, descending: !self.sort.descending } } else { Sort { key, descending: false } };
            self.ord = None;
            self.refresh();
        }
        let mods = ctx.input(|i| i.modifiers);
        if let Some(i) = clicked {
            let node = self.rows[i];
            if mods.command {
                if !self.selected.remove(&node) {
                    self.selected.insert(node);
                }
                self.anchor = Some(i);
            } else if mods.shift && self.anchor.is_some() {
                let a = self.anchor.unwrap_or(i);
                let (lo, hi) = (a.min(i), a.max(i));
                self.selected = self.rows[lo..=hi].iter().copied().collect();
            } else {
                self.selected = [node].into_iter().collect();
                self.anchor = Some(i);
            }
        }
        if let Some(node) = activated {
            self.activate(ctx, node);
        }
        if let Some(i) = dragged {
            let node = self.rows[i];
            if !self.selected.contains(&node) {
                self.selected = [node].into_iter().collect();
            }
            self.drag_out(frame);
        }
    }

    fn keys(&mut self, ctx: &egui::Context, commands: &mut Vec<Command>) {
        if ctx.wants_keyboard_input() || self.dialog.is_some() || self.job.is_some() {
            return;
        }
        let (select_all, enter, back, escape) = ctx.input(|i| {
            (
                i.modifiers.command && i.key_pressed(Key::A),
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::Backspace),
                i.key_pressed(Key::Escape),
            )
        });
        if select_all {
            commands.push(Command::SelectAll);
        }
        if back {
            commands.push(Command::GoUp);
        }
        if escape && !self.filter.is_empty() {
            self.filter.clear();
            self.refresh();
        }
        if enter && self.selected.len() == 1 {
            if let Some(&node) = self.selected.iter().next() {
                self.activate(ctx, node);
            }
        }
    }

    /// Files dropped from Finder: an archive is opened; anything else is
    /// offered for compression.
    fn dropped(&mut self, ctx: &egui::Context) {
        let paths: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        // Our own drags out of the list come from the temporary folder.
        let paths: Vec<PathBuf> = paths.into_iter().filter(|p| !p.starts_with(&self.temp_root)).collect();
        if paths.is_empty() || self.job.is_some() || self.dialog.is_some() {
            return;
        }
        if paths.len() == 1 && paths[0].is_file() && kibotos_core::detect_path(&paths[0]).is_ok() {
            self.open(ctx, paths[0].clone(), None);
        } else {
            self.dialog = Some(Dialog::Create(CreateForm::new(paths)));
        }
    }

    fn progress_window(&mut self, ctx: &egui::Context) {
        let Some((job, _)) = &self.job else { return };
        if self.question.is_none() {
            self.question = job.question();
        }
        if let Some(q) = &self.question {
            let mut answer = None;
            Modal::new(egui::Id::new("conflict")).show(ctx, |ui| {
                ui.set_width(420.0);
                ui.heading("File already exists");
                ui.label(format!("“{}” already exists. Replace it?", q.path.display()));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    for (label, c) in [
                        ("Replace", Conflict::Replace),
                        ("Skip", Conflict::Skip),
                        ("Replace All", Conflict::ReplaceAll),
                        ("Skip All", Conflict::SkipAll),
                        ("Stop", Conflict::Cancel),
                    ] {
                        if ui.button(label).clicked() {
                            answer = Some(c);
                        }
                    }
                });
            });
            if let Some(c) = answer {
                if let Some(q) = self.question.take() {
                    let _ = q.reply.send(c);
                }
            }
            return;
        }
        let p = job.ctl.progress();
        let fraction = if p.bytes_total > 0 {
            p.bytes_done as f32 / p.bytes_total as f32
        } else if p.entries_total > 0 {
            p.entries_done as f32 / p.entries_total as f32
        } else {
            0.0
        };
        let mut cancel = false;
        Modal::new(egui::Id::new("progress")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.label(egui::RichText::new(&job.title).strong());
            ui.add_space(6.0);
            ui.add(egui::ProgressBar::new(fraction.clamp(0.0, 1.0)).show_percentage());
            if p.entries_total > 0 {
                ui.label(format!(
                    "{} of {} items · {} of {}",
                    p.entries_done,
                    p.entries_total,
                    format::size(Some(p.bytes_done)),
                    format::size(Some(p.bytes_total))
                ));
            }
            ui.add_space(6.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
        if cancel {
            job.ctl.cancel();
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.dialog else { return };
        enum Action {
            None,
            Close,
            Password(Retry, String),
            Create,
        }
        let mut action = Action::None;
        match dialog {
            Dialog::Message { title, text, reveal } => {
                Modal::new(egui::Id::new("message")).show(ctx, |ui| {
                    ui.set_width(420.0);
                    ui.heading(title.as_str());
                    ui.add_space(4.0);
                    ui.label(text.as_str());
                    ui.add_space(8.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("OK").clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                            action = Action::Close;
                        }
                        if let Some(path) = reveal {
                            if ui.button("Show in Finder").clicked() {
                                let _ = Process::new("/usr/bin/open").arg("-R").arg(&*path).spawn();
                                action = Action::Close;
                            }
                        }
                    });
                });
            }
            Dialog::Password { retry, name, text, wrong, focused } => {
                Modal::new(egui::Id::new("password")).show(ctx, |ui| {
                    ui.set_width(360.0);
                    ui.heading("Password required");
                    ui.label(if *wrong { format!("Wrong password for “{name}”. Try again:") } else { format!("“{name}” is encrypted. Enter its password:") });
                    let r = ui.add(egui::TextEdit::singleline(text).password(true).desired_width(f32::INFINITY));
                    if !*focused {
                        r.request_focus();
                        *focused = true;
                    }
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    ui.add_space(8.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("OK").clicked() || enter {
                            action = Action::Password(retry.clone(), text.clone());
                        }
                        if ui.button("Cancel").clicked() {
                            action = Action::Close;
                        }
                    });
                });
            }
            Dialog::Create(form) => {
                Modal::new(egui::Id::new("create")).show(ctx, |ui| {
                    ui.set_width(460.0);
                    ui.heading("New Archive");
                    ui.add_space(4.0);
                    let mut remove = None;
                    egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                        for (i, p) in form.inputs.iter().enumerate() {
                            ui.horizontal(|ui| {
                                if ui.small_button("✕").on_hover_text("Remove").clicked() {
                                    remove = Some(i);
                                }
                                ui.label(format!("{} {}", if p.is_dir() { "📁" } else { "📄" }, file_name(p)));
                            });
                        }
                    });
                    if let Some(i) = remove {
                        form.inputs.remove(i);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Add Files…").clicked() {
                            if let Some(more) = rfd::FileDialog::new().pick_files() {
                                form.inputs.extend(more);
                            }
                        }
                        if ui.button("Add Folders…").clicked() {
                            if let Some(more) = rfd::FileDialog::new().pick_folders() {
                                form.inputs.extend(more);
                            }
                        }
                    });
                    ui.add_space(6.0);
                    egui::Grid::new("create-options").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                        ui.label("Format:");
                        egui::ComboBox::from_id_salt("format").selected_text(FORMATS[form.format].0).show_ui(ui, |ui| {
                            for (i, (name, _)) in FORMATS.iter().enumerate() {
                                ui.selectable_value(&mut form.format, i, *name);
                            }
                        });
                        ui.end_row();
                        ui.label("Compression:");
                        egui::ComboBox::from_id_salt("level").selected_text(LEVELS[form.level].0).show_ui(ui, |ui| {
                            for (i, (name, _)) in LEVELS.iter().enumerate() {
                                ui.selectable_value(&mut form.level, i, *name);
                            }
                        });
                        ui.end_row();
                        let format = FORMATS[form.format].1;
                        ui.label("Password:");
                        ui.add_enabled(
                            format.supports_password(),
                            egui::TextEdit::singleline(&mut form.password).password(true).hint_text("optional"),
                        );
                        ui.end_row();
                        if format == CreateFormat::SevenZip {
                            ui.label("");
                            ui.add_enabled(!form.password.is_empty(), egui::Checkbox::new(&mut form.encrypt_names, "Encrypt file names"));
                            ui.end_row();
                        }
                    });
                    ui.add_space(8.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add_enabled(!form.inputs.is_empty(), egui::Button::new("Create…")).clicked() {
                            action = Action::Create;
                        }
                        if ui.button("Cancel").clicked() {
                            action = Action::Close;
                        }
                    });
                });
            }
        }
        match action {
            Action::None => {}
            Action::Close => self.dialog = None,
            Action::Password(retry, text) => {
                self.dialog = None;
                let pw = Some(text);
                match retry {
                    Retry::Open(p) => self.open(ctx, p, pw),
                    Retry::Extract(req) => {
                        if let Some(a) = self.archive.as_mut().filter(|a| a.path == req.archive) {
                            a.password = pw.clone();
                            a.reader.set_password(pw.as_deref());
                        }
                        self.extract(ctx, req, pw);
                    }
                    Retry::None => {}
                }
            }
            Action::Create => {
                if let Some(Dialog::Create(form)) = self.dialog.take() {
                    self.create(ctx, &form);
                    if self.job.is_none() {
                        // The save dialog was cancelled: keep the form.
                        self.dialog = Some(Dialog::Create(form));
                    }
                }
            }
        }
    }
}

fn tree_id(node: NodeId) -> egui::Id {
    egui::Id::new(("kibotos-tree", node))
}

fn tree_node(ui: &mut egui::Ui, a: &Loaded, node: NodeId, label: &str, current: NodeId, goto: &mut Option<NodeId>) {
    let mut dirs: Vec<NodeId> = a.index.children(node).iter().copied().filter(|&c| a.index.is_dir(c)).collect();
    dirs.sort_by_key(|&c| a.names.rank(c));
    let text = format!("📁 {label}");
    if dirs.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(ui.spacing().indent);
            if ui.selectable_label(node == current, text).clicked() {
                *goto = Some(node);
            }
        });
        return;
    }
    egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), tree_id(node), node == ArchiveIndex::ROOT)
        .show_header(ui, |ui| {
            if ui.selectable_label(node == current, text).clicked() {
                *goto = Some(node);
            }
        })
        .body(|ui| {
            for d in dirs {
                tree_node(ui, a, d, a.index.name(d), current, goto);
            }
        });
}

impl eframe::App for KibotosApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Finished work.
        if let Some((job, _)) = &self.job {
            if let Some(result) = job.poll() {
                if let Some((_, retry)) = self.job.take() {
                    self.question = None;
                    self.finish(ctx, retry, result);
                }
            } else {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
        // Files from Finder (double click, Open With, Dock).
        if let Some(path) = finder::take().pop() {
            if self.job.is_none() {
                self.dialog = None;
                self.open(ctx, path, None);
            }
        }
        self.dropped(ctx);

        let mut commands = menu::take();
        self.keys(ctx, &mut commands);

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui, &mut commands);
            ui.add_space(4.0);
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let items = if self.archive.is_some() { format!("{} items", self.rows.len()) } else { String::new() };
                ui.label(items);
                if !self.selected.is_empty() {
                    ui.label(format!("· {} selected", self.selected.len()));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.weak(&self.status));
            });
        });
        let mut goto = None;
        egui::SidePanel::left("tree").resizable(true).default_width(220.0).show(ctx, |ui| self.tree(ui, &mut goto));
        egui::CentralPanel::default().show(ctx, |ui| self.list(ui, ctx, frame));
        if let Some(node) = goto {
            self.go(ctx, node);
        }

        // Feedback while files are dragged over the window.
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drop")));
            let rect = ctx.screen_rect();
            painter.rect_filled(rect, 0.0, egui::Color32::from_black_alpha(120));
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Drop an archive to open it, or files to compress them",
                egui::FontId::proportional(20.0),
                egui::Color32::WHITE,
            );
        }

        self.progress_window(ctx);
        self.dialogs(ctx);
        for c in commands {
            self.command(ctx, c);
        }

        let has_archive = self.archive.is_some();
        self.menu.update(has_archive, !self.selected.is_empty());
        let title = match &self.archive {
            Some(a) => format!("{} — Kibotos", file_name(&a.path)),
            None => "Kibotos".to_owned(),
        };
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }
}

impl Drop for KibotosApp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.temp_root);
    }
}
