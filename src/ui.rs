//! Rendering.

use std::ops::Range;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::buffer::Buffer as TBuf;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};

use crate::app::{App, Areas, Button, FileView, Focus};
use crate::buffer::Buffer;
use crate::diff::{Kind, inline_changes};
use crate::highlight::Highlighter;
use crate::scan::Status;
use crate::text::{char_width, display_col, visible_char};

const FG: Color = Color::Rgb(208, 208, 208);
const DIM: Color = Color::Rgb(100, 100, 112);
const ACCENT: Color = Color::Rgb(97, 175, 239);
const YELLOW: Color = Color::Rgb(229, 192, 123);
const GREEN: Color = Color::Rgb(152, 195, 121);
const RED: Color = Color::Rgb(224, 108, 117);
const BG_DEL: Color = Color::Rgb(72, 30, 34);
const BG_DEL_EMPH: Color = Color::Rgb(135, 45, 52);
const BG_INS: Color = Color::Rgb(28, 64, 38);
const BG_INS_EMPH: Color = Color::Rgb(24, 96, 46);
const BG_CHG: Color = Color::Rgb(30, 46, 78);
const BG_FILL: Color = Color::Rgb(30, 30, 34);
const FG_FILL: Color = Color::Rgb(52, 52, 60);
const BG_CUR: Color = Color::Rgb(40, 42, 52);
const BG_SEL: Color = Color::Rgb(78, 78, 130);
const BG_HEADER: Color = Color::Rgb(46, 49, 62);
const BG_STATUS: Color = Color::Rgb(34, 36, 46);
const BG_TREE_SEL: Color = Color::Rgb(52, 64, 98);
const BG_TREE_SEL_UNFOCUSED: Color = Color::Rgb(48, 48, 54);

fn fill(buf: &mut TBuf, area: Rect, style: Style) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_char(' ').set_style(style);
        }
    }
}

/// Writes `s` at (x, y) without passing `max_x`; returns the next x.
fn put(buf: &mut TBuf, x: u16, y: u16, s: &str, style: Style, max_x: u16) -> u16 {
    if x >= max_x {
        return x;
    }
    buf.set_stringn(x, y, s, (max_x - x) as usize, style).0
}

/// Thumb (start, length) of a scrollbar `track` cells tall over `total` rows of which `visible` start at `offset`.
pub fn thumb(track: u16, total: usize, offset: usize, visible: usize) -> (u16, u16) {
    if track == 0 || total <= visible {
        return (0, track);
    }
    let t = track as usize;
    let len = (t * visible).div_ceil(total).clamp(1, t);
    let max_off = total - visible;
    let start = ((t - len) * offset.min(max_off) + max_off / 2) / max_off;
    (start as u16, len as u16)
}

/// Offset that puts the thumb's top at `y - grab` (track-relative); the inverse of [`thumb`].
pub fn offset_at(track: u16, total: usize, visible: usize, y: u16, grab: u16) -> usize {
    let (_, len) = thumb(track, total, 0, visible);
    let span = (track - len) as usize;
    if span == 0 {
        return 0;
    }
    let max_off = total - visible;
    let pos = (y.saturating_sub(grab) as usize).min(span);
    (pos * max_off + span / 2) / span
}

fn draw_vbar(buf: &mut TBuf, area: Rect, total: usize, offset: usize, focused: bool) {
    let (start, len) = thumb(area.height, total, offset, area.height as usize);
    let thumb_bg = if focused { ACCENT } else { DIM };
    for i in 0..area.height {
        let bg = if (start..start + len).contains(&i) {
            thumb_bg
        } else {
            BG_FILL
        };
        buf[(area.x, area.y + i)].set_char(' ').set_style(Style::new().bg(bg));
    }
}

/// Time per frame spent on syntax highlighting before deferring the rest.
const HL_BUDGET: Duration = Duration::from_millis(25);

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    app.areas = Areas {
        width: area.width,
        ..Default::default()
    };
    app.hl_pending = false;
    let [main, status] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
    let diff_area = if app.dir_mode {
        let tw = app
            .tree_width
            .clamp(12, main.width.saturating_sub(20).max(12))
            .min(main.width);
        let [t, d] = Layout::horizontal([Constraint::Length(tw), Constraint::Min(0)]).areas(main);
        draw_tree(f.buffer_mut(), app, t);
        d
    } else {
        main
    };
    let cursor = draw_diff(f.buffer_mut(), app, diff_area);
    let status_cursor = draw_status(f.buffer_mut(), app, status);
    if app.help.is_some() {
        app.areas.buttons.clear();
        draw_help(f, app, main);
    } else if let Some(cursor) = status_cursor.or(cursor) {
        f.set_cursor_position(cursor);
    }
}

type HelpSection = (&'static str, &'static [(&'static str, &'static str)]);

