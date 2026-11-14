//! kibotos-cli: lists and extracts archives with timings, to test and
//! benchmark `kibotos-core` without the UI.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use kibotos_core::view::{Ordering, Sort, refine_rows};
use kibotos_core::{ArchiveIndex, ArchiveReader, Control, DiskSink, EntryKind, OpenOptions, Overwrite, Timestamp};

const USAGE: &str = "\
kibotos-cli - Kibotos core test tool

USAGE:
  kibotos-cli list    <archive> [-p PASSWORD] [-q] [--filter TEXT]
  kibotos-cli extract <archive> <dest> [-p PASSWORD] [--only PATH]... [--keep] [--threads N]
  kibotos-cli gen-zip <out.zip> <entries>

  -q          print only timings, not the entries
  --filter    time a whole-archive name search, as the UI filter does
  --only      extract only this file or folder (repeatable)
  --keep      do not overwrite existing files
  --threads N writer threads for small files (0 = write on the extraction thread)
  gen-zip     write a test ZIP with <entries> small stored files
";

fn ms(d: Duration) -> String {
    format!("{:.2} ms", d.as_secs_f64() * 1000.0)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("list") => cmd_list(&args[1..]),
        Some("extract") => cmd_extract(&args[1..]),
        Some("gen-zip") => cmd_gen_zip(&args[1..]),
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

struct Common {
    positional: Vec<String>,
    password: Option<String>,
    quiet: bool,
    filter: Option<String>,
    only: Vec<String>,
    keep: bool,
    threads: Option<usize>,
}

fn parse(args: &[String]) -> Result<Common, String> {
    let mut c = Common { positional: vec![], password: None, quiet: false, filter: None, only: vec![], keep: false, threads: None };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-p" | "--password" => c.password = Some(it.next().ok_or("-p needs a value")?.clone()),
            "-q" | "--quiet" => c.quiet = true,
            "--filter" => c.filter = Some(it.next().ok_or("--filter needs a value")?.clone()),
            "--only" => c.only.push(it.next().ok_or("--only needs a value")?.clone()),
            "--keep" => c.keep = true,
            "--threads" => {
                c.threads = Some(it.next().ok_or("--threads needs a value")?.parse().map_err(|_| "--threads needs a number")?)
            }
            _ => c.positional.push(a.clone()),
        }
    }
    Ok(c)
}

fn open(path: &Path, password: Option<String>) -> Result<(Box<dyn ArchiveReader>, Duration), String> {
    let t = Instant::now();
    let reader = kibotos_core::open(path, &OpenOptions { password }).map_err(|e| e.to_string())?;
    Ok((reader, t.elapsed()))
}

fn fmt_time(t: Option<Timestamp>) -> String {
    match t {
        None => "                   ".into(),
        Some(t) => {
            let (y, mo, d, h, mi, s) = t.civil();
            let z = if matches!(t, Timestamp::Utc(_)) { "Z" } else { " " };
            format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}{z}")
        }
    }
}

