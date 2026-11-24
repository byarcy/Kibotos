//! Changing archives: each writable format is created, changed with every
//! kind of change at once, and read back.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kibotos_core::edit::{Changes, UpdateOptions, can_update, update};
use kibotos_core::{ArchiveReader, Codec, Control, CreateFormat, CreateOptions, EntryKind, Error, Format, Level, MemorySink, OpenOptions};
use tempfile::TempDir;

fn write(path: &Path, data: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, data).unwrap();
}

/// datos/{a.txt, b.txt, docs/uno.md, docs/dos.md, vacía/}
fn source(dir: &Path) -> PathBuf {
    let base = dir.join("datos");
    write(&base.join("a.txt"), b"contenido de a\n");
    write(&base.join("b.txt"), "bebé\n".repeat(1000).as_bytes());
    write(&base.join("docs/uno.md"), b"# uno\n");
    write(&base.join("docs/dos.md"), b"# dos\n");
    std::fs::create_dir_all(base.join("vacía")).unwrap();
    base
}

fn open(path: &Path, password: Option<&str>) -> Box<dyn ArchiveReader> {
    kibotos_core::open(path, &OpenOptions { password: password.map(str::to_owned) }).unwrap()
}

/// Every file's content and every folder (value `None`), by path.
fn contents(path: &Path, password: Option<&str>) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut r = open(path, password);
    let mut sink = MemorySink::new();
    r.extract_all(&mut sink, &Control::new()).unwrap();
    let mut out: BTreeMap<String, Option<Vec<u8>>> = sink.files.into_iter().map(|(k, v)| (k, Some(v))).collect();
    for e in r.entries() {
        if e.kind == EntryKind::Dir {
            out.insert(e.path.to_string(), None);
        }
    }
    out
}

fn id_of(r: &dyn ArchiveReader, path: &str) -> u32 {
    r.entries().iter().find(|e| &*e.path == path).unwrap_or_else(|| panic!("no entry {path}")).id
}

/// All kinds of change at once:
/// - remove `datos/a.txt`;
/// - rename the folder `datos/docs` to `datos/notas` (its entries too);
/// - replace `datos/b.txt` with an edited copy;
/// - add a file and a folder from disk into `datos/notas`;
/// - create the empty folder `datos/nueva`.
fn exercise(format: CreateFormat, password: Option<&str>) {
    let dir = TempDir::new().unwrap();
    let base = source(dir.path());
    let archive = dir.path().join(format!("prueba.{}", format.extension()));
    let copts = CreateOptions { format, level: Level::Normal, password: password.map(str::to_owned), ..Default::default() };
    kibotos_core::create(&archive, std::slice::from_ref(&base), &copts, &Control::new()).unwrap();

    let edited = dir.path().join("editado.txt");
    write(&edited, b"texto nuevo\n");
    let extra = dir.path().join("extra.txt");
    write(&extra, b"extra\n");
    let fotos = dir.path().join("fotos");
    write(&fotos.join("f1.jpg"), &[0xFF, 0xD8, 1, 2, 3]);

    let r = open(&archive, password);
    let r = r.as_ref();
    let mut changes = Changes {
        remove: vec![id_of(r, "datos/a.txt")],
        replace: vec![(id_of(r, "datos/b.txt"), edited.clone())],
        add: vec![extra.clone(), fotos.clone()],
        add_into: "datos/notas".into(),
        new_dirs: vec!["datos/nueva".into()],
        ..Default::default()
    };
    for e in r.entries() {
        if let Some(rest) = e.path.strip_prefix("datos/docs") {
            changes.rename.push((e.id, format!("datos/notas{rest}")));
        }
    }
    let opts = UpdateOptions { password: password.map(str::to_owned), encrypt_new: password.is_some(), ..Default::default() };
    let report = update(&archive, &changes, &opts, &Control::new()).unwrap();
    assert_eq!(report.removed, 1);
    assert_eq!(report.replaced, 1);
    assert!(report.renamed >= 2, "{report:?}");
    assert!(report.skipped.is_empty());
    assert!(!dir.path().join(format!("prueba.{}.kibotos-part", format.extension())).exists());

    let got = contents(&archive, password);
    let file = |p: &str| got.get(p).cloned().flatten();
    assert_eq!(file("datos/a.txt"), None, "removed");
    assert_eq!(file("datos/b.txt").as_deref(), Some(&b"texto nuevo\n"[..]), "replaced");
    assert_eq!(file("datos/notas/uno.md").as_deref(), Some(&b"# uno\n"[..]), "renamed folder");
    assert_eq!(file("datos/notas/dos.md").as_deref(), Some(&b"# dos\n"[..]));
    assert!(!got.keys().any(|k| k.starts_with("datos/docs")), "old folder gone: {:?}", got.keys());
    assert_eq!(file("datos/notas/extra.txt").as_deref(), Some(&b"extra\n"[..]), "added file");
    assert_eq!(file("datos/notas/fotos/f1.jpg").as_deref(), Some(&[0xFF, 0xD8, 1, 2, 3][..]), "added folder");
    assert!(got.contains_key("datos/nueva"), "new folder: {:?}", got.keys());
    assert!(got.contains_key("datos/vacía"), "empty folder kept");
}

#[test]
fn zip_changes() {
    exercise(CreateFormat::Zip, None);
}

#[test]
fn zip_aes_changes() {
    exercise(CreateFormat::Zip, Some("contraseña"));
}

#[test]
fn sevenz_changes() {
    exercise(CreateFormat::SevenZip, None);
}

