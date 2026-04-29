//! Free-standing motion helpers used by `Vim`.
//!
//! These are pure functions over `&str` buffers and byte positions; they hold
//! no editor state. The `Vim` state machine calls into them to compute cursor
//! targets for h/j/k/l, w/b/e, f/F/t/T, %/]/[/{/}, G/gg, etc.

use super::FindKind;
use crate::text::{char_class, line_end, line_start, CharClass};

pub(super) fn move_left(buf: &str, cpos: usize) -> usize {
    if cpos == 0 {
        return 0;
    }
    let sol = line_start(buf, cpos);
    if cpos <= sol {
        return cpos; // Don't cross line boundary.
    }
    prev_char_boundary(buf, cpos)
}

/// Move right, staying within the current line and not landing on '\n'.
pub(super) fn move_right_normal(buf: &str, cpos: usize) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let eol = line_end(buf, cpos);
    let last_on_line = if eol > line_start(buf, cpos) {
        prev_char_boundary(buf, eol)
    } else {
        eol // Empty line — stay put.
    };
    if cpos >= last_on_line {
        return cpos;
    }
    next_char_boundary(buf, cpos)
}

/// Move right inclusive (for operator motions on `l`).
pub(super) fn move_right_inclusive(buf: &str, cpos: usize) -> usize {
    next_char_boundary(buf, cpos).min(buf.len())
}

pub(super) fn word_end_pos(buf: &str, cpos: usize, mode: CharClass) -> usize {
    let next = next_char_boundary(buf, cpos);
    if next >= buf.len() {
        return cpos;
    }
    let chars: Vec<(usize, char)> = buf[next..].char_indices().collect();
    if chars.is_empty() {
        return cpos;
    }
    let mut i = 0;
    // Skip whitespace.
    while i < chars.len() && char_class(chars[i].1, mode) == 0 {
        i += 1;
    }
    if i >= chars.len() {
        return buf.len().saturating_sub(1);
    }
    let target_class = char_class(chars[i].1, mode);
    // Skip same class.
    while i + 1 < chars.len() && char_class(chars[i + 1].1, mode) == target_class {
        i += 1;
    }
    next + chars[i].0
}

/// End of line for normal mode (on last char, not past it).
pub(super) fn line_end_normal(buf: &str, cpos: usize) -> usize {
    let end = line_end(buf, cpos);
    if end > line_start(buf, cpos) {
        prev_char_boundary(buf, end)
    } else {
        end
    }
}

pub(super) fn first_non_blank(buf: &str, cpos: usize) -> usize {
    first_non_blank_at(buf, line_start(buf, cpos))
}

pub(super) fn first_non_blank_at(buf: &str, from: usize) -> usize {
    let eol = line_end(buf, from);
    let mut pos = from;
    while pos < eol {
        let c = buf[pos..].chars().next().unwrap();
        if c != ' ' && c != '\t' {
            break;
        }
        pos += c.len_utf8();
    }
    pos
}

/// Range of the full current line including trailing newline (for dd).
pub(super) fn current_line_range(buf: &str, cpos: usize) -> (usize, usize) {
    let start = line_start(buf, cpos);
    let end = line_end(buf, cpos);
    (start, end)
}

/// Range of just the content of the current line (no newline) — for S/cc.
pub(super) fn current_line_content_range(buf: &str, cpos: usize) -> (usize, usize) {
    let start = line_start(buf, cpos);
    let end = line_end(buf, cpos);
    (start, end)
}

pub(super) fn goto_line(buf: &str, line_idx: usize) -> usize {
    let mut pos = 0;
    for _ in 0..line_idx {
        match buf[pos..].find('\n') {
            Some(i) => pos += i + 1,
            None => return pos,
        }
    }
    pos
}

/// Move down one line. If `want_col` is Some, use that column instead of
/// the current one (for curswant support). Returns (new_cpos, actual_col).
pub(crate) fn move_down_col(buf: &str, cpos: usize, want_col: Option<usize>) -> (usize, usize) {
    let sol = line_start(buf, cpos);
    let col = want_col.unwrap_or(cpos - sol);
    let eol = line_end(buf, cpos);
    if eol >= buf.len() {
        return (cpos, col);
    }
    let next_sol = eol + 1;
    let next_eol = line_end(buf, next_sol);
    let next_len = next_eol - next_sol;
    (next_sol + col.min(next_len), col)
}

pub(crate) fn move_up_col(buf: &str, cpos: usize, want_col: Option<usize>) -> (usize, usize) {
    let sol = line_start(buf, cpos);
    if sol == 0 {
        let col = want_col.unwrap_or(cpos - sol);
        return (cpos, col);
    }
    let col = want_col.unwrap_or(cpos - sol);
    let prev_eol = sol - 1;
    let prev_sol = line_start(buf, prev_eol);
    let prev_len = prev_eol - prev_sol;
    (prev_sol + col.min(prev_len), col)
}

