//! Changing the open archive from the list: delete (Del), rename (F2 or a
//! slow second click), new folder (Ctrl+Shift+N), files dropped from
//! Explorer, files opened from the archive and saved by their program, and
//! the SHA-256 of the selection.
//!
//! Every change goes through [`apply`]: the archive's file handle is closed,
//! `kibotos_core::edit::update` writes the new archive on a worker thread
//! (with the progress window), and the archive is opened again at the same
//! folder.

use std::collections::{HashMap, HashSet};

use kibotos_core::Format;
use kibotos_core::edit::{Changes, UpdateOptions};
use windows::Win32::Graphics::Gdi::MapWindowPoints;

use super::*;

/// Stands in for the reader while the archive is being rewritten, so the
/// list can still paint its rows without holding the file open.
struct Detached {
    format: Format,
    entries: Vec<Entry>,
}

impl ArchiveReader for Detached {
    fn format(&self) -> Format {
        self.format
    }

    fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn set_password(&mut self, _password: Option<&str>) {}

    fn extract(&mut self, _ids: &[EntryId], _sink: &mut dyn kibotos_core::ExtractSink, _ctl: &Control) -> kibotos_core::Result<()> {
        Err(Error::Cancelled)
    }
}

/// Why the open archive cannot be changed, if it cannot.
fn read_only_reason(arc: &Loaded) -> Option<String> {
    let format = arc.reader.format();
    if format == Format::Rar {
        return Some(t::READ_ONLY_RAR.into());
    }
    if !kibotos_core::edit::can_update(format) {
        return Some(strings::read_only_format(format.name()));
    }
    let name = file_name(&arc.path);
    if kibotos_core::is_part_name(&name) || arc.path.with_extension("z01").exists() {
        return Some(t::READ_ONLY_SPLIT.into());
    }
    if std::fs::metadata(&arc.path).is_ok_and(|m| m.permissions().readonly()) {
        return Some(t::READ_ONLY_ATTRIBUTE.into());
    }
    None
}

/// Whether the open archive can be changed now. With `explain`, says why
/// not in a message.
pub(super) fn editable(hwnd: HWND, explain: bool) -> bool {
    let state = with_app(|a| {
        if a.job.is_some() || a.loading || a.fs.is_some() {
            return Err(None);
        }
        let arc = a.archive.as_ref().ok_or(None)?;
        match read_only_reason(arc) {
            Some(reason) => Err(Some((file_name(&arc.path), reason))),
            None => Ok(()),
        }
    });
    match state {
        Ok(()) => true,
        Err(Some((name, reason))) if explain => {
            dialogs::message(hwnd, t::READ_ONLY_TITLE, &format!("{name}\n\n{reason}"), MB_ICONINFORMATION);
            false
        }
        Err(_) => false,
    }
}

/// What to select once the archive has been opened again.
pub(super) struct AfterLoad {
    /// Path inside the archive.
    pub select: String,
    /// Start renaming it (a new folder).
    pub rename: bool,
}

