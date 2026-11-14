//! In-memory folder tree, built once per archive so navigation and filtering
//! never touch the disk again.
//!
//! Layout is flat to keep 100,000 entries cheap: names live in two string
//! arenas (original and lowercase) and children are stored as one
//! compressed-sparse-row array instead of a `Vec` per folder.

use std::collections::HashMap;

use crate::{Entry, EntryId, EntryKind};

pub type NodeId = u32;

const NO_ENTRY: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
struct Node {
    parent: NodeId,
    name: (u32, u32),
    lower: (u32, u32),
    entry: u32,
    is_dir: bool,
    children: (u32, u32),
}

#[derive(Debug, Default)]
pub struct ArchiveIndex {
    nodes: Vec<Node>,
    names: String,
    lower: String,
    children: Vec<NodeId>,
}

struct Builder<'a> {
    nodes: Vec<Node>,
    names: String,
    lower: String,
    dirs: HashMap<&'a str, NodeId>,
    last_dir: (&'a str, NodeId),
}

impl<'a> Builder<'a> {
    fn push(&mut self, parent: NodeId, name: &str, entry: u32, is_dir: bool) -> NodeId {
        let start = self.names.len() as u32;
        self.names.push_str(name);
        let lstart = self.lower.len() as u32;
        if name.is_ascii() {
            self.lower.extend(name.bytes().map(|b| b.to_ascii_lowercase() as char));
        } else {
            self.lower.push_str(&name.to_lowercase());
        }
        let node = Node {
            parent,
            name: (start, self.names.len() as u32 - start),
            lower: (lstart, self.lower.len() as u32 - lstart),
            entry,
            is_dir,
            children: (0, 0),
        };
        self.nodes.push(node);
        (self.nodes.len() - 1) as NodeId
    }

    /// Returns the node for a folder path, creating missing folders.
    fn dir(&mut self, path: &'a str) -> NodeId {
        if path.is_empty() {
            return ArchiveIndex::ROOT;
        }
        if self.last_dir.0 == path {
            return self.last_dir.1;
        }
        let id = match self.dirs.get(path) {
            Some(&id) => id,
            None => {
                let (parent_path, name) = split_last(path);
                let parent = self.dir(parent_path);
                let id = self.push(parent, name, NO_ENTRY, true);
                self.dirs.insert(path, id);
                id
            }
        };
        self.last_dir = (path, id);
        id
    }
}

