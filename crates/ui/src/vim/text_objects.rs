//! Vim text-object selection (iw/aw, i"/a", i(/a(, etc.).

use crate::text::{char_class, line_end, line_start, CharClass};

pub(super) fn text_object(
    buf: &str,
    cpos: usize,
    inner: bool,
    kind: char,
) -> Option<(usize, usize)> {
    match kind {
        'w' => text_object_word(buf, cpos, inner, CharClass::Word),
        'W' => text_object_word(buf, cpos, inner, CharClass::WORD),
        '"' | '\'' | '`' => text_object_quote(buf, cpos, inner, kind),
        '(' | ')' | 'b' => text_object_pair(buf, cpos, inner, '(', ')'),
        '[' | ']' => text_object_pair(buf, cpos, inner, '[', ']'),
        '{' | '}' | 'B' => text_object_pair(buf, cpos, inner, '{', '}'),
        '<' | '>' => text_object_pair(buf, cpos, inner, '<', '>'),
        _ => None,
    }
}

fn text_object_word(
    buf: &str,
    cpos: usize,
    inner: bool,
    mode: CharClass,
) -> Option<(usize, usize)> {
    if buf.is_empty() || cpos >= buf.len() {
        return None;
    }
    let chars: Vec<(usize, char)> = buf.char_indices().collect();
    let ci = chars.iter().position(|(i, _)| *i >= cpos)?;
    let cur_char = chars[ci].1;
    let cur_class = char_class(cur_char, mode);

    // Newlines are their own unit — never expand across them.
    if cur_char == '\n' {
        let byte_pos = chars[ci].0;
        return Some((byte_pos, byte_pos + 1));
    }

    // Expand backward over same class, stopping at newlines.
    let mut start = ci;
    while start > 0 {
        let prev = chars[start - 1].1;
        if prev == '\n' || char_class(prev, mode) != cur_class {
            break;
        }
        start -= 1;
    }
    // Expand forward over same class, stopping at newlines.
    let mut end = ci;
    while end + 1 < chars.len() {
        let next = chars[end + 1].1;
        if next == '\n' || char_class(next, mode) != cur_class {
            break;
        }
        end += 1;
    }

    let byte_start = chars[start].0;
    let byte_end = if end + 1 < chars.len() {
        chars[end + 1].0
    } else {
        buf.len()
    };

    if inner {
        Some((byte_start, byte_end))
    } else {
        // "a word" includes trailing whitespace (spaces/tabs, not newlines),
        // or leading whitespace if no trailing.
        let mut a_end = byte_end;
        while a_end < buf.len() && matches!(buf.as_bytes()[a_end], b' ' | b'\t') {
            a_end += 1;
        }
        if a_end > byte_end {
            Some((byte_start, a_end))
        } else {
            let mut a_start = byte_start;
            while a_start > 0 && matches!(buf.as_bytes()[a_start - 1], b' ' | b'\t') {
                a_start -= 1;
            }
            Some((a_start, byte_end))
        }
    }
}

fn text_object_quote(buf: &str, cpos: usize, inner: bool, quote: char) -> Option<(usize, usize)> {
    let line_s = line_start(buf, cpos);
    let line_e = line_end(buf, cpos);
    let line = &buf[line_s..line_e];
    let rel = cpos - line_s;

    let positions: Vec<usize> = line
        .char_indices()
        .filter(|(_, c)| *c == quote)
        .map(|(i, _)| i)
        .collect();

    for pair in positions.chunks(2) {
        if pair.len() == 2 && pair[0] <= rel && rel <= pair[1] {
            let abs_open = line_s + pair[0];
            let abs_close = line_s + pair[1];
            return if inner {
                Some((abs_open + quote.len_utf8(), abs_close))
            } else {
                Some((abs_open, abs_close + quote.len_utf8()))
            };
        }
    }
    None
}

fn text_object_pair(
    buf: &str,
    cpos: usize,
    inner: bool,
    open: char,
    close: char,
) -> Option<(usize, usize)> {
    let mut depth = 0i32;
    let mut open_pos = None;
    for (i, c) in buf[..=cpos.min(buf.len().saturating_sub(1))]
        .char_indices()
        .rev()
    {
        if c == close && i != cpos {
            depth += 1;
        } else if c == open {
            if depth == 0 {
                open_pos = Some(i);
                break;
            }
            depth -= 1;
        }
    }
    let open_pos = open_pos?;

    depth = 0;
    let search_start = open_pos + open.len_utf8();
    for (i, c) in buf[search_start..].char_indices() {
        if c == open {
            depth += 1;
        } else if c == close {
            if depth == 0 {
                let close_pos = search_start + i;
                return if inner {
                    Some((open_pos + open.len_utf8(), close_pos))
                } else {
                    Some((open_pos, close_pos + close.len_utf8()))
                };
            }
            depth -= 1;
        }
    }
    None
}
