//! Line alignment for side-by-side display and inline (word) change detection.

use std::ops::Range;
use std::time::{Duration, Instant};

use similar::{Algorithm, DiffOp, capture_diff_slices_deadline};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Equal,
    Delete,
    Insert,
    Replace,
}

/// One display row: the line index shown on each side (None = filler).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub left: Option<usize>,
    pub right: Option<usize>,
    pub kind: Kind,
}

impl Row {
    pub fn side(&self, side: usize) -> Option<usize> {
        if side == 0 { self.left } else { self.right }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub rows: Range<usize>,
    pub left: Range<usize>,
    pub right: Range<usize>,
}

impl Hunk {
    pub fn side(&self, side: usize) -> Range<usize> {
        if side == 0 {
            self.left.clone()
        } else {
            self.right.clone()
        }
    }
}

#[derive(Default, Debug)]
pub struct DiffResult {
    pub rows: Vec<Row>,
    pub hunks: Vec<Hunk>,
    /// Display row for each line, per side.
    pub line_row: [Vec<usize>; 2],
}

impl DiffResult {
    /// Display row for `line` on `side` (row 0 for empty sides).
    pub fn row_of(&self, side: usize, line: usize) -> usize {
        let map = &self.line_row[side];
        match map.get(line) {
            Some(r) => *r,
            None => map.last().map_or(0, |r| r + 1).min(self.rows.len().saturating_sub(1)),
        }
    }

    pub fn hunk_at_row(&self, row: usize) -> Option<usize> {
        self.hunks
            .iter()
            .position(|h| h.rows.contains(&row) || (h.rows.is_empty() && h.rows.start == row))
    }

    /// Nearest real line on `side` for display row `row` (snapping filler rows).
    pub fn line_near_row(&self, side: usize, row: usize) -> usize {
        if self.rows.is_empty() {
            return 0;
        }
        let row = row.min(self.rows.len() - 1);
        if let Some(l) = self.rows[row].side(side) {
            return l;
        }
        // Filler: the next real line if any (insertion point), else the previous one.
        if let Some(l) = self.rows[row..].iter().find_map(|r| r.side(side)) {
            return l;
        }
        self.rows[..row]
            .iter()
            .rev()
            .find_map(|r| r.side(side))
            .map_or(0, |l| l + 1)
    }
}

pub fn compute(left: &[String], right: &[String]) -> DiffResult {
    let deadline = Instant::now() + Duration::from_secs(2);
    let ops = capture_diff_slices_deadline(Algorithm::Patience, left, right, Some(deadline));
    let mut res = DiffResult {
        line_row: [vec![0; left.len()], vec![0; right.len()]],
        ..Default::default()
    };
    let mut cur_hunk: Option<Hunk> = None;
    for op in ops {
        let (kind, l0, ll, r0, rl) = match op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => (Kind::Equal, old_index, len, new_index, len),
            DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => (Kind::Delete, old_index, old_len, new_index, 0),
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => (Kind::Insert, old_index, 0, new_index, new_len),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => (Kind::Replace, old_index, old_len, new_index, new_len),
        };
        let start_row = res.rows.len();
        for i in 0..ll.max(rl) {
            let left = (i < ll).then_some(l0 + i);
            let right = (i < rl).then_some(r0 + i);
            if let Some(l) = left {
                res.line_row[0][l] = res.rows.len();
            }
            if let Some(r) = right {
                res.line_row[1][r] = res.rows.len();
            }
            res.rows.push(Row { left, right, kind });
        }
        if kind == Kind::Equal {
            if let Some(h) = cur_hunk.take() {
                res.hunks.push(h);
            }
            continue;
        }
        let end_row = res.rows.len();
        match &mut cur_hunk {
            Some(h) => {
                h.rows.end = end_row;
                h.left.end = l0 + ll;
                h.right.end = r0 + rl;
            }
            None => {
                cur_hunk = Some(Hunk {
                    rows: start_row..end_row,
                    left: l0..l0 + ll,
                    right: r0..r0 + rl,
                });
            }
        }
    }
    if let Some(h) = cur_hunk {
        res.hunks.push(h);
    }
    res
}

fn tokenize(s: &str) -> Vec<(usize, &str)> {
    // (char offset, token): runs of word chars, runs of whitespace, single other chars.
    let mut out = Vec::new();
    let mut chars = s.char_indices().enumerate().peekable();
    while let Some((ci, (bi, c))) = chars.next() {
        let class = |c: char| {
            if c.is_alphanumeric() || c == '_' {
                0
            } else if c.is_whitespace() {
                1
            } else {
                2
            }
        };
        let k = class(c);
        let mut end = bi + c.len_utf8();
        if k != 2 {
            while let Some(&(_, (b2, c2))) = chars.peek() {
                if class(c2) != k {
                    break;
                }
                end = b2 + c2.len_utf8();
                chars.next();
            }
        }
        out.push((ci, &s[bi..end]));
    }
    out
}

