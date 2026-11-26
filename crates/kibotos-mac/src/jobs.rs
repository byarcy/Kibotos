//! Long operations run on a worker thread: opening an archive, extracting,
//! creating. The UI keeps painting, shows progress from the shared
//! `Control` and polls for the result each frame.
//!
//! When an extraction finds an existing file, the worker asks the UI
//! through a channel and waits for the answer.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use eframe::egui;
use kibotos_core::view::Ordering;
use kibotos_core::{ArchiveIndex, ArchiveReader, Conflict, Control, CreateReport, Entry, ExtractReport, Result};

/// An opened archive.
pub struct Loaded {
    pub path: PathBuf,
    pub password: Option<String>,
    pub reader: Box<dyn ArchiveReader>,
    pub index: ArchiveIndex,
    pub names: Ordering,
}

pub enum Outcome {
    Opened(Loaded),
    /// Extracted; `open` is a file to open afterwards (double click).
    Extracted { report: ExtractReport, dest: PathBuf, open: Option<PathBuf> },
    Created { report: CreateReport, output: PathBuf },
}

/// "This file already exists": the worker waits on `reply`.
pub struct ConflictQuestion {
    pub path: PathBuf,
    pub reply: Sender<Conflict>,
}

pub struct Job {
    pub title: String,
    pub ctl: Arc<Control>,
    result: Receiver<Result<Outcome>>,
    questions: Receiver<ConflictQuestion>,
}

/// Lets a worker ask the UI about an existing file.
#[derive(Clone)]
pub struct Asker {
    tx: Sender<ConflictQuestion>,
    ctx: egui::Context,
}

impl Asker {
    pub fn ask(&self, path: &Path, _entry: &Entry) -> Conflict {
        let (reply, answer) = mpsc::channel();
        if self.tx.send(ConflictQuestion { path: path.to_owned(), reply }).is_err() {
            return Conflict::Cancel;
        }
        self.ctx.request_repaint();
        answer.recv().unwrap_or(Conflict::Cancel)
    }
}

impl Job {
    pub fn spawn<F>(ctx: &egui::Context, title: String, work: F) -> Job
    where
        F: FnOnce(&Control, Asker) -> Result<Outcome> + Send + 'static,
    {
        let ctl = Arc::new(Control::new());
        let (tx, result) = mpsc::channel();
        let (qtx, questions) = mpsc::channel();
        let asker = Asker { tx: qtx, ctx: ctx.clone() };
        let (ctl2, ctx2) = (ctl.clone(), ctx.clone());
        std::thread::spawn(move || {
            let r = work(&ctl2, asker);
            let _ = tx.send(r);
            ctx2.request_repaint();
        });
        Job { title, ctl, result, questions }
    }

    /// The result, once the worker has finished.
    pub fn poll(&self) -> Option<Result<Outcome>> {
        self.result.try_recv().ok()
    }

    pub fn question(&self) -> Option<ConflictQuestion> {
        self.questions.try_recv().ok()
    }
}

/// Opens and indexes an archive (worker side).
pub fn open(path: &Path, password: Option<String>) -> Result<Outcome> {
    let reader = kibotos_core::open(path, &kibotos_core::OpenOptions { password: password.clone() })?;
    let index = ArchiveIndex::build(reader.entries());
    let names = Ordering::by_name(&index);
    Ok(Outcome::Opened(Loaded { path: path.to_owned(), password, reader, index, names }))
}
