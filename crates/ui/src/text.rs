//! Pure text-motion helpers shared by the vim keymap, the non-vim input
//! editor, and dialog input fields. All functions operate on `&str` buffers
//! and byte positions; they never mutate state.

/// Wrap a line to fit within `width` display columns, breaking at
/// word boundaries. Words longer than `width` are broken
/// character-by-character. Width is measured in terminal columns
/// (wide chars like CJK count as 2). Used by widgets (OptionList,
/// TabBar, custom dialog widgets) and by renderers that pre-wrap
/// content before emitting into `SpanCollector`.
pub fn wrap_line(line: &str, width: usize) -> Vec<String> {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if width == 0 {
        return vec![line.to_string()];
    }
    let mut chunks: Vec<String> = Vec::new();
    for logical_line in line.split('\n') {
        let mut current = String::new();
        let mut col = 0;
        for word in logical_line.split_inclusive(' ') {
            let wlen = UnicodeWidthStr::width(word);
            if col + wlen > width && col > 0 {
                chunks.push(current);
                current = String::new();
                col = 0;
            }
            if wlen > width {
                for ch in word.chars() {
                    let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                    if col + cw > width && col > 0 {
                        chunks.push(current);
                        current = String::new();
                        col = 0;
                    }
                    current.push(ch);
                    col += cw;
                }
            } else {
                current.push_str(word);
                col += wlen;
            }
        }
        chunks.push(current);
    }
    chunks
}

#[derive(Clone, Copy)]
pub enum CharClass {
    /// vim "word" boundaries: alphanumeric+underscore vs punctuation vs whitespace.
    Word,
    /// vim "WORD" boundaries: non-whitespace vs whitespace.
    #[allow(clippy::upper_case_acronyms)]
    WORD,
}

/// Clamp `pos` to `buf.len()` and snap backward to the nearest char
/// boundary. Prevents byte-slicing panics when callers hand us an
/// offset that was computed on a different snapshot of the string.
pub fn snap(buf: &str, pos: usize) -> usize {
    let mut p = pos.min(buf.len());
    while p > 0 && !buf.is_char_boundary(p) {
        p -= 1;
    }
    p
}

/// Convert a byte offset inside `line` to the terminal column the
/// character there would occupy (sum of `unicode-width` cells of every
/// preceding char). Handles offsets mid-multibyte-char by snapping
/// backward to the nearest char boundary first.
pub fn byte_to_cell(line: &str, byte: usize) -> usize {
    use unicode_width::UnicodeWidthStr;
    UnicodeWidthStr::width(&line[..snap(line, byte)])
}

/// Shared vertical-motion helper: move `cpos` up or down by one line in
/// `buf`, preserving the preferred display column (`curswant`). Returns
/// `(new_cpos, new_curswant)` where `new_curswant` is the column the
/// caller should remember for the next vertical motion — either the
/// supplied one (if we landed short of it on a shorter line) or the
/// current cell column (on the first vertical motion after a horizontal
/// one, when `curswant` is `None`).
///
/// This is the single code path for every vertical-motion source that
/// wants vim's "stay on the column you wanted, even if the intermediate
/// line was too short" behaviour: shift+arrow, vim j/k in Normal,
/// vim j/k in Visual, mouse-wheel scroll, Ctrl+U/D half-page, etc.
///
/// Columns are in terminal display cells, so wide glyphs (`⏺`, `─`,
/// CJK) behave correctly — you end up under the glyph, not mid-bytes.
pub fn vertical_move(
    buf: &str,
    cpos: usize,
    delta_lines: isize,
    curswant: Option<usize>,
) -> (usize, usize) {
    let cpos = snap(buf, cpos);
    // Build (line_start, line_end) pairs by walking `\n`.
    let mut lines: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    for (i, &b) in buf.as_bytes().iter().enumerate() {
        if b == b'\n' {
            lines.push((start, i));
            start = i + 1;
        }
    }
    lines.push((start, buf.len()));
    let cur_line = lines
        .iter()
        .position(|&(s, e)| cpos >= s && cpos <= e)
        .unwrap_or(lines.len() - 1);
    let (cur_sol, _) = lines[cur_line];
    let cur_col = byte_to_cell(&buf[cur_sol..], cpos - cur_sol);
    let want = curswant.unwrap_or(cur_col);
    if delta_lines == 0 {
        return (cpos, want);
    }
    let target_line = if delta_lines > 0 {
        (cur_line + delta_lines as usize).min(lines.len() - 1)
    } else {
        cur_line.saturating_sub((-delta_lines) as usize)
    };
    let (sol, eol) = lines[target_line];
    let byte_in_line = cell_to_byte(&buf[sol..eol], want);
    (sol + byte_in_line, want)
}

/// Byte offset of the char boundary before `pos`. Returns 0 at start.
pub fn prev_char_boundary(s: &str, pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    let mut p = pos - 1;
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

/// Byte offset of the char boundary after `pos`. Returns `s.len()` at end.
pub fn next_char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    let mut p = pos + 1;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}

/// Inverse of [`byte_to_cell`]: find the byte offset whose preceding
/// text occupies `cell` terminal columns. Wide glyphs that cross the
/// target land on their starting byte (never mid-glyph).
pub fn cell_to_byte(line: &str, cell: usize) -> usize {
    use unicode_width::UnicodeWidthChar;
    let mut acc = 0usize;
    for (b, ch) in line.char_indices() {
        if acc >= cell {
            return b;
        }
        acc += UnicodeWidthChar::width(ch).unwrap_or(0);
    }
    line.len()
}

pub fn char_class(c: char, mode: CharClass) -> u8 {
    match mode {
        CharClass::Word => {
            if c.is_alphanumeric() || c == '_' {
                1
            } else if c.is_whitespace() {
                0
            } else {
                2
            }
        }
        CharClass::WORD => {
            if c.is_whitespace() {
                0
            } else {
                1
            }
        }
    }
}

pub fn word_forward_pos(buf: &str, cpos: usize, mode: CharClass) -> usize {
    let cpos = snap(buf, cpos);
    let chars: Vec<(usize, char)> = buf[cpos..].char_indices().collect();
    if chars.is_empty() {
        return cpos;
    }
    let mut i = 0;
    let start_class = char_class(chars[0].1, mode);
    // Skip same class.
    while i < chars.len() && char_class(chars[i].1, mode) == start_class {
        i += 1;
    }
    // Skip whitespace.
    while i < chars.len() && char_class(chars[i].1, mode) == 0 {
        i += 1;
    }
    if i < chars.len() {
        cpos + chars[i].0
    } else {
        buf.len()
    }
}

pub fn word_backward_pos(buf: &str, cpos: usize, mode: CharClass) -> usize {
    let cpos = snap(buf, cpos);
    if cpos == 0 {
        return 0;
    }
    let chars: Vec<(usize, char)> = buf[..cpos].char_indices().collect();
    if chars.is_empty() {
        return 0;
    }
    let mut i = chars.len() - 1;
    // Skip whitespace backward.
    while i > 0 && char_class(chars[i].1, mode) == 0 {
        i -= 1;
    }
    let target_class = char_class(chars[i].1, mode);
    // Skip same class backward.
    while i > 0 && char_class(chars[i - 1].1, mode) == target_class {
        i -= 1;
    }
    chars[i].0
}

pub fn line_start(buf: &str, cpos: usize) -> usize {
    let cpos = snap(buf, cpos);
    buf[..cpos].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

pub fn line_end(buf: &str, cpos: usize) -> usize {
    let cpos = snap(buf, cpos);
    cpos + buf[cpos..].find('\n').unwrap_or(buf.len() - cpos)
}
