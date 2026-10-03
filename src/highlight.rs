//! Incremental syntax highlighting with syntect.

use std::ops::Range;
use std::path::Path;
use std::time::Instant;

use ratatui::style::Color;
use syntect::highlighting::{HighlightState, Highlighter as SynHighlighter, RangedHighlightIterator, Theme, ThemeSet};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

/// Lines longer than this are not highlighted (keeps minified files responsive).
const MAX_LINE: usize = 4000;

pub struct Highlighter {
    pub ss: SyntaxSet,
    pub theme: Theme,
}

impl Highlighter {
    pub fn new(theme_name: &str) -> Self {
        let ss = SyntaxSet::load_defaults_newlines();
        let mut ts = ThemeSet::load_defaults();
        let theme = ts
            .themes
            .remove(theme_name)
            .or_else(|| ts.themes.remove("base16-eighties.dark"))
            .expect("bundled theme");
        Self { ss, theme }
    }

    pub fn theme_names() -> Vec<String> {
        ThemeSet::load_defaults().themes.into_keys().collect()
    }

    /// Picks a syntax from the candidate paths (first match wins), then the first line.
    pub fn detect(&self, paths: &[&Path], first_line: &str) -> String {
        for p in paths {
            if let Ok(Some(s)) = self.ss.find_syntax_for_file(p) {
                return s.name.clone();
            }
        }
        self.ss
            .find_syntax_by_first_line(first_line)
            .unwrap_or_else(|| self.ss.find_syntax_plain_text())
            .name
            .clone()
    }

    fn syntax(&self, name: &str) -> &SyntaxReference {
        self.ss
            .find_syntax_by_name(name)
            .unwrap_or_else(|| self.ss.find_syntax_plain_text())
    }
}

/// Per-buffer cache: `states[i]` is the parser state before line `i`,
/// `lines[i]` the colored byte ranges of line `i`.
pub struct HlCache {
    syntax: String,
    states: Vec<(ParseState, HighlightState)>,
    pub lines: Vec<Vec<(Range<usize>, Color)>>,
}

impl HlCache {
    pub fn new(syntax: String) -> Self {
        Self {
            syntax,
            states: Vec::new(),
            lines: Vec::new(),
        }
    }

    pub fn invalidate(&mut self, from: usize) {
        self.lines.truncate(from);
        self.states.truncate(from + 1);
    }

    /// Highlights lines up to (excluding) `upto`, stopping at `deadline`.
    /// Returns false if work remains (lines past the frontier render plain).
    pub fn ensure(&mut self, hl: &Highlighter, text: &[String], upto: usize, deadline: Instant) -> bool {
        let upto = upto.min(text.len());
        if self.lines.len() >= upto {
            return true;
        }
        let highlighter = SynHighlighter::new(&hl.theme);
        if self.states.is_empty() {
            let syntax = hl.syntax(&self.syntax);
            self.states.push((
                ParseState::new(syntax),
                HighlightState::new(&highlighter, ScopeStack::new()),
            ));
        }
        let (mut ps, mut hs) = self.states[self.lines.len()].clone();
        let mut buf = String::new();
        for (i, line) in text[self.lines.len()..upto].iter().enumerate() {
            if i % 64 == 63 && Instant::now() >= deadline {
                return false;
            }
            let mut spans = Vec::new();
            if line.len() <= MAX_LINE {
                buf.clear();
                buf.push_str(line);
                buf.push('\n');
                if let Ok(ops) = ps.parse_line(&buf, &hl.ss) {
                    for (style, _, range) in RangedHighlightIterator::new(&mut hs, &ops, &buf, &highlighter) {
                        let end = range.end.min(line.len());
                        if range.start < end {
                            let c = style.foreground;
                            spans.push((range.start..end, Color::Rgb(c.r, c.g, c.b)));
                        }
                    }
                }
            }
            self.lines.push(spans);
            self.states.push((ps.clone(), hs.clone()));
        }
        true
    }
}