/// Applies `changes` to the open archive. Returns whether it worked. With
/// `reopen`, the archive is opened again at the same folder afterwards
/// (always, even on failure: the reader was detached).
pub(super) fn apply(hwnd: HWND, changes: Changes, after: Option<AfterLoad>, reopen: bool) -> bool {
    let Some((path, mut password, needs_password, encrypted)) = with_app(|a| {
        let arc = a.archive.as_ref()?;
        let entries = arc.reader.entries();
        let encrypted = entries.iter().any(|e| e.encrypted);
        // 7z content is compressed again (it must be decrypted); added or
        // replaced ZIP files are encrypted like the rest.
        let rewrites = arc.reader.format() == Format::SevenZip || !changes.add.is_empty() || !changes.replace.is_empty();
        Some((arc.path.clone(), arc.password.clone(), encrypted && rewrites, encrypted))
    }) else {
        return false;
    };
    let name = file_name(&path);
    let mut wrong = false;
    let (_, level) = crate::compress::remembered();
    let outcome = loop {
        if needs_password && password.is_none() {
            match dialogs::ask_password(hwnd, &name, wrong) {
                Some(pw) => password = Some(pw),
                None => break None,
            }
        }
        // The list keeps painting from a copy of the entries; the archive
        // file itself must be closed to be replaced.
        with_app(|a| {
            if let Some(arc) = a.archive.as_mut() {
                let detached = Detached { format: arc.reader.format(), entries: arc.reader.entries().to_vec() };
                arc.reader = Box::new(detached);
            }
        });
        let opts = UpdateOptions { password: password.clone(), encrypt_new: encrypted, level, threads: 0 };
        let (p2, c2, check) = (path.clone(), changes.clone(), needs_password);
        let title = format!("{} {name}", t::PROGRESS_UPDATING);
        let (result, took) = crate::task::run(Some(hwnd), &title, &name, move |ctl| {
            if check {
                verify_password(&p2, opts.password.as_deref(), ctl)?;
            }
            kibotos_core::edit::update(&p2, &c2, &opts, ctl)
        });
        match result {
            Err(Error::WrongPassword | Error::PasswordRequired) if needs_password => {
                password = None;
                wrong = true;
            }
            other => break Some((other, took)),
        }
    };
    let ok = match &outcome {
        Some((Ok(report), took)) => {
            if !report.skipped.is_empty() {
                let text = format!("{}\n{}", t::COMPRESS_SKIPPED_NOTE, strings::skipped_list(&report.skipped));
                dialogs::message(hwnd, t::COMPRESS_SKIPPED_TITLE, &text, MB_ICONWARNING);
            }
            with_app(|a| a.status_note = Some(strings::updated(took.as_secs_f64())));
            true
        }
        Some((Err(Error::Cancelled), _)) | None => false,
        Some((Err(e), _)) => {
            let text = format!("{name}\n\n{}", strings::error_message(e));
            dialogs::message(hwnd, t::UPDATE_ERROR_TITLE, &text, MB_ICONERROR);
            false
        }
    };
    if reopen {
        with_app(|a| a.after_load = if ok { after } else { None });
        reopen_here(hwnd, password);
    }
    ok
}

/// Checks the password on the smallest encrypted file before anything is
/// written (a wrong ZIP password would encrypt new files differently).
fn verify_password(path: &Path, password: Option<&str>, ctl: &Control) -> kibotos_core::Result<()> {
    let mut reader = kibotos_core::open(path, &OpenOptions { password: password.map(str::to_owned) })?;
    let smallest = reader
        .entries()
        .iter()
        .filter(|e| e.encrypted && e.kind == EntryKind::File)
        .min_by_key(|e| e.size.unwrap_or(u64::MAX))
        .map(|e| e.id);
    if let Some(id) = smallest {
        reader.extract(&[id], &mut kibotos_core::MemorySink::new(), ctl)?;
    }
    Ok(())
}

/// Opens the archive again at the folder shown now.
fn reopen_here(hwnd: HWND, password: Option<String>) {
    let Some((path, folder)) = with_app(|a| a.archive.as_ref().map(|l| (l.path.clone(), l.index.path(a.folder)))) else {
        return;
    };
    let target = if folder.is_empty() { path } else { path.join(folder.replace('/', "\\")) };
    start_open(hwnd, Target::Path(target), password, None);
}

/// After the archive opened again: selects the changed item and, for a new
/// folder, starts renaming it.
pub(super) fn after_loaded() {
    let Some(after) = with_app(|a| a.after_load.take()) else { return };
    let Some((list, row)) = with_app(|a| {
        let arc = a.archive.as_ref()?;
        let node = arc.index.find(&after.select)?;
        Some((a.list, a.rows.iter().position(|&r| r == node)?))
    }) else {
        return;
    };
    let none = LVITEMW { stateMask: SEL_FOCUS, ..Default::default() };
    send(list, LVM_SETITEMSTATE, usize::MAX, &none as *const _ as isize);
    let item = LVITEMW { state: SEL_FOCUS, stateMask: SEL_FOCUS, ..Default::default() };
    send(list, LVM_SETITEMSTATE, row, &item as *const _ as isize);
    send(list, LVM_ENSUREVISIBLE, row, 0);
    if after.rename {
        unsafe {
            let _ = SetFocus(Some(list));
        }
        send(list, LVM_EDITLABELW, row, 0);
    }
    if let Some(note) = with_app(|a| a.status_note.take()) {
        let summary = with_app(|a| a.shown.2.clone());
        set_status(&note, &summary);
    }
}

/// The selected nodes of the archive view (never "..").
fn selected_nodes() -> Vec<NodeId> {
    let list = with_app(|a| a.list);
    let rows = selected_rows(list);
    with_app(|a| rows.iter().filter_map(|&i| a.rows.get(i).copied()).filter(|&n| n != UP).collect())
}

