# Kibotos

A fast, lightweight archive manager for Windows, written in Rust.

Status: **v0.1 in progress — step 1 of 8** (core library and console tool; no window yet).

## Layout

| Path | What |
|---|---|
| `crates/kibotos-core` | Format backends behind one `ArchiveReader` trait, the in-memory folder index, path validation and extraction. No UI code. |
| `crates/kibotos-core/src/bin/kibotos-cli.rs` | Console tool to list and extract archives with timings. |
| `licenses/unRAR-license.txt` | License of the unRAR code used to read RAR archives. |

`kibotos-app` (the Win32 window and Windows integration) arrives in step 2.

## Formats (read and extract)

ZIP (and jar, apk, cbz), 7z, RAR (and cbr), tar, gz, tgz, bz2, xz, zst and the `tar.*` combinations. Formats are detected by content, not by extension.

RAR support is **extraction only** and always will be: the unRAR license forbids using its code to create a RAR-compatible compressor.

## Build and test

Requires Rust stable and the MSVC Build Tools (C++ workload).

```bash
cargo test -p kibotos-core
```

```bash
cargo build --release
```

## Console tool

```bash
target/release/kibotos-cli list archive.7z
```

```bash
target/release/kibotos-cli list big.zip -q --filter report
```

```bash
target/release/kibotos-cli extract archive.rar out -p PASSWORD --only docs/2026
```

```bash
target/release/kibotos-cli gen-zip test-100k.zip 100000
```

## Third-party notices

RAR decompression uses the UnRAR source code by Alexander Roshal, via the
`unrar_sys` crate. UnRAR source code may be used in any software to handle RAR
archives without limitations free of charge, but cannot be used to develop
RAR (WinRAR) compatible archiver and to re-create RAR compression algorithm,
which is proprietary. See `licenses/unRAR-license.txt` for the full text.

The RAR test archives in `crates/kibotos-core/tests/fixtures` come from the
`unrar` crate (MIT, see `LICENSE-unrar-rs-MIT.txt` in that folder).
