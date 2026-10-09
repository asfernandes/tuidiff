//! Application state and input handling.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use crate::buffer::{Buffer, EditKind, Pos, char_len, is_null_device};
use crate::diff::{self, DiffResult};
use crate::highlight::{Highlighter, HlCache};
use crate::scan;
use crate::text::{col_at_display, display_col};
use crate::tree::Tree;
use crate::ui;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Tree,
    Pane(usize),
}

pub type InlineDiff = (Vec<std::ops::Range<usize>>, Vec<std::ops::Range<usize>>);

pub struct FileView {
    pub bufs: [Buffer; 2],
    pub hl: [HlCache; 2],
    pub diff: DiffResult,
    diff_versions: Option<[u64; 2]>,
    /// First display row shown (shared by both panes).
    pub scroll: usize,
    pub hscroll: [usize; 2],
    /// Word-level changes per (left line, right line) of replaced rows; cleared when the diff is recomputed.
    pub inline: HashMap<(usize, usize), InlineDiff>,
}

impl FileView {
    /// Recomputes the diff and invalidates highlighting after edits.
    pub fn refresh(&mut self) {
        let v = [self.bufs[0].version, self.bufs[1].version];
        if self.diff_versions != Some(v) {
            self.diff = diff::compute(self.bufs[0].diff_lines(), self.bufs[1].diff_lines());
            self.diff_versions = Some(v);
            self.inline.clear();
        }
        for s in 0..2 {
            if let Some(from) = self.bufs[s].hl_invalid_from.take() {
                self.hl[s].invalidate(from);
            }
        }
        self.scroll = self.scroll.min(self.diff.rows.len().saturating_sub(1));
    }

    /// Row count the scrollbar spans for a pane `height` rows tall (grows when scrolled past the end).
    pub fn scroll_total(&self, height: usize) -> usize {
        self.diff.rows.len().max(self.scroll + height)
    }

    pub fn cursor_row(&self, side: usize) -> usize {
        self.diff.row_of(side, self.bufs[side].cursor.line)
    }

    pub fn any_dirty(&self) -> bool {
        self.bufs.iter().any(|b| b.dirty)
    }

    /// Hunk under the cursor of `side`, falling back to the other side's cursor.
    pub fn current_hunk(&self, side: usize) -> Option<usize> {
        self.diff
            .hunk_at_row(self.cursor_row(side))
            .or_else(|| self.diff.hunk_at_row(self.cursor_row(1 - side)))
    }

    /// Line on `side` where the cursor lands when jumping to hunk `i`.
    pub fn hunk_line(&self, side: usize, i: usize) -> usize {
        let start = self.diff.hunks[i].rows.start;
        self.diff
            .line_near_row(side, start)
            .min(self.bufs[side].lines.len() - 1)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    PrevHunk,
    NextHunk,
    PrevFile,
    NextFile,
    Save,
    Quit,
    SaveAll,
    Discard,
    Cancel,
}

/// Screen regions from the last draw, used for mouse hit-testing.
#[derive(Default)]
pub struct Areas {
    pub tree: Rect,
    pub splitter: Option<u16>,
    /// Gutter + text area of each pane.
    pub panes: [Rect; 2],
    pub gutter: [u16; 2],
    pub center: Rect,
    /// Body of the diff area: left pane, center column and right pane.
    pub diff_body: Rect,
    /// Vertical scrollbars of the tree, left pane and right pane (empty when not drawn).
    pub vbars: [Rect; 3],
    pub buttons: Vec<(Rect, Button)>,
    pub width: u16,
}

enum Drag {
    Splitter,
    /// The divider between the left and right panes.
    PaneSplit,
    /// Scrollbar thumb of panel (0 = tree, 1/2 = left/right pane), grabbed `grab` cells below its top.
    Scrollbar {
        panel: usize,
        grab: u16,
    },
    Select(usize),
}

pub struct Options {
    pub git: bool,
    pub right_editable: bool,
    pub readonly: bool,
    pub theme: String,
    /// True for internal `tuidiff REV1..REV2` (both sides are commits, no
    /// copy-back). External `git difftool -d` temp copies stay right-editable
    /// so `--no-symlinks` (Windows default) works; git copies them back.
    pub range_mode: bool,
    /// Pair left-only and right-only files with the same or similar content as renames.
    pub renames: bool,
}

pub struct App {
    pub left_root: PathBuf,
    pub right_root: PathBuf,
    pub dir_mode: bool,
    pub git: bool,
    force_right_editable: bool,
    range_mode: bool,
    renames: bool,
    /// New relative path → old relative path of renamed files.
    pub renamed: HashMap<PathBuf, PathBuf>,
    readonly: bool,
    pub tree: Tree,
    pub tree_width: u16,
    /// Left pane's share of the panes' width, in permille (not persisted; starts at 50:50).
    pub pane_split: u16,
    pub views: HashMap<PathBuf, FileView>,
    pub current: Option<PathBuf>,
    pub focus: Focus,
    pub last_side: usize,
    pub message: Option<(String, bool)>,
    pub quit_prompt: bool,
    /// Scroll offset of the F1 help overlay while it is open.
    pub help: Option<usize>,
    /// Find prompt text while the prompt is open.
    pub search: Option<String>,
    last_search: String,
    pub quit: bool,
    pub hl: Highlighter,
    pub areas: Areas,
    clipboard: String,
    sys_clipboard: Option<arboard::Clipboard>,
    /// OSC 52 clipboard sequences to write to the terminal.
    pub osc_out: Vec<String>,
    /// Set by draw when syntax highlighting ran out of time budget.
    pub hl_pending: bool,
    pub mouse_enabled: bool,
    pub mouse_toggle_requested: bool,
    drag: Option<Drag>,
    last_click: Option<(Instant, u16, u16)>,
}

fn under_temp_dir(path: &Path) -> bool {
    let tmp = fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
    let p = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    p.starts_with(&tmp)
}

/// Heuristic for `git difftool -d` temp dirs (`.../git-difftool.XXXXXX/left`).
pub fn looks_like_git_difftool(path: &Path) -> bool {
    path.components()
        .any(|c| c.as_os_str().to_string_lossy().starts_with("git-difftool."))
}

impl App {
    pub fn new(left: PathBuf, right: PathBuf, opts: Options) -> Result<Self> {
        let is_dir = |p: &Path| !is_null_device(p) && p.is_dir();
        let dir_mode = is_dir(&left) && is_dir(&right);
        if !dir_mode && (is_dir(&left) || is_dir(&right)) {
            bail!("cannot compare a directory with a file");
        }
        for p in [&left, &right] {
            if !is_null_device(p) && !p.exists() {
                bail!("{} does not exist", p.display());
            }
        }
        let git = opts.git || looks_like_git_difftool(&left) || looks_like_git_difftool(&right);
        let mut app = Self {
            tree: Tree::default(),
            left_root: left,
            right_root: right,
            dir_mode,
            git,
            force_right_editable: opts.right_editable,
            range_mode: opts.range_mode,
            renames: opts.renames,
            renamed: HashMap::new(),
            readonly: opts.readonly,
            tree_width: 34,
            pane_split: 500,
            views: HashMap::new(),
            current: None,
            focus: if dir_mode { Focus::Tree } else { Focus::Pane(1) },
            last_side: 1,
            message: None,
            quit_prompt: false,
            help: None,
            search: None,
            last_search: String::new(),
            quit: false,
            hl: Highlighter::new(&opts.theme),
            areas: Areas::default(),
            clipboard: String::new(),
            sys_clipboard: None,
            osc_out: Vec::new(),
            hl_pending: false,
            mouse_enabled: true,
            mouse_toggle_requested: false,
            drag: None,
            last_click: None,
        };
        if dir_mode {
            app.rescan();
        } else {
            app.open_view(PathBuf::new());
            if let Some(fv) = app.current_view()
                && !fv.bufs[1].editable
                && fv.bufs[0].editable
            {
                app.focus = Focus::Pane(0);
                app.last_side = 0;
            }
        }
        Ok(app)
    }

