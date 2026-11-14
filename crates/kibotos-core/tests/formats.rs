//! End-to-end tests: build archives of every format, then list and extract
//! them through `kibotos_core::open`.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use kibotos_core::{
    ArchiveIndex, ArchiveReader, Codec, Conflict, Control, DiskSink, EntryKind, Error, Format, LimitKind, Limits, MemorySink,
    OpenOptions, Overwrite, SkipReason, UnsafeReason,
};
use tempfile::TempDir;

/// Deterministic, poorly compressible bytes.
fn noise(len: usize) -> Vec<u8> {
    let mut x: u32 = 0x1234_5678;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

/// The sample tree used by every format.
fn sample() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("hello.txt", b"Hello, Kibotos!\n".to_vec()),
        ("docs/readme.md", b"# Readme\n".to_vec()),
        ("docs/sub/data.bin", noise(300_000)),
        ("empty.txt", Vec::new()),
        ("ñandú/日本.txt", "unicode ✓\n".as_bytes().to_vec()),
    ]
}

fn open(path: &Path) -> Box<dyn ArchiveReader> {
    kibotos_core::open(path, &OpenOptions::default()).unwrap()
}

fn open_pw(path: &Path, pw: &str) -> Box<dyn ArchiveReader> {
    kibotos_core::open(path, &OpenOptions { password: Some(pw.into()) }).unwrap()
}

fn extract_all(r: &mut dyn ArchiveReader) -> HashMap<String, Vec<u8>> {
    let mut sink = MemorySink::new();
    r.extract_all(&mut sink, &Control::new()).unwrap();
    sink.files
}

/// Lists, checks sizes and extracts everything, comparing with `sample()`.
fn check_roundtrip(path: &Path, format: Format) {
    let mut r = open(path);
    assert_eq!(r.format(), format, "{}", path.display());
    let files: Vec<_> = r.entries().iter().filter(|e| e.kind == EntryKind::File).collect();
    assert_eq!(files.len(), sample().len());
    for (name, data) in sample() {
        let e = r.entries().iter().find(|e| &*e.path == name).unwrap_or_else(|| panic!("missing {name}"));
        assert_eq!(e.size, Some(data.len() as u64), "{name}");
    }
    let out = extract_all(r.as_mut());
    for (name, data) in sample() {
        assert_eq!(out.get(name), Some(&data), "content of {name}");
    }
    let index = ArchiveIndex::build(r.entries());
    assert!(index.find("docs/sub/data.bin").is_some());
}

fn make_zip(path: &Path, password: Option<&str>) {
    let mut z = zip::ZipWriter::new(File::create(path).unwrap());
    let base = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let opts = match password {
        Some(pw) => base.with_aes_encryption(zip::AesMode::Aes256, pw),
        None => base,
    };
    z.add_directory("docs/", base).unwrap();
    for (name, data) in sample() {
        z.start_file(name, opts).unwrap();
        z.write_all(&data).unwrap();
    }
    z.finish().unwrap();
}

fn make_tar() -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (name, data) in sample() {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(1_700_000_000);
        h.set_entry_type(tar::EntryType::Regular);
        b.append_data(&mut h, name, data.as_slice()).unwrap();
    }
    b.into_inner().unwrap()
}

fn compress(codec: Codec, data: &[u8]) -> Vec<u8> {
    match codec {
        Codec::Gzip => {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(data).unwrap();
            e.finish().unwrap()
        }
        Codec::Bzip2 => {
            let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
            e.write_all(data).unwrap();
            e.finish().unwrap()
        }
        Codec::Xz => {
            let mut e = lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(1)).unwrap();
            e.write_all(data).unwrap();
            e.finish().unwrap()
        }
        Codec::Zstd => zstd::encode_all(data, 3).unwrap(),
    }
}