pub(crate) fn move_down(buf: &str, cpos: usize) -> usize {
    move_down_col(buf, cpos, None).0
}

pub(crate) fn move_up(buf: &str, cpos: usize) -> usize {
    move_up_col(buf, cpos, None).0
}

// ── Find char on line ───────────────────────────────────────────────────────

pub(super) fn find_char(buf: &str, cpos: usize, kind: FindKind, ch: char) -> Option<usize> {
    let sol = line_start(buf, cpos);
    let eol = line_end(buf, cpos);

    match kind {
        FindKind::Forward | FindKind::ForwardTill => {
            let start = next_char_boundary(buf, cpos);
            for (i, c) in buf[start..eol].char_indices() {
                if c == ch {
                    let pos = start + i;
                    return Some(match kind {
                        FindKind::ForwardTill => prev_char_boundary(buf, pos).max(cpos),
                        _ => pos,
                    });
                }
            }
            None
        }
        FindKind::Backward | FindKind::BackwardTill => {
            let search = &buf[sol..cpos];
            for (i, c) in search.char_indices().rev() {
                if c == ch {
                    let pos = sol + i;
                    return Some(match kind {
                        FindKind::BackwardTill => next_char_boundary(buf, pos).min(cpos),
                        _ => pos,
                    });
                }
            }
            None
        }
    }
}

/// Repeat a find-char motion `n` times, adjusting for till variants so
/// repeated `;`/`,` don't get stuck on the same character.
pub(super) fn repeat_find(buf: &str, mut pos: usize, kind: FindKind, ch: char, n: usize) -> usize {
    for _ in 0..n {
        let search_pos = match kind {
            FindKind::ForwardTill => next_char_boundary(buf, pos),
            FindKind::BackwardTill => prev_char_boundary(buf, pos),
            _ => pos,
        };
        if let Some(p) = find_char(buf, search_pos, kind, ch) {
            pos = p;
        }
    }
    pos
}

// ── Match bracket ───────────────────────────────────────────────────────────

pub(super) fn find_matching_bracket(buf: &str, cpos: usize) -> Option<usize> {
    let eol = line_end(buf, cpos);
    let mut start = cpos;
    while start < eol {
        let c = buf[start..].chars().next()?;
        if matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>') {
            break;
        }
        start += c.len_utf8();
    }
    if start >= eol && (start >= buf.len() || buf.as_bytes()[start] == b'\n') {
        return None;
    }
    let bracket = buf[start..].chars().next()?;
    let (open, close, forward) = match bracket {
        '(' => ('(', ')', true),
        ')' => ('(', ')', false),
        '[' => ('[', ']', true),
        ']' => ('[', ']', false),
        '{' => ('{', '}', true),
        '}' => ('{', '}', false),
        '<' => ('<', '>', true),
        '>' => ('<', '>', false),
        _ => return None,
    };
    let mut depth = 0i32;
    if forward {
        for (i, c) in buf[start..].char_indices() {
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return Some(start + i);
                }
            }
        }
    } else {
        for (i, c) in buf[..=start].char_indices().rev() {
            if c == close {
                depth += 1;
            } else if c == open {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
    }
    None
}

// ── Byte boundary helpers ───────────────────────────────────────────────────

pub(super) fn prev_char_boundary(s: &str, pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    let mut p = pos - 1;
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

pub(super) fn next_char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    let mut p = pos + 1;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}

pub(super) fn advance_chars(buf: &str, pos: usize, n: usize) -> usize {
    let mut p = pos;
    for _ in 0..n {
        if p >= buf.len() {
            break;
        }
        p = next_char_boundary(buf, p);
    }
    p
}

pub(super) fn retreat_chars(buf: &str, pos: usize, n: usize) -> usize {
    let mut p = pos;
    for _ in 0..n {
        if p == 0 {
            break;
        }
        p = prev_char_boundary(buf, p);
    }
    p
}

/// Clamp cursor to valid normal-mode position (on a char, not past end).
/// Exception: if the buffer ends with '\n', `buf.len()` is valid — it
/// represents the cursor on the empty trailing line.
pub(super) fn clamp_normal(buf: &str, cpos: &mut usize) {
    if buf.is_empty() {
        *cpos = 0;
        return;
    }
    if *cpos >= buf.len() {
        *cpos = if buf.ends_with('\n') {
            buf.len()
        } else {
            prev_char_boundary(buf, buf.len())
        };
        return;
    }
    // Don't let cursor sit on a '\n' in the middle of the buffer.
    if buf.as_bytes()[*cpos] == b'\n' && *cpos > 0 {
        let sol = line_start(buf, *cpos);
        if *cpos > sol {
            *cpos = prev_char_boundary(buf, *cpos);
        }
    }
}
