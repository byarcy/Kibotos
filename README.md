# Kibotos

A fast, lightweight archive manager for Windows, written in Rust.

Status: **v0.1** — viewer and extractor, all 10 steps done. Archive creation is planned for v0.2. The deliverable is `dist\kibotos-v0.1.exe` (see *Build and test*).

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

## Disk navigation

Without an argument the window starts in **Este equipo** (Desktop, Downloads, Documents and the drives). Folders open in the window, archives open inside Kibotós and other files open with their program. Backspace at an archive's root returns to the folder that holds it, and keeps going up to the drive and *Este equipo*. The filter applies to the current folder on disk and to the whole archive inside one. Folder listings are read on a worker thread; hidden and system files are not shown.

The path bar (Ctrl+L or Alt+D) accepts a folder, an archive, or a folder inside an archive, such as `D:\Descargas\fotos.zip\2026`. On disk, the context menu offers *Extraer aquí*, *Extraer a "nombre"* and *Extraer...* for one or several archives (extracted one after another), *Mostrar en el Explorador*, *Copiar ruta* and the shell's *Propiedades*. The disk view never moves, renames or deletes files: dragging files out of it copies them.

## Keyboard

| Key | Action |
|---|---|
| Enter / double-click | Open a folder, or open a file with its program (extracted to a temporary folder) |
| Ctrl+Enter | On a filter result: show it in its folder |
| Backspace / Alt+Up | Up one folder (or leave the filter); from an archive's root to its folder on disk |
| Typing in the list, Ctrl+F | Filter names across the whole archive |
| Ctrl+E | Extract the selection, or the whole folder / filter result if nothing is selected |
| Esc | Clear the filter; cancel an extraction in the progress window |
| Ctrl+O | Open an archive |
| Ctrl+L, Alt+D | Type a path in the path bar |
| F5 | Read the folder or archive again |
| Ctrl+A | Select all rows |
| Alt+Enter | Properties of the selection (sizes, ratio, date, CRC, encryption) |
| Menu key, Shift+F10, right-click | Context menu: Abrir, Abrir con..., Extraer..., Extraer junto al comprimido, Mostrar en su carpeta, Copiar ruta, Propiedades |

## Drag and drop

Drop an archive on the window to open it. Drag rows out of the list to extract them:

- **To Explorer, the desktop or a file dialog**, the files are extracted **straight into the folder you drop on** (also a subfolder icon or a navigation-pane item), written once, with Kibotós' own progress, overwrite questions and safety checks. Windows does not tell a drag source where it was dropped, so the drag carries a tiny hidden marker file; when the shell copies it, its change notification reveals the exact folder, and Kibotós deletes the marker and extracts there.
- **To any other program** (mail, editors, other file managers), the rows are extracted to the temporary folder when the mouse button is released, and the program receives those files — what other archivers do for every target.

Esc cancels the drag.

## Command-line modes

Used by the Explorer context menu (step 7); they never open the main window, only a small progress window when the extraction takes more than 300 ms.

| Command | Extracts to |
|---|---|
| `kibotos.exe [archive or folder]` | (opens the main window) |
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

## Performance

Measured on the release build run from an NVMe SSD (Windows 11, 12 logical cores; medians of 10 cold starts). Times are from process creation as Windows records it (`KIBOTOS_TIMING`).

| Target | Result |
|---|---|
| Cold start under 100 ms | Window visible in **56 ms**; *Este equipo* listed at 106 ms |
| List a 100,000-entry ZIP under 300 ms | **77 ms** to open, index and sort on the worker thread; rows on screen 131 ms after launch |
| Filter under 16 ms per keystroke | **2–15 ms** on 100,000 entries (worst case: every row matches), 8–11 ms of it repainting the list |
| Single .exe under 5 MB, no runtime dependencies | **1.9 MB**, C runtime linked statically |

Sorting by another column takes 1–6 ms: the name order is computed once (on several threads) and other keys only compare integers, with names as tie-breaker; reversing a column needs no sort at all. Multi-block `.xz` files (from `xz -T` or 7-Zip) decode on several threads when the file's index shows every block is small; memory stays under 512 MiB.

Extraction compared with 7-Zip 26.05 on the same SSD (`kibotos-cli extract` vs `7z x`):

| Archive | Kibotós | 7-Zip |
|---|---|---|
| ZIP, 100,000 small files | 34 s | 90 s |
| ZIP, 37 MB with many files | 4.4 s | 11.9 s |
| 7z, 12 MB | 0.65 s | 0.56 s |
| xz, 105 MB to 323 MB, 13 blocks | 1.0 s | 0.7 s |
| gz, 323 MB of text | 1.0 s | 2.2 s |

Running the .exe from a mechanical disk makes extraction 2–3 times slower for any version (antivirus scanning on a slow disk), so keep it on an SSD.

## Build and test

Requires Rust stable and the MSVC Build Tools (C++ workload).

```powershell
.\package.ps1
```

builds the release and writes `dist\kibotos-v<version>.exe` (version from `Cargo.toml`, e.g. `kibotos-v0.1.exe`).

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
