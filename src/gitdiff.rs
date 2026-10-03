//! Single-path mode: reproduces `git difftool -d HEAD` by materialising the
//! changed files of HEAD (left) and symlinks to the working tree (right).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use tempfile::TempDir;

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

pub struct GitDiffDirs {
    pub left: PathBuf,
    pub right: PathBuf,
    _tmp: TempDir,
}

#[derive(Debug, PartialEq)]
struct Change {
    status: char,
    path: String,
}

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("failed to run git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// Parses `git diff --name-status -z --no-renames` output.
fn parse_name_status(raw: &[u8]) -> Vec<Change> {
    let mut parts = raw.split(|b| *b == 0).filter(|p| !p.is_empty());
    let mut out = Vec::new();
    while let (Some(st), Some(path)) = (parts.next(), parts.next()) {
        out.push(Change {
            status: st[0] as char,
            path: String::from_utf8_lossy(path).into_owned(),
        });
    }
    out
}

#[cfg(unix)]
fn symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(windows)]
fn symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(src, dst)
}

pub fn prepare(path: &Path) -> Result<GitDiffDirs> {
    if !path.is_dir() {
        bail!("{}: single-path mode requires a directory", path.display());
    }
    let abs = fs::canonicalize(path)?;
    let top = git(&abs, &["rev-parse", "--show-toplevel"])
        .context("a single path requires a git repository (or pass two paths)")?;
    let root = fs::canonicalize(String::from_utf8_lossy(&top).trim())?;
    let rel = abs.strip_prefix(&root).unwrap_or(Path::new(""));
    let spec = if rel.as_os_str().is_empty() {
        ".".to_string()
    } else {
        rel.to_string_lossy().into_owned()
    };

    let has_head = git(&root, &["rev-parse", "--verify", "-q", "HEAD"]).is_ok();
    let base = if has_head { "HEAD" } else { EMPTY_TREE };
    let raw = git(
        &root,
        &["diff", "--name-status", "-z", "--no-renames", base, "--", &spec],
    )?;

    let tmp = tempfile::Builder::new().prefix("git-difftool.").tempdir()?;
    let left = tmp.path().join("left");
    let right = tmp.path().join("right");
    fs::create_dir_all(&left)?;
    fs::create_dir_all(&right)?;

    for c in parse_name_status(&raw) {
        if c.status != 'A' {
            let blob = git(&root, &["show", &format!("{base}:{}", c.path)])?;
            let dst = left.join(&c.path);
            fs::create_dir_all(dst.parent().unwrap())?;
            fs::write(dst, blob)?;
        }
        if c.status != 'D' {
            let dst = right.join(&c.path);
            fs::create_dir_all(dst.parent().unwrap())?;
            symlink(&root.join(&c.path), &dst)?;
        }
    }
    Ok(GitDiffDirs { left, right, _tmp: tmp })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_status() {
        let c = parse_name_status(b"M\0a/b.rs\0D\0c.txt\0A\0d\0");
        assert_eq!(c.len(), 3);
        assert_eq!(
            c[0],
            Change {
                status: 'M',
                path: "a/b.rs".into()
            }
        );
        assert_eq!(c[1].status, 'D');
        assert_eq!(c[2].path, "d");
    }

    #[test]
    fn prepares_head_and_worktree() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let sh = |a: &[&str]| {
            assert!(Command::new("git").arg("-C").arg(r).args(a).status().unwrap().success());
        };
        sh(&["init", "-q"]);
        sh(&["config", "user.email", "t@t"]);
        sh(&["config", "user.name", "t"]);
        fs::write(r.join("a.txt"), "one\n").unwrap();
        fs::write(r.join("gone.txt"), "x\n").unwrap();
        sh(&["add", "."]);
        sh(&["commit", "-qm", "i"]);
        fs::write(r.join("a.txt"), "two\n").unwrap();
        fs::remove_file(r.join("gone.txt")).unwrap();
        fs::write(r.join("untracked.txt"), "u\n").unwrap();
        let g = prepare(r).unwrap();
        assert_eq!(fs::read_to_string(g.left.join("a.txt")).unwrap(), "one\n");
        assert_eq!(fs::read_to_string(g.right.join("a.txt")).unwrap(), "two\n");
        assert!(g.left.join("gone.txt").exists() && !g.right.join("gone.txt").exists());
        assert!(!g.left.join("untracked.txt").exists() && !g.right.join("untracked.txt").exists());
    }
}
