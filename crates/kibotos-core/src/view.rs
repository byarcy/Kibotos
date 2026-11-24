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
    /// The CRC-32 the archive stores (entries without one first).
    Crc,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

/// Every node of an index (root excluded) in display order for one [`Sort`].
/// Folders always come before files, as in Explorer and WinRAR.
#[derive(Clone)]
pub struct Ordering {
    pub sort: Sort,
    order: Vec<NodeId>,
    /// node -> position in `order` (root: 0).
    rank: Vec<u32>,
}

fn entry_of<'a>(index: &ArchiveIndex, entries: &'a [Entry], node: NodeId) -> Option<&'a Entry> {
    index.entry(node).map(|id| &entries[id as usize])
}

/// Sorts `v` with `cmp` on several threads (chunks sorted in parallel,
/// then merged). String comparisons dominate the cost of opening a large
/// archive, and they split well.
fn par_sort<F>(v: &mut Vec<NodeId>, cmp: F)
where
    F: Fn(&NodeId, &NodeId) -> std::cmp::Ordering + Sync,
{
    const MIN_CHUNK: usize = 16 * 1024;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8);
    let chunks = threads.min(v.len() / MIN_CHUNK).max(1);
    if chunks == 1 {
        v.sort_unstable_by(&cmp);
        return;
    }
    let size = v.len().div_ceil(chunks);
    std::thread::scope(|s| {
        for part in v.chunks_mut(size) {
            let cmp = &cmp;
            s.spawn(move || part.sort_unstable_by(cmp));
        }
    });
    // Merge sorted runs pairwise until one is left.
    let mut runs: Vec<Vec<NodeId>> = v.chunks(size).map(<[NodeId]>::to_vec).collect();
    while runs.len() > 1 {
        let mut next = Vec::with_capacity(runs.len().div_ceil(2));
        let mut it = runs.into_iter();
        while let Some(a) = it.next() {
            match it.next() {
                Some(b) => next.push(merge(&a, &b, &cmp)),
                None => next.push(a),
            }
        }
        runs = next;
    }
    *v = runs.pop().unwrap_or_default();
}

