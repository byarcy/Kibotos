//! Sorting and filtering of the folder index, kept free of UI code so it
//! can be tested and benchmarked.
//!
//! Sorting a large archive by a string key costs tens of milliseconds, far
//! too much per keystroke. So the whole index is sorted once per sort key
//! ([`Ordering`]), and every later operation reuses that order:
//! - a folder's rows are its children sorted by their precomputed rank
//!   (integer comparisons only);
//! - filter results are collected by walking the global order, so they come
//!   out already sorted and no sort runs while typing.

use crate::{ArchiveIndex, Entry, NodeId};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortKey {
    #[default]
    Name,
    Size,
    Packed,
    Modified,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

/// Every node of an index (root excluded) in display order for one [`Sort`].
/// Folders always come before files, as in Explorer and WinRAR.
pub struct Ordering {
    pub sort: Sort,
    order: Vec<NodeId>,
    /// node -> position in `order` (root: 0).
    rank: Vec<u32>,
}

fn entry_of<'a>(index: &ArchiveIndex, entries: &'a [Entry], node: NodeId) -> Option<&'a Entry> {
    index.entry(node).map(|id| &entries[id as usize])
}

impl Ordering {
    pub fn new(index: &ArchiveIndex, entries: &[Entry], sort: Sort) -> Ordering {
        let mut order: Vec<NodeId> = (1..index.len() as NodeId).collect();
        let by_name = |a: NodeId, b: NodeId| index.name_lower(a).cmp(index.name_lower(b)).then(a.cmp(&b));
        let key = |n: NodeId| -> i64 {
            let e = entry_of(index, entries, n);
            match sort.key {
                SortKey::Name => 0,
                SortKey::Size => e.and_then(|e| e.size).map_or(-1, |v| v as i64),
                SortKey::Packed => e.and_then(|e| e.packed).map_or(-1, |v| v as i64),
                SortKey::Modified => e.and_then(|e| e.modified).map_or(i64::MIN, |t| t.sort_key()),
            }
        };
        order.sort_unstable_by(|&a, &b| {
            let (da, db) = (index.is_dir(a), index.is_dir(b));
            if da != db {
                return db.cmp(&da);
            }
            // Folders have no size, so for size keys they stay sorted by name.
            let primary = if sort.key == SortKey::Name || (da && matches!(sort.key, SortKey::Size | SortKey::Packed)) {
                by_name(a, b)
            } else {
                key(a).cmp(&key(b)).then_with(|| by_name(a, b))
            };
            if sort.descending { primary.reverse() } else { primary }
        });
        let mut rank = vec![0u32; index.len()];
        for (pos, &n) in order.iter().enumerate() {
            rank[n as usize] = pos as u32;
        }
        Ordering { sort, order, rank }
    }

    pub fn rank(&self, node: NodeId) -> u32 {
        self.rank[node as usize]
    }

    /// Children of `folder` in display order.
    pub fn folder_rows(&self, index: &ArchiveIndex, folder: NodeId, out: &mut Vec<NodeId>) {
        out.clear();
        out.extend_from_slice(index.children(folder));
        out.sort_unstable_by_key(|&n| self.rank(n));
    }

    /// Every node in the archive whose name contains `query` (case-insensitive),
    /// in display order.
    pub fn filter_rows(&self, index: &ArchiveIndex, query: &str, out: &mut Vec<NodeId>) {
        out.clear();
        let q = query.to_lowercase();
        if q.is_empty() {
            return;
        }
        out.extend(self.order.iter().copied().filter(|&n| index.name_lower(n).contains(q.as_str())));
    }
}

/// Narrows `rows` (the result for an earlier query) to a longer query that
/// contains it: only earlier matches can still match, and order is kept.
pub fn refine_rows(index: &ArchiveIndex, query: &str, rows: &mut Vec<NodeId>) {
    let q = query.to_lowercase();
    rows.retain(|&n| index.name_lower(n).contains(q.as_str()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EntryKind, Timestamp};

    fn sample() -> (Vec<Entry>, ArchiveIndex) {
        let mut v = Vec::new();
        for (i, (p, k, size, t)) in [
            ("b.txt", EntryKind::File, 30, 3),
            ("A.txt", EntryKind::File, 10, 1),
            ("zdir/x.bin", EntryKind::File, 5, 9),
            ("c.TXT", EntryKind::File, 20, 2),
            ("adir/y.txt", EntryKind::File, 1, 5),
        ]
        .into_iter()
        .enumerate()
        {
            let mut e = Entry::new(i, p, k);
            e.size = Some(size);
            e.modified = Some(Timestamp::Utc(t));
            v.push(e);
        }
        let ix = ArchiveIndex::build(&v);
        (v, ix)
    }

    fn names(ix: &ArchiveIndex, rows: &[NodeId]) -> Vec<String> {
        rows.iter().map(|&n| ix.name(n).to_owned()).collect()
    }

    #[test]
    fn folders_first_then_name() {
        let (e, ix) = sample();
        let ord = Ordering::new(&ix, &e, Sort::default());
        let mut rows = Vec::new();
        ord.folder_rows(&ix, ArchiveIndex::ROOT, &mut rows);
        assert_eq!(names(&ix, &rows), ["adir", "zdir", "A.txt", "b.txt", "c.TXT"]);
        let ord = Ordering::new(&ix, &e, Sort { key: SortKey::Name, descending: true });
        ord.folder_rows(&ix, ArchiveIndex::ROOT, &mut rows);
        assert_eq!(names(&ix, &rows), ["zdir", "adir", "c.TXT", "b.txt", "A.txt"]);
    }

    #[test]
    fn by_size_and_date() {
        let (e, ix) = sample();
        let mut rows = Vec::new();
        let ord = Ordering::new(&ix, &e, Sort { key: SortKey::Size, descending: true });
        ord.folder_rows(&ix, ArchiveIndex::ROOT, &mut rows);
        assert_eq!(names(&ix, &rows)[2..], ["b.txt", "c.TXT", "A.txt"]);
        let ord = Ordering::new(&ix, &e, Sort { key: SortKey::Modified, descending: false });
        ord.folder_rows(&ix, ArchiveIndex::ROOT, &mut rows);
        assert_eq!(names(&ix, &rows)[2..], ["A.txt", "c.TXT", "b.txt"]);
    }

    #[test]
    fn filter_is_sorted_and_global() {
        let (e, ix) = sample();
        let ord = Ordering::new(&ix, &e, Sort { key: SortKey::Size, descending: false });
        let mut rows = Vec::new();
        ord.filter_rows(&ix, "TXT", &mut rows);
        assert_eq!(names(&ix, &rows), ["y.txt", "A.txt", "c.TXT", "b.txt"]);
        refine_rows(&ix, "c.t", &mut rows);
        assert_eq!(names(&ix, &rows), ["c.TXT"]);
        ord.filter_rows(&ix, "", &mut rows);
        assert!(rows.is_empty());
    }
}
