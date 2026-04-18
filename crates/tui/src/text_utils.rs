//! Pure text-motion helpers shared by the vim keymap, the non-vim input
//! editor, and dialog input fields. All functions operate on `&str` buffers
//! and byte positions; they never mutate state.

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
