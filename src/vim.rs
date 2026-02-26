use crate::input::{byte_of_char, char_pos};

#[derive(Clone, Copy, PartialEq)]
pub enum ViMode {
    Insert,
    Normal,
}

impl ViMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ViMode::Insert => "insert",
            ViMode::Normal => "normal",
        }
    }
}

/// Move cursor one char left. Returns new byte position.
pub fn move_left(buf: &str, cpos: usize) -> usize {
    if cpos > 0 {
        let cp = char_pos(buf, cpos);
        byte_of_char(buf, cp - 1)
    } else {
        cpos
    }
}

/// Move cursor one char right. Returns new byte position.
pub fn move_right(buf: &str, cpos: usize) -> usize {
    if cpos < buf.len() {
        let cp = char_pos(buf, cpos);
        byte_of_char(buf, cp + 1)
    } else {
        cpos
    }
}

/// Move to start of current line.
pub fn line_start(buf: &str, cpos: usize) -> usize {
    buf[..cpos].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

/// Move to end of current line.
pub fn line_end(buf: &str, cpos: usize) -> usize {
    let after = &buf[cpos..];
    cpos + after.find('\n').unwrap_or(after.len())
}

/// Move forward to start of next word (whitespace-delimited).
pub fn word_forward(buf: &str, cpos: usize) -> usize {
    let bytes = buf.as_bytes();
    let len = bytes.len();
    let mut i = cpos;
    // Skip current non-whitespace
    while i < len && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    // Skip whitespace
    while i < len && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Move backward to start of current/previous word.
pub fn word_backward(buf: &str, cpos: usize) -> usize {
    let bytes = buf.as_bytes();
    let mut i = cpos;
    if i == 0 {
        return 0;
    }
    i -= 1;
    // Skip whitespace backward
    while i > 0 && bytes[i].is_ascii_whitespace() {
        i -= 1;
    }
    // Skip non-whitespace backward
    while i > 0 && !bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    i
}

/// Move to end of current word.
pub fn word_end(buf: &str, cpos: usize) -> usize {
    let bytes = buf.as_bytes();
    let len = bytes.len();
    let mut i = cpos;
    if i < len {
        i += 1;
    }
    // Skip whitespace
    while i < len && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    // Skip non-whitespace
    while i < len && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i > cpos + 1 {
        i - 1
    } else {
        i.min(len)
    }
}

/// Delete char at cursor position. Returns (new_buf, new_cpos).
pub fn delete_char(buf: &mut String, cpos: usize) -> usize {
    if cpos < buf.len() {
        let cp = char_pos(buf, cpos);
        let end = byte_of_char(buf, cp + 1);
        buf.drain(cpos..end);
    }
    cpos.min(buf.len())
}
