//! Folder tree of differing files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::scan::{Entry, Status};

pub struct Node {
    pub name: String,
    pub rel: PathBuf,
    pub is_dir: bool,
    pub status: Status,
    /// Old path of a renamed file.
    pub renamed_from: Option<PathBuf>,
    pub children: Vec<usize>,
    pub parent: Option<usize>,
    pub expanded: bool,
}

pub struct VisRow {
    pub node: usize,
    /// Tree guide characters (`│  ├─ └─`) preceding the icon.
    pub prefix: String,
}

#[derive(Default)]
pub struct Tree {
    pub nodes: Vec<Node>,
    pub roots: Vec<usize>,
    pub visible: Vec<VisRow>,
    pub selected: usize,
    pub offset: usize,
    /// Selection when the tree was last drawn; the offset follows the selection only when it changes, so the
    /// tree can be scrolled away from it with the wheel or the scrollbar.
    pub shown_selected: Option<usize>,
    /// File nodes in display (DFS) order, regardless of expansion.
    pub files: Vec<usize>,
    /// Display (DFS) position of every node.
    dfs_pos: Vec<usize>,
}

impl Tree {
    pub fn build(entries: &[Entry]) -> Self {
        let mut t = Tree::default();
        let mut dirs: HashMap<PathBuf, usize> = HashMap::new();
        for e in entries {
            let mut parent: Option<usize> = None;
            let mut acc = PathBuf::new();
            let comps: Vec<_> = e.rel.components().collect();
            for (i, c) in comps.iter().enumerate() {
                acc.push(c);
                let last = i + 1 == comps.len();
                if !last && let Some(&id) = dirs.get(&acc) {
                    parent = Some(id);
                    continue;
                }
                let id = t.nodes.len();
                t.nodes.push(Node {
                    name: c.as_os_str().to_string_lossy().into_owned(),
                    rel: acc.clone(),
                    is_dir: !last,
                    status: e.status,
                    renamed_from: if last { e.renamed_from.clone() } else { None },
                    children: Vec::new(),
                    parent,
                    expanded: true,
                });
                match parent {
                    Some(p) => t.nodes[p].children.push(id),
                    None => t.roots.push(id),
                }
                if !last {
                    dirs.insert(acc.clone(), id);
                }
                parent = Some(id);
            }
        }
        let mut roots = std::mem::take(&mut t.roots);
        t.sort(&mut roots);
        t.roots = roots;
        for i in 0..t.nodes.len() {
            let mut c = std::mem::take(&mut t.nodes[i].children);
            t.sort(&mut c);
            t.nodes[i].children = c;
        }
        let roots = t.roots.clone();
        for r in roots {
            t.update_dir_status(r);
        }
        t.rebuild();
        t
    }

    fn sort(&self, ids: &mut [usize]) {
        ids.sort_by(|&a, &b| {
            let (na, nb) = (&self.nodes[a], &self.nodes[b]);
            nb.is_dir
                .cmp(&na.is_dir)
                .then_with(|| na.name.to_lowercase().cmp(&nb.name.to_lowercase()))
        });
    }

    /// Recomputes directory status bottom-up: uniform children → that status, else Modified.
    fn update_dir_status(&mut self, id: usize) -> Status {
        if !self.nodes[id].is_dir {
            return self.nodes[id].status;
        }
        let children = self.nodes[id].children.clone();
        let sts: Vec<Status> = children.into_iter().map(|c| self.update_dir_status(c)).collect();
        let changed: Vec<Status> = sts.into_iter().filter(|s| *s != Status::Same).collect();
        let s = match changed.first() {
            None => Status::Same,
            Some(&f) if changed.iter().all(|s| *s == f) => f,
            _ => Status::Modified,
        };
        self.nodes[id].status = s;
        s
    }

    /// Rebuilds the visible row list, keeping the selected node if still visible.
    pub fn rebuild(&mut self) {
        let sel_node = self.visible.get(self.selected).map(|r| r.node);
        self.visible.clear();
        self.files.clear();
        self.dfs_pos = vec![0; self.nodes.len()];
        let mut counter = 0;
        let roots = self.roots.clone();
        let n = roots.len();
        for (i, r) in roots.into_iter().enumerate() {
            self.walk(r, String::new(), i + 1 == n, true, &mut counter);
        }
        if let Some(node) = sel_node {
            // Fall back to the nearest visible ancestor.
            let mut cur = Some(node);
            while let Some(c) = cur {
                if let Some(pos) = self.visible.iter().position(|r| r.node == c) {
                    self.selected = pos;
                    break;
                }
                cur = self.nodes[c].parent;
            }
        }
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
    }

