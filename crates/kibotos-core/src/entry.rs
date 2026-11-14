/// Index of an entry in [`crate::ArchiveReader::entries`].
pub type EntryId = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    File,
    Dir,
    /// Symbolic or hard link. Never created on disk by Kibotos v0.1.
    Link,
}

/// Modification time of an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timestamp {
    /// Seconds since 1970-01-01 00:00:00 UTC (7z, tar, RAR5).
    Utc(i64),
    /// Wall-clock seconds since 1970-01-01 00:00:00 with no time zone
    /// (ZIP and RAR4 DOS times, which store the creator's local time).
    Local(i64),
}

impl Timestamp {
    /// Approximate key for sorting mixed timestamps.
    pub fn sort_key(self) -> i64 {
        match self {
            Timestamp::Utc(s) | Timestamp::Local(s) => s,
        }
    }

    /// Wall-clock fields (year, month, day, hour, minute, second).
    /// UTC timestamps are returned in UTC; the UI converts them for display.
    pub fn civil(self) -> (i32, u32, u32, u32, u32, u32) {
        crate::time::civil_from_secs(self.sort_key())
    }
}

/// One item in an archive, as read from the archive index.
#[derive(Clone, Debug)]
pub struct Entry {
    pub id: EntryId,
    /// Path inside the archive with `/` separators, no leading `./` and no
    /// trailing `/`. It is still untrusted: it may contain `..`, drive
    /// letters or device names, which extraction rejects.
    pub path: Box<str>,
    pub kind: EntryKind,
    /// Uncompressed size, when the format records it.
    pub size: Option<u64>,
    /// Compressed size, when the format records it per entry.
    pub packed: Option<u64>,
    pub modified: Option<Timestamp>,
    pub crc: Option<u32>,
    pub encrypted: bool,
    pub link_target: Option<Box<str>>,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }

    /// Last path component.
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    /// An entry with only a path and kind; the path is normalized.
    pub fn new(id: usize, raw_path: &str, kind: EntryKind) -> Entry {
        Entry {
            id: id as EntryId,
            path: normalize_path(raw_path).into(),
            kind,
            size: None,
            packed: None,
            modified: None,
            crc: None,
            encrypted: false,
            link_target: None,
        }
    }
}

/// Converts `\` to `/`, removes `./` prefixes and leading/trailing slashes
/// that carry no meaning, and collapses repeated slashes. Leading slashes are
/// kept as one so that absolute paths stay detectable by the safety check.
pub(crate) fn normalize_path(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let absolute = raw.starts_with('/') || raw.starts_with('\\');
    if absolute {
        out.push('/');
    }
    for comp in raw.split(['/', '\\']) {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if !out.is_empty() && !out.ends_with('/') {
            out.push('/');
        }
        out.push_str(comp);
    }
    // Keep UNC paths (`\\server\share`) recognisable as absolute.
    if raw.starts_with("\\\\") || raw.starts_with("//") {
        out.insert(0, '/');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::normalize_path;

    #[test]
    fn normalizes_separators() {
        assert_eq!(normalize_path("a\\b/c/"), "a/b/c");
        assert_eq!(normalize_path("./a//b"), "a/b");
        assert_eq!(normalize_path("/etc/passwd"), "/etc/passwd");
        assert_eq!(normalize_path("\\\\srv\\share\\x"), "//srv/share/x");
        assert_eq!(normalize_path("../x"), "../x");
    }
}
