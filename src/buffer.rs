//! Editable text buffer backing one side of a diff.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Cursor/selection position. `col` is a char index into the line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

impl Pos {
    pub fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

#[derive(Clone)]
struct Snapshot {
    lines: Vec<String>,
    trailing_newline: bool,
    cursor: Pos,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EditKind {
    None,
    Insert,
    Delete,
    Other,
}

const MAX_UNDO: usize = 1000;
const BINARY_PROBE: usize = 8000;

pub struct Buffer {
    pub path: PathBuf,
    /// Always holds at least one line.
    pub lines: Vec<String>,
    pub crlf: bool,
    pub trailing_newline: bool,
    pub exists: bool,
    pub binary: bool,
    pub editable: bool,
    pub dirty: bool,
    pub cursor: Pos,
    pub anchor: Option<Pos>,
    /// Desired column kept across vertical moves.
    want_col: Option<usize>,
    /// Bumped on each content change.
    pub version: u64,
    /// Lowest line changed since the highlighter last synced.
    pub hl_invalid_from: Option<usize>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last_kind: EditKind,
    saved_hash: u64,
}

pub fn is_null_device(path: &Path) -> bool {
    path == Path::new("/dev/null") || path.as_os_str().eq_ignore_ascii_case("nul")
}

/// Byte offset of char index `col` in `s` (clamped to the end).
pub fn byte_idx(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map(|(b, _)| b).unwrap_or(s.len())
}

pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl Buffer {
    fn with_lines(path: PathBuf, lines: Vec<String>, trailing_newline: bool) -> Self {
        let mut b = Self {
            path,
            lines: if lines.is_empty() { vec![String::new()] } else { lines },
            crlf: false,
            trailing_newline,
            exists: true,
            binary: false,
            editable: false,
            dirty: false,
            cursor: Pos::default(),
            anchor: None,
            want_col: None,
            version: 0,
            hl_invalid_from: None,
            undo: Vec::new(),
            redo: Vec::new(),
            last_kind: EditKind::None,
            saved_hash: 0,
        };
        b.saved_hash = b.content_hash();
        b
    }

    pub fn from_text(path: PathBuf, text: &str) -> Self {
        let crlf = text.contains("\r\n");
        let trailing_newline = text.ends_with('\n');
        let body = if trailing_newline {
            &text[..text.len() - 1]
        } else {
            text
        };
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            body.split('\n')
                .map(|l| if crlf { l.strip_suffix('\r').unwrap_or(l) } else { l }.to_string())
                .collect()
        };
        let mut b = Self::with_lines(path, lines, trailing_newline);
        b.crlf = crlf;
        b.saved_hash = b.content_hash();
        b
    }

    pub fn missing(path: PathBuf) -> Self {
        let mut b = Self::with_lines(path, vec![], false);
        b.exists = false;
        b
    }

    pub fn load(path: &Path, editable: bool) -> Result<Self> {
        if is_null_device(path) || !path.exists() {
            let mut b = Self::missing(path.to_path_buf());
            b.editable = editable && !is_null_device(path);
            return Ok(b);
        }
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let probe = &bytes[..bytes.len().min(BINARY_PROBE)];
        let text = match std::str::from_utf8(&bytes) {
            Ok(t) if !probe.contains(&0) => t,
            _ => {
                let mut b = Self::with_lines(path.to_path_buf(), vec![], false);
                b.binary = true;
                return Ok(b);
            }
        };
        let mut b = Self::from_text(path.to_path_buf(), text);
        b.editable = editable;
        Ok(b)
    }

    /// True when the buffer represents a zero-length file.
    pub fn is_empty_content(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty() && !self.trailing_newline
    }

    /// Lines participating in the diff.
    pub fn diff_lines(&self) -> &[String] {
        if self.binary || self.is_empty_content() {
            &[]
        } else {
            &self.lines
        }
    }

    pub fn to_text(&self) -> String {
        if self.is_empty_content() {
            return String::new();
        }
        let eol = if self.crlf { "\r\n" } else { "\n" };
        let mut s = self.lines.join(eol);
        if self.trailing_newline {
            s.push_str(eol);
        }
        s
    }

    fn content_hash(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.lines.hash(&mut h);
        self.trailing_newline.hash(&mut h);
        h.finish()
    }

    pub fn save(&mut self) -> Result<()> {
        if !self.exists
            && let Some(parent) = self.path.parent()
        {
            fs::create_dir_all(parent)?;
        }
        // fs::write opens with truncate and follows symlinks, so git difftool's
        // symlinks into the working tree are written through.
        fs::write(&self.path, self.to_text()).with_context(|| format!("writing {}", self.path.display()))?;
        self.exists = true;
        self.saved_hash = self.content_hash();
        self.dirty = false;
        Ok(())
    }