fn make_7z(path: &Path, password: Option<&str>, solid: bool) {
    use sevenz_rust2::{ArchiveEntry, ArchiveWriter, Password};
    let mut w = ArchiveWriter::create(path).unwrap();
    if let Some(pw) = password {
        w.set_content_methods(vec![
            sevenz_rust2::encoder_options::AesEncoderOptions::new(Password::from(pw)).into(),
            sevenz_rust2::EncoderMethod::LZMA2.into(),
        ]);
        w.set_encrypt_header(true);
    }
    w.push_archive_entry::<&[u8]>(ArchiveEntry::new_directory("docs"), None).unwrap();
    if solid {
        let entries: Vec<_> = sample().iter().map(|(n, _)| ArchiveEntry::new_file(n)).collect();
        let data = sample();
        let readers = data.iter().map(|(_, d)| d.as_slice().into()).collect();
        w.push_archive_entries(entries, readers).unwrap();
    } else {
        for (name, data) in sample() {
            w.push_archive_entry(ArchiveEntry::new_file(name), Some(data.as_slice())).unwrap();
        }
    }
    w.finish().unwrap();
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

// ---------------------------------------------------------------- formats

#[test]
fn zip_roundtrip() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("a.cbz"); // extension does not matter
    make_zip(&p, None);
    check_roundtrip(&p, Format::Zip);
    let r = open(&p);
    let docs = r.entries().iter().find(|e| &*e.path == "docs").unwrap();
    assert!(docs.is_dir());
    assert!(r.entries().iter().all(|e| e.modified.is_some()));
}

#[test]
fn zip_password() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("enc.zip");
    make_zip(&p, Some("secreto"));

    let mut r = open(&p);
    assert!(r.entries().iter().any(|e| e.encrypted));
    let err = r.extract_all(&mut MemorySink::new(), &Control::new()).unwrap_err();
    assert!(matches!(err, Error::PasswordRequired), "{err:?}");

    r.set_password(Some("wrong"));
    let err = r.extract_all(&mut MemorySink::new(), &Control::new()).unwrap_err();
    assert!(matches!(err, Error::WrongPassword), "{err:?}");

    r.set_password(Some("secreto"));
    let out = extract_all(r.as_mut());
    assert_eq!(out["hello.txt"], b"Hello, Kibotos!\n");
}

#[test]
fn sevenz_roundtrip_solid_and_not() {
    let dir = TempDir::new().unwrap();
    for solid in [true, false] {
        let p = dir.path().join(format!("a{solid}.7z"));
        make_7z(&p, None, solid);
        check_roundtrip(&p, Format::SevenZip);
    }
}

#[test]
fn sevenz_selective_extraction() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("s.7z");
    make_7z(&p, None, true);
    let mut r = open(&p);
    let id = r.entries().iter().find(|e| &*e.path == "ñandú/日本.txt").unwrap().id;
    let mut sink = MemorySink::new();
    r.extract(&[id], &mut sink, &Control::new()).unwrap();
    assert_eq!(sink.files.len(), 1);
    assert_eq!(sink.files["ñandú/日本.txt"], "unicode ✓\n".as_bytes());
}

#[test]
fn sevenz_encrypted_header() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("enc.7z");
    make_7z(&p, Some("clave"), true);
    let err = kibotos_core::open(&p, &OpenOptions::default()).err().unwrap();
    assert!(matches!(err, Error::PasswordRequired), "{err:?}");
    let err = kibotos_core::open(&p, &OpenOptions { password: Some("mala".into()) }).err().unwrap();
    assert!(matches!(err, Error::WrongPassword | Error::Corrupt(_)), "{err:?}");
    let mut r = open_pw(&p, "clave");
    assert_eq!(extract_all(r.as_mut())["hello.txt"], b"Hello, Kibotos!\n");
}

#[test]
fn tar_all_codecs() {
    let dir = TempDir::new().unwrap();
    let tar = make_tar();
    let p = dir.path().join("a.tar");
    std::fs::write(&p, &tar).unwrap();
    check_roundtrip(&p, Format::Tar(None));
    for codec in [Codec::Gzip, Codec::Bzip2, Codec::Xz, Codec::Zstd] {
        let p = dir.path().join(format!("a.tar.{}", codec.name()));
        std::fs::write(&p, compress(codec, &tar)).unwrap();
        check_roundtrip(&p, Format::Tar(Some(codec)));
    }
}