fn cmd_list(args: &[String]) -> Result<(), String> {
    let c = parse(args)?;
    let [archive] = c.positional.as_slice() else { return Err("usage: list <archive>".into()) };
    let total = Instant::now();
    let (reader, t_open) = open(Path::new(archive), c.password)?;
    let t = Instant::now();
    let index = ArchiveIndex::build(reader.entries());
    let t_index = t.elapsed();
    let t_total = total.elapsed();

    let entries = reader.entries();
    if !c.quiet {
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        let _ = writeln!(out, "{:>14} {:>14}  {:<20} Name", "Size", "Packed", "Modified");
        for e in entries {
            let size = e.size.map(|s| s.to_string()).unwrap_or_default();
            let packed = e.packed.map(|s| s.to_string()).unwrap_or_default();
            let mark = match e.kind {
                EntryKind::Dir => "/",
                EntryKind::Link => " -> link",
                EntryKind::File => "",
            };
            let lock = if e.encrypted { " *" } else { "" };
            let _ = writeln!(out, "{size:>14} {packed:>14}  {} {}{mark}{lock}", fmt_time(e.modified), e.path);
        }
    }

    let files = entries.iter().filter(|e| e.kind == EntryKind::File).count();
    let bytes: u64 = entries.iter().filter_map(|e| e.size).sum();
    println!();
    println!("format       {}", reader.format().name());
    println!("entries      {} ({} files, {} index nodes)", entries.len(), files, index.len());
    println!("total size   {bytes} bytes");
    println!("open+list    {}", ms(t_open));
    println!("index build  {}", ms(t_index));
    println!("total        {}", ms(t_total));

    if let Some(q) = c.filter {
        // Same code path as the window: one global sort, then each keystroke
        // walks it (or narrows the previous result).
        let t = Instant::now();
        let ord = Ordering::new(&index, reader.entries(), Sort::default());
        let t_sort = t.elapsed();
        let mut results = Vec::new();
        let mut worst = Duration::ZERO;
        let mut sum = Duration::ZERO;
        let chars: Vec<char> = q.chars().collect();
        for n in 1..=chars.len() {
            let prefix: String = chars[..n].iter().collect();
            let t = Instant::now();
            if n == 1 {
                ord.filter_rows(&index, &prefix, &mut results);
            } else {
                refine_rows(&index, &prefix, &mut results);
            }
            let d = t.elapsed();
            worst = worst.max(d);
            sum += d;
        }
        // The first keystroke matches the most nodes: the worst case.
        let t = Instant::now();
        ord.filter_rows(&index, &chars[..1].iter().collect::<String>(), &mut Vec::new());
        let first = t.elapsed();
        println!("sort by name {}", ms(t_sort));
        println!(
            "filter       '{q}': {} matches, per keystroke avg {} / worst {} (first key, full scan: {})",
            results.len(),
            ms(sum / chars.len().max(1) as u32),
            ms(worst),
            ms(first)
        );
    }
    Ok(())
}

fn cmd_extract(args: &[String]) -> Result<(), String> {
    let c = parse(args)?;
    let [archive, dest] = c.positional.as_slice() else { return Err("usage: extract <archive> <dest>".into()) };
    let (mut reader, t_open) = open(Path::new(archive), c.password)?;

    let ids: Vec<u32> = if c.only.is_empty() {
        (0..reader.entries().len() as u32).collect()
    } else {
        let index = ArchiveIndex::build(reader.entries());
        let mut ids = Vec::new();
        for p in &c.only {
            let node = index.find(p).ok_or_else(|| format!("not in archive: {p}"))?;
            index.entries_under(node, &mut ids);
        }
        ids
    };

    let mut sink = DiskSink::new(PathBuf::from(dest)).overwrite(if c.keep { Overwrite::Never } else { Overwrite::Always });
    if let Some(n) = c.threads {
        sink = sink.writer_threads(n);
    }
    let ctl = Control::new();
    let t = Instant::now();
    let res = reader.extract(&ids, &mut sink, &ctl);
    let closed = sink.close();
    let t_extract = t.elapsed();
    let res = res.and(closed.as_ref().map(|_| ()).map_err(|e| kibotos_core::Error::Corrupt(e.to_string())));
    let report = closed.unwrap_or_default();
    let r = &report;
    for s in &r.skipped {
        eprintln!("skipped  {}  ({:?})", s.path, s.reason);
    }
    let secs = t_extract.as_secs_f64().max(1e-9);
    println!("format       {}", reader.format().name());
    println!("open+list    {}", ms(t_open));
    println!("extract      {}", ms(t_extract));
    println!("written      {} files, {} folders, {} bytes ({:.1} MB/s)", r.files, r.dirs, r.bytes, r.bytes as f64 / secs / 1e6);
    println!("skipped      {}", r.skipped.len());
    res.map_err(|e| e.to_string())
}

fn cmd_gen_zip(args: &[String]) -> Result<(), String> {
    let [out, count] = args else { return Err("usage: gen-zip <out.zip> <entries>".into()) };
    let count: usize = count.parse().map_err(|_| "entries must be a number")?;
    let file = std::fs::File::create(out).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(std::io::BufWriter::new(file));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for i in 0..count {
        let name = format!("dir{:03}/sub{:02}/file{i:07}.txt", i % 997, i % 13);
        zip.start_file(name, opts).map_err(|e| e.to_string())?;
        zip.write_all(format!("entry {i}\n").as_bytes()).map_err(|e| e.to_string())?;
    }
    zip.finish().map_err(|e| e.to_string())?;
    println!("wrote {out} with {count} entries");
    Ok(())
}