const HELP: &[HelpSection] = &[
    (
        "Global",
        &[
            (
                "Tab / Shift+Tab / F6",
                "switch focus between tree, left and right (Tab indents in an editable pane)",
            ),
            ("Ctrl+D / Ctrl+E, Alt+↓ / Alt+↑", "next / previous change"),
            (
                "Alt+→ / Alt+←",
                "copy the change under the cursor left→right / right→left",
            ),
            ("Alt+Shift+→ / Alt+Shift+←", "move the divider between the panes"),
            ("Ctrl+N / Ctrl+P", "next / previous file"),
            ("Ctrl+S / F2", "save the current file pair / save all"),
            ("Ctrl+Z / Ctrl+Y (Ctrl+Shift+Z)", "undo / redo"),
            ("Ctrl+F / Ctrl+G", "find / find next"),
            ("F5", "rescan folders (folder mode)"),
            ("F12", "toggle mouse capture (off = terminal text selection)"),
            ("F1", "show / hide this help"),
            ("Ctrl+Q", "quit; asks first if anything is unsaved"),
        ],
    ),
    (
        "File tree",
        &[
            ("↑ ↓ / k j, PgUp PgDn, Home End", "select (opens the file)"),
            ("Enter", "focus the diff, or expand / collapse a folder"),
            ("→ / l", "expand folder / next row"),
            ("← / h", "collapse folder / go to parent"),
            ("Space", "fold / unfold folder"),
            ("Ctrl+← / Ctrl+→", "resize the tree"),
            ("Tab", "focus the diff"),
            ("q / Esc", "quit"),
        ],
    ),
    (
        "Editor (diff panes)",
        &[
            ("Arrows, Ctrl+←/→, Home/End", "move (hold Shift to select)"),
            ("Ctrl+Home / Ctrl+End, PgUp / PgDn", "document start / end, page"),
            ("Ctrl+↑ / Ctrl+↓", "scroll without moving the cursor"),
            ("Ctrl+A", "select all"),
            (
                "Ctrl+C / Ctrl+X / Ctrl+V",
                "copy / cut / paste (no selection = whole line)",
            ),
            ("Ctrl+K", "delete line"),
            ("Ctrl+Backspace / Ctrl+Del", "delete previous / next word"),
            ("Esc", "back to the tree (folder mode)"),
        ],
    ),
    (
        "Find prompt",
        &[("Enter / Ctrl+F / Ctrl+G", "search"), ("Esc", "cancel")],
    ),
    (
        "Quit prompt",
        &[("S", "save all & quit"), ("D", "discard & quit"), ("C / Esc", "cancel")],
    ),
    (
        "Mouse",
        &[
            (
                "Click / drag / double-click",
                "place cursor / select / select word; open or fold tree rows",
            ),
            ("Wheel (Shift = sideways)", "scroll"),
            ("« / »", "copy a change to the other side"),
            ("Scrollbar click / drag", "scroll"),
            ("Drag the tree border", "resize the tree"),
            ("Drag the pane divider", "resize the panes (double-click: 50:50)"),
            ("Status-bar buttons", "navigate, save, quit"),
        ],
    ),
];

fn draw_help(f: &mut Frame, app: &mut App, area: Rect) {
    let w = area.width.saturating_sub(4).min(100);
    if w < 10 || area.height < 6 {
        return;
    }
    let key_w = HELP
        .iter()
        .flat_map(|(_, rows)| rows.iter())
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0)
        .min(w as usize / 2);
    let mut lines: Vec<(Option<&str>, &str, &str)> = Vec::new();
    for (i, (title, rows)) in HELP.iter().enumerate() {
        if i > 0 {
            lines.push((None, "", ""));
        }
        lines.push((Some(title), "", ""));
        lines.extend(rows.iter().map(|(k, a)| (None, *k, *a)));
    }

    let h = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h);
    let buf = f.buffer_mut();
    let bg = Style::new().bg(BG_HEADER).fg(FG);
    fill(buf, popup, bg);

    let inner = Rect::new(popup.x + 2, popup.y + 1, w - 4, h - 2);
    let max_off = lines.len().saturating_sub(inner.height as usize);
    let off = app.help.unwrap_or(0).min(max_off);
    app.help = Some(off);

    let right = inner.right();
    for (i, (title, k, a)) in lines.iter().skip(off).take(inner.height as usize).enumerate() {
        let y = inner.y + i as u16;
        if let Some(t) = title {
            put(buf, inner.x, y, t, bg.fg(ACCENT).add_modifier(Modifier::BOLD), right);
        } else {
            let x = put(buf, inner.x + 1, y, &format!("{k:<key_w$}"), bg.fg(YELLOW), right);
            put(buf, x + 2, y, a, bg, right);
        }
    }

    // Border with title and scroll hints.
    let edge = bg.fg(DIM);
    for x in popup.left() + 1..popup.right() - 1 {
        buf[(x, popup.top())].set_char('─').set_style(edge);
        buf[(x, popup.bottom() - 1)].set_char('─').set_style(edge);
    }
    for y in popup.top() + 1..popup.bottom() - 1 {
        buf[(popup.left(), y)].set_char('│').set_style(edge);
        buf[(popup.right() - 1, y)].set_char('│').set_style(edge);
    }
    buf[(popup.left(), popup.top())].set_char('┌').set_style(edge);
    buf[(popup.right() - 1, popup.top())].set_char('┐').set_style(edge);
    buf[(popup.left(), popup.bottom() - 1)].set_char('└').set_style(edge);
    buf[(popup.right() - 1, popup.bottom() - 1)]
        .set_char('┘')
        .set_style(edge);
    put(
        buf,
        popup.x + 2,
        popup.y,
        " Keys · ↑↓ scroll · F1/Esc close ",
        bg.fg(ACCENT).add_modifier(Modifier::BOLD),
        popup.right() - 1,
    );
    if off > 0 {
        put(buf, popup.right() - 4, popup.y, " ↑ ", edge, popup.right() - 1);
    }
    if off < max_off {
        put(
            buf,
            popup.right() - 4,
            popup.bottom() - 1,
            " ↓ ",
            edge,
            popup.right() - 1,
        );
    }
}