#[test]
fn tgz_by_content_not_extension() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("misnamed.zip");
    std::fs::write(&p, compress(Codec::Gzip, &make_tar())).unwrap();
    assert_eq!(kibotos_core::detect_path(&p).unwrap(), Format::Tar(Some(Codec::Gzip)));
}

#[test]
fn single_compressed_files() {
    let dir = TempDir::new().unwrap();
    let data = noise(100_000);
    for codec in [Codec::Gzip, Codec::Bzip2, Codec::Xz, Codec::Zstd] {
        let p = dir.path().join(format!("notes.txt.{}", codec.extensions_for_tests()));
        std::fs::write(&p, compress(codec, &data)).unwrap();
        let mut r = open(&p);
        assert_eq!(r.format(), Format::Compressed(codec));
        assert_eq!(r.entries().len(), 1);
        assert_eq!(&*r.entries()[0].path, "notes.txt");
        if matches!(codec, Codec::Xz | Codec::Zstd) {
            assert_eq!(r.entries()[0].size, Some(data.len() as u64), "{codec:?}");
        }
        assert_eq!(extract_all(r.as_mut())["notes.txt"], data);
    }
}

trait TestExt {
    fn extensions_for_tests(self) -> &'static str;
}
impl TestExt for Codec {
    fn extensions_for_tests(self) -> &'static str {
        match self {
            Codec::Gzip => "gz",
            Codec::Bzip2 => "bz2",
            Codec::Xz => "xz",
            Codec::Zstd => "zst",
        }
    }
}

#[test]
fn rar_fixtures() {
    let mut r = open(&fixture("version.rar"));
    assert_eq!(r.format(), Format::Rar);
    assert!(!r.format().can_write());
    assert_eq!(&*r.entries()[0].path, "VERSION");
    assert_eq!(extract_all(r.as_mut())["VERSION"], b"unrar-0.4.0");

    let mut r = open(&fixture("solid.rar"));
    assert_eq!(extract_all(r.as_mut())[".gitignore"], b"target\nCargo.lock\n");

    let r = open(&fixture("unicode.rar"));
    assert_eq!(&*r.entries()[0].path, "te…―st✌");
    // Dates come from the RAR5 FILETIME field (checks the packed FFI layout).
    let (y, ..) = r.entries()[0].modified.unwrap().civil();
    assert_eq!(y, 2019);
}

#[test]
fn rar_passwords() {
    let mut r = open(&fixture("crypted.rar"));
    assert!(r.entries()[0].encrypted);
    let err = r.extract_all(&mut MemorySink::new(), &Control::new()).unwrap_err();
    assert!(matches!(err, Error::PasswordRequired), "{err:?}");
    let mut r = open_pw(&fixture("crypted.rar"), "bad");
    let err = r.extract_all(&mut MemorySink::new(), &Control::new()).unwrap_err();
    assert!(matches!(err, Error::WrongPassword), "{err:?}");
    let mut r = open_pw(&fixture("crypted.rar"), "unrar");
    assert_eq!(extract_all(r.as_mut())[".gitignore"], b"target\nCargo.lock\n");

    // Encrypted headers: even listing needs the password.
    let err = kibotos_core::open(&fixture("comment-hpw-password.rar"), &OpenOptions::default()).err().unwrap();
    assert!(matches!(err, Error::PasswordRequired), "{err:?}");
    let r = open_pw(&fixture("comment-hpw-password.rar"), "password");
    assert_eq!(&*r.entries()[0].path, ".gitignore");
}

// ---------------------------------------------------------------- errors

