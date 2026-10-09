//! Recursive folder comparison.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Modified,
    LeftOnly,
    RightOnly,
    /// Identical (only after the user edits a file into equality).
    Same,
    /// Moved to another path with identical content (`Entry::renamed_from` is the old path).
    Renamed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub rel: PathBuf,
    pub status: Status,
    /// For a rename, the path on the left side (`rel` is the path on the right side).
    pub renamed_from: Option<PathBuf>,
}

/// Files larger than this are only paired as renames when byte-identical.
const MAX_RENAME_SIZE: u64 = 16 << 20;
/// Above this many left-only × right-only pairs, only identical files are paired (like git's `renameLimit`).
const RENAME_LIMIT: usize = 250_000;
/// Minimum similarity for a rename with edits (git's default `-M50%`).
const MIN_SIMILARITY: f64 = 0.5;
/// Minimum similarity when only the extension changed (same directory and file stem), e.g. `a.epp` → `a.cpp`
/// where the right side is mostly generated code.
const MIN_SIMILARITY_SAME_STEM: f64 = 0.2;

fn list_files(root: &Path) -> BTreeSet<PathBuf> {
    WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.path().strip_prefix(root).ok().map(Path::to_path_buf))
        .collect()
}

/// Byte-compares two files (following symlinks).
pub fn files_equal(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (fs::metadata(a), fs::metadata(b)) else {
        return false;
    };
    if ma.len() != mb.len() {
        return false;
    }
    let (Ok(mut fa), Ok(mut fb)) = (fs::File::open(a), fs::File::open(b)) else {
        return false;
    };
    let mut ba = vec![0u8; 64 * 1024];
    let mut bb = vec![0u8; 64 * 1024];
    loop {
        let Ok(n) = fa.read(&mut ba) else { return false };
        if n == 0 {
            return true;
        }
        if fb.read_exact(&mut bb[..n]).is_err() || ba[..n] != bb[..n] {
            return false;
        }
    }
}

pub fn status_of(left: &Path, right: &Path) -> Status {
    match (left.is_file(), right.is_file()) {
        (true, false) => Status::LeftOnly,
        (false, true) => Status::RightOnly,
        _ if files_equal(left, right) => Status::Same,
        _ => Status::Modified,
    }
}

/// Differing files between two folders, sorted by path. Identical files are omitted.
/// With `renames`, left-only and right-only files with the same or similar content are paired into one entry.
pub fn scan_dirs(left: &Path, right: &Path, renames: bool) -> Vec<Entry> {
    let lf = list_files(left);
    let rf = list_files(right);
    let mut entries: Vec<Entry> = lf
        .union(&rf)
        .filter_map(|rel| {
            let status = match (lf.contains(rel), rf.contains(rel)) {
                (true, false) => Status::LeftOnly,
                (false, true) => Status::RightOnly,
                _ if files_equal(&left.join(rel), &right.join(rel)) => return None,
                _ => Status::Modified,
            };
            Some(Entry {
                rel: rel.clone(),
                status,
                renamed_from: None,
            })
        })
        .collect();
    if renames {
        detect_renames(left, right, &mut entries);
    }
    entries
}

fn read_candidate(path: &Path) -> Option<Vec<u8>> {
    fs::metadata(path)
        .ok()
        .filter(|m| m.len() <= MAX_RENAME_SIZE)
        .and_then(|_| fs::read(path).ok())
}

/// Fraction (0..=1) of the larger file that is made of lines common to both; 1.0 only for identical files.
fn similarity(a: &[u8], b: &[u8], exact_only: bool) -> f64 {
    if a == b {
        return if a.is_empty() { 0.0 } else { 1.0 };
    }
    let binary = |d: &[u8]| d[..d.len().min(8192)].contains(&0);
    if exact_only || a.is_empty() || b.is_empty() || binary(a) || binary(b) {
        return 0.0;
    }
    let mut counts: HashMap<&[u8], usize> = HashMap::new();
    for l in a.split_inclusive(|c| *c == b'\n') {
        *counts.entry(l).or_default() += 1;
    }
    let mut common = 0;
    for l in b.split_inclusive(|c| *c == b'\n') {
        if let Some(n) = counts.get_mut(l)
            && *n > 0
        {
            *n -= 1;
            common += l.len();
        }
    }
    common as f64 / a.len().max(b.len()) as f64
}