#[test]
fn sevenz_encrypted_names_stay_encrypted() {
    exercise(CreateFormat::SevenZip, Some("clave"));
    let dir = TempDir::new().unwrap();
    let base = source(dir.path());
    let archive = dir.path().join("x.7z");
    let copts = CreateOptions { format: CreateFormat::SevenZip, password: Some("clave".into()), encrypt_names: true, ..Default::default() };
    kibotos_core::create(&archive, &[base], &copts, &Control::new()).unwrap();
    let r = open(&archive, Some("clave"));
    let changes = Changes { remove: vec![id_of(r.as_ref(), "datos/a.txt")], ..Default::default() };
    drop(r);
    let opts = UpdateOptions { password: Some("clave".into()), ..Default::default() };
    update(&archive, &changes, &opts, &Control::new()).unwrap();
    let blind = kibotos_core::open(&archive, &OpenOptions::default());
    assert!(matches!(blind, Err(Error::PasswordRequired)), "names must stay hidden");
    assert!(!contents(&archive, Some("clave")).contains_key("datos/a.txt"));
}

#[test]
fn tar_changes() {
    for codec in [None, Some(Codec::Gzip), Some(Codec::Xz), Some(Codec::Zstd), Some(Codec::Bzip2)] {
        exercise(CreateFormat::Tar(codec), None);
    }
}

#[test]
fn tar_keeps_links_and_order() {
    let dir = TempDir::new().unwrap();
    let archive = dir.path().join("links.tar.gz");
    {
        let f = std::fs::File::create(&archive).unwrap();
        let gz = flate2::write::GzEncoder::new(f, flate2::Compression::default());
        let mut b = tar::Builder::new(gz);
        for (name, data) in [("uno.txt", &b"1"[..]), ("dos.txt", b"2"), ("tres.txt", b"3")] {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o640);
            b.append_data(&mut h, name, data).unwrap();
        }
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        b.append_link(&mut h, "enlace", "tres.txt").unwrap();
        b.into_inner().unwrap().finish().unwrap();
    }
    let r = open(&archive, None);
    let changes = Changes { remove: vec![id_of(r.as_ref(), "dos.txt")], ..Default::default() };
    drop(r);
    update(&archive, &changes, &UpdateOptions::default(), &Control::new()).unwrap();
    let r = open(&archive, None);
    let names: Vec<_> = r.entries().iter().map(|e| (e.path.to_string(), e.kind)).collect();
    assert_eq!(
        names,
        vec![("uno.txt".into(), EntryKind::File), ("tres.txt".into(), EntryKind::File), ("enlace".into(), EntryKind::Link)]
    );
    assert_eq!(r.entries()[2].link_target.as_deref(), Some("tres.txt"));
}

#[test]
fn zip_keeps_compressed_data() {
    let dir = TempDir::new().unwrap();
    let base = source(dir.path());
    let archive = dir.path().join("z.zip");
    kibotos_core::create(&archive, &[base], &CreateOptions::default(), &Control::new()).unwrap();
    let before: BTreeMap<String, (Option<u64>, Option<u32>)> =
        open(&archive, None).entries().iter().map(|e| (e.path.to_string(), (e.packed, e.crc))).collect();
    let r = open(&archive, None);
    let changes = Changes { rename: vec![(id_of(r.as_ref(), "datos/b.txt"), "datos/c.txt".into())], ..Default::default() };
    drop(r);
    update(&archive, &changes, &UpdateOptions::default(), &Control::new()).unwrap();
    let after = open(&archive, None);
    let c = after.entries().iter().find(|e| &*e.path == "datos/c.txt").unwrap();
    assert_eq!((c.packed, c.crc), before["datos/b.txt"], "copied, not recompressed");
}

#[test]
fn read_only_formats_are_refused() {
    assert!(!can_update(Format::Rar));
    assert!(!can_update(Format::Iso));
    assert!(!can_update(Format::Cab));
    assert!(!can_update(Format::Msi));
    assert!(!can_update(Format::Compressed(Codec::Gzip)));
    assert!(!can_update(Format::Tar(Some(Codec::Lz4))));
    assert!(can_update(Format::Zip) && can_update(Format::SevenZip) && can_update(Format::Tar(Some(Codec::Zstd))));
}

#[test]
fn bad_names_are_refused_and_original_kept() {
    let dir = TempDir::new().unwrap();
    let base = source(dir.path());
    let archive = dir.path().join("z.zip");
    kibotos_core::create(&archive, &[base], &CreateOptions::default(), &Control::new()).unwrap();
    let before = std::fs::read(&archive).unwrap();
    let r = open(&archive, None);
    let changes = Changes { rename: vec![(id_of(r.as_ref(), "datos/b.txt"), "../fuera.txt".into())], ..Default::default() };
    drop(r);
    assert!(update(&archive, &changes, &UpdateOptions::default(), &Control::new()).is_err());
    assert_eq!(std::fs::read(&archive).unwrap(), before);
}

#[test]
fn sha256_of_entries() {
    let dir = TempDir::new().unwrap();
    write(&dir.path().join("in/abc.txt"), b"abc");
    let archive = dir.path().join("h.7z");
    let opts = CreateOptions { format: CreateFormat::SevenZip, ..Default::default() };
    kibotos_core::create(&archive, &[dir.path().join("in")], &opts, &Control::new()).unwrap();
    let mut r = open(&archive, None);
    let ids: Vec<u32> = (0..r.entries().len() as u32).collect();
    let hashes = kibotos_core::sha256(r.as_mut(), &ids, &Control::new()).unwrap();
    assert_eq!(hashes.len(), 1);
    let hex: String = hashes[0].1.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
}