#[test]
fn damaged_and_unknown_files() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("junk.zip");
    std::fs::write(&p, b"this is not an archive at all").unwrap();
    assert!(matches!(kibotos_core::open(&p, &OpenOptions::default()), Err(Error::UnknownFormat)));

    // A ZIP cut in half loses its central directory.
    let full = dir.path().join("full.zip");
    make_zip(&full, None);
    let bytes = std::fs::read(&full).unwrap();
    let cut = dir.path().join("cut.zip");
    std::fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
    let err = kibotos_core::open(&cut, &OpenOptions::default()).err().unwrap();
    assert!(matches!(err, Error::Corrupt(_)), "{err:?}");

    // A truncated tar.gz lists until the damage and reports it.
    let tgz = compress(Codec::Gzip, &make_tar());
    let p = dir.path().join("cut.tgz");
    std::fs::write(&p, &tgz[..tgz.len() / 2]).unwrap();
    let err = kibotos_core::open(&p, &OpenOptions::default()).err().unwrap();
    assert!(matches!(err, Error::Corrupt(_)), "{err:?}");
}

// ---------------------------------------------------------------- safety

fn evil_zip(path: &Path) {
    let mut z = zip::ZipWriter::new(File::create(path).unwrap());
    let o = zip::write::SimpleFileOptions::default();
    for name in ["../escape.txt", "/abs.txt", "C:/Windows/evil.txt", "dir/../../up.txt", "CON.txt", "ok/fine.txt"] {
        z.start_file(name, o).unwrap();
        z.write_all(b"x").unwrap();
    }
    z.add_symlink("link", "../../outside", o).unwrap();
    z.finish().unwrap();
}

#[test]
fn unsafe_paths_are_blocked() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("evil.zip");
    evil_zip(&p);
    let dest = dir.path().join("out");
    let mut r = open(&p);
    let mut sink = DiskSink::new(&dest);
    r.extract_all(&mut sink, &Control::new()).unwrap();
    let report = sink.close().unwrap();

    assert_eq!(std::fs::read(dest.join("ok/fine.txt")).unwrap(), b"x");
    assert_eq!(report.files, 1);
    assert_eq!(report.blocked().count(), 6);
    let reasons: HashMap<_, _> = report.skipped.iter().map(|s| (s.path.as_str(), s.reason.clone())).collect();
    assert_eq!(reasons["../escape.txt"], SkipReason::Unsafe(UnsafeReason::Traversal));
    assert_eq!(reasons["/abs.txt"], SkipReason::Unsafe(UnsafeReason::Absolute));
    assert_eq!(reasons["C:/Windows/evil.txt"], SkipReason::Unsafe(UnsafeReason::Absolute));
    assert_eq!(reasons["dir/../../up.txt"], SkipReason::Unsafe(UnsafeReason::Traversal));
    assert_eq!(reasons["CON.txt"], SkipReason::Unsafe(UnsafeReason::ReservedName));
    assert_eq!(reasons["link"], SkipReason::Link);
    // Nothing was written next to the destination.
    let siblings: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(siblings.len(), 2, "{siblings:?}");
}

#[test]
fn disk_sink_writes_tree_and_respects_overwrite() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("a.zip");
    make_zip(&p, None);
    let dest = dir.path().join("out");
    let mut r = open(&p);
    let mut sink = DiskSink::new(&dest);
    r.extract_all(&mut sink, &Control::new()).unwrap();
    sink.close().unwrap();
    for (name, data) in sample() {
        assert_eq!(std::fs::read(dest.join(name)).unwrap(), data, "{name}");
    }

    std::fs::write(dest.join("hello.txt"), b"mine").unwrap();
    let mut sink = DiskSink::new(&dest).overwrite(Overwrite::Never);
    r.extract_all(&mut sink, &Control::new()).unwrap();
    let report = sink.close().unwrap();
    assert_eq!(std::fs::read(dest.join("hello.txt")).unwrap(), b"mine");
    assert!(report.skipped.iter().any(|s| s.reason == SkipReason::Exists));
    assert_eq!(report.blocked().count(), 0);
}