/// Pairs `LeftOnly` and `RightOnly` entries that look like the same file moved: identical ones become `Renamed`,
/// edited ones `Modified`, both with `renamed_from` set on the right-side entry.
fn detect_renames(left: &Path, right: &Path, entries: &mut Vec<Entry>) {
    let of = |st: Status| -> Vec<usize> { (0..entries.len()).filter(|&i| entries[i].status == st).collect() };
    let (lefts, rights) = (of(Status::LeftOnly), of(Status::RightOnly));
    if lefts.is_empty() || rights.is_empty() {
        return;
    }
    let exact_only = lefts.len() * rights.len() > RENAME_LIMIT;
    let ldata: Vec<_> = lefts
        .iter()
        .map(|&i| read_candidate(&left.join(&entries[i].rel)))
        .collect();
    let rdata: Vec<_> = rights
        .iter()
        .map(|&i| read_candidate(&right.join(&entries[i].rel)))
        .collect();

    // (score, same file name, left idx, right idx) into `lefts` / `rights`.
    let mut cands = Vec::new();
    for (li, l) in ldata.iter().enumerate() {
        let Some(l) = l else { continue };
        for (ri, r) in rdata.iter().enumerate() {
            let Some(r) = r else { continue };
            let score = similarity(l, r, exact_only);
            let (lp, rp) = (&entries[lefts[li]].rel, &entries[rights[ri]].rel);
            let same_stem = lp.parent() == rp.parent() && lp.file_stem() == rp.file_stem();
            let min = if same_stem {
                MIN_SIMILARITY_SAME_STEM
            } else {
                MIN_SIMILARITY
            };
            if score >= min {
                cands.push((score, lp.file_name() == rp.file_name(), li, ri));
            }
        }
    }
    cands.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then(b.1.cmp(&a.1))
            .then(a.2.cmp(&b.2))
            .then(a.3.cmp(&b.3))
    });
    let (mut lused, mut rused) = (vec![false; lefts.len()], vec![false; rights.len()]);
    let mut dropped = vec![false; entries.len()];
    for (score, _, li, ri) in cands {
        if lused[li] || rused[ri] {
            continue;
        }
        (lused[li], rused[ri]) = (true, true);
        dropped[lefts[li]] = true;
        let from = entries[lefts[li]].rel.clone();
        let e = &mut entries[rights[ri]];
        e.renamed_from = Some(from);
        e.status = if score >= 1.0 {
            Status::Renamed
        } else {
            Status::Modified
        };
    }
    let mut i = 0;
    entries.retain(|_| {
        i += 1;
        !dropped[i - 1]
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    #[test]
    fn scan_finds_differences() {
        let l = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        write(&l.path().join("same.txt"), "x");
        write(&r.path().join("same.txt"), "x");
        write(&l.path().join("a/b/mod.rs"), "1");
        write(&r.path().join("a/b/mod.rs"), "2");
        write(&l.path().join("gone.txt"), "x");
        write(&r.path().join("new/added.txt"), "x");
        write(&r.path().join(".git/config"), "x");
        let e = scan_dirs(l.path(), r.path(), false);
        assert_eq!(
            e,
            vec![
                Entry {
                    rel: "a/b/mod.rs".into(),
                    status: Status::Modified,
                    renamed_from: None
                },
                Entry {
                    rel: "gone.txt".into(),
                    status: Status::LeftOnly,
                    renamed_from: None
                },
                Entry {
                    rel: "new/added.txt".into(),
                    status: Status::RightOnly,
                    renamed_from: None
                },
            ]
        );
    }

    #[test]
    fn scan_follows_symlinks() {
        let l = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        let w = tempfile::tempdir().unwrap();
        write(&l.path().join("f.txt"), "same");
        write(&w.path().join("f.txt"), "same");
        #[cfg(unix)]
        std::os::unix::fs::symlink(w.path().join("f.txt"), r.path().join("f.txt")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(w.path().join("f.txt"), r.path().join("f.txt")).unwrap();
        assert!(scan_dirs(l.path(), r.path(), true).is_empty());
    }

    #[test]
    fn scan_pairs_renames() {
        let l = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        write(&l.path().join("old/same.txt"), "a\nb\nc\n");
        write(&r.path().join("new/moved.txt"), "a\nb\nc\n");
        write(&l.path().join("edit.rs"), "1\n2\n3\n4\n");
        write(&r.path().join("sub/edit2.rs"), "1\n2\n3\nfour\n");
        write(&l.path().join("lonely.txt"), "completely\n");
        write(&r.path().join("other.txt"), "different\n");
        let e = scan_dirs(l.path(), r.path(), true);
        let find = |rel: &str| e.iter().find(|x| x.rel == Path::new(rel)).unwrap();
        assert_eq!(find("new/moved.txt").status, Status::Renamed);
        assert_eq!(
            find("new/moved.txt").renamed_from.as_deref(),
            Some(Path::new("old/same.txt"))
        );
        assert_eq!(find("sub/edit2.rs").status, Status::Modified);
        assert_eq!(find("sub/edit2.rs").renamed_from.as_deref(), Some(Path::new("edit.rs")));
        assert_eq!(find("lonely.txt").status, Status::LeftOnly);
        assert_eq!(find("other.txt").status, Status::RightOnly);
        assert_eq!(e.len(), 4);
        assert_eq!(scan_dirs(l.path(), r.path(), false).len(), 6);
    }

    #[test]
    fn scan_rename_prefers_same_name() {
        let l = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        write(&l.path().join("a/x.txt"), "same\n");
        write(&l.path().join("b/y.txt"), "same\n");
        write(&r.path().join("c/a.txt"), "same\n");
        write(&r.path().join("d/y.txt"), "same\n");
        let e = scan_dirs(l.path(), r.path(), true);
        let d = e.iter().find(|x| x.rel == Path::new("d/y.txt")).unwrap();
        assert_eq!(d.renamed_from.as_deref(), Some(Path::new("b/y.txt")));
    }

    #[test]
    fn scan_pairs_same_stem_with_low_similarity() {
        let l = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        write(&l.path().join("d/gen.epp"), "a\nb\n1\n2\n3\n");
        write(&r.path().join("d/gen.cpp"), "a\nb\nc\nd\ne\nf\n");
        write(&l.path().join("e/gen.epp"), "a\nb\n1\n2\n3\n");
        write(&r.path().join("f/gen.cpp"), "a\nb\nc\nd\ne\nf\n");
        let e = scan_dirs(l.path(), r.path(), true);
        let d = e.iter().find(|x| x.rel == Path::new("d/gen.cpp")).unwrap();
        assert_eq!(d.renamed_from.as_deref(), Some(Path::new("d/gen.epp")));
        assert_eq!(e.len(), 3);
    }
}