    fn rescan(&mut self) {
        let entries = scan::scan_dirs(&self.left_root, &self.right_root, self.renames);
        self.renamed = entries
            .iter()
            .filter_map(|e| e.renamed_from.clone().map(|old| (e.rel.clone(), old)))
            .collect();
        self.tree = Tree::build(&entries);
        let target = self
            .current
            .clone()
            .and_then(|c| self.tree.find(&c))
            .or_else(|| self.tree.files.first().copied());
        match target {
            Some(id) => {
                self.tree.reveal(id);
                self.open_view(self.tree.nodes[id].rel.clone());
            }
            None => self.current = None,
        }
        if entries.is_empty() {
            self.set_message("No differences found", false);
        }
    }

    pub fn set_message(&mut self, msg: impl Into<String>, error: bool) {
        self.message = Some((msg.into(), error));
    }

    pub fn paths_for(&self, rel: &Path) -> (PathBuf, PathBuf) {
        if self.dir_mode {
            (
                self.left_root.join(self.renamed.get(rel).map_or(rel, PathBuf::as_path)),
                self.right_root.join(rel),
            )
        } else {
            (self.left_root.clone(), self.right_root.clone())
        }
    }

    fn editable(&self, side: usize, path: &Path) -> bool {
        if self.readonly || is_null_device(path) {
            return false;
        }
        if !self.git {
            return true;
        }
        if side == 0 {
            return false;
        }
        if self.force_right_editable {
            return true;
        }
        // Internal `tuidiff REV..REV`: both sides are commit blobs with no
        // copy-back, so the right side stays read-only.
        if self.range_mode {
            return false;
        }
        // git difftool -d symlinks working-tree files into the right temp dir.
        if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
            return true;
        }
        // git difftool -d --no-symlinks (the default on Windows) uses plain
        // copies instead of symlinks. Editing the temp copy is still correct:
        // git copies modified working-tree files back when the tool exits.
        // Only applies to dir-diff temp dirs, not plain temp files.
        if self.dir_mode && looks_like_git_difftool(path) {
            return true;
        }
        path.exists() && !under_temp_dir(path)
    }

    fn open_view(&mut self, rel: PathBuf) {
        if !self.views.contains_key(&rel) {
            let (lp, rp) = self.paths_for(&rel);
            let mut load = |side: usize, p: &Path| match Buffer::load(p, self.editable(side, p)) {
                Ok(b) => b,
                Err(e) => {
                    self.message = Some((format!("{e:#}"), true));
                    Buffer::missing(p.to_path_buf())
                }
            };
            let bufs = [load(0, &lp), load(1, &rp)];
            let first = bufs
                .iter()
                .map(|b| b.lines[0].as_str())
                .find(|l| !l.is_empty())
                .unwrap_or("");
            let syntax = self.hl.detect(&[&rel, &rp, &lp], first);
            let mut fv = FileView {
                hl: [HlCache::new(syntax.clone()), HlCache::new(syntax)],
                bufs,
                diff: DiffResult::default(),
                diff_versions: None,
                scroll: 0,
                hscroll: [0, 0],
                inline: HashMap::new(),
            };
            fv.refresh();
            if let Some(h) = fv.diff.hunks.first().cloned() {
                for s in 0..2 {
                    let line = fv.diff.line_near_row(s, h.rows.start).min(fv.bufs[s].lines.len() - 1);
                    fv.bufs[s].set_cursor(Pos::new(line, 0), false);
                }
                fv.scroll = h.rows.start.saturating_sub(3);
            } else if fv.bufs.iter().all(|b| !b.binary) {
                self.message = Some(("Files differ only in line endings or final newline".into(), false));
            }
            self.views.insert(rel.clone(), fv);
        }
        self.current = Some(rel);
    }