// ---------------------------------------------------------------- delete

/// Del: removes the selected rows (folders with their content).
pub(super) fn delete_selected(hwnd: HWND) {
    let nodes = selected_nodes();
    if nodes.is_empty() || !editable(hwnd, true) {
        return;
    }
    let Some((ids, single, archive)) = with_app(|a| {
        let arc = a.archive.as_ref()?;
        let mut ids = Vec::new();
        for &n in &nodes {
            arc.index.entries_under(n, &mut ids);
        }
        ids.sort_unstable();
        ids.dedup();
        let single = (nodes.len() == 1).then(|| arc.index.name(nodes[0]).to_owned());
        Some((ids, single, file_name(&arc.path)))
    }) else {
        return;
    };
    if ids.is_empty() {
        return;
    }
    const DELETE: i32 = 100;
    let main = strings::delete_main(nodes.len(), single.as_deref(), &archive);
    let choice = dialogs::choose(hwnd, t::DELETE_TITLE, &main, t::DELETE_TEXT, &[(DELETE, t::DELETE_BUTTON), (IDCANCEL.0, t::CANCEL)]);
    if choice == DELETE {
        apply(hwnd, Changes { remove: ids, ..Default::default() }, None, true);
    }
}

// ---------------------------------------------------------------- rename

/// F2: starts editing the name of the focused row.
pub(super) fn begin_rename(hwnd: HWND) {
    let Some(row) = focused_row() else { return };
    if !with_app(|a| a.rows.get(row).is_some_and(|&n| n != UP) && a.query.is_empty()) {
        return;
    }
    if editable(hwnd, true) {
        let list = with_app(|a| a.list);
        send(list, LVM_EDITLABELW, row, 0);
    }
}

/// LVN_BEGINLABELEDIT: whether the row may be renamed (archive view, not
/// while filtering, not "..", archive writable). Selects the name without
/// its extension, as Explorer does.
pub(super) fn allow_label_edit(row: usize) -> bool {
    let Some((list, stem)) = with_app(|a| {
        if a.job.is_some() || a.loading || a.fs.is_some() || !a.query.is_empty() {
            return None;
        }
        let arc = a.archive.as_ref()?;
        read_only_reason(arc).is_none().then_some(())?;
        let node = *a.rows.get(row).filter(|&&n| n != UP)?;
        let name = arc.index.name(node);
        let stem = match name.rfind('.') {
            Some(dot) if dot > 0 && !arc.index.is_dir(node) => name[..dot].encode_utf16().count(),
            _ => name.encode_utf16().count(),
        };
        Some((a.list, stem))
    }) else {
        return false;
    };
    let edit = HWND(send(list, LVM_GETEDITCONTROL, 0, 0).0 as _);
    // The list selects everything after this returns: select later.
    unsafe {
        let _ = PostMessageW(Some(edit), EM_SETSEL, WPARAM(0), LPARAM(stem as isize));
    }
    true
}

/// Device names Windows reserves, with or without an extension.
fn reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0')
}

/// Checks a name typed for an item and returns it cleaned up.
fn check_name(typed: &str) -> std::result::Result<String, &'static str> {
    // Like Explorer: surrounding spaces and trailing dots are dropped.
    let name = typed.trim().trim_end_matches(['.', ' ']).to_owned();
    if name.is_empty() || name.chars().any(|c| c < ' ' || "\\/:*?\"<>|".contains(c)) {
        return Err(t::RENAME_INVALID);
    }
    if reserved(&name) {
        return Err(t::RENAME_RESERVED);
    }
    Ok(name)
}