fn split_last(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

impl ArchiveIndex {
    pub const ROOT: NodeId = 0;

    pub fn build(entries: &[Entry]) -> ArchiveIndex {
        let mut b = Builder {
            nodes: Vec::with_capacity(entries.len() + 1),
            names: String::with_capacity(entries.len() * 16),
            lower: String::with_capacity(entries.len() * 16),
            dirs: HashMap::new(),
            last_dir: ("", ArchiveIndex::ROOT),
        };
        b.push(0, "", NO_ENTRY, true);

        for e in entries {
            // Absolute paths are shown relative to the root; extraction
            // still rejects them because it validates `Entry::path`.
            let path = e.path.trim_start_matches('/');
            if path.is_empty() {
                continue;
            }
            if e.kind == EntryKind::Dir {
                let id = b.dir(path);
                if b.nodes[id as usize].entry == NO_ENTRY {
                    b.nodes[id as usize].entry = e.id;
                }
            } else {
                let (parent_path, name) = split_last(path);
                let parent = b.dir(parent_path);
                b.push(parent, name, e.id, false);
            }
        }

        // Children in CSR form, preserving archive order within each folder.
        let n = b.nodes.len();
        let mut counts = vec![0u32; n];
        for node in &b.nodes[1..] {
            counts[node.parent as usize] += 1;
        }
        let mut offset = 0u32;
        for (i, c) in counts.iter_mut().enumerate() {
            b.nodes[i].children = (offset, *c);
            offset += *c;
            *c = 0;
        }
        let mut children = vec![0 as NodeId; n - 1];
        for id in 1..n {
            let p = b.nodes[id].parent as usize;
            let (start, _) = b.nodes[p].children;
            children[(start + counts[p]) as usize] = id as NodeId;
            counts[p] += 1;
        }

        ArchiveIndex { nodes: b.nodes, names: b.names, lower: b.lower, children }
    }

    /// Number of nodes, including the root.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.len() <= 1
    }

    pub fn children(&self, node: NodeId) -> &[NodeId] {
        let (s, l) = self.nodes[node as usize].children;
        &self.children[s as usize..(s + l) as usize]
    }

    pub fn parent(&self, node: NodeId) -> Option<NodeId> {
        (node != Self::ROOT).then(|| self.nodes[node as usize].parent)
    }

    pub fn name(&self, node: NodeId) -> &str {
        let (s, l) = self.nodes[node as usize].name;
        &self.names[s as usize..(s + l) as usize]
    }

    /// Lowercase name, for case-insensitive sorting and filtering.
    pub fn name_lower(&self, node: NodeId) -> &str {
        let (s, l) = self.nodes[node as usize].lower;
        &self.lower[s as usize..(s + l) as usize]
    }

    pub fn is_dir(&self, node: NodeId) -> bool {
        self.nodes[node as usize].is_dir
    }

    /// The archive entry behind a node. Folders that exist only implicitly
    /// (no entry of their own in the archive) return `None`.
    pub fn entry(&self, node: NodeId) -> Option<EntryId> {
        let e = self.nodes[node as usize].entry;
        (e != NO_ENTRY).then_some(e)
    }

    /// Full path of a node inside the archive, `/`-separated.
    pub fn path(&self, node: NodeId) -> String {
        let mut parts = Vec::new();
        let mut n = node;
        while n != Self::ROOT {
            parts.push(self.name(n));
            n = self.nodes[n as usize].parent;
        }
        parts.reverse();
        parts.join("/")
    }

    /// Finds a node by its `/`-separated path.
    pub fn find(&self, path: &str) -> Option<NodeId> {
        let mut node = Self::ROOT;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            node = *self.children(node).iter().find(|&&c| self.name(c) == comp)?;
        }
        Some(node)
    }

    /// Entries of a node and everything below it, for extracting a folder.
    pub fn entries_under(&self, node: NodeId, out: &mut Vec<EntryId>) {
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if let Some(e) = self.entry(n) {
                out.push(e);
            }
            stack.extend_from_slice(self.children(n));
        }
    }

    /// Case-insensitive substring search over every node name in the
    /// archive (not only the current folder). Results are in node order.
    pub fn search(&self, query: &str, out: &mut Vec<NodeId>) {
        out.clear();
        let q = query.to_lowercase();
        if q.is_empty() {
            return;
        }
        let finder = q.as_str();
        for id in 1..self.nodes.len() as NodeId {
            if self.name_lower(id).contains(finder) {
                out.push(id);
            }
        }
    }

    /// Narrows a previous result when the query grew (the user typed one
    /// more character): only earlier matches can still match.
    pub fn refine(&self, query: &str, results: &mut Vec<NodeId>) {
        let q = query.to_lowercase();
        results.retain(|&id| self.name_lower(id).contains(q.as_str()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(paths: &[(&str, EntryKind)]) -> Vec<Entry> {
        paths.iter().enumerate().map(|(i, (p, k))| Entry::new(i, p, *k)).collect()
    }

    #[test]
    fn builds_tree_with_implicit_folders() {
        use EntryKind::*;
        let e = entries(&[("a/b/c.txt", File), ("a/d.txt", File), ("a/b", Dir), ("e.txt", File)]);
        let ix = ArchiveIndex::build(&e);
        let root: Vec<_> = ix.children(ArchiveIndex::ROOT).iter().map(|&n| ix.name(n)).collect();
        assert_eq!(root, ["a", "e.txt"]);
        let a = ix.find("a").unwrap();
        assert!(ix.is_dir(a));
        assert_eq!(ix.entry(a), None);
        let b = ix.find("a/b").unwrap();
        assert_eq!(ix.entry(b), Some(2));
        assert_eq!(ix.children(a).len(), 2);
        assert_eq!(ix.path(ix.find("a/b/c.txt").unwrap()), "a/b/c.txt");
        assert_eq!(ix.parent(b), Some(a));
        let mut under = Vec::new();
        ix.entries_under(a, &mut under);
        under.sort();
        assert_eq!(under, [0, 1, 2]);
    }

    #[test]
    fn search_is_global_and_case_insensitive() {
        use EntryKind::*;
        let e = entries(&[("Docs/Report.PDF", File), ("x/report-2.txt", File), ("Ñandú.txt", File)]);
        let ix = ArchiveIndex::build(&e);
        let mut r = Vec::new();
        ix.search("REPORT", &mut r);
        assert_eq!(r.len(), 2);
        ix.refine("report-", &mut r);
        assert_eq!(r.len(), 1);
        ix.search("ñan", &mut r);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn backslashes_and_duplicates() {
        use EntryKind::*;
        let e = entries(&[("a\\b.txt", File), ("a/b.txt", File)]);
        let ix = ArchiveIndex::build(&e);
        assert_eq!(ix.children(ix.find("a").unwrap()).len(), 2);
    }
}
