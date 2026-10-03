//! Display-width helpers shared by rendering and mouse hit-testing.

use unicode_width::UnicodeWidthChar;

pub const TAB_WIDTH: usize = 4;

/// Width of `c` when drawn at display column `dcol`.
pub fn char_width(c: char, dcol: usize) -> usize {
    if c == '\t' {
        TAB_WIDTH - dcol % TAB_WIDTH
    } else if c.is_control() {
        1
    } else {
        c.width().unwrap_or(1)
    }
}

/// Printable stand-in for control characters (they would corrupt the terminal).
pub fn visible_char(c: char) -> char {
    match c as u32 {
        0..=0x1f => char::from_u32(0x2400 + c as u32).unwrap_or('?'),
        0x7f => '␡',
        _ if c.is_control() => '�',
        _ => c,
    }
}

/// Display column of char index `col` in `s`.
pub fn display_col(s: &str, col: usize) -> usize {
    let mut d = 0;
    for c in s.chars().take(col) {
        d += char_width(c, d);
    }
    d
}

/// Char index at display column `target` (clamped to the line length).
pub fn col_at_display(s: &str, target: usize) -> usize {
    let mut d = 0;
    for (i, c) in s.chars().enumerate() {
        let w = char_width(c, d);
        if d + w > target {
            // Clicking the right half of a wide cell lands after it.
            return if w > 1 && target - d >= w.div_ceil(2) { i + 1 } else { i };
        }
        d += w;
    }
    s.chars().count()
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_with_tabs_and_wide_chars() {
        assert_eq!(display_col("\tab", 1), 4);
        assert_eq!(display_col("a\tb", 2), 4);
        assert_eq!(display_col("日本x", 2), 4);
        assert_eq!(col_at_display("\tab", 1), 0);
        assert_eq!(col_at_display("\tab", 3), 1);
        assert_eq!(col_at_display("\tab", 4), 1);
        assert_eq!(col_at_display("日本x", 3), 2);
        assert_eq!(col_at_display("ab", 99), 2);
    }

    #[test]
    fn base64_encodes() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