/// LVN_ENDLABELEDIT (posted): renames `node` to `typed`.
pub(super) fn finish_rename(hwnd: HWND, node: NodeId, typed: String) {
    let name = match check_name(&typed) {
        Ok(n) => n,
        Err(why) => {
            dialogs::message(hwnd, t::RENAME_TITLE, why, MB_ICONWARNING);
            return;
        }
    };
    let Some(result) = with_app(|a| {
        let arc = a.archive.as_ref()?;
        let index = &arc.index;
        if node as usize >= index.len() || index.name(node) == name {
            return None;
        }
        let parent = index.parent(node).unwrap_or(ArchiveIndex::ROOT);
        let lower = name.to_lowercase();
        if index.children(parent).iter().any(|&c| c != node && index.name_lower(c) == lower) {
            return Some(Err(strings::rename_exists(&name)));
        }
        let old = index.path(node);
        let parent_path = index.path(parent);
        let new = if parent_path.is_empty() { name.clone() } else { format!("{parent_path}/{name}") };
        let mut ids = Vec::new();
        index.entries_under(node, &mut ids);
        let entries = arc.reader.entries();
        let rename = ids
            .into_iter()
            .map(|id| {
                let p = &entries[id as usize].path;
                // The index path may differ from the stored one only in
                // normalization; everything under `old` is moved.
                let rest = p.get(old.len()..).unwrap_or("");
                (id, format!("{new}{rest}"))
            })
            .collect::<Vec<_>>();
        Some(Ok((rename, new)))
    }) else {
        return;
    };
    match result {
        Err(text) => dialogs::message(hwnd, t::RENAME_TITLE, &text, MB_ICONWARNING),
        Ok((rename, new)) if !rename.is_empty() => {
            apply(hwnd, Changes { rename, ..Default::default() }, Some(AfterLoad { select: new, rename: false }), true);
        }
        // A folder that only exists through its content has no entry of its
        // own: everything under it was listed above, so this is empty only
        // for an empty implied folder, which cannot exist.
        Ok(_) => {}
    }
}

// ------------------------------------------------------------ new folder

/// Ctrl+Shift+N: an empty folder "Nueva carpeta" in the current folder,
/// then its name is edited.
pub(super) fn new_folder(hwnd: HWND) {
    if !with_app(|a| a.query.is_empty()) || !editable(hwnd, true) {
        return;
    }
    let Some(path) = with_app(|a| {
        let arc = a.archive.as_ref()?;
        let taken: HashSet<String> = arc.index.children(a.folder).iter().map(|&c| arc.index.name_lower(c).to_owned()).collect();
        let mut name = t::NEW_FOLDER_NAME.to_owned();
        let mut n = 2;
        while taken.contains(&name.to_lowercase()) {
            name = format!("{} ({n})", t::NEW_FOLDER_NAME);
            n += 1;
        }
        let base = arc.index.path(a.folder);
        Some(if base.is_empty() { name } else { format!("{base}/{name}") })
    }) else {
        return;
    };
    let changes = Changes { new_dirs: vec![path.clone()], ..Default::default() };
    apply(hwnd, changes, Some(AfterLoad { select: path, rename: true }), true);
}

// ------------------------------------------------------------------ drop

/// Files dropped on the list of an open archive. Returns false when they
/// were not for the archive (the caller opens the first one instead).
pub(super) fn drop_into_archive(hwnd: HWND, paths: Vec<PathBuf>, client: POINT) -> bool {
    if with_app(|a| a.archive.is_none() || a.fs.is_some()) {
        return false;
    }
    // Our own drags out of the list hand over files in the session folder.
    if paths.iter().any(|p| p.starts_with(temp::session_dir())) {
        return true;
    }
    let looks_like_archive = paths.len() == 1 && fsview::is_archive_name(&paths[0].to_string_lossy()) && paths[0].is_file();
    if !editable(hwnd, !looks_like_archive) {
        return !looks_like_archive;
    }
    // A folder row under the cursor receives the files; anywhere else, the
    // folder on screen.
    let list = with_app(|a| a.list);
    let mut pt = client;
    unsafe {
        let _ = MapWindowPoints(Some(hwnd), Some(list), std::slice::from_mut(&mut pt));
    }
    let mut hit = LVHITTESTINFO { pt, ..Default::default() };
    let row = send(list, LVM_HITTEST, 0, &mut hit as *mut _ as isize).0;
    let Some((folder_path, folder_name, archive, replaces)) = with_app(|a| {
        let arc = a.archive.as_ref()?;
        let index = &arc.index;
        let target = usize::try_from(row)
            .ok()
            .and_then(|r| a.rows.get(r).copied())
            .filter(|&n| n != UP && index.is_dir(n))
            .unwrap_or(a.folder);
        let taken: HashSet<String> = index.children(target).iter().map(|&c| index.name_lower(c).to_owned()).collect();
        let replaces = paths
            .iter()
            .filter(|p| p.file_name().is_some_and(|n| taken.contains(&n.to_string_lossy().to_lowercase())))
            .count();
        let folder_name = if target == ArchiveIndex::ROOT { String::new() } else { index.name(target).to_owned() };
        Some((index.path(target), folder_name, file_name(&arc.path), replaces))
    }) else {
        return true;
    };
    const ADD: i32 = 100;
    const OPEN: i32 = 101;
    let single = (paths.len() == 1).then(|| file_name(&paths[0]));
    let main = strings::add_main(paths.len(), single.as_deref(), &folder_name, &archive);
    let content = if replaces > 0 { strings::add_replaces(replaces) } else { String::new() };
    let mut buttons = vec![(ADD, t::ADD_BUTTON)];
    if looks_like_archive {
        buttons.push((OPEN, t::ADD_OPEN_BUTTON));
    }
    buttons.push((IDCANCEL.0, t::CANCEL));
    let icon_info = replaces == 0;
    let choice = if icon_info {
        dialogs::choose_info(hwnd, t::ADD_TITLE, &main, &content, &buttons)
    } else {
        dialogs::choose(hwnd, t::ADD_TITLE, &main, &content, &buttons)
    };
    match choice {
        ADD => {
            let select = paths
                .first()
                .map(|p| {
                    let n = file_name(p);
                    if folder_path.is_empty() { n } else { format!("{folder_path}/{n}") }
                })
                .unwrap_or_default();
            let changes = Changes { add: paths, add_into: folder_path, ..Default::default() };
            apply(hwnd, changes, Some(AfterLoad { select, rename: false }), true);
            true
        }
        OPEN => false,
        _ => true,
    }
}