fn merge<F>(a: &[NodeId], b: &[NodeId], cmp: &F) -> Vec<NodeId>
where
    F: Fn(&NodeId, &NodeId) -> std::cmp::Ordering,
{
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if cmp(&b[j], &a[i]) == std::cmp::Ordering::Less {
            out.push(b[j]);
            j += 1;
        } else {
            out.push(a[i]);
            i += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

impl Ordering {
    fn from_order(sort: Sort, order: Vec<NodeId>, len: usize) -> Ordering {
        let mut rank = vec![0u32; len];
        for (pos, &n) in order.iter().enumerate() {
            rank[n as usize] = pos as u32;
        }
        Ordering { sort, order, rank }
    }

    /// Sorts the index for `sort`. For keys other than the name this sorts
    /// by name first; with a name order at hand, use [`Ordering::by_key`].
    pub fn new(index: &ArchiveIndex, entries: &[Entry], sort: Sort) -> Ordering {
        let names = Ordering::by_name(index);
        let asc = if sort.key == SortKey::Name { names } else { Ordering::by_key(index, entries, sort.key, &names) };
        if sort.descending { asc.reversed(index) } else { asc }
    }

    /// Folders first, then by lowercase name (ascending).
    pub fn by_name(index: &ArchiveIndex) -> Ordering {
        let mut order: Vec<NodeId> = (1..index.len() as NodeId).collect();
        par_sort(&mut order, |&a, &b| {
            index
                .is_dir(b)
                .cmp(&index.is_dir(a))
                .then_with(|| index.name_lower(a).cmp(index.name_lower(b)))
                .then(a.cmp(&b))
        });
        Ordering::from_order(Sort::default(), order, index.len())
    }

    /// Ascending order for `key`, ties broken by `names` (an ascending
    /// name order): integer comparisons only, no strings.
    pub fn by_key(index: &ArchiveIndex, entries: &[Entry], key: SortKey, names: &Ordering) -> Ordering {
        if key == SortKey::Name {
            return names.clone();
        }
        let value = |n: NodeId| -> i64 {
            // Folders have no size, so for size keys they stay sorted by name.
            if index.is_dir(n) && matches!(key, SortKey::Size | SortKey::Packed | SortKey::Crc) {
                return 0;
            }
            let e = entry_of(index, entries, n);
            match key {
                SortKey::Name => 0,
                SortKey::Size => e.and_then(|e| e.size).map_or(-1, |v| v as i64),
                SortKey::Packed => e.and_then(|e| e.packed).map_or(-1, |v| v as i64),
                SortKey::Modified => e.and_then(|e| e.modified).map_or(i64::MIN, |t| t.sort_key()),
                SortKey::Crc => e.and_then(|e| e.crc).map_or(-1, i64::from),
            }
        };
        let mut keyed: Vec<(bool, i64, u32, NodeId)> =
            (1..index.len() as NodeId).map(|n| (!index.is_dir(n), value(n), names.rank(n), n)).collect();
        keyed.sort_unstable();
        let order = keyed.into_iter().map(|k| k.3).collect();
        Ordering::from_order(Sort { key, descending: false }, order, index.len())
    }

    /// The same key in the other direction, folders still first: each
    /// group is reversed, no comparisons needed.
    pub fn reversed(&self, index: &ArchiveIndex) -> Ordering {
        let dirs = self.order.iter().take_while(|&&n| index.is_dir(n)).count();
        let mut order = Vec::with_capacity(self.order.len());
        order.extend(self.order[..dirs].iter().rev());
        order.extend(self.order[dirs..].iter().rev());
        let sort = Sort { key: self.sort.key, descending: !self.sort.descending };
        Ordering::from_order(sort, order, self.rank.len())
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
    fn fast_paths_match_full_sorts() {
        // Many entries so the parallel sort really splits.
        let mut v = Vec::new();
        for i in 0..40_000usize {
            let p = format!("d{}/f{:05}_{}.txt", i % 37, (i * 7919) % 40_000, i % 3);
            let mut e = Entry::new(i, &p, EntryKind::File);
            e.size = Some((i % 1000) as u64);
            e.modified = Some(Timestamp::Utc((i % 500) as i64));
            e.crc = (i % 5 != 0).then_some((i * 2_654_435_761 % 4_000_000_000) as u32);
            v.push(e);
        }
        let ix = ArchiveIndex::build(&v);
        let names = Ordering::by_name(&ix);
        for key in [SortKey::Name, SortKey::Size, SortKey::Packed, SortKey::Modified, SortKey::Crc] {
            let asc = Ordering::by_key(&ix, &v, key, &names);
            let desc = asc.reversed(&ix);
            // Reference: one comparison sort over everything.
            for (ord, descending) in [(asc, false), (desc, true)] {
                let mut want: Vec<NodeId> = (1..ix.len() as NodeId).collect();
                want.sort_by(|&a, &b| {
                    let (da, db) = (ix.is_dir(a), ix.is_dir(b));
                    if da != db {
                        return db.cmp(&da);
                    }
                    let name = ix.name_lower(a).cmp(ix.name_lower(b)).then(a.cmp(&b));
                    let val = |n: NodeId| {
                        let e = ix.entry(n).map(|id| &v[id as usize]);
                        match key {
                            SortKey::Name => 0,
                            SortKey::Size | SortKey::Packed | SortKey::Crc if ix.is_dir(n) => 0,
                            SortKey::Size => e.and_then(|e| e.size).map_or(-1, |x| x as i64),
                            SortKey::Packed => e.and_then(|e| e.packed).map_or(-1, |x| x as i64),
                            SortKey::Modified => e.and_then(|e| e.modified).map_or(i64::MIN, |t| t.sort_key()),
                            SortKey::Crc => e.and_then(|e| e.crc).map_or(-1, i64::from),
                        }
                    };
                    let o = val(a).cmp(&val(b)).then(name);
                    if descending { o.reverse() } else { o }
                });
                assert_eq!(ord.order, want, "{key:?} descending={descending}");
            }
        }
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