fn status_color(s: Status) -> Color {
    match s {
        Status::Modified => YELLOW,
        Status::RightOnly => GREEN,
        Status::LeftOnly => RED,
        Status::Same => DIM,
    }
}

fn draw_tree(buf: &mut TBuf, app: &mut App, area: Rect) {
    if area.width < 3 || area.height < 2 {
        return;
    }
    let focused = app.focus == Focus::Tree;
    let sep_x = area.right() - 1;
    for y in area.top()..area.bottom() {
        buf[(sep_x, y)]
            .set_char('│')
            .set_style(Style::new().fg(if focused { ACCENT } else { DIM }));
    }
    app.areas.splitter = Some(sep_x);

    let header = Rect::new(area.x, area.y, area.width - 1, 1);
    fill(buf, header, Style::new().bg(BG_HEADER));
    let n = app.tree.files.len();
    let title = format!(" Changes ({n} file{})", if n == 1 { "" } else { "s" });
    put(
        buf,
        header.x,
        header.y,
        &title,
        Style::new().fg(FG).bg(BG_HEADER).add_modifier(Modifier::BOLD),
        header.right(),
    );

    let list = Rect::new(area.x, area.y + 1, area.width - 2, area.height - 1);
    let bar = Rect::new(list.right(), list.y, 1, list.height);
    app.areas.tree = list;
    app.areas.vbars[0] = bar;
    app.tree.ensure_visible(list.height as usize);
    draw_vbar(buf, bar, app.tree.visible.len(), app.tree.offset, focused);
    let tree = &app.tree;
    for (i, vr) in tree
        .visible
        .iter()
        .enumerate()
        .skip(tree.offset)
        .take(list.height as usize)
    {
        let y = list.y + (i - tree.offset) as u16;
        let node = &tree.nodes[vr.node];
        let bg = if i == tree.selected {
            if focused { BG_TREE_SEL } else { BG_TREE_SEL_UNFOCUSED }
        } else {
            Color::Reset
        };
        fill(buf, Rect::new(list.x, y, list.width, 1), Style::new().bg(bg));
        let right = list.right();
        let mut x = put(buf, list.x, y, &vr.prefix, Style::new().fg(DIM).bg(bg), right);
        let color = status_color(node.status);
        let (icon, icon_color) = if node.is_dir {
            (if node.expanded { "📂" } else { "📁" }, color)
        } else {
            let g = match node.status {
                Status::Modified => "●",
                Status::RightOnly => "✚",
                Status::LeftOnly => "✖",
                Status::Same => "✔",
            };
            (g, color)
        };
        x = put(buf, x, y, icon, Style::new().fg(icon_color).bg(bg), right);
        x = put(buf, x, y, " ", Style::new().bg(bg), right);
        let mut style = Style::new().fg(if node.is_dir { ACCENT } else { color }).bg(bg);
        if node.is_dir {
            style = style.add_modifier(Modifier::BOLD);
        }
        if app.current.as_deref() == Some(node.rel.as_path()) && !node.is_dir {
            style = style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
        }
        if node.status == Status::LeftOnly {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        x = put(buf, x, y, &node.name, style, right);
        if !node.is_dir && app.views.get(&node.rel).is_some_and(FileView::any_dirty) {
            put(buf, x, y, " ✎", Style::new().fg(YELLOW).bg(bg), right);
        }
    }
}

fn header_label(app: &App, side: usize, rel: &std::path::Path) -> String {
    let root = if side == 0 { &app.left_root } else { &app.right_root };
    if !app.dir_mode {
        return root.display().to_string();
    }
    let root_name = if app.git {
        if side == 0 { "a".to_string() } else { "b".to_string() }
    } else {
        root.file_name()
            .map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned())
    };
    format!("{root_name}/{}", rel.display())
}

fn draw_header(buf: &mut TBuf, area: Rect, label: &str, b: &Buffer, focused: bool) {
    fill(buf, area, Style::new().bg(BG_HEADER));
    let right = area.right();
    let mut x = area.x + 1;
    if b.dirty {
        x = put(buf, x, area.y, "● ", Style::new().fg(YELLOW).bg(BG_HEADER), right);
    }
    let mut style = Style::new().fg(if focused { Color::White } else { FG }).bg(BG_HEADER);
    if focused {
        style = style.add_modifier(Modifier::BOLD);
    }
    x = put(buf, x, area.y, label, style, right);
    let tag = Style::new().fg(RED).bg(BG_HEADER);
    if !b.exists {
        x = put(buf, x, area.y, "  [missing]", tag, right);
    }
    if b.binary {
        x = put(buf, x, area.y, "  [binary]", tag, right);
    }
    if !b.editable {
        put(
            buf,
            x,
            area.y,
            "  [read-only]",
            Style::new().fg(DIM).bg(BG_HEADER),
            right,
        );
    }
}