/// Changed char ranges in `a` and `b` (word granularity). Empty when the lines
/// are too different for inline emphasis to be useful.
pub fn inline_changes(a: &str, b: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let ta = tokenize(a);
    let tb = tokenize(b);
    let wa: Vec<&str> = ta.iter().map(|t| t.1).collect();
    let wb: Vec<&str> = tb.iter().map(|t| t.1).collect();
    let deadline = Instant::now() + Duration::from_millis(50);
    let ops = capture_diff_slices_deadline(Algorithm::Myers, &wa, &wb, Some(deadline));
    let span = |toks: &[(usize, &str)], i: usize, len: usize| -> Range<usize> {
        if len == 0 {
            return 0..0;
        }
        let start = toks[i].0;
        let (lc, lt) = toks[i + len - 1];
        start..lc + lt.chars().count()
    };
    let (mut ra, mut rb) = (Vec::new(), Vec::new());
    let mut equal = 0usize;
    for op in ops {
        match op {
            DiffOp::Equal { old_index, len, .. } => {
                equal += span(&ta, old_index, len).len();
            }
            DiffOp::Delete { old_index, old_len, .. } => ra.push(span(&ta, old_index, old_len)),
            DiffOp::Insert { new_index, new_len, .. } => rb.push(span(&tb, new_index, new_len)),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                ra.push(span(&ta, old_index, old_len));
                rb.push(span(&tb, new_index, new_len));
            }
        }
    }
    let total = a.chars().count().max(b.chars().count());
    // Mostly-rewritten lines: whole-line tint is clearer than confetti.
    if total > 0 && equal * 10 < total * 3 {
        return (Vec::new(), Vec::new());
    }
    (ra, rb)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn aligns_insert_delete_replace() {
        let l = v(&["a", "b", "c", "d", "e"]);
        let r = v(&["a", "B", "c", "x", "y", "d"]);
        let d = compute(&l, &r);
        // a | B~b | c | +x +y | d | -e
        let rows: Vec<(Option<usize>, Option<usize>, Kind)> =
            d.rows.iter().map(|r| (r.left, r.right, r.kind)).collect();
        assert_eq!(
            rows,
            vec![
                (Some(0), Some(0), Kind::Equal),
                (Some(1), Some(1), Kind::Replace),
                (Some(2), Some(2), Kind::Equal),
                (None, Some(3), Kind::Insert),
                (None, Some(4), Kind::Insert),
                (Some(3), Some(5), Kind::Equal),
                (Some(4), None, Kind::Delete),
            ]
        );
        assert_eq!(d.hunks.len(), 3);
        assert_eq!(
            d.hunks[1],
            Hunk {
                rows: 3..5,
                left: 3..3,
                right: 3..5
            }
        );
        assert_eq!(
            d.hunks[2],
            Hunk {
                rows: 6..7,
                left: 4..5,
                right: 6..6
            }
        );
        assert_eq!(d.row_of(1, 5), 5);
        assert_eq!(d.line_near_row(0, 3), 3);
        assert_eq!(d.line_near_row(1, 6), 6);
    }

    #[test]
    fn replace_pads_shorter_side() {
        let d = compute(&v(&["x", "a1", "a2", "a3", "y"]), &v(&["x", "b1", "y"]));
        assert_eq!(d.rows.len(), 5);
        assert_eq!(d.hunks[0].rows, 1..4);
        assert_eq!(d.rows[3].right, None);
        assert_eq!(d.rows[3].kind, Kind::Replace);
    }

    #[test]
    fn empty_sides() {
        let d = compute(&[], &v(&["a"]));
        assert_eq!(d.rows.len(), 1);
        assert_eq!(d.hunks[0].left, 0..0);
        assert_eq!(d.row_of(0, 0), 0);
        assert!(compute(&[], &[]).rows.is_empty());
    }

    #[test]
    fn inline_word_changes() {
        let (a, b) = inline_changes("let foo = bar(1);", "let foo = baz(1);");
        assert_eq!(a, vec![10..13]);
        assert_eq!(b, vec![10..13]);
        let (a, _) = inline_changes("completely", "different words here");
        assert!(a.is_empty());
    }
}
