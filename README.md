# Kibotos

A fast, lightweight archive manager for Windows, written in Rust.

Status: **v0.1 in progress — step 7 of 8** (main window, context-menu modes and Windows integration).

## Layout

| Path | What |
|---|---|
| `crates/kibotos-core` | Format backends behind one `ArchiveReader` trait, the in-memory folder index, path validation and extraction. No UI code. |
| `crates/kibotos-core/src/bin/kibotos-cli.rs` | Console tool to list and extract archives with timings. |
| `crates/kibotos-app` | `kibotos.exe`: native Win32 window with a virtual ListView. UI text lives in `src/strings.rs`. |
| `licenses/unRAR-license.txt` | License of the unRAR code used to read RAR archives. |

The release build is a single `kibotos.exe` with the C runtime linked statically (see `.cargo/config.toml`), so it needs no Visual C++ Redistributable.

Files opened from an archive go to `%TEMP%\Kibotos\<process>`, removed on exit (leftovers from closed instances are removed at the next start). If an opened file was edited, Kibotós offers to save a copy next to the archive, since it cannot update archives until v0.2. Extracted and opened files inherit the archive's Mark of the Web, so SmartScreen and Office Protected View still apply.

Set `KIBOTOS_TIMING=<file>` to log startup timings (milliseconds since process creation) to that file.

## Keyboard

| Key | Action |
|---|---|
| Enter / double-click | Open a folder, or open a file with its program (extracted to a temporary folder) |
| Ctrl+Enter | On a filter result: show it in its folder |
| Backspace / Alt+Up | Up one folder (or leave the filter) |
| Typing in the list, Ctrl+F | Filter names across the whole archive |
| Ctrl+E | Extract the selection, or the whole folder / filter result if nothing is selected |
| Esc | Clear the filter; cancel an extraction in the progress window |
| Ctrl+O | Open an archive |

## Command-line modes

Used by the Explorer context menu (step 7); they never open the main window, only a small progress window when the extraction takes more than 300 ms.

| Command | Extracts to |
|---|---|
| `kibotos.exe archive` | (opens the main window) |
| `kibotos.exe --extract-here archive...` | Next to the archive; into a folder named after it if the archive has several loose items at its root |
| `kibotos.exe --extract-to-folder archive...` | `<archive folder>\<archive name>\` |
| `kibotos.exe --extract-dialog archive...` | A folder chosen in a dialog |
| `kibotos.exe --integrate` / `--unintegrate` | Add / remove the Windows integration |

Exit code: 0 success, 1 error, 2 cancelled.

## Windows integration

*Herramientas › Integrar con Windows* (or `kibotos.exe --integrate`) registers, for the current user only and without administrator rights:

- a **Kibotós** submenu on archives (zip, jar, apk, cbz, 7z, rar, cbr, tar, gz, tgz, bz2, tbz, tbz2, xz, txz, zst, tzst) with *Abrir con Kibotós*, *Extraer aquí*, *Extraer en una carpeta con su nombre* and *Extraer ficheros...*; on Windows 11 it is under *Mostrar más opciones*;
- Kibotós as an option in *Abrir con* and in *Configuración › Aplicaciones › Aplicaciones predeterminadas*.

It never changes the default program: Windows protects that choice, so pick Kibotós once via *Abrir con › Elegir otra aplicación › Siempre*. The registered path is the .exe's current location; integrate again after moving it. *Quitar integración* (or `--unintegrate`) deletes everything it wrote under `HKEY_CURRENT_USER`, keeping other programs' entries.

Labels are fixed text because registry verbs cannot show the archive name ("Extraer en «fotos»\" needs a shell extension, planned with the Windows 11 menu).

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