    fn walk(&mut self, id: usize, indent: String, last: bool, visible: bool, counter: &mut usize) {
        self.dfs_pos[id] = *counter;
        *counter += 1;
        if visible {
            let prefix = format!("{indent}{}", if last { "└─ " } else { "├─ " });
            self.visible.push(VisRow { node: id, prefix });
        }
        if !self.nodes[id].is_dir {
            self.files.push(id);
            return;
        }
        let child_indent = format!("{indent}{}", if last { "   " } else { "│  " });
        let children = self.nodes[id].children.clone();
        let show = visible && self.nodes[id].expanded;
        let n = children.len();
        for (i, c) in children.into_iter().enumerate() {
            self.walk(c, child_indent.clone(), i + 1 == n, show, counter);
        }
    }

    pub fn selected_node(&self) -> Option<usize> {
        self.visible.get(self.selected).map(|r| r.node)
    }

    pub fn toggle(&mut self, id: usize) {
        if self.nodes[id].is_dir {
            self.nodes[id].expanded = !self.nodes[id].expanded;
            self.rebuild();
        }
    }

    pub fn find(&self, rel: &Path) -> Option<usize> {
        self.nodes.iter().position(|n| n.rel == rel && !n.is_dir)
    }

    /// Selects `id`, expanding its ancestors.
    pub fn reveal(&mut self, id: usize) {
        let mut p = self.nodes[id].parent;
        while let Some(pid) = p {
            self.nodes[pid].expanded = true;
            p = self.nodes[pid].parent;
        }
        self.rebuild();
        if let Some(pos) = self.visible.iter().position(|r| r.node == id) {
            self.selected = pos;
        }
    }

    pub fn set_status(&mut self, rel: &Path, status: Status) {
        if let Some(id) = self.find(rel) {
            self.nodes[id].status = status;
            for r in self.roots.clone() {
                self.update_dir_status(r);
            }
        }
    }

    /// The file after/before the selected node in DFS order, or `None` past the last/first file.
    pub fn adjacent_file(&self, forward: bool) -> Option<usize> {
        let cur = self.selected_node();
        let pos = cur.and_then(|c| self.files.iter().position(|&f| f == c));
        let idx = match (pos, forward) {
            (Some(p), true) => p + 1,
            (Some(p), false) => p.checked_sub(1)?,
            (None, true) => {
                // From a directory: first file whose display position follows it.
                let after = cur.map(|c| self.dfs_pos[c]);
                self.files
                    .iter()
                    .position(|&f| after.is_none_or(|a| self.dfs_pos[f] > a))?
            }
            (None, false) => {
                let before = cur.map_or(usize::MAX, |c| self.dfs_pos[c]);
                self.files.iter().rposition(|&f| self.dfs_pos[f] < before)?
            }
        };
        self.files.get(idx).copied()
    }

    /// Clamps the offset to `height` rows and, if the selection changed since the last call, scrolls it into view.
    pub fn ensure_visible(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.shown_selected != Some(self.selected) {
            self.shown_selected = Some(self.selected);
            if self.selected < self.offset {
                self.offset = self.selected;
            } else if self.selected >= self.offset + height {
                self.offset = self.selected + 1 - height;
            }
        }
        let max = self.visible.len().saturating_sub(height);
        self.offset = self.offset.min(max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(p: &str, s: Status) -> Entry {
        Entry {
            rel: p.into(),
            status: s,
            renamed_from: None,
        }
    }

    #[test]
    fn builds_sorted_tree_with_guides() {
        let t = Tree::build(&[
            e("b.txt", Status::Modified),
            e("src/main.rs", Status::Modified),
            e("src/util/x.rs", Status::RightOnly),
            e("a.txt", Status::LeftOnly),
        ]);
        let rows: Vec<String> = t
            .visible
            .iter()
            .map(|r| format!("{}{}", r.prefix, t.nodes[r.node].name))
            .collect();
        assert_eq!(
            rows,
            vec![
                "├─ src",
                "│  ├─ util",
                "│  │  └─ x.rs",
                "│  └─ main.rs",
                "├─ a.txt",
                "└─ b.txt"
            ]
        );
        let util = t.visible[1].node;
        assert_eq!(t.nodes[util].status, Status::RightOnly);
        assert_eq!(t.nodes[t.visible[0].node].status, Status::Modified);
    }

    #[test]
    fn collapse_and_navigate_files() {
        let mut t = Tree::build(&[
            e("d/x", Status::Modified),
            e("d/y", Status::Modified),
            e("z", Status::Modified),
        ]);
        t.selected = 1; // d/x
        let d = t.visible[0].node;
        t.toggle(d);
        assert_eq!(t.visible.len(), 2);
        assert_eq!(t.selected, 0);
        let next = t.adjacent_file(true).unwrap();
        assert_eq!(t.nodes[next].name, "x");
        t.reveal(t.find(Path::new("d/y")).unwrap());
        assert_eq!(t.visible.len(), 4);
        let next = t.adjacent_file(true).unwrap();
        assert_eq!(t.nodes[next].name, "z");
        t.reveal(next);
        assert_eq!(t.adjacent_file(true), None);
        t.reveal(t.find(Path::new("d/x")).unwrap());
        assert_eq!(t.adjacent_file(false), None);
    }
}