    // ---------------------------------------------------------------- editing

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            lines: self.lines.clone(),
            trailing_newline: self.trailing_newline,
            cursor: self.cursor,
        }
    }

    fn begin_edit(&mut self, kind: EditKind) {
        if kind == EditKind::Other || kind != self.last_kind {
            self.undo.push(self.snapshot());
            if self.undo.len() > MAX_UNDO {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
        self.last_kind = kind;
    }

    fn after_edit(&mut self, from_line: usize) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.version += 1;
        self.hl_invalid_from = Some(self.hl_invalid_from.map_or(from_line, |l| l.min(from_line)));
        self.dirty = self.content_hash() != self.saved_hash;
        self.want_col = None;
        self.clamp_cursor();
    }

    /// Ends the current undo group (typing after a cursor move starts a new one).
    pub fn break_undo_group(&mut self) {
        self.last_kind = EditKind::None;
    }

    fn clamp_cursor(&mut self) {
        self.cursor.line = self.cursor.line.min(self.lines.len() - 1);
        self.cursor.col = self.cursor.col.min(char_len(&self.lines[self.cursor.line]));
    }

    pub fn selection(&self) -> Option<(Pos, Pos)> {
        let a = self.anchor?;
        if a == self.cursor {
            return None;
        }
        Some(if a < self.cursor {
            (a, self.cursor)
        } else {
            (self.cursor, a)
        })
    }

    pub fn selected_text(&self) -> Option<String> {
        let (s, e) = self.selection()?;
        if s.line == e.line {
            let l = &self.lines[s.line];
            return Some(l[byte_idx(l, s.col)..byte_idx(l, e.col)].to_string());
        }
        let mut out = String::new();
        let first = &self.lines[s.line];
        out.push_str(&first[byte_idx(first, s.col)..]);
        for l in &self.lines[s.line + 1..e.line] {
            out.push('\n');
            out.push_str(l);
        }
        out.push('\n');
        let last = &self.lines[e.line];
        out.push_str(&last[..byte_idx(last, e.col)]);
        Some(out)
    }

    /// Removes the selection without recording undo. Returns true if anything was removed.
    fn delete_selection_raw(&mut self) -> bool {
        let Some((s, e)) = self.selection() else {
            self.anchor = None;
            return false;
        };
        let tail = {
            let l = &self.lines[e.line];
            l[byte_idx(l, e.col)..].to_string()
        };
        let first = &mut self.lines[s.line];
        first.truncate(byte_idx(first, s.col));
        first.push_str(&tail);
        self.lines.drain(s.line + 1..=e.line);
        self.cursor = s;
        self.anchor = None;
        true
    }

    pub fn insert_text(&mut self, text: &str, kind: EditKind) {
        let had_sel = self.selection().is_some();
        self.begin_edit(if had_sel { EditKind::Other } else { kind });
        let was_empty = self.is_empty_content();
        self.delete_selection_raw();
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let Pos { line, col } = self.cursor;
        let cur = &self.lines[line];
        let b = byte_idx(cur, col);
        let tail = cur[b..].to_string();
        let head = cur[..b].to_string();
        let parts: Vec<&str> = text.split('\n').collect();
        if parts.len() == 1 {
            self.lines[line] = format!("{head}{}{tail}", parts[0]);
            self.cursor.col = col + char_len(parts[0]);
        } else {
            self.lines[line] = format!("{head}{}", parts[0]);
            let last = parts[parts.len() - 1];
            let mut new_lines: Vec<String> = parts[1..parts.len() - 1].iter().map(|s| s.to_string()).collect();
            new_lines.push(format!("{last}{tail}"));
            let n = new_lines.len();
            self.lines.splice(line + 1..line + 1, new_lines);
            self.cursor = Pos::new(line + n, char_len(last));
        }
        if was_empty && !self.is_empty_content() {
            self.trailing_newline = true;
        }
        self.after_edit(line);
    }

    pub fn backspace(&mut self) {
        if self.selection().is_some() {
            self.begin_edit(EditKind::Other);
            let l = self.selection().unwrap().0.line;
            self.delete_selection_raw();
            self.after_edit(l);
            return;
        }
        let Pos { line, col } = self.cursor;
        if col == 0 && line == 0 {
            return;
        }
        self.begin_edit(EditKind::Delete);
        if col > 0 {
            let l = &mut self.lines[line];
            let b = byte_idx(l, col - 1);
            l.remove(b);
            self.cursor.col -= 1;
            self.after_edit(line);
        } else {
            let cur = self.lines.remove(line);
            let prev = &mut self.lines[line - 1];
            let pcol = char_len(prev);
            prev.push_str(&cur);
            self.cursor = Pos::new(line - 1, pcol);
            self.after_edit(line - 1);
        }
    }

    pub fn delete(&mut self) {
        if self.selection().is_some() {
            self.backspace();
            return;
        }
        let Pos { line, col } = self.cursor;
        let len = char_len(&self.lines[line]);
        if col >= len && line + 1 >= self.lines.len() {
            return;
        }
        self.begin_edit(EditKind::Delete);
        if col < len {
            let l = &mut self.lines[line];
            let b = byte_idx(l, col);
            l.remove(b);
        } else {
            let next = self.lines.remove(line + 1);
            self.lines[line].push_str(&next);
        }
        self.after_edit(line);
    }

    /// Replaces whole lines `range` with `new` (used for hunk copying).
    pub fn replace_lines(&mut self, range: std::ops::Range<usize>, new: &[String], src_trailing_newline: bool) {
        self.begin_edit(EditKind::Other);
        let was_empty = self.is_empty_content();
        let range = if was_empty {
            0..1
        } else {
            range.start.min(self.lines.len())..range.end.min(self.lines.len())
        };
        let at_end = range.end == self.lines.len();
        let start = range.start;
        self.lines.splice(range, new.iter().cloned());
        if self.lines.is_empty() {
            self.lines.push(String::new());
            self.trailing_newline = false;
        } else if was_empty || (at_end && !new.is_empty()) {
            self.trailing_newline = src_trailing_newline;
        }
        self.cursor = Pos::new(start, 0);
        self.anchor = None;
        self.after_edit(start);
        self.break_undo_group();
    }

    pub fn undo(&mut self) -> bool {
        let Some(s) = self.undo.pop() else { return false };
        self.redo.push(self.snapshot());
        self.restore(s);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(s) = self.redo.pop() else { return false };
        self.undo.push(self.snapshot());
        self.restore(s);
        true
    }

    fn restore(&mut self, s: Snapshot) {
        self.lines = s.lines;
        self.trailing_newline = s.trailing_newline;
        self.cursor = s.cursor;
        self.anchor = None;
        self.last_kind = EditKind::None;
        self.after_edit(0);
    }

    // --------------------------------------------------------------- movement

    fn begin_move(&mut self, select: bool) {
        if select {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.break_undo_group();
    }

    pub fn set_cursor(&mut self, pos: Pos, select: bool) {
        self.begin_move(select);
        self.cursor = pos;
        self.want_col = None;
        self.clamp_cursor();
    }

    pub fn move_left(&mut self, select: bool) {
        if !select && let Some((s, _)) = self.selection() {
            self.set_cursor(s, false);
            return;
        }
        self.begin_move(select);
        if self.cursor.col > 0 {
            self.cursor.col -= 1;
        } else if self.cursor.line > 0 {
            self.cursor.line -= 1;
            self.cursor.col = char_len(&self.lines[self.cursor.line]);
        }
        self.want_col = None;
    }

    pub fn move_right(&mut self, select: bool) {
        if !select && let Some((_, e)) = self.selection() {
            self.set_cursor(e, false);
            return;
        }
        self.begin_move(select);
        if self.cursor.col < char_len(&self.lines[self.cursor.line]) {
            self.cursor.col += 1;
        } else if self.cursor.line + 1 < self.lines.len() {
            self.cursor.line += 1;
            self.cursor.col = 0;
        }
        self.want_col = None;
    }

    pub fn move_vert(&mut self, delta: isize, select: bool) {
        self.begin_move(select);
        let want = *self.want_col.get_or_insert(self.cursor.col);
        let max = self.lines.len() as isize - 1;
        let line = (self.cursor.line as isize + delta).clamp(0, max) as usize;
        self.cursor.line = line;
        self.cursor.col = want.min(char_len(&self.lines[line]));
    }

    pub fn move_home(&mut self, select: bool) {
        self.begin_move(select);
        let l = &self.lines[self.cursor.line];
        let indent = l.chars().take_while(|c| c.is_whitespace()).count();
        self.cursor.col = if self.cursor.col == indent { 0 } else { indent };
        self.want_col = None;
    }

    pub fn move_end(&mut self, select: bool) {
        self.begin_move(select);
        self.cursor.col = char_len(&self.lines[self.cursor.line]);
        self.want_col = None;
    }

    pub fn move_doc_start(&mut self, select: bool) {
        self.set_cursor(Pos::new(0, 0), select);
    }

    pub fn move_doc_end(&mut self, select: bool) {
        let l = self.lines.len() - 1;
        self.set_cursor(Pos::new(l, char_len(&self.lines[l])), select);
    }

    pub fn move_word(&mut self, forward: bool, select: bool) {
        self.begin_move(select);
        let Pos { mut line, mut col } = self.cursor;
        if forward {
            let chars: Vec<char> = self.lines[line].chars().collect();
            if col >= chars.len() {
                if line + 1 < self.lines.len() {
                    line += 1;
                    col = 0;
                }
            } else {
                while col < chars.len() && !is_word(chars[col]) {
                    col += 1;
                }
                while col < chars.len() && is_word(chars[col]) {
                    col += 1;
                }
            }
        } else if col == 0 {
            if line > 0 {
                line -= 1;
                col = char_len(&self.lines[line]);
            }
        } else {
            let chars: Vec<char> = self.lines[line].chars().collect();
            while col > 0 && !is_word(chars[col - 1]) {
                col -= 1;
            }
            while col > 0 && is_word(chars[col - 1]) {
                col -= 1;
            }
        }
        self.cursor = Pos::new(line, col);
        self.want_col = None;
    }

    pub fn select_word_at(&mut self, pos: Pos) {
        self.set_cursor(pos, false);
        let chars: Vec<char> = self.lines[self.cursor.line].chars().collect();
        let mut s = self.cursor.col;
        let mut e = self.cursor.col;
        let pred: fn(char) -> bool = if chars.get(s).is_some_and(|c| is_word(*c)) {
            is_word
        } else if chars.get(s).is_some_and(|c| c.is_whitespace()) {
            char::is_whitespace
        } else {
            |c| !is_word(c) && !c.is_whitespace()
        };
        while s > 0 && pred(chars[s - 1]) {
            s -= 1;
        }
        while e < chars.len() && pred(chars[e]) {
            e += 1;
        }
        self.anchor = Some(Pos::new(self.cursor.line, s));
        self.cursor.col = e;
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(Pos::new(0, 0));
        let l = self.lines.len() - 1;
        self.cursor = Pos::new(l, char_len(&self.lines[l]));
        self.break_undo_group();
    }

    /// Indentation unit to insert on Tab: a tab if the file indents with tabs, else 4 spaces.
    pub fn indent_unit(&self) -> &'static str {
        if self.lines.iter().any(|l| l.starts_with('\t')) {
            "\t"
        } else {
            "    "
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(text: &str) -> Buffer {
        let mut b = Buffer::from_text(PathBuf::from("x"), text);
        b.editable = true;
        b
    }

    #[test]
    fn roundtrip_lf_crlf_and_trailing_newline() {
        for t in ["a\nb\n", "a\nb", "a\r\nb\r\n", "", "\n", "x"] {
            assert_eq!(buf(t).to_text(), t, "roundtrip {t:?}");
        }
        assert!(buf("a\r\nb").crlf);
        assert_eq!(buf("").diff_lines().len(), 0);
        assert_eq!(buf("\n").diff_lines().len(), 1);
    }

    #[test]
    fn insert_and_undo() {
        let mut b = buf("hello\nworld\n");
        b.set_cursor(Pos::new(0, 5), false);
        b.insert_text("!", EditKind::Insert);
        b.insert_text("?", EditKind::Insert);
        assert_eq!(b.lines[0], "hello!?");
        assert!(b.dirty);
        b.insert_text("\nnew", EditKind::Other);
        assert_eq!(b.lines, ["hello!?", "new", "world"]);
        assert_eq!(b.cursor, Pos::new(1, 3));
        b.undo();
        assert_eq!(b.lines[0], "hello!?");
        b.undo();
        assert_eq!(b.lines[0], "hello");
        assert!(!b.dirty);
        b.redo();
        assert_eq!(b.lines[0], "hello!?");
    }

    #[test]
    fn backspace_delete_join_and_selection() {
        let mut b = buf("ab\ncd\n");
        b.set_cursor(Pos::new(1, 0), false);
        b.backspace();
        assert_eq!(b.lines, ["abcd"]);
        b.set_cursor(Pos::new(0, 1), false);
        b.set_cursor(Pos::new(0, 3), true);
        assert_eq!(b.selected_text().as_deref(), Some("bc"));
        b.insert_text("X", EditKind::Insert);
        assert_eq!(b.lines, ["aXd"]);
        b.set_cursor(Pos::new(0, 3), false);
        b.delete();
        assert_eq!(b.lines, ["aXd"]);
    }

    #[test]
    fn typing_into_empty_file_adds_trailing_newline() {
        let mut b = Buffer::missing(PathBuf::from("x"));
        b.insert_text("a", EditKind::Insert);
        assert_eq!(b.to_text(), "a\n");
    }

    #[test]
    fn replace_lines_into_empty() {
        let mut b = buf("");
        b.replace_lines(0..0, &["x".into(), "y".into()], true);
        assert_eq!(b.to_text(), "x\ny\n");
        b.replace_lines(0..2, &[], true);
        assert_eq!(b.to_text(), "");
    }

    #[test]
    fn save_writes_through_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.txt");
        let link = dir.path().join("link.txt");
        fs::write(&target, "old\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut b = Buffer::load(&link, true).unwrap();
        b.set_cursor(Pos::new(0, 3), false);
        b.insert_text("er", EditKind::Insert);
        b.save().unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "older\n");
    }
}