// ------------------------------------------------------- edited files

thread_local!(static ASKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });

/// When Kibotós comes back to the front: files opened from this archive
/// and saved by their program are offered to be saved into the archive.
pub(super) fn check_edited(hwnd: HWND) {
    if ASKING.with(|a| a.get()) || MENU_OPEN.with(|m| m.get()) {
        return;
    }
    let Some(path) = with_app(|a| a.archive.as_ref().filter(|_| a.job.is_none() && !a.loading).map(|l| l.path.clone())) else {
        return;
    };
    let edited = temp::edited(&path);
    if edited.is_empty() || !editable(hwnd, false) {
        return;
    }
    ASKING.with(|a| a.set(true));
    struct Done;
    impl Drop for Done {
        fn drop(&mut self) {
            ASKING.with(|a| a.set(false));
        }
    }
    let _done = Done;
    const UPDATE: i32 = 100;
    let names: Vec<String> = edited.iter().map(|(f, _)| file_name(f)).collect();
    let main = if names.len() == 1 { strings::edited_main(&names[0]) } else { t::MODIFIED_MAIN.to_owned() };
    let mut content = strings::edited_text(&file_name(&path));
    if names.len() > 1 {
        content.push('\n');
        for n in &names {
            content.push_str(&format!("\n•  {n}"));
        }
    }
    let choice = dialogs::choose_info(hwnd, t::EDITED_TITLE, &main, &content, &[(UPDATE, t::UPDATE_ARCHIVE_BUTTON), (IDCANCEL.0, t::NOT_NOW)]);
    if choice != UPDATE {
        for (f, _) in &edited {
            temp::decline(f);
        }
        return;
    }
    save_edited(hwnd, &edited, true);
}

/// Saves edited files back into the open archive. Returns whether it
/// worked.
pub(super) fn save_edited(hwnd: HWND, edited: &[(PathBuf, String)], reopen: bool) -> bool {
    let replace: Vec<(EntryId, PathBuf)> = with_app(|a| {
        let Some(arc) = a.archive.as_ref() else { return Vec::new() };
        edited
            .iter()
            .filter_map(|(file, entry)| {
                let node = arc.index.find(entry)?;
                Some((arc.index.entry(node)?, file.clone()))
            })
            .collect()
    });
    if replace.is_empty() {
        return false;
    }
    let files: Vec<PathBuf> = replace.iter().map(|(_, f)| f.clone()).collect();
    let select = edited.first().map(|(_, e)| e.clone()).unwrap_or_default();
    let ok = apply(hwnd, Changes { replace, ..Default::default() }, Some(AfterLoad { select, rename: false }), reopen);
    if ok {
        for f in &files {
            temp::accept(f);
        }
    }
    ok
}

// --------------------------------------------------------------- SHA-256