#[test]
fn conflict_callback_decides() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("a.zip");
    make_zip(&p, None);
    let dest = dir.path().join("out");
    let mut r = open(&p);
    let mut first = DiskSink::new(&dest);
    r.extract_all(&mut first, &Control::new()).unwrap();
    first.close().unwrap();
    for (name, _) in sample() {
        std::fs::write(dest.join(name), b"old").unwrap();
    }

    // Replace the first conflict, then skip all the others.
    let mut asked = 0;
    let answers = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = answers.clone();
    let sink = DiskSink::new(&dest).overwrite(Overwrite::Ask).on_conflict(Box::new(move |path, _| {
        seen.lock().unwrap().push(path.to_path_buf());
        asked += 1;
        if asked == 1 { Conflict::Replace } else { Conflict::SkipAll }
    }));
    let mut sink = sink;
    r.extract_all(&mut sink, &Control::new()).unwrap();
    let report = sink.close().unwrap();
    assert_eq!(answers.lock().unwrap().len(), 2, "asked once, then 'skip all'");
    assert_eq!(report.files, 1);
    let replaced = answers.lock().unwrap()[0].clone();
    assert_ne!(std::fs::read(&replaced).unwrap(), b"old");

    // Cancel stops the extraction.
    let mut sink = DiskSink::new(&dest).overwrite(Overwrite::Ask).on_conflict(Box::new(|_, _| Conflict::Cancel));
    let err = r.extract_all(&mut sink, &Control::new()).unwrap_err();
    assert!(matches!(err, Error::Cancelled));
}

#[test]
fn many_small_files_through_writer_pool() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("many.zip");
    let mut z = zip::ZipWriter::new(File::create(&p).unwrap());
    let o = zip::write::SimpleFileOptions::default();
    for i in 0..500 {
        z.start_file(format!("d{}/f{i}.txt", i % 7), o).unwrap();
        z.write_all(format!("file {i}").as_bytes()).unwrap();
    }
    // The same path twice (written with different separators, which the
    // zip writer accepts): the later entry must win with parallel writes.
    for (name, v) in [("x/dup.txt", "first"), (r"x\dup.txt", "second")] {
        z.start_file(name, o).unwrap();
        z.write_all(v.as_bytes()).unwrap();
    }
    z.finish().unwrap();

    let mut r = open(&p);
    let dest = dir.path().join("out");
    let mut sink = DiskSink::new(&dest).writer_threads(4);
    r.extract_all(&mut sink, &Control::new()).unwrap();
    let report = sink.close().unwrap();
    assert_eq!(report.files, 502);
    for i in [0, 123, 499] {
        let got = std::fs::read_to_string(dest.join(format!("d{}/f{i}.txt", i % 7))).unwrap();
        assert_eq!(got, format!("file {i}"));
    }
    assert_eq!(std::fs::read_to_string(dest.join("x/dup.txt")).unwrap(), "second");
}

#[test]
fn extract_folder_relative_to_base() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("a.zip");
    make_zip(&p, None);
    let mut r = open(&p);
    let index = ArchiveIndex::build(r.entries());
    let mut ids = Vec::new();
    index.entries_under(index.find("docs/sub").unwrap(), &mut ids);
    let dest = dir.path().join("out");
    let mut sink = DiskSink::new(&dest).base("docs");
    r.extract(&ids, &mut sink, &Control::new()).unwrap();
    sink.close().unwrap();
    assert!(dest.join("sub/data.bin").exists());
    assert!(!dest.join("docs").exists());
}

#[test]
fn extract_items_from_several_folders_by_name() {
    // Filter results from different folders, dragged out together.
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("a.zip");
    make_zip(&p, None);
    let mut r = open(&p);
    let index = ArchiveIndex::build(r.entries());
    let mut ids = Vec::new();
    for path in ["docs/sub/data.bin", "docs/readme.md", "ñandú/日本.txt"] {
        index.entries_under(index.find(path).unwrap(), &mut ids);
    }
    let dest = dir.path().join("out");
    let mut sink = DiskSink::new(&dest).bases(["docs/sub", "docs", "ñandú"]);
    r.extract(&ids, &mut sink, &Control::new()).unwrap();
    sink.close().unwrap();
    for name in ["data.bin", "readme.md", "日本.txt"] {
        assert!(dest.join(name).is_file(), "{name}");
    }
    assert!(!dest.join("docs").exists());
}

