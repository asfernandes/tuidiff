//! Recursive folder comparison.

use std::collections::BTreeSet;
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub rel: PathBuf,
    pub status: Status,
}

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
pub fn scan_dirs(left: &Path, right: &Path) -> Vec<Entry> {
    let lf = list_files(left);
    let rf = list_files(right);
    lf.union(&rf)
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
            })
        })
        .collect()
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
        let e = scan_dirs(l.path(), r.path());
        assert_eq!(
            e,
            vec![
                Entry {
                    rel: "a/b/mod.rs".into(),
                    status: Status::Modified
                },
                Entry {
                    rel: "gone.txt".into(),
                    status: Status::LeftOnly
                },
                Entry {
                    rel: "new/added.txt".into(),
                    status: Status::RightOnly
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
        assert!(scan_dirs(l.path(), r.path()).is_empty());
    }
}
