//! Archive creation: every format is written, then read back through
//! `kibotos_core::open` and compared with the files on disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use kibotos_core::{
    ArchiveReader, Codec, Control, CreateFormat, CreateOptions, EntryKind, Error, Format, Level, MemorySink, OpenOptions,
};
use tempfile::TempDir;

fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

/// A folder with text, binary, empty, nested, Unicode and larger files.
fn make_tree(root: &Path) -> PathBuf {
    let base = root.join("datos");
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("hola.txt", b"Hola, Kibotos!\n".to_vec()),
        ("docs/leeme.md", "# Léeme\n".repeat(500).into_bytes()),
        ("docs/sub/ruido.bin", noise(200_000, 7)),
        ("vacío.txt", Vec::new()),
        ("ñandú/日本.txt", "unicode ✓\n".repeat(50).into_bytes()),
        // Above the ZIP writer's in-memory limit: streamed.
        ("grande.bin", [noise(5_000_000, 9), vec![b'a'; 5_000_000]].concat()),
    ];
    for (name, data) in &files {
        let p = base.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }
    std::fs::create_dir_all(base.join("carpeta vacía")).unwrap();
    base
}

/// Every file under `dir`, keyed by its path relative to `dir`'s parent.
fn disk_files(dir: &Path) -> HashMap<String, Vec<u8>> {
    let mut out = HashMap::new();
    let parent = dir.parent().unwrap();
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let rel = p.strip_prefix(parent).unwrap().to_string_lossy().replace('\\', "/");
                out.insert(rel, std::fs::read(&p).unwrap());
            }
        }
    }
    out
}

fn read_back(path: &Path, password: Option<&str>) -> (Box<dyn ArchiveReader>, HashMap<String, Vec<u8>>) {
    let mut r = kibotos_core::open(path, &OpenOptions { password: password.map(str::to_owned) }).unwrap();
    let mut sink = MemorySink::new();
    r.extract_all(&mut sink, &Control::new()).unwrap();
    (r, sink.files)
}

fn create(out: &Path, inputs: &[PathBuf], opts: &CreateOptions) -> kibotos_core::CreateReport {
    kibotos_core::create(out, inputs, opts, &Control::new()).unwrap()
}

#[test]
fn every_format_and_level_roundtrips() {
    let dir = TempDir::new().unwrap();
    let src = make_tree(dir.path());
    let expected = disk_files(&src);
    let formats = [
        (CreateFormat::Zip, Format::Zip),
        (CreateFormat::SevenZip, Format::SevenZip),
        (CreateFormat::Tar(None), Format::Tar(None)),
        (CreateFormat::Tar(Some(Codec::Gzip)), Format::Tar(Some(Codec::Gzip))),
        (CreateFormat::Tar(Some(Codec::Bzip2)), Format::Tar(Some(Codec::Bzip2))),
        (CreateFormat::Tar(Some(Codec::Xz)), Format::Tar(Some(Codec::Xz))),
        (CreateFormat::Tar(Some(Codec::Zstd)), Format::Tar(Some(Codec::Zstd))),
    ];
    for (create_as, read_as) in formats {
        for level in [Level::Store, Level::Fast, Level::Normal] {
            let out = dir.path().join(format!("out-{level:?}.{}", create_as.extension()));
            let opts = CreateOptions { format: create_as, level, ..Default::default() };
            let report = create(&out, std::slice::from_ref(&src), &opts);
            assert_eq!(report.files, expected.len() as u64, "{create_as:?} {level:?}");
            assert!(report.skipped.is_empty());
            assert!(!out.with_file_name(format!("{}.kibotos-part", out.file_name().unwrap().to_string_lossy())).exists());
            let (r, files) = read_back(&out, None);
            assert_eq!(r.format(), read_as);
            assert_eq!(files, expected, "{create_as:?} {level:?}");
            assert!(
                r.entries().iter().any(|e| &*e.path == "datos/carpeta vacía" && e.kind == EntryKind::Dir),
                "{create_as:?}: empty folder kept"
            );
        }
    }
}