    pub fn current_view(&self) -> Option<&FileView> {
        self.current.as_ref().and_then(|k| self.views.get(k))
    }

    pub fn current_view_mut(&mut self) -> Option<&mut FileView> {
        self.current.as_ref().and_then(|k| self.views.get_mut(k))
    }

    pub fn active_side(&self) -> usize {
        match self.focus {
            Focus::Pane(s) => s,
            Focus::Tree => self.last_side,
        }
    }

    fn set_focus(&mut self, f: Focus) {
        if let Focus::Pane(s) = f {
            self.last_side = s;
        }
        if f == Focus::Tree && !self.dir_mode {
            return;
        }
        self.focus = f;
    }

    fn cycle_focus(&mut self, forward: bool) {
        let order: &[Focus] = if self.dir_mode {
            &[Focus::Tree, Focus::Pane(0), Focus::Pane(1)]
        } else {
            &[Focus::Pane(0), Focus::Pane(1)]
        };
        let i = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let n = order.len();
        let next = order[if forward { (i + 1) % n } else { (i + n - 1) % n }];
        self.set_focus(next);
    }

    // ----------------------------------------------------------------- events

    pub fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                self.message = None;
                self.on_key(k);
            }
            Event::Mouse(m) => self.on_mouse(m),
            Event::Paste(s) => match &mut self.search {
                Some(q) => q.push_str(s.lines().next().unwrap_or("")),
                None => self.paste(&s),
            },
            _ => {}
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);

        if self.quit_prompt {
            match k.code {
                KeyCode::Char('s' | 'S') => self.press(Button::SaveAll),
                KeyCode::Char('d' | 'D') => self.press(Button::Discard),
                KeyCode::Char('c' | 'C') | KeyCode::Esc => self.press(Button::Cancel),
                _ => {}
            }
            return;
        }

        if self.help.is_some() {
            return self.help_key(k);
        }

        if self.search.is_some() {
            return self.search_key(k);
        }

        let lower = |c: char| c.to_ascii_lowercase();
        match k.code {
            KeyCode::Char(c) if ctrl && lower(c) == 'f' => return self.open_search(),
            KeyCode::Char(c) if ctrl && lower(c) == 'g' => return self.search_next(),
            KeyCode::Char(c) if ctrl && lower(c) == 'q' => return self.request_quit(),
            KeyCode::Char(c) if ctrl && lower(c) == 's' => return self.save_current(),
            KeyCode::F(1) => {
                self.search = None;
                self.help = Some(0);
                return;
            }
            KeyCode::F(2) => {
                self.save_all();
                return;
            }
            KeyCode::Char(c) if ctrl && lower(c) == 'n' => return self.goto_file(true),
            KeyCode::Char(c) if ctrl && lower(c) == 'p' => return self.goto_file(false),
            KeyCode::Char(c) if ctrl && lower(c) == 'd' => return self.goto_hunk(true),
            KeyCode::Char(c) if ctrl && lower(c) == 'e' => return self.goto_hunk(false),
            KeyCode::Down if alt => return self.goto_hunk(true),
            KeyCode::Up if alt => return self.goto_hunk(false),
            KeyCode::Right if alt && shift => return self.set_split_x(self.areas.center.x + 3),
            KeyCode::Left if alt && shift => return self.set_split_x(self.areas.center.x.saturating_sub(1)),
            KeyCode::Right if alt => return self.copy_current_hunk(0, 1),
            KeyCode::Left if alt => return self.copy_current_hunk(1, 0),
            KeyCode::F(6) => return self.cycle_focus(!shift),
            KeyCode::BackTab => return self.cycle_focus(false),
            KeyCode::F(12) => {
                self.mouse_toggle_requested = true;
                return;
            }
            KeyCode::F(5) if self.dir_mode => {
                self.rescan();
                return self.set_message("Rescanned folders", false);
            }
            KeyCode::Char(c) if ctrl && (lower(c) == 'y' || (lower(c) == 'z' && shift)) => {
                return self.undo_redo(false);
            }
            KeyCode::Char(c) if ctrl && lower(c) == 'z' => return self.undo_redo(true),
            _ => {}
        }
        match self.focus {
            Focus::Tree => self.tree_key(k),
            Focus::Pane(side) => self.pane_key(side, k),
        }
    }

    fn help_key(&mut self, k: KeyEvent) {
        let page = self.areas.panes[0].height.max(4) as usize - 2;
        let Some(off) = &mut self.help else { return };
        match k.code {
            KeyCode::F(1) | KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => self.help = None,
            KeyCode::Up | KeyCode::Char('k') => *off = off.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => *off = off.saturating_add(1),
            KeyCode::PageUp => *off = off.saturating_sub(page),
            KeyCode::PageDown => *off = off.saturating_add(page),
            KeyCode::Home => *off = 0,
            KeyCode::End => *off = usize::MAX,
            _ => {}
        }
    }

    fn open_search(&mut self) {
        let side = self.active_side();
        let sel = self
            .current_view()
            .and_then(|fv| fv.bufs[side].selected_text())
            .filter(|t| !t.contains('\n'));
        self.search = Some(sel.unwrap_or_else(|| self.last_search.clone()));
    }

    fn search_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some(q) = &mut self.search else { return };
        match k.code {
            KeyCode::Esc => self.search = None,
            KeyCode::Enter => self.finish_search(),
            KeyCode::Char(c) if ctrl && matches!(c.to_ascii_lowercase(), 'f' | 'g') => self.finish_search(),
            KeyCode::Backspace => {
                q.pop();
            }
            KeyCode::Char(c) if !ctrl && !k.modifiers.contains(KeyModifiers::ALT) => q.push(c),
            _ => {}
        }
    }

    fn finish_search(&mut self) {
        if let Some(q) = self.search.take()
            && !q.is_empty()
        {
            self.last_search = q;
            self.search_next();
        }
    }

    fn search_next(&mut self) {
        if self.last_search.is_empty() {
            return self.open_search();
        }
        let side = self.active_side();
        let query = self.last_search.clone();
        let Some(fv) = self.current_view_mut() else { return };
        let b = &mut fv.bufs[side];
        let from = b.selection().map_or(b.cursor, |(_, e)| e);
        let Some((s, e, wrapped)) = b.find(&query, from) else {
            return self.set_message(format!("Not found: {query}"), true);
        };
        b.set_cursor(s, false);
        b.set_cursor(e, true);
        self.set_focus(Focus::Pane(side));
        self.ensure_cursor_visible(side);
        if wrapped {
            self.set_message("Search wrapped", false);
        }
    }

    fn tree_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.areas.tree.height.max(1) as usize;
        let len = self.tree.visible.len();
        if len == 0 {
            if matches!(k.code, KeyCode::Char('q') | KeyCode::Esc) {
                self.request_quit();
            }
            return;
        }
        let sel = self.tree.selected;
        let mut new_sel = sel;
        match k.code {
            KeyCode::Left if ctrl => self.tree_width = self.tree_width.saturating_sub(2).max(12),
            KeyCode::Right if ctrl => self.tree_width = (self.tree_width + 2).min(self.areas.width.saturating_sub(20)),
            KeyCode::Up | KeyCode::Char('k') => new_sel = sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => new_sel = (sel + 1).min(len - 1),
            KeyCode::PageUp => new_sel = sel.saturating_sub(page),
            KeyCode::PageDown => new_sel = (sel + page).min(len - 1),
            KeyCode::Home => new_sel = 0,
            KeyCode::End => new_sel = len - 1,
            KeyCode::Char(' ') => {
                if let Some(n) = self.tree.selected_node() {
                    self.tree.toggle(n);
                }
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                let Some(n) = self.tree.selected_node() else { return };
                if self.tree.nodes[n].is_dir {
                    if !self.tree.nodes[n].expanded {
                        self.tree.toggle(n);
                    } else if k.code != KeyCode::Enter {
                        new_sel = (sel + 1).min(len - 1);
                    } else {
                        self.tree.toggle(n);
                    }
                } else if k.code == KeyCode::Enter {
                    let side = if self
                        .current_view()
                        .is_some_and(|v| !v.bufs[1].editable && v.bufs[0].editable)
                    {
                        0
                    } else {
                        1
                    };
                    self.set_focus(Focus::Pane(side));
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                let Some(n) = self.tree.selected_node() else { return };
                if self.tree.nodes[n].is_dir && self.tree.nodes[n].expanded {
                    self.tree.toggle(n);
                } else if let Some(p) = self.tree.nodes[n].parent
                    && let Some(pos) = self.tree.visible.iter().position(|r| r.node == p)
                {
                    new_sel = pos;
                }
            }
            KeyCode::Tab => self.set_focus(Focus::Pane(self.last_side)),
            KeyCode::Char('q') | KeyCode::Esc => self.request_quit(),
            _ => {}
        }
        if new_sel != sel {
            self.tree.selected = new_sel;
            self.open_selected_file();
        }
    }

    fn open_selected_file(&mut self) {
        if let Some(n) = self.tree.selected_node()
            && !self.tree.nodes[n].is_dir
        {
            self.open_view(self.tree.nodes[n].rel.clone());
        }
    }

    fn pane_key(&mut self, side: usize, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let page = self.areas.panes[side].height.max(2) as usize - 1;
        let dir_mode = self.dir_mode;
        let Some(fv) = self.current_view_mut() else {
            if k.code == KeyCode::Esc && dir_mode {
                self.set_focus(Focus::Tree);
            }
            return;
        };
        let editable = fv.bufs[side].editable;
        let b = &mut fv.bufs[side];
        let edit = |b: &mut Buffer, f: &dyn Fn(&mut Buffer)| -> bool {
            if b.editable {
                f(b);
                true
            } else {
                false
            }
        };
        let mut ro = false;
        match k.code {
            KeyCode::Esc => {
                if dir_mode {
                    self.set_focus(Focus::Tree);
                }
                return;
            }
            KeyCode::Left if ctrl => b.move_word(false, shift),
            KeyCode::Right if ctrl => b.move_word(true, shift),
            KeyCode::Left => b.move_left(shift),
            KeyCode::Right => b.move_right(shift),
            KeyCode::Up if ctrl => {
                fv.scroll = fv.scroll.saturating_sub(1);
                return;
            }
            KeyCode::Down if ctrl => {
                fv.scroll = (fv.scroll + 1).min(fv.diff.rows.len().saturating_sub(1));
                return;
            }
            KeyCode::Up => b.move_vert(-1, shift),
            KeyCode::Down => b.move_vert(1, shift),
            KeyCode::PageUp => {
                b.move_vert(-(page as isize), shift);
                fv.scroll = fv.scroll.saturating_sub(page);
            }
            KeyCode::PageDown => {
                b.move_vert(page as isize, shift);
                fv.scroll = (fv.scroll + page).min(fv.diff.rows.len().saturating_sub(1));
            }
            KeyCode::Home if ctrl => b.move_doc_start(shift),
            KeyCode::End if ctrl => b.move_doc_end(shift),
            KeyCode::Home => b.move_home(shift),
            KeyCode::End => b.move_end(shift),
            KeyCode::Char(c) if ctrl && c.eq_ignore_ascii_case(&'a') => b.select_all(),
            KeyCode::Char(c) if ctrl && c.eq_ignore_ascii_case(&'c') => {
                let t = Self::copy_text(b, false);
                return self.set_clipboard(t);
            }
            KeyCode::Char(c) if ctrl && c.eq_ignore_ascii_case(&'x') => {
                if !editable {
                    return self.set_message("Read-only file", true);
                }
                let t = Self::copy_text(b, true);
                self.set_clipboard(t);
            }
            KeyCode::Char(c) if ctrl && c.eq_ignore_ascii_case(&'v') => {
                let t = self.clipboard_text();
                return self.paste(&t);
            }
            KeyCode::Tab => {
                if editable {
                    let unit = b.indent_unit();
                    b.insert_text(unit, EditKind::Other);
                } else {
                    return self.cycle_focus(true);
                }
            }
            KeyCode::Enter => {
                let indent: String = b.lines[b.cursor.line]
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                ro = !edit(b, &|b| b.insert_text(&format!("\n{indent}"), EditKind::Other));
            }
            KeyCode::Backspace if ctrl => ro = !edit(b, &|b| b.delete_word(false)),
            // Many terminals send Ctrl+Backspace as ^H.
            KeyCode::Char(c) if ctrl && c.eq_ignore_ascii_case(&'h') => ro = !edit(b, &|b| b.delete_word(false)),
            KeyCode::Delete if ctrl => ro = !edit(b, &|b| b.delete_word(true)),
            KeyCode::Char(c) if ctrl && c.eq_ignore_ascii_case(&'k') => ro = !edit(b, &|b| b.delete_line()),
            KeyCode::Backspace => ro = !edit(b, &|b| b.backspace()),
            KeyCode::Delete => ro = !edit(b, &|b| b.delete()),
            KeyCode::Char(c) if !ctrl && !alt => {
                ro = !edit(b, &|b| {
                    b.insert_text(
                        c.encode_utf8(&mut [0; 4]),
                        if c == ' ' { EditKind::Other } else { EditKind::Insert },
                    )
                })
            }
            _ => return,
        }
        if ro {
            self.set_message("Read-only file", true);
        }
        self.ensure_cursor_visible(side);
    }

    /// Selected text, or the whole current line when nothing is selected.
    fn copy_text(b: &mut Buffer, cut: bool) -> String {
        if b.selection().is_none() {
            let l = b.cursor.line;
            let end = if l + 1 < b.lines.len() {
                Pos::new(l + 1, 0)
            } else {
                Pos::new(l, char_len(&b.lines[l]))
            };
            b.set_cursor(Pos::new(l, 0), false);
            b.set_cursor(end, true);
        }
        let t = b.selected_text().unwrap_or_default();
        if cut {
            b.backspace();
        }
        t
    }

    fn set_clipboard(&mut self, text: String) {
        self.osc_out
            .push(format!("\x1b]52;c;{}\x07", crate::text::base64(text.as_bytes())));
        // Kept alive: on X11/Wayland the contents vanish when the owner is dropped.
        if self.sys_clipboard.is_none() {
            self.sys_clipboard = arboard::Clipboard::new().ok();
        }
        if let Some(c) = &mut self.sys_clipboard {
            let _ = c.set_text(text.clone());
        }
        self.clipboard = text;
    }

    fn clipboard_text(&mut self) -> String {
        if self.sys_clipboard.is_none() {
            self.sys_clipboard = arboard::Clipboard::new().ok();
        }
        match self.sys_clipboard.as_mut().and_then(|c| c.get_text().ok()) {
            Some(t) if !t.is_empty() => t,
            _ => self.clipboard.clone(),
        }
    }

    fn paste(&mut self, text: &str) {
        let Focus::Pane(side) = self.focus else { return };
        let Some(fv) = self.current_view_mut() else { return };
        if !fv.bufs[side].editable {
            return self.set_message("Read-only file", true);
        }
        fv.bufs[side].insert_text(text, EditKind::Other);
        self.ensure_cursor_visible(side);
    }

    fn undo_redo(&mut self, undo: bool) {
        let Focus::Pane(side) = self.focus else { return };
        let Some(fv) = self.current_view_mut() else { return };
        let b = &mut fv.bufs[side];
        let ok = if undo { b.undo() } else { b.redo() };
        if !ok {
            self.set_message(if undo { "Nothing to undo" } else { "Nothing to redo" }, false);
        }
        self.ensure_cursor_visible(side);
    }

    pub fn ensure_cursor_visible(&mut self, side: usize) {
        let area = self.areas.panes[side];
        let h = area.height as usize;
        let w = area.width.saturating_sub(self.areas.gutter[side]) as usize;
        let Some(fv) = self.current_view_mut() else { return };
        fv.refresh();
        if h == 0 {
            return;
        }
        let row = fv.cursor_row(side);
        if row < fv.scroll {
            fv.scroll = row;
        } else if row >= fv.scroll + h {
            fv.scroll = row + 1 - h;
        }
        let b = &fv.bufs[side];
        let dc = display_col(&b.lines[b.cursor.line], b.cursor.col);
        let hs = &mut fv.hscroll[side];
        if dc < *hs {
            *hs = dc.saturating_sub(4);
        } else if w > 0 && dc >= *hs + w {
            *hs = dc + 1 - w + 4.min(w / 4);
        }
    }

    // ---------------------------------------------------------------- actions

    fn goto_hunk(&mut self, forward: bool) {
        let side = self.active_side();
        let h = self.areas.panes[side].height as usize;
        let Some(fv) = self.current_view_mut() else { return };
        let row = fv.cursor_row(side);
        let target = if forward {
            fv.diff.hunks.iter().position(|h| h.rows.start > row)
        } else {
            // Compare where the cursor would land, not the hunk start: a hunk that is all filler on
            // this side lands on the line below it, which may be where the cursor already is.
            (0..fv.diff.hunks.len())
                .rev()
                .find(|&i| fv.diff.row_of(side, fv.hunk_line(side, i)) < row)
        };
        let Some(i) = target else {
            let hint = if self.dir_mode {
                " (Ctrl+N/Ctrl+P: change file)"
            } else {
                ""
            };
            return self.set_message(format!("No more changes{hint}"), false);
        };
        Self::jump_to_hunk(fv, i, h);
    }

    fn jump_to_hunk(fv: &mut FileView, i: usize, height: usize) {
        let start = fv.diff.hunks[i].rows.start;
        for s in 0..2 {
            let line = fv.hunk_line(s, i);
            fv.bufs[s].set_cursor(Pos::new(line, 0), false);
        }
        fv.scroll = start.saturating_sub(height / 4);
        fv.hscroll = [0, 0];
    }

    fn copy_current_hunk(&mut self, from: usize, to: usize) {
        let side = self.active_side();
        let Some(i) = self.current_view().and_then(|fv| fv.current_hunk(side)) else {
            return self.set_message("Cursor is not on a change (Ctrl+E/D to navigate)", false);
        };
        self.copy_hunk(i, from, to);
    }

    fn copy_hunk(&mut self, i: usize, from: usize, to: usize) {
        let Some(fv) = self.current_view_mut() else { return };
        if !fv.bufs[to].editable {
            return self.set_message("Target side is read-only", true);
        }
        let Some(h) = fv.diff.hunks.get(i).cloned() else { return };
        let src = fv.bufs[from].diff_lines()[h.side(from)].to_vec();
        let tnl = fv.bufs[from].trailing_newline;
        fv.bufs[to].replace_lines(h.side(to), &src, tnl);
        fv.refresh();
    }

    fn goto_file(&mut self, forward: bool) {
        if !self.dir_mode {
            return;
        }
        match self.tree.adjacent_file(forward) {
            Some(id) => {
                self.tree.reveal(id);
                self.open_view(self.tree.nodes[id].rel.clone());
            }
            None if !self.tree.files.is_empty() => self.set_message(
                if forward {
                    "Already at the last file"
                } else {
                    "Already at the first file"
                },
                false,
            ),
            None => {}
        }
    }

    fn save_buffer(&mut self, key: &Path, side: usize) -> bool {
        let Some(fv) = self.views.get_mut(key) else { return true };
        let b = &mut fv.bufs[side];
        if !b.dirty || !b.editable {
            return true;
        }
        match b.save() {
            Ok(()) => {
                let msg = format!("Saved {}", b.path.display());
                self.set_message(msg, false);
                if self.dir_mode {
                    let (l, r) = self.paths_for(key);
                    let mut status = scan::status_of(&l, &r);
                    if status == scan::Status::Same && self.renamed.contains_key(key) {
                        status = scan::Status::Renamed;
                    }
                    self.tree.set_status(key, status);
                }
                true
            }
            Err(e) => {
                self.set_message(format!("Save failed: {e:#}"), true);
                false
            }
        }
    }

    /// Saves the modified side(s) of the current file pair.
    fn save_current(&mut self) {
        let Some(key) = self.current.clone() else { return };
        let mut any = false;
        for s in 0..2 {
            any |= self.views[&key].bufs[s].dirty;
            self.save_buffer(&key, s);
        }
        if !any {
            self.set_message("Nothing to save", false);
        }
    }

    fn save_all(&mut self) -> bool {
        let keys: Vec<PathBuf> = self
            .views
            .iter()
            .filter(|(_, v)| v.any_dirty())
            .map(|(k, _)| k.clone())
            .collect();
        let mut ok = true;
        for k in &keys {
            for s in 0..2 {
                ok &= self.save_buffer(k, s);
            }
        }
        if ok && !keys.is_empty() {
            self.set_message(format!("Saved {} file(s)", keys.len()), false);
        }
        ok
    }

    pub fn dirty_count(&self) -> usize {
        self.views
            .values()
            .flat_map(|v| v.bufs.iter())
            .filter(|b| b.dirty)
            .count()
    }

    fn request_quit(&mut self) {
        if self.dirty_count() > 0 {
            self.quit_prompt = true;
        } else {
            self.quit = true;
        }
    }

    fn press(&mut self, b: Button) {
        match b {
            Button::PrevHunk => self.goto_hunk(false),
            Button::NextHunk => self.goto_hunk(true),
            Button::PrevFile => self.goto_file(false),
            Button::NextFile => self.goto_file(true),
            Button::Save => self.save_current(),
            Button::Quit => self.request_quit(),
            Button::SaveAll => {
                if self.save_all() {
                    self.quit = true;
                }
                self.quit_prompt = false;
            }
            Button::Discard => self.quit = true,
            Button::Cancel => self.quit_prompt = false,
        }
    }

    // ------------------------------------------------------------------ mouse

    /// Buffer position under screen cell (x, y) in `side`'s pane.
    fn pos_at(&self, side: usize, x: u16, y: u16) -> Option<Pos> {
        let a = self.areas.panes[side];
        let fv = self.current_view()?;
        let y = y.clamp(a.y, a.bottom().saturating_sub(1));
        let row = fv.scroll + (y - a.y) as usize;
        let b = &fv.bufs[side];
        let line = fv.diff.line_near_row(side, row).min(b.lines.len() - 1);
        let dcol = x.saturating_sub(a.x + self.areas.gutter[side]) as usize + fv.hscroll[side];
        Some(Pos::new(line, col_at_display(&b.lines[line], dcol)))
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        let (x, y) = (m.column, m.row);
        let p = Position::new(x, y);
        let shift = m.modifiers.contains(KeyModifiers::SHIFT);
        if let Some(off) = &mut self.help {
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) => self.help = None,
                MouseEventKind::ScrollDown => *off = off.saturating_add(3),
                MouseEventKind::ScrollUp => *off = off.saturating_sub(3),
                _ => {}
            }
            return;
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(&(_, b)) = self.areas.buttons.iter().find(|(r, _)| r.contains(p)) {
                    self.message = None;
                    return self.press(b);
                }
                if self.quit_prompt {
                    return;
                }
                self.message = None;
                self.search = None;
                if self.areas.splitter == Some(x) && self.areas.tree.y <= y && y < self.areas.tree.bottom() {
                    self.drag = Some(Drag::Splitter);
                } else if self.areas.center.contains(p) && x == self.areas.center.x + 1 {
                    if self.double_click(x, y) {
                        self.pane_split = 500;
                    } else {
                        self.drag = Some(Drag::PaneSplit);
                    }
                } else if let Some(panel) = (0..3).find(|&i| self.areas.vbars[i].contains(p)) {
                    self.click_scrollbar(panel, y);
                } else if self.areas.tree.contains(p) {
                    self.click_tree(y);
                } else if self.areas.center.contains(p) {
                    self.click_center(x, y);
                } else if let Some(side) = (0..2).find(|&s| self.areas.panes[s].contains(p)) {
                    self.click_pane(side, x, y, shift);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => match self.drag {
                Some(Drag::Splitter) => {
                    self.tree_width = (x + 1).clamp(12, self.areas.width.saturating_sub(20).max(12));
                }
                Some(Drag::PaneSplit) => self.set_split_x(x),
                Some(Drag::Scrollbar { panel, grab }) => self.drag_scrollbar(panel, y, grab),
                Some(Drag::Select(side)) => {
                    let a = self.areas.panes[side];
                    if let Some(fv) = self.current_view_mut() {
                        if y < a.y {
                            fv.scroll = fv.scroll.saturating_sub(1);
                        } else if y >= a.bottom() {
                            fv.scroll = (fv.scroll + 1).min(fv.diff.rows.len().saturating_sub(1));
                        }
                    }
                    if let Some(pos) = self.pos_at(side, x, y) {
                        if let Some(fv) = self.current_view_mut() {
                            fv.bufs[side].set_cursor(pos, true);
                        }
                        self.ensure_cursor_visible(side);
                    }
                }
                None => {}
            },
            MouseEventKind::Up(MouseButton::Left) => self.drag = None,
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let down = m.kind == MouseEventKind::ScrollDown;
                if self.areas.tree.contains(p) || self.areas.vbars[0].contains(p) {
                    let max = self.tree.visible.len().saturating_sub(self.areas.tree.height as usize);
                    self.tree.offset = if down {
                        (self.tree.offset + 3).min(max)
                    } else {
                        self.tree.offset.saturating_sub(3)
                    };
                } else if shift {
                    self.hscroll(p, if down { 6 } else { -6 });
                } else if let Some(fv) = self.current_view_mut() {
                    let max = fv.diff.rows.len().saturating_sub(1);
                    fv.scroll = if down {
                        (fv.scroll + 3).min(max)
                    } else {
                        fv.scroll.saturating_sub(3)
                    };
                }
            }
            MouseEventKind::ScrollRight => self.hscroll(p, 6),
            MouseEventKind::ScrollLeft => self.hscroll(p, -6),
            _ => {}
        }
    }

    fn hscroll(&mut self, p: Position, delta: isize) {
        let sides: Vec<usize> = (0..2).filter(|&s| self.areas.panes[s].contains(p)).collect();
        let sides = if sides.is_empty() { vec![0, 1] } else { sides };
        if let Some(fv) = self.current_view_mut() {
            for s in sides {
                fv.hscroll[s] = fv.hscroll[s].saturating_add_signed(delta);
            }
        }
    }

    /// (total rows, offset) behind scrollbar `panel`.
    fn scroll_state(&self, panel: usize) -> Option<(usize, usize)> {
        if panel == 0 {
            return Some((self.tree.visible.len(), self.tree.offset));
        }
        let fv = self.current_view()?;
        Some((fv.scroll_total(self.areas.vbars[panel].height as usize), fv.scroll))
    }

    /// Grabs the thumb, or centers it on `y` when clicking the track.
    fn click_scrollbar(&mut self, panel: usize, y: u16) {
        let a = self.areas.vbars[panel];
        let Some((total, offset)) = self.scroll_state(panel) else {
            return;
        };
        let (start, len) = ui::thumb(a.height, total, offset, a.height as usize);
        let rel = y - a.y;
        let grab = if (start..start + len).contains(&rel) {
            rel - start
        } else {
            len / 2
        };
        self.drag = Some(Drag::Scrollbar { panel, grab });
        self.drag_scrollbar(panel, y, grab);
    }

    fn drag_scrollbar(&mut self, panel: usize, y: u16, grab: u16) {
        let a = self.areas.vbars[panel];
        let Some((total, _)) = self.scroll_state(panel) else {
            return;
        };
        let rel = y.clamp(a.y, a.bottom().saturating_sub(1)) - a.y;
        let offset = ui::offset_at(a.height, total, a.height as usize, rel, grab);
        if panel == 0 {
            self.tree.offset = offset;
        } else if let Some(fv) = self.current_view_mut() {
            fv.scroll = offset;
        }
    }

    fn click_tree(&mut self, y: u16) {
        self.set_focus(Focus::Tree);
        let row = self.tree.offset + (y - self.areas.tree.y) as usize;
        if row >= self.tree.visible.len() {
            return;
        }
        self.tree.selected = row;
        let n = self.tree.visible[row].node;
        if self.tree.nodes[n].is_dir {
            self.tree.toggle(n);
        } else {
            self.open_view(self.tree.nodes[n].rel.clone());
        }
    }

    /// Records a click at (x, y); true when it completes a double-click there.
    fn double_click(&mut self, x: u16, y: u16) -> bool {
        let now = Instant::now();
        let double = self
            .last_click
            .is_some_and(|(t, lx, ly)| lx == x && ly == y && now.duration_since(t) < Duration::from_millis(400));
        self.last_click = if double { None } else { Some((now, x, y)) };
        double
    }

    /// Moves the divider between the panes to screen column `x`.
    fn set_split_x(&mut self, x: u16) {
        let body = self.areas.diff_body;
        let avail = u32::from(body.width.saturating_sub(3));
        if avail == 0 {
            return;
        }
        // The divider is the middle of the 3-column center, one column after the left pane.
        let left = u32::from(x.saturating_sub(body.x + 1)).min(avail);
        self.pane_split = ((left * 1000 + avail / 2) / avail) as u16;
    }

    fn click_center(&mut self, x: u16, y: u16) {
        let c = self.areas.center;
        let Some(fv) = self.current_view() else { return };
        let row = fv.scroll + (y - c.y) as usize;
        let Some(i) = fv.diff.hunks.iter().position(|h| h.rows.start == row) else {
            return;
        };
        match x - c.x {
            0 => self.copy_hunk(i, 1, 0),
            2 => self.copy_hunk(i, 0, 1),
            _ => {}
        }
    }

    fn click_pane(&mut self, side: usize, x: u16, y: u16, shift: bool) {
        self.set_focus(Focus::Pane(side));
        let Some(pos) = self.pos_at(side, x, y) else { return };
        let double = self.double_click(x, y);
        let Some(fv) = self.current_view_mut() else { return };
        if double {
            fv.bufs[side].select_word_at(pos);
        } else {
            fv.bufs[side].set_cursor(pos, shift);
            self.drag = Some(Drag::Select(side));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Options {
        Options {
            git: false,
            right_editable: false,
            readonly: false,
            range_mode: false,
            renames: true,
            theme: String::new(),
        }
    }

    #[test]
    fn git_difftool_editability() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("git-difftool.abc123");
        let worktree = tmp.path().join("wt");
        for d in [root.join("left"), root.join("right"), worktree.clone()] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(root.join("left/a.txt"), "old\n").unwrap();
        fs::write(worktree.join("a.txt"), "new\n").unwrap();
        // Working-tree file with --symlinks: git symlinks it into the right dir.
        #[cfg(unix)]
        std::os::unix::fs::symlink(worktree.join("a.txt"), root.join("right/a.txt")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(worktree.join("a.txt"), root.join("right/a.txt")).unwrap();
        // Working-tree file with --no-symlinks (Windows default): plain copy
        // in the temp dir. Git copies it back when the tool exits.
        fs::write(root.join("left/c.txt"), "old\n").unwrap();
        fs::write(root.join("right/c.txt"), "new\n").unwrap();

        let mut app = App::new(root.join("left"), root.join("right"), opts()).unwrap();
        assert!(app.git, "auto-detected from the temp dir name");
        app.open_view("a.txt".into());
        let v = app.current_view().unwrap();
        assert!(!v.bufs[0].editable && v.bufs[1].editable);
        app.open_view("c.txt".into());
        let v = app.current_view().unwrap();
        assert!(!v.bufs[0].editable && v.bufs[1].editable);

        // Internal `tuidiff REV..REV`: both sides are commits with no
        // copy-back, so the right side stays read-only.
        let range_app = App::new(
            root.join("left"),
            root.join("right"),
            Options {
                range_mode: true,
                ..opts()
            },
        )
        .unwrap();
        assert!(range_app.git);
        assert!(!range_app.editable(1, &root.join("right/c.txt")));
        assert!(!range_app.editable(1, &root.join("right/a.txt")));

        let app = App::new(
            root.join("left"),
            root.join("right"),
            Options {
                right_editable: true,
                range_mode: true,
                ..opts()
            },
        )
        .unwrap();
        assert!(app.editable(1, &root.join("right/c.txt")));
    }

    #[test]
    fn standalone_both_editable_and_dev_null_readonly() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("x.txt");
        fs::write(&f, "x\n").unwrap();
        let app = App::new("/dev/null".into(), f.clone(), opts()).unwrap();
        let v = app.current_view().unwrap();
        assert!(!v.bufs[0].editable && !v.bufs[0].exists && v.bufs[1].editable);
        assert!(!app.dir_mode);
    }
}
