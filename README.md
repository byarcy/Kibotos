# Kibotos

A fast, lightweight archive manager for Windows, written in Rust.

Status: **v0.4** — opens and extracts many formats, creates ZIP, 7z and tar archives, and changes them: add, delete, rename, new folders, and files edited in their program saved back into the archive. The single .exe installs itself for the current user. The deliverable is `dist\kibotos-v0.4.exe` (see *Build and test*).

## Install

1. Download `kibotos-v0.4.exe` from the [latest release](https://github.com/byarcy/Kibotos/releases/latest).
2. Open it. The .exe is not signed, so Windows may show *Windows protected your PC*: choose *More info › Run anyway*.
3. Choose *Install*. Kibotós copies itself to `%LOCALAPPDATA%\Programs\Kibotos` (no administrator rights), adds a Start menu shortcut, an entry in *Settings › Apps* (where it is uninstalled) and the Explorer menus. The downloaded file can then be deleted.

To open archives with a double click: right-click one › *Open with › Choose another app › Kibotós › Always* (Windows does not let a program make itself the default). Opening a newer download later offers to update the installed copy, keeping settings and integration. It also runs without installing, from any folder.

The interface is in Spanish. Menu, button and dialog names in this README are translated (for example *Tools › Integrate with Windows* is *Herramientas › Integrar con Windows* on screen).

## macOS

A macOS version (v0.1, branch `macos`) runs on macOS 11 Big Sur or later, on Apple silicon and Intel Macs (one universal app). It shares the archive engine with the Windows version and has its own interface, in English: open and browse an archive (folder tree and file list), extract everything or the selection, create ZIP, 7z and tar archives, progress with cancel, password prompts, and drag and drop to and from Finder. Changing an existing archive is not in v0.1.

**Install.**

1. Download `kibotos-mac-v0.1.dmg`: from the latest successful run of the *Build* workflow in the *Actions* tab (artifact `kibotos-mac-dmg`, a .zip that contains the .dmg), or from a release once one is published.
2. Open the .dmg and drag *Kibotos* onto *Applications*.
3. Open Kibotos from *Applications*. The app is not notarized by Apple (it has only an ad-hoc signature), so the first time macOS says it cannot verify it. Choose *Done*, then open *System Settings › Privacy & Security*, scroll down to the message about Kibotos and click *Open Anyway*, then confirm with your password. Later launches open normally.

To open archives with a double click: select one in Finder, *File › Get Info › Open with › Kibotos › Change All*. *Open With* in the right-click menu and dropping archives on the Dock icon work too.

Files extracted from a downloaded archive get its quarantine mark (`com.apple.quarantine`), the macOS counterpart of the Windows Mark of the Web, so Gatekeeper still checks programs and scripts that came from the internet. Unix permissions are not kept yet: extracted programs and scripts lose their executable bit.

The app is built in GitHub Actions (`.github/workflows/build.yml`) with `scripts/macos/bundle.sh`, which needs a Mac; see *Build and test*.

## Layout

| Path | What |
|---|---|
| `crates/kibotos-core` | Format backends behind one `ArchiveReader` trait, the in-memory folder index, path validation and extraction. No UI code. |
| `crates/kibotos-core/src/bin/kibotos-cli.rs` | Console tool to list and extract archives with timings. |
| `crates/kibotos-app` | `kibotos.exe`: native Win32 window with a virtual ListView. UI text lives in `src/strings.rs`. |
| `crates/kibotos-app/assets` | Icons: `make_icons.ps1` draws the app icon and one icon per file type from `logo-source.png`. |
| `licenses/unRAR-license.txt` | License of the unRAR code used to read RAR archives. |

The release build is a single `kibotos.exe` with the C runtime linked statically (see `.cargo/config.toml`), so it needs no Visual C++ Redistributable.

Files opened from an archive go to `%TEMP%\Kibotos\<process>`, removed on exit (leftovers from closed instances are removed at the next start). If an opened file was saved by its program, Kibotós offers to save it back into the archive when you return to its window, and again on exit (for archives it cannot change, a copy next to the archive). Extracted and opened files inherit the archive's Mark of the Web, so SmartScreen and Office Protected View still apply.

Set `KIBOTOS_TIMING=<file>` to log startup timings (milliseconds since process creation) to that file.

## Disk navigation

Without an argument the window starts in **This PC** (Desktop, Downloads, Documents and the drives). Folders open in the window, archives open inside Kibotós and other files open with their program. Backspace at an archive's root returns to the folder that holds it, and keeps going up to the drive and *This PC*. The filter applies to the current folder on disk and to the whole archive inside one. Folder listings are read on a worker thread; hidden and system files are not shown.

The path bar (Ctrl+L or Alt+D) accepts a folder, an archive, or a folder inside an archive, such as `D:\Downloads\photos.zip\2026`. On disk, the context menu offers *Extract here*, *Extract to "name"* and *Extract...* for one or several archives (extracted one after another), *Show in Explorer*, *Copy path* and the shell's *Properties*. The disk view never moves, renames or deletes files: dragging files out of it copies them.

For any selection, the context menu also offers *Compress to "name.zip"*, *Compress to "name.7z"* and *Compress...* (also *File › Compress...*, Ctrl+N); see *Creating archives*.

## Keyboard

| Key | Action |
|---|---|
| Enter / double-click | Open a folder, or open a file with its program (extracted to a temporary folder) |
| Ctrl+Enter | On a filter result: show it in its folder |
| Backspace / Alt+Up | Up one folder (or leave the filter); from an archive's root to its folder on disk |
| Typing in the list, Ctrl+F | Filter names across the whole archive |
| Ctrl+E | Extract the selection, or the whole folder / filter result if nothing is selected |
| Ctrl+N | Disk view: compress the selection (the *Compress* dialog) |
| Del | Archive view: remove the selection from the archive (asks first) |
| F2, or a slow second click | Archive view: rename a file or folder |
| Ctrl+Shift+N | Archive view: new folder in the current folder |
| Esc | Clear the filter; cancel an extraction in the progress window |
| Ctrl+O | Open an archive |
| Ctrl+L, Alt+D | Type a path in the path bar |
| F5 | Read the folder or archive again |
| Ctrl+A | Select all rows |
| Alt+Enter | Properties of the selection (sizes, ratio, date, CRC, encryption) |
| Menu key, Shift+F10, right-click | Context menu: Open, Open with..., Extract..., Extract next to the archive, Show in its folder, Copy path, Compute SHA-256, Properties, Rename, Delete, New folder |

The archive view has a **CRC32** column, the checksum each ZIP, 7z or RAR entry stores (sortable; tar and single compressed files have none). *Compute SHA-256* decompresses the selected files on a worker thread and shows their SHA-256, with a button that copies them in `sha256sum` format (`<hash> *<path>`).

## Drag and drop

Drop an archive on the window to open it. Drop files or folders on an open archive to **add** them: Kibotós asks first, and says how many existing items will be replaced; dropped on a folder row, they go into that folder. Drag rows out of the list to extract them:

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
| `kibotos.exe --compress-zip item...` | Creates `name.zip` next to the items |
| `kibotos.exe --compress-7z item...` | Creates `name.7z` next to the items |
| `kibotos.exe --compress-dialog item...` | Opens the *Compress* dialog |
| `kibotos.exe --integrate` / `--unintegrate` | Add / remove the Windows integration (settings are kept) |
| `kibotos.exe --install` | Install this .exe for the current user (see *Installing and updating*) |
| `kibotos.exe --uninstall [--quiet]` | Uninstall: what *Settings › Apps › Uninstall* runs |

Exit code: 0 success, 1 error, 2 cancelled.

## Windows integration

*Tools › Integrate with Windows* (or `kibotos.exe --integrate`) registers, for the current user only and without administrator rights:

- a **Kibotós** submenu on archives (zip, jar, apk, cbz, 7z, rar, cbr, tar, gz, tgz, bz2, tbz, tbz2, xz, txz, zst, tzst, lz4, tlz4, lzma, tlz, lz, Z, taz, iso, cab, msi, 001) with *Open with Kibotós*, *Extract here*, *Extract to folder* and *Extract files...*; on Windows 11 it is under *Show more options*;
- a **Compress with Kibotós** submenu on every file and folder, with *Compress to ZIP*, *Compress to 7z* and *Compress...*;
- Kibotós as an option in *Open with* and in *Settings › Apps › Default apps*, with **an icon per type**: a page with a colored band (blue ZIP family, green 7z, purple RAR, orange to red for tar and compressors, gray for disc images and Windows packages), the extension and the logo. Each extension has its own ProgID (`Kibotos.zip`, `Kibotos.7z`...) pointing to its icon inside the .exe.

Explorer starts a context-menu command once per selected item, so choosing *Compress to ZIP* with 40 files selected starts 40 processes. They find each other (a named mutex and a message-only window per folder and command), hand their paths to the first one and exit, and that one makes a single archive once no new path has arrived for 0.4 s.

It never changes the default program: Windows protects that choice, so pick Kibotós once via *Open with › Choose another app › Always*. Versions up to 0.2 used a single ProgID (`Kibotos.Archive`), which integrating again removes: a default chosen with those versions has to be chosen again. The registered path is the .exe's current location; integrate again after moving it. *Quitar integración* (or `--unintegrate`) deletes everything it wrote under `HKEY_CURRENT_USER`, keeping other programs' entries.

Labels are fixed text: registry verbs cannot show the archive name. Showing it (*Extract to "photos"*) needs a shell extension, a DLL loaded into Explorer, and the Windows 11 top-level menu also needs a signed package; both are left for a later version. Inside Kibotós the disk-view menu does show the name.

## Installing and updating

The offer to install appears only when the running .exe came from the internet (it carries the Mark of the Web), so copies built or copied locally never ask; they can be installed from *Tools › Install on this PC*. Installing copies the .exe to `%LOCALAPPDATA%\Programs\Kibotos\kibotos.exe` (on the system drive, which is usually the fastest: extraction from an .exe on an SSD measured 23–28 % faster than from a hard disk), removes its Mark of the Web, integrates it, and writes the shortcut and the uninstall entry under `HKEY_CURRENT_USER`. A running installed copy is renamed aside and replaced (a running .exe can be renamed but not overwritten), and the old file is removed the next time the installed copy starts. Uninstalling removes the program, the shortcut, the integration and the settings; a hidden command deletes the folder once the process has ended.

**New versions.** When installing, a checkbox (ticked) turns on a daily check: a moment after the window appears, a worker thread asks GitHub's API for the latest release, at most once a day, and a dialog offers its page (*See the new version*, *Not now*, *Skip this version*). *Help › Check for updates* checks at any time; *Help › Notify me of new versions* turns the daily check on or off. It uses WinHTTP from Windows (no TLS library inside Kibotós), delay-loaded so it costs nothing at startup; the request carries only the user agent `Kibotos/<version>`.

## Formats (read and extract)

| Family | Formats |
|---|---|
| Archives | ZIP (and jar, apk, cbz), 7z, RAR (and cbr), tar |
| Compressors, alone or around tar | gzip, bzip2, xz, zstd, lz4, lzma, lzip (`.lz`), Unix compress (`.Z`) |
| Disc images | ISO 9660 (with Joliet and Rock Ridge names) and UDF, including DVD/Blu-ray and >4 GiB images |
| Windows | CAB (stored, MSZIP, LZX), MSI (the files it installs, with their real folders) |
| Split archives | `.7z.001`, `.zip.001`, `.tar.gz.001`..., spanned ZIP (`.z01`... `.zip`), RAR volumes (`.part1.rar`, `.r00`) |

Formats are detected by content, not by extension. Any part of a split archive opens the whole set; a missing part is reported by name. MSI packages show the files they install (from their File, Component and Directory tables), not their internal streams.

RAR support is **extraction only** and always will be: the unRAR license forbids using its code to create a RAR-compatible compressor.

## Creating archives

| Format | Notes |
|---|---|
| ZIP | Deflate; files that do not shrink are stored. Zip64 for large files and archives. Optional AES-256 password. |
| 7z | LZMA2 in solid blocks, BCJ x86 filter for programs (`.exe`, `.dll`...). Optional AES-256 password, with the file names encrypted too by default. |
| tar.gz, tar.xz, tar.zst, tar.bz2, tar | No password. xz and zstd compress on several threads. |

Levels: *No compression*, *Fast*, *Normal* and *Maximum*. The dialog remembers the last format and level (in `HKCU\Software\Kibotos\Settings`).

The archive is written to `name.kibotos-part` and renamed when complete, so cancelling or an error never leaves a half-written archive. Links and junctions inside the folders are not followed (they are listed as skipped), files that cannot be read are skipped and listed at the end, and an archive created inside one of its own folders does not include itself.

## Changing archives

| Format | How it is changed |
|---|---|
| ZIP (jar, apk, cbz) | Entries that stay are **copied still compressed** (and still encrypted), so renaming or deleting in a large ZIP costs little more than copying the file. Added and replaced files are compressed at the remembered level; in an encrypted archive they are encrypted too (AES-256, after checking the password). |
| tar, tar.gz, tar.bz2, tar.xz, tar.zst | Rewritten in one pass from the old stream to the new one; kept entries keep their headers (links, modes, owners). |
| 7z | Kept entries are extracted to a hidden folder next to the archive and everything is compressed again (solid blocks cannot be cut). Encrypted archives stay encrypted, and an archive whose file names were encrypted keeps them encrypted. |
| RAR, ISO, CAB, MSI, single compressed files, split archives | Read-only. RAR never: its license forbids creating RAR archives. |

The new archive is written to `name.kibotos-part`, opened again to check that it lists what was planned (ZIP and 7z), and only then renamed over the original: a failure, a cancellation or a full disk leaves the original untouched. Changes in the list: *Delete* (Del), *Rename* (F2; a folder takes its content along; names Windows reserves or cannot store are refused), *New folder* (Ctrl+Shift+N, then type its name), and files dropped on the list. Files opened from the archive and saved by their program are offered back when Kibotós comes to the front (*Update the archive* or *Not now*; the latter is offered again on exit).

ZIP passwords have no defined encoding: 7-Zip and most Windows tools use the ANSI code page. Kibotós writes the password that way when it can (so 7-Zip opens the archive) and, when reading, tries UTF-8 and then ANSI. The dialog warns that a ZIP password with accents may not open in other programs; 7z has no such problem.

## Performance

Measured on the release build run from an NVMe SSD (Windows 11, 12 logical cores; medians of 10 cold starts). Times are from process creation as Windows records it (`KIBOTOS_TIMING`).

| Target | Result |
|---|---|
| Cold start under 100 ms | Window visible in **56 ms**; *This PC* listed at 106 ms |
| List a 100,000-entry ZIP under 300 ms | **77 ms** to open, index and sort on the worker thread; rows on screen 131 ms after launch |
| Filter under 16 ms per keystroke | **2–15 ms** on 100,000 entries (worst case: every row matches), 8–11 ms of it repainting the list |
| Single .exe under 5 MB, no runtime dependencies | **4.8 MB** with 29 icons (0.8 MB; the 256 px images are 256-color PNG), C runtime linked statically |

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

Creating archives, compared with 7-Zip 26.05 (`kibotos-cli create` vs `7z a`, *Normal* vs `-mx5`, best of two runs):

| Input | Kibotós | 7-Zip |
|---|---|---|
| ZIP, 260 files, 47 MB (cemu) | 0.74 s, 25.4 MB | 1.67 s, 24.4 MB |
| ZIP, 2,231 files, 62 MB (dolphin) | 0.84 s, 27.1 MB | 1.72 s, 25.6 MB |
| ZIP, 20,000 small files, 73 MB | 1.73 s, 9.4 MB | 1.56 s, 9.6 MB |
| 7z, the three folders together (182 MB) | 18.3 s, 38.3 MB | 33.1 s, 33.4 MB |
| 7z *Maximum* vs `-mx5`, same 182 MB | 40.2 s, 32.9 MB | 33.1 s, 33.4 MB |
| 7z *Fast* vs `-mx1`, same 182 MB | 6.9 s, 46.5 MB | 4.0 s, 47.4 MB |

ZIP is about twice as fast as 7-Zip, with archives about 4% larger. For 7z, Kibotós compresses independent blocks in parallel: on a large input it finishes sooner, at the cost of a slightly larger archive; on a single folder of about 50 MB (one or two blocks) 7-Zip's faster LZMA encoder wins (cemu: 10 s against 6.9 s).

Changing the cemu archive (260 files, 47 MB; remove a folder, rename one with 238 entries, add 5 files, a new folder and a replaced file, all at once, *Normal*): ZIP 0.05 s, tar 0.03 s, tar.zst 0.19 s, tar.gz 0.55 s, tar.xz 10.7 s, 7z 11.4 s. Every result passes `7z t` and extracts identical to the expected tree.

Versions are compared on the same archives with the console tools saved under `dist\versiones` (see `dist\benchmarks\HISTORIAL.md`): v0.3 extracts as fast as v0.2 and its window appears as fast (67.9 ms against 65.6 ms, medians of 7, ranges overlapping). v0.4 adds installing and the update check for 107 KB and no measurable startup time (61.7 ms against 63.3 ms for v0.3, medians of 9); `winhttp.dll` is not loaded at startup.

## Build and test

Requires Rust stable and the MSVC Build Tools (C++ workload).

```powershell
.\package.ps1
```

builds the release and writes `dist\kibotos-v<version>.exe` (version from `Cargo.toml`, e.g. `kibotos-v0.4.exe`).

```bash
cargo test -p kibotos-core
```

```bash
cargo build --release
```

On a Mac (Xcode command line tools, and `rustup target add aarch64-apple-darwin x86_64-apple-darwin`):

```bash
scripts/macos/bundle.sh
```

builds the universal app, signs it ad hoc and writes `build/kibotos-mac-v<version>.dmg` (version from `crates/kibotos-mac/Cargo.toml`).

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
target/release/kibotos-cli create backup.7z Documents Photos -f 7z -l max -p PASSWORD
```

```bash
target/release/kibotos-cli update backup.zip --rm old --mv docs=notas --add report.pdf --into notas --mkdir nueva
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