#[test]
fn passwords_protect_zip_and_7z() {
    let dir = TempDir::new().unwrap();
    let src = make_tree(dir.path());
    let expected = disk_files(&src);
    for format in [CreateFormat::Zip, CreateFormat::SevenZip] {
        for pw in ["secreto", "contraseña ñ"] {
            let out = dir.path().join(format!("p-{}.{}", pw.len(), format.extension()));
            let opts = CreateOptions { format, password: Some(pw.into()), ..Default::default() };
            create(&out, std::slice::from_ref(&src), &opts);
            let (_, files) = read_back(&out, Some(pw));
            assert_eq!(files, expected, "{format:?} {pw}");
            // Without the password: 7z cannot even be listed; ZIP lists but
            // will not extract.
            match kibotos_core::open(&out, &OpenOptions::default()) {
                Err(Error::PasswordRequired) => assert_eq!(format, CreateFormat::SevenZip),
                Ok(mut r) => {
                    assert_eq!(format, CreateFormat::Zip);
                    assert!(r.entries().iter().any(|e| e.encrypted));
                    let err = r.extract_all(&mut MemorySink::new(), &Control::new()).unwrap_err();
                    assert!(matches!(err, Error::PasswordRequired | Error::WrongPassword), "{err}");
                }
                Err(e) => panic!("unexpected {e}"),
            }
        }
    }
    // 7z with visible names: listable without the password.
    let out = dir.path().join("names.7z");
    let opts = CreateOptions { format: CreateFormat::SevenZip, password: Some("x".into()), encrypt_names: false, ..Default::default() };
    create(&out, std::slice::from_ref(&src), &opts);
    let r = kibotos_core::open(&out, &OpenOptions::default()).unwrap();
    assert!(r.entries().iter().any(|e| &*e.path == "datos/hola.txt"));
}

#[test]
fn several_inputs_and_name_clashes() {
    let dir = TempDir::new().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    for d in [&a, &b] {
        std::fs::create_dir_all(d.join("misma")).unwrap();
        std::fs::write(d.join("misma/x.txt"), d.to_string_lossy().as_bytes()).unwrap();
    }
    std::fs::write(dir.path().join("suelto.txt"), b"1").unwrap();
    let out = dir.path().join("varios.zip");
    let inputs = [a.join("misma"), b.join("misma"), dir.path().join("suelto.txt")];
    create(&out, &inputs, &CreateOptions::default());
    let (_, files) = read_back(&out, None);
    assert_eq!(files.len(), 3);
    assert!(files.contains_key("misma/x.txt"));
    assert!(files.contains_key("misma (2)/x.txt"));
    assert_eq!(files["suelto.txt"], b"1");
}

#[test]
fn archive_inside_its_own_input_is_left_out() {
    let dir = TempDir::new().unwrap();
    let src = make_tree(dir.path());
    let out = src.join("yo mismo.7z");
    let opts = CreateOptions { format: CreateFormat::SevenZip, level: Level::Fast, ..Default::default() };
    create(&out, std::slice::from_ref(&src), &opts);
    let (r, _) = read_back(&out, None);
    assert!(r.entries().iter().all(|e| !e.path.ends_with(".7z") && !e.path.contains("kibotos-part")));
}

#[test]
fn cancelling_leaves_nothing_behind() {
    let dir = TempDir::new().unwrap();
    let src = make_tree(dir.path());
    for format in [CreateFormat::Zip, CreateFormat::SevenZip, CreateFormat::Tar(Some(Codec::Xz))] {
        let out = dir.path().join(format!("cancelado.{}", format.extension()));
        let ctl = Control::new();
        ctl.cancel();
        let opts = CreateOptions { format, ..Default::default() };
        let err = kibotos_core::create(&out, std::slice::from_ref(&src), &opts, &ctl).unwrap_err();
        assert!(matches!(err, Error::Cancelled), "{format:?}: {err}");
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from("datos")], "{format:?}");
    }
}

#[test]
fn progress_counts_every_byte() {
    let dir = TempDir::new().unwrap();
    let src = make_tree(dir.path());
    let total: u64 = disk_files(&src).values().map(|v| v.len() as u64).sum();
    for format in [CreateFormat::Zip, CreateFormat::SevenZip, CreateFormat::Tar(Some(Codec::Zstd))] {
        let ctl = Control::new();
        let out = dir.path().join(format!("p.{}", format.extension()));
        kibotos_core::create(&out, std::slice::from_ref(&src), &CreateOptions { format, ..Default::default() }, &ctl).unwrap();
        let p = ctl.progress();
        assert_eq!(p.bytes_total, total, "{format:?}");
        assert_eq!(p.bytes_done, total, "{format:?}");
        assert_eq!(p.entries_done, p.entries_total, "{format:?}");
    }
}
