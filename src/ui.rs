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
    if let Some(cursor) = draw_diff(f.buffer_mut(), app, diff_area) {
        f.set_cursor_position(cursor);
    }
    if let Some(cursor) = draw_status(f.buffer_mut(), app, status) {
        f.set_cursor_position(cursor);
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
    if area.width < 2 || area.height < 2 {
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

    let list = Rect::new(area.x, area.y + 1, area.width - 1, area.height - 1);
    app.areas.tree = list;
    app.tree.ensure_visible(list.height as usize);
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
    let cols = [Constraint::Fill(1), Constraint::Length(3), Constraint::Fill(1)];
    let [lp, center, rp] = Layout::horizontal(cols).areas(body);
    let [lh, ch, rh] = Layout::horizontal(cols).areas(header);
    fill(buf, ch, Style::new().bg(BG_HEADER));
    let labels = [header_label(app, 0, &key), header_label(app, 1, &key)];
    app.areas.center = center;
    app.areas.panes = [lp, rp];

    let focus = app.focus;
    let deadline = Instant::now() + HL_BUDGET;
    let fv = app.views.get_mut(&key)?;
    fv.refresh();
    let mut cursor = None;
    for (side, (pane, head)) in [(lp, lh), (rp, rh)].into_iter().enumerate() {
        let focused = focus == Focus::Pane(side);
        draw_header(buf, head, &labels[side], &fv.bufs[side], focused);
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
                Focus::Tree => "↑↓ select · Enter open · Space fold · Tab diff · Ctrl+N/P file · F5 rescan · q quit",
                Focus::Pane(_) => "Ctrl+F/G find · Ctrl+E/D change · Alt+←→ copy change · Ctrl+S save · F2 save all · Ctrl+Z/Y undo/redo · F6 focus · Esc tree · F12 mouse",
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

    fn opts() -> Options {
        Options {
            git: false,
            right_editable: false,
            readonly: false,
            theme: "base16-eighties.dark".into(),
        }
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
}