/// SHA-256 of the selected files, computed on a worker thread and shown
/// with a button to copy them (in `sha256sum` format).
pub(super) fn sha256_selected(hwnd: HWND) {
    let nodes = selected_nodes();
    let Some((path, password, ids, encrypted, paths)) = with_app(|a| {
        let arc = a.archive.as_ref()?;
        let entries = arc.reader.entries();
        let mut ids = Vec::new();
        for &n in &nodes {
            arc.index.entries_under(n, &mut ids);
        }
        ids.sort_unstable();
        ids.dedup();
        ids.retain(|&id| entries[id as usize].kind == EntryKind::File);
        let encrypted = ids.iter().any(|&id| entries[id as usize].encrypted);
        let paths: HashMap<EntryId, String> = ids.iter().map(|&id| (id, entries[id as usize].path.to_string())).collect();
        Some((arc.path.clone(), arc.password.clone(), ids, encrypted, paths))
    }) else {
        return;
    };
    if ids.is_empty() {
        dialogs::message(hwnd, t::SHA_TITLE, t::SHA_NOTHING, MB_ICONINFORMATION);
        return;
    }
    let mut password = password;
    let mut wrong = false;
    let name = file_name(&path);
    let hashes = loop {
        if encrypted && password.is_none() {
            password = Some(match dialogs::ask_password(hwnd, &name, wrong) {
                Some(p) => p,
                None => return,
            });
        }
        let (p2, pw, ids2) = (path.clone(), password.clone(), ids.clone());
        let title = format!("{} {name}", t::PROGRESS_HASHING);
        let (result, _) = crate::task::run(Some(hwnd), &title, &name, move |ctl| {
            let mut reader = kibotos_core::open(&p2, &OpenOptions { password: pw })?;
            kibotos_core::sha256(reader.as_mut(), &ids2, ctl)
        });
        match result {
            Ok(h) => break h,
            Err(Error::WrongPassword | Error::PasswordRequired) => {
                password = None;
                wrong = true;
            }
            Err(Error::Cancelled) => return,
            Err(e) => {
                dialogs::message(hwnd, t::SHA_TITLE, &format!("{name}\n\n{}", strings::error_message(&e)), MB_ICONERROR);
                return;
            }
        }
    };
    if password.is_some() {
        with_app(|a| {
            if let Some(arc) = a.archive.as_mut().filter(|l| l.path == path) {
                arc.password = password.clone();
            }
        });
    }
    let hex = |h: &[u8; 32]| h.iter().map(|b| format!("{b:02x}")).collect::<String>();
    // sha256sum format: "<hash> *<path>", one line per file.
    let all: Vec<String> = hashes.iter().map(|(id, h)| format!("{} *{}", hex(h), paths[id])).collect();
    const SHOWN: usize = 12;
    let mut content = String::new();
    for (id, h) in hashes.iter().take(SHOWN) {
        let file = paths[id].rsplit('/').next().unwrap_or_default();
        content.push_str(&format!("{file}\n{}\n\n", hex(h)));
    }
    if hashes.len() > SHOWN {
        content.push_str(&format!("… y {} más (se copian todos).", hashes.len() - SHOWN));
    }
    let main = if hashes.len() == 1 { paths[&hashes[0].0].rsplit('/').next().unwrap_or_default().to_owned() } else { format!("{} archivos", hashes.len()) };
    const COPY: i32 = 100;
    let choice = dialogs::choose_info_wide(hwnd, t::SHA_TITLE, &main, content.trim_end(), &[(COPY, t::COPY_BUTTON), (IDCANCEL.0, t::CLOSE_BUTTON)]);
    if choice == COPY && set_clipboard_text(hwnd, &(all.join("\r\n") + "\r\n")) {
        let summary = with_app(|a| a.shown.2.clone());
        set_status(t::STATUS_SHA_COPIED, &summary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(check_name("  informe.txt "), Ok("informe.txt".into()));
        assert_eq!(check_name("notas..."), Ok("notas".into()));
        assert!(check_name("").is_err());
        assert!(check_name("...").is_err());
        assert!(check_name("a/b").is_err());
        assert!(check_name("a:b").is_err());
        assert!(check_name("con.txt").is_err());
        assert!(check_name("COM1").is_err());
        assert_eq!(check_name("COM0"), Ok("COM0".into()));
        assert_eq!(check_name("console"), Ok("console".into()));
    }
}