fn draw_diff(buf: &mut TBuf, app: &mut App, area: Rect) -> Option<(u16, u16)> {
    if area.height < 2 || area.width < 10 {
        return None;
    }
    let Some(key) = app.current.clone() else {
        let msg = "No differences";
        let x = area.x + area.width.saturating_sub(msg.len() as u16) / 2;
        put(
            buf,
            x,
            area.y + area.height / 2,
            msg,
            Style::new().fg(DIM),
            area.right(),
        );
        return None;
    };
    let [header, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let avail = u32::from(body.width.saturating_sub(3));
    let min = 10.min(avail / 2);
    let lw = ((avail * u32::from(app.pane_split) + 500) / 1000).clamp(min, avail - min) as u16;
    let cols = [Constraint::Length(lw), Constraint::Length(3), Constraint::Fill(1)];
    let [lp, center, rp] = Layout::horizontal(cols).areas(body);
    let [lh, ch, rh] = Layout::horizontal(cols).areas(header);
    fill(buf, ch, Style::new().bg(BG_HEADER));
    let labels = [header_label(app, 0, &key), header_label(app, 1, &key)];
    // The last column of each pane is its scrollbar.
    let split = |r: Rect| {
        if r.width < 2 {
            return (r, Rect::default());
        }
        let bar = Rect::new(r.right() - 1, r.y, 1, r.height);
        (
            Rect {
                width: r.width - 1,
                ..r
            },
            bar,
        )
    };
    let ((lp, lbar), (rp, rbar)) = (split(lp), split(rp));
    app.areas.center = center;
    app.areas.diff_body = body;
    app.areas.panes = [lp, rp];
    app.areas.vbars[1] = lbar;
    app.areas.vbars[2] = rbar;

    let focus = app.focus;
    let deadline = Instant::now() + HL_BUDGET;
    let fv = app.views.get_mut(&key)?;
    fv.refresh();
    let mut cursor = None;
    let total = fv.scroll_total(body.height as usize);
    for (side, (pane, head, bar)) in [(lp, lh, lbar), (rp, rh, rbar)].into_iter().enumerate() {
        let focused = focus == Focus::Pane(side);
        draw_header(buf, head, &labels[side], &fv.bufs[side], focused);
        draw_vbar(buf, bar, total, fv.scroll, focused);
        let digits = fv.bufs[side].lines.len().to_string().len().max(3) as u16;
        let gutter = (digits + 1).min(pane.width);
        app.areas.gutter[side] = gutter;
        let (c, done) = draw_pane(buf, pane, gutter, fv, side, focused, &app.hl, deadline);
        app.hl_pending |= !done;
        if focused {
            cursor = c;
        }
    }
    draw_center(buf, center, fv);
    cursor
}

/// Draws one side; returns the cursor position and whether highlighting is complete.
#[allow(clippy::too_many_arguments)]
fn draw_pane(
    buf: &mut TBuf,
    area: Rect,
    gutter: u16,
    fv: &mut FileView,
    side: usize,
    focused: bool,
    hl: &Highlighter,
    deadline: Instant,
) -> (Option<(u16, u16)>, bool) {
    let h = area.height as usize;
    let text_x = area.x + gutter;
    let text_w = area.width - gutter;
    if fv.bufs[side].binary {
        put(
            buf,
            text_x,
            area.y,
            "Binary file — not shown",
            Style::new().fg(DIM),
            area.right(),
        );
        return (None, true);
    }
    let scroll = fv.scroll;
    let rows = &fv.diff.rows;
    let end = (scroll + h).min(rows.len());
    let mut done = true;
    if let Some(max_line) = rows[scroll.min(end)..end].iter().filter_map(|r| r.side(side)).max() {
        done = fv.hl[side].ensure(hl, &fv.bufs[side].lines, max_line + 1, deadline);
    }
    for row in &rows[scroll.min(end)..end] {
        if row.kind == Kind::Replace
            && let (Some(ll), Some(rl)) = (row.left, row.right)
        {
            fv.inline
                .entry((ll, rl))
                .or_insert_with(|| inline_changes(&fv.bufs[0].lines[ll], &fv.bufs[1].lines[rl]));
        }
    }
    let b = &fv.bufs[side];
    let sel = b.selection();
    let hscroll = fv.hscroll[side];
    for i in 0..h {
        let y = area.y + i as u16;
        let ri = scroll + i;
        let gutter_rect = Rect::new(area.x, y, gutter, 1);
        let text_rect = Rect::new(text_x, y, text_w, 1);
        let Some(row) = rows.get(ri) else {
            fill(buf, Rect::new(area.x, y, area.width, 1), Style::new());
            continue;
        };
        let Some(l) = row.side(side) else {
            fill(buf, gutter_rect, Style::new().bg(BG_FILL));
            fill(buf, text_rect, Style::new().bg(BG_FILL));
            for x in text_rect.left()..text_rect.right() {
                buf[(x, y)].set_char('╱').set_fg(FG_FILL);
            }
            continue;
        };
        let is_cur = l == b.cursor.line;
        let base = match row.kind {
            Kind::Equal if focused && is_cur => BG_CUR,
            Kind::Equal => Color::Reset,
            Kind::Delete => BG_DEL,
            Kind::Insert => BG_INS,
            Kind::Replace => BG_CHG,
        };
        let gstyle = Style::new().fg(if is_cur { FG } else { DIM }).bg(base);
        fill(buf, gutter_rect, gstyle);
        let num = format!("{:>w$} ", l + 1, w = gutter.saturating_sub(1) as usize);
        put(buf, area.x, y, &num, gstyle, area.x + gutter);

        let line = &b.lines[l];
        let mut emph: &[Range<usize>] = &[];
        if row.kind == Kind::Replace
            && let (Some(ll), Some(rl)) = (row.left, row.right)
            && let Some((a, bb)) = fv.inline.get(&(ll, rl))
        {
            emph = if side == 0 { a } else { bb };
        }
        let emph_bg = if side == 0 { BG_DEL_EMPH } else { BG_INS_EMPH };
        let line_sel = sel.and_then(|(s, e)| {
            (s.line <= l && l <= e.line).then_some((
                if l == s.line { s.col } else { 0 },
                if l == e.line { e.col } else { usize::MAX },
            ))
        });
        let spans = fv.hl[side].lines.get(l).map(Vec::as_slice).unwrap_or(&[]);
        draw_text(buf, text_rect, line, spans, hscroll, base, emph, emph_bg, line_sel);
    }
    if !focused {
        return (None, done);
    }
    let row = fv.cursor_row(side);
    let dc = display_col(&b.lines[b.cursor.line], b.cursor.col);
    if row < scroll || row >= scroll + h || dc < hscroll || dc - hscroll >= text_w as usize {
        return (None, done);
    }
    (
        Some((text_x + (dc - hscroll) as u16, area.y + (row - scroll) as u16)),
        done,
    )
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    buf: &mut TBuf,
    area: Rect,
    line: &str,
    spans: &[(Range<usize>, Color)],
    hscroll: usize,
    base_bg: Color,
    emph: &[Range<usize>],
    emph_bg: Color,
    sel: Option<(usize, usize)>,
) {
    let y = area.y;
    let right = area.right();
    let mut x = area.x;
    let mut d = 0usize;
    let mut hi = 0;
    let mut nchars = 0;
    for (ci, (bi, ch)) in line.char_indices().enumerate() {
        nchars = ci + 1;
        let w = char_width(ch, d);
        let start = d;
        d += w;
        if d <= hscroll || w == 0 {
            continue;
        }
        if x >= right {
            break;
        }
        while hi < spans.len() && spans[hi].0.end <= bi {
            hi += 1;
        }
        let mut fg = if hi < spans.len() && spans[hi].0.start <= bi {
            spans[hi].1
        } else {
            FG
        };
        let mut bg = base_bg;
        if emph.iter().any(|r| r.contains(&ci)) {
            bg = emph_bg;
        }
        if sel.is_some_and(|(s, e)| ci >= s && ci < e) {
            bg = BG_SEL;
        }
        let shown = visible_char(ch);
        if shown != ch {
            fg = DIM;
        }
        let style = Style::new().fg(fg).bg(bg);
        let visible_w = d - start.max(hscroll);
        if ch == '\t' || start < hscroll || x as usize + w > right as usize {
            for _ in 0..visible_w {
                if x >= right {
                    break;
                }
                buf[(x, y)].set_char(' ').set_style(style);
                x += 1;
            }
        } else {
            buf.set_stringn(x, y, shown.encode_utf8(&mut [0; 4]), w, style);
            x += w as u16;
        }
    }
    if line.is_empty() {
        nchars = 0;
    }
    if let Some((s, e)) = sel
        && s <= nchars
        && e > nchars
        && x < right
        && d >= hscroll
    {
        buf[(x, y)].set_char(' ').set_style(Style::new().bg(BG_SEL));
        x += 1;
    }
    while x < right {
        buf[(x, y)].set_char(' ').set_style(Style::new().bg(base_bg));
        x += 1;
    }
}

fn draw_center(buf: &mut TBuf, area: Rect, fv: &FileView) {
    let editable = [fv.bufs[0].editable, fv.bufs[1].editable];
    for i in 0..area.height {
        let y = area.y + i;
        let ri = fv.scroll + i as usize;
        let kind = fv.diff.rows.get(ri).map(|r| r.kind);
        let (mid, color) = match kind {
            None | Some(Kind::Equal) => ('│', DIM),
            Some(Kind::Delete) => ('┃', RED),
            Some(Kind::Insert) => ('┃', GREEN),
            Some(Kind::Replace) => ('┃', ACCENT),
        };
        let style = Style::new().fg(color);
        let starts = fv.diff.hunks.iter().any(|h| h.rows.start == ri);
        let arrow = Style::new().fg(Color::White).bg(color).add_modifier(Modifier::BOLD);
        let cell = |c: bool, ch: char| if starts && c { (ch, arrow) } else { (' ', Style::new()) };
        let cells = [cell(editable[0], '«'), (mid, style), cell(editable[1], '»')];
        for (j, (ch, st)) in cells.into_iter().enumerate() {
            buf[(area.x + j as u16, y)].set_char(ch).set_style(st);
        }
    }
}

fn draw_status(buf: &mut TBuf, app: &mut App, area: Rect) -> Option<(u16, u16)> {
    let bg = Style::new().bg(BG_STATUS);
    fill(buf, area, bg);
    let y = area.y;
    let right = area.right();
    let btn = Style::new().fg(Color::Black).bg(ACCENT);
    let mut buttons = Vec::new();
    let mut add_button = |buf: &mut TBuf, x: u16, label: &str, b: Button| -> u16 {
        let w = label.chars().count() as u16 + 2;
        let nx = put(buf, x, y, &format!(" {label} "), btn, right);
        buttons.push((Rect::new(x, y, w.min(right.saturating_sub(x)), 1), b));
        put(buf, nx, y, " ", bg, right)
    };
    if app.quit_prompt {
        let msg = format!(" {} unsaved file(s). ", app.dirty_count());
        let mut x = put(buf, area.x, y, &msg, bg.fg(YELLOW).add_modifier(Modifier::BOLD), right);
        for (label, b) in [
            ("[S]ave all & quit", Button::SaveAll),
            ("[D]iscard & quit", Button::Discard),
            ("[C]ancel", Button::Cancel),
        ] {
            x = add_button(buf, x, label, b);
        }
        app.areas.buttons = buttons;
        return None;
    }
    if let Some(q) = &app.search {
        let x = put(
            buf,
            area.x,
            y,
            " Find: ",
            bg.fg(ACCENT).add_modifier(Modifier::BOLD),
            right,
        );
        let end = put(buf, x, y, q, bg.fg(Color::White), right);
        put(buf, end, y, "   Enter search · Esc cancel", bg.fg(DIM), right);
        app.areas.buttons = Vec::new();
        return Some((end.min(right.saturating_sub(1)), y));
    }

    let mut items: Vec<(&str, Button)> = vec![("◀ Chg", Button::PrevHunk), ("Chg ▶", Button::NextHunk)];
    if app.dir_mode {
        items.extend([("◀ File", Button::PrevFile), ("File ▶", Button::NextFile)]);
    }
    items.extend([("Save", Button::Save), ("Quit", Button::Quit)]);
    let side = app.active_side();
    let info = app.current_view().map(|fv| {
        let n = fv.diff.hunks.len();
        match fv.current_hunk(side) {
            Some(i) => format!("change {}/{n} ", i + 1),
            None => format!("{n} change{} ", if n == 1 { "" } else { "s" }),
        }
    });
    let info = format!(
        "{}{}",
        if app.mouse_enabled { "" } else { "[mouse off] " },
        info.unwrap_or_default()
    );
    let buttons_w: u16 = items.iter().map(|(l, _)| l.chars().count() as u16 + 3).sum();
    let info_w = info.chars().count() as u16;
    let start = right.saturating_sub(buttons_w + info_w);
    let mut x = put(buf, start, y, &info, bg.fg(FG), right);
    for (label, b) in items {
        x = add_button(buf, x, label, b);
    }
    app.areas.buttons = buttons;

    let (text, style) = match &app.message {
        Some((m, true)) => (m.clone(), bg.fg(RED).add_modifier(Modifier::BOLD)),
        Some((m, false)) => (m.clone(), bg.fg(GREEN)),
        None => (
            match app.focus {
                Focus::Tree => "F1 help · ↑↓ select · Enter open · Space fold · Tab diff · Ctrl+N/P file · F5 rescan · q quit",
                Focus::Pane(_) => "F1 help · Ctrl+F/G find · Ctrl+E/D change · Alt+←→ copy change · Ctrl+S save · F2 save all · Ctrl+Z/Y undo/redo · F6 focus · Esc tree · F12 mouse",
            }
            .to_string(),
            bg.fg(DIM),
        ),
    };
    put(
        buf,
        area.x + 1,
        y,
        &text,
        style,
        start.saturating_sub(1).max(area.x + 1),
    );
    None
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Position;

    use super::{offset_at, thumb};
    use crate::app::{App, Options};

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    fn render(term: &mut Terminal<TestBackend>, app: &mut App) -> String {
        term.draw(|f| super::draw(f, app)).unwrap();
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            let mut skip = 0;
            for x in 0..buf.area.width {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                let sym = buf[(x, y)].symbol();
                skip = unicode_width::UnicodeWidthStr::width(sym).saturating_sub(1);
                out.push_str(sym);
            }
            out.push('\n');
        }
        out
    }

    fn key(app: &mut App, code: KeyCode, m: KeyModifiers) {
        app.handle_event(Event::Key(KeyEvent::new(code, m)));
    }

    fn click(app: &mut App, x: u16, y: u16) {
        let ev = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            })
        };
        app.handle_event(ev(MouseEventKind::Down(MouseButton::Left)));
        app.handle_event(ev(MouseEventKind::Up(MouseButton::Left)));
    }

    fn mouse(app: &mut App, kind: MouseEventKind, x: u16, y: u16) {
        app.handle_event(Event::Mouse(MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }));
    }

    fn opts() -> Options {
        Options {
            git: false,
            right_editable: false,
            readonly: false,
            range_mode: false,
            theme: "base16-eighties.dark".into(),
        }
    }

    #[test]
    fn help_overlay() {
        let l = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        write(&l.path().join("a.txt"), "one\n");
        write(&r.path().join("a.txt"), "two\n");
        let mut app = App::new(l.path().join("a.txt"), r.path().join("a.txt"), opts()).unwrap();
        let mut term = Terminal::new(TestBackend::new(110, 30)).unwrap();
        render(&mut term, &mut app);
        key(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        let s = render(&mut term, &mut app);
        assert!(s.contains("File tree") && s.contains("Editor") && s.contains("Ctrl+Q"));
        key(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(app.current_view().unwrap().bufs[1].lines[0], "two");
        key(&mut app, KeyCode::End, KeyModifiers::NONE);
        assert!(render(&mut term, &mut app).contains("Status-bar buttons"));
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.help.is_none());
        assert!(!render(&mut term, &mut app).contains("Status-bar buttons"));
    }

    #[test]
    fn find_prompt_and_next() {
        let l = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        write(&l.path().join("a.txt"), "one\nfoo\nbar foo\n");
        write(&r.path().join("a.txt"), "one\nFOO\nbar foo\n");
        let mut app = App::new(l.path().join("a.txt"), r.path().join("a.txt"), opts()).unwrap();
        let mut term = Terminal::new(TestBackend::new(110, 12)).unwrap();
        render(&mut term, &mut app);
        key(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        for c in "foo".chars() {
            key(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert!(render(&mut term, &mut app).contains("Find: foo"));
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let sel = |app: &App| app.current_view().unwrap().bufs[1].selected_text();
        assert_eq!(sel(&app).as_deref(), Some("FOO"));
        key(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert_eq!(app.current_view().unwrap().bufs[1].cursor.line, 2);
        key(&mut app, KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert_eq!(app.current_view().unwrap().bufs[1].cursor.line, 1);
    }

    #[test]
    fn prev_change_skips_deletion_above_cursor() {
        let l = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        write(&l.path().join("a.txt"), "a\nX\nb\nc\nY\nd\n");
        write(&r.path().join("a.txt"), "a\nb\nc\nd\n");
        let mut app = App::new(l.path().join("a.txt"), r.path().join("a.txt"), opts()).unwrap();
        let mut term = Terminal::new(TestBackend::new(110, 12)).unwrap();
        render(&mut term, &mut app);
        app.focus = crate::app::Focus::Pane(1);
        let line = |app: &App| app.current_view().unwrap().bufs[1].cursor.line;
        key(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        key(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(line(&app), 3);
        // The deletion right above the cursor lands on the same line, so go past it.
        key(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(line(&app), 1);
        key(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(line(&app), 1);
        assert!(render(&mut term, &mut app).contains("No more changes"));
    }

    #[test]
    fn folder_diff_edit_and_save() {
        let l = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        write(
            &l.path().join("src/main.rs"),
            "fn main() {\n    let a = 1;\n    println!(\"{a}\");\n}\n",
        );
        write(
            &r.path().join("src/main.rs"),
            "fn main() {\n    let a = 2;\n    println!(\"{a}\");\n    done();\n}\n",
        );
        write(&l.path().join("same.txt"), "x\n");
        write(&r.path().join("same.txt"), "x\n");
        write(&r.path().join("docs/new.md"), "# hi\n");
        write(&l.path().join("old.txt"), "bye\n");
        let mut app = App::new(l.path().into(), r.path().into(), opts()).unwrap();
        let mut term = Terminal::new(TestBackend::new(110, 12)).unwrap();
        let s = render(&mut term, &mut app);
        println!("{s}");
        assert!(s.contains("Changes (3 files)"));
        assert!(s.contains("📂 docs") && s.contains("✖ old.txt") && !s.contains("same.txt"));
        assert!(s.contains("├─") && s.contains("└─"));

        // Go to src/main.rs, copy the first change right→left with Alt+←, then save the left file.
        key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        key(&mut app, KeyCode::Left, KeyModifiers::ALT);
        key(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        let s = render(&mut term, &mut app);
        println!("{s}");
        assert_eq!(
            fs::read_to_string(l.path().join("src/main.rs")).unwrap().lines().nth(1),
            Some("    let a = 2;")
        );

        // Type in the (focused) right pane and undo, then copy the remaining change by clicking the » arrow.
        key(&mut app, KeyCode::Char('Z'), KeyModifiers::SHIFT);
        assert!(app.current_view().unwrap().bufs[1].dirty);
        key(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert!(!app.current_view().unwrap().bufs[1].dirty);
        let fv = app.current_view().unwrap();
        let start = fv.diff.hunks[0].rows.start - fv.scroll;
        let c = app.areas.center;
        click(&mut app, c.x + 2, c.y + start as u16);
        let s = render(&mut term, &mut app);
        println!("{s}");
        assert!(app.current_view().unwrap().diff.hunks.is_empty());
        assert!(s.contains("✎"));
    }

    #[test]
    fn scrollbar_geometry() {
        assert_eq!(thumb(10, 5, 0, 10), (0, 10));
        assert_eq!(thumb(10, 100, 0, 10), (0, 1));
        assert_eq!(thumb(10, 100, 90, 10), (9, 1));
        assert_eq!(thumb(10, 20, 10, 10), (5, 5));
        assert_eq!(offset_at(10, 5, 10, 7, 0), 0);
        assert_eq!(offset_at(10, 100, 10, 9, 0), 90);
        assert_eq!(offset_at(10, 100, 10, 0, 3), 0);
        assert_eq!(offset_at(10, 20, 10, 9, 2), 10);
        for off in 0..=90 {
            let (start, _) = thumb(10, 100, off, 10);
            let back = offset_at(10, 100, 10, start, 0);
            assert_eq!(thumb(10, 100, back, 10).0, start);
        }
    }

    #[test]
    fn scrollbars_scroll_panes_and_tree() {
        let l = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        let long: String = (0..100).map(|i| format!("line {i}\n")).collect();
        write(&l.path().join("a/long.txt"), &long);
        write(&r.path().join("a/long.txt"), &long.replace("line 50\n", "changed\n"));
        for i in 0..30 {
            write(&r.path().join(format!("f{i:02}.txt")), "x\n");
        }
        let mut app = App::new(l.path().into(), r.path().into(), opts()).unwrap();
        let mut term = Terminal::new(TestBackend::new(100, 14)).unwrap();
        render(&mut term, &mut app);
        let sel = app.tree.selected;

        // Click the bottom of the tree's track: the tree scrolls to the end and stays there after a redraw.
        let tb = app.areas.vbars[0];
        assert!(tb.height > 0 && tb.x < app.areas.splitter.unwrap());
        click(&mut app, tb.x, tb.bottom() - 1);
        let max = app.tree.visible.len() - tb.height as usize;
        assert_eq!(app.tree.offset, max);
        render(&mut term, &mut app);
        assert_eq!(app.tree.offset, max);
        assert_eq!(app.tree.selected, sel);
        // The wheel over the tree's scrollbar scrolls the tree.
        mouse(&mut app, MouseEventKind::ScrollUp, tb.x, tb.y);
        render(&mut term, &mut app);
        assert_eq!(app.tree.offset, max - 3);

        // Open the long file; both panes get a scrollbar sharing the scroll.
        key(&mut app, KeyCode::Home, KeyModifiers::NONE);
        while app.current.as_deref() != Some(Path::new("a/long.txt")) {
            key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        }
        app.current_view_mut().unwrap().scroll = 0;
        render(&mut term, &mut app);
        let [_, lb, rb] = app.areas.vbars;
        assert_eq!(lb.right(), app.areas.center.x);
        assert_eq!(rb.right(), 100);
        assert!(!app.areas.panes[1].contains(Position::new(rb.x, rb.y)));
        let cell_bg = |term: &Terminal<TestBackend>, x, y| term.backend().buffer()[(x, y)].bg;
        assert_eq!(cell_bg(&term, rb.x, rb.y), super::DIM);
        assert_eq!(cell_bg(&term, rb.x, rb.bottom() - 1), super::BG_FILL);
        let cursor = app.current_view().unwrap().bufs[1].cursor;

        // Click the right track's bottom: scroll to the end.
        click(&mut app, rb.x, rb.bottom() - 1);
        let rows = app.current_view().unwrap().diff.rows.len();
        let end = rows - rb.height as usize;
        assert_eq!(app.current_view().unwrap().scroll, end);
        render(&mut term, &mut app);

        // Drag the left thumb from the bottom back to the top.
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), lb.x, lb.bottom() - 1);
        mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), lb.x, lb.y + 3);
        let mid = app.current_view().unwrap().scroll;
        assert!(0 < mid && mid < end);
        mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), lb.x, 0);
        mouse(&mut app, MouseEventKind::Up(MouseButton::Left), lb.x, 0);
        assert_eq!(app.current_view().unwrap().scroll, 0);
        assert_eq!(app.current_view().unwrap().bufs[1].cursor, cursor);
    }

    #[test]
    fn pane_divider_resizes_panes() {
        let d = tempfile::tempdir().unwrap();
        let (l, r) = (d.path().join("l.txt"), d.path().join("r.txt"));
        write(&l, "a\nb\n");
        write(&r, "a\nc\n");
        let mut app = App::new(l, r, opts()).unwrap();
        let mut term = Terminal::new(TestBackend::new(83, 10)).unwrap();
        render(&mut term, &mut app);
        // 80 columns for the panes (each including its scrollbar), split 50:50.
        let c = app.areas.center;
        assert_eq!(c.x, 40);
        let div = c.x + 1;

        // Drag the divider 15 columns left; the headers follow the panes.
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), div, c.y + 3);
        mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), div - 15, c.y + 3);
        mouse(&mut app, MouseEventKind::Up(MouseButton::Left), div - 15, c.y + 3);
        render(&mut term, &mut app);
        assert_eq!(app.areas.center.x, 25);
        assert_eq!(app.areas.panes[1].x, 28);
        assert_eq!(app.areas.vbars[2].right(), 83);
        assert_eq!(app.areas.panes[0].width + 1, 25);

        // Dragging past the edge keeps a minimum width.
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 26, c.y);
        mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 200, c.y);
        mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 200, c.y);
        render(&mut term, &mut app);
        assert_eq!(app.areas.center.x, 70);

        // Alt+Shift+arrows nudge it by 2 columns.
        key(&mut app, KeyCode::Left, KeyModifiers::ALT | KeyModifiers::SHIFT);
        render(&mut term, &mut app);
        assert_eq!(app.areas.center.x, 68);
        assert_eq!(app.current_view().unwrap().bufs[1].lines[1], "c");

        // Double-click resets to 50:50.
        click(&mut app, 69, c.y);
        click(&mut app, 69, c.y);
        render(&mut term, &mut app);
        assert_eq!(app.areas.center.x, 40);
    }
}
