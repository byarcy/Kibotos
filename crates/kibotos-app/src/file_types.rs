// Shared with build.rs (through `include!`), which embeds one document icon
// per extension: keep this file free of inner attributes and `use` lines.

/// Extensions Kibotós offers to open. `.jar` and `.apk` get the context
/// menu and "Abrir con", which never changes how they run by default.
pub const EXTENSIONS: &[&str] = &[
    "zip", "jar", "apk", "cbz", "7z", "rar", "cbr", "tar", "gz", "tgz", "bz2", "tbz", "tbz2", "xz", "txz", "zst", "tzst",
    "lz4", "tlz4", "lzma", "tlz", "lz", "z", "taz", "iso", "cab", "msi", "001",
];

/// Resource id of the document icon of `EXTENSIONS[i]` is this plus `i`
/// (`assets/types/<ext>.ico`). The app icon is id 1.
pub const TYPE_ICON_BASE: u16 = 101;