// ---------------------------------------------------------------- limits

#[test]
fn ratio_limit_stops_bombs() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("bomb.zip");
    let mut z = zip::ZipWriter::new(File::create(&p).unwrap());
    let o = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(false);
    z.start_file("zeros.bin", o).unwrap();
    let chunk = vec![0u8; 1 << 20];
    for _ in 0..20 {
        z.write_all(&chunk).unwrap();
    }
    z.finish().unwrap();

    let mut r = open(&p);
    let ctl = Control::with_limits(Limits { max_ratio: 100, ratio_threshold: 1 << 20, ..Limits::default() });
    let err = r.extract_all(&mut MemorySink::new(), &ctl).unwrap_err();
    assert!(matches!(err, Error::LimitExceeded(LimitKind::Ratio)), "{err:?}");

    let ctl = Control::with_limits(Limits { max_total_bytes: 1 << 20, ..Limits::default() });
    let err = r.extract_all(&mut MemorySink::new(), &ctl).unwrap_err();
    assert!(matches!(err, Error::LimitExceeded(LimitKind::TotalSize)), "{err:?}");

    // Default limits allow it: 20 MiB of zeros is a normal file.
    extract_all(r.as_mut());
}

#[test]
fn cancellation_removes_partial_file() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("a.zip");
    make_zip(&p, None);
    let mut r = open(&p);
    let ctl = Control::new();
    ctl.cancel();
    let dest = dir.path().join("out");
    let err = r.extract_all(&mut DiskSink::new(&dest), &ctl).unwrap_err();
    assert!(matches!(err, Error::Cancelled));
    assert!(!dest.join("hello.txt").exists());
}

#[test]
fn progress_is_reported() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("a.7z");
    make_7z(&p, None, true);
    let mut r = open(&p);
    let ctl = Control::new();
    r.extract_all(&mut MemorySink::new(), &ctl).unwrap();
    let pr = ctl.progress();
    let total: u64 = sample().iter().map(|(_, d)| d.len() as u64).sum();
    assert_eq!(pr.bytes_done, total);
    assert_eq!(pr.bytes_total, total);
    assert_eq!(pr.entries_done, pr.entries_total);
}

#[cfg(windows)]
#[test]
fn mark_of_the_web_reaches_risky_files() {
    use kibotos_core::zone::{self, Zone, ZonePolicy};
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("descargado.zip");
    let mut z = zip::ZipWriter::new(File::create(&p).unwrap());
    let o = zip::write::SimpleFileOptions::default();
    // A small file (writer pool), a large one (streamed) and a harmless one.
    for (name, len) in [("setup.exe", 10), ("grande.docm", 400_000), ("leeme.txt", 10)] {
        z.start_file(name, o).unwrap();
        z.write_all(&noise(len)).unwrap();
    }
    z.finish().unwrap();
    let mut ads = p.as_os_str().to_owned();
    ads.push(":Zone.Identifier");
    std::fs::write(&ads, b"[ZoneTransfer]\r\nZoneId=3\r\n").unwrap();

    let mark = zone::read(&p).expect("archive has a mark");
    let dest = dir.path().join("out");
    let mut sink = DiskSink::new(&dest).zone(Some(Zone { data: mark, policy: ZonePolicy::Risky }));
    open(&p).extract_all(&mut sink, &Control::new()).unwrap();
    sink.close().unwrap();
    assert!(zone::read(&dest.join("setup.exe")).is_some());
    assert!(zone::read(&dest.join("grande.docm")).is_some());
    assert!(zone::read(&dest.join("leeme.txt")).is_none());
}
