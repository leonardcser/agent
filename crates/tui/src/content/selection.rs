//! Prompt text spans, wrapping, and styled-char rendering.
//!
//! Shared between prompt rendering and queued/btw overlays. Handles the
//! raw-buffer → display-buffer expansion (attachments, `@path` refs),
//! wrapping text into visual lines while tracking the cursor column,
//! and painting those lines with selection + cursor highlighting.

use crate::attachment::{AttachmentId, AttachmentStore};
use crate::input::ATTACHMENT_MARKER;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) fn truncate_str(s: &str, max: usize) -> String {
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    let target = max.saturating_sub(1);
    let mut truncated = String::new();
    let mut col = 0;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if col + w > target {
            break;
        }
        truncated.push(ch);
        col += w;
    }
    truncated.push('…');
    truncated
}

pub(crate) use ui::text::wrap_line;

pub(crate) fn wrap_and_locate_cursor(
    buf: &str,
    char_kinds: &[SpanKind],
    cursor_char: usize,
    usable: usize,
) -> (Vec<(String, Vec<SpanKind>)>, usize, usize, usize) {
    let _perf = crate::perf::begin("render:wrap_cursor");
    let mut visual_lines: Vec<(String, Vec<SpanKind>)> = Vec::new();
    let mut cursor_line = 0;
    let mut cursor_col = 0;
    let mut cursor_char_in_line = 0usize;
    let mut chars_seen = 0usize;
    let mut cursor_set = false;
    let max_col = usable.max(1);
    let prompt_col = 1usize;

    for text_line in buf.split('\n') {
        let chars: Vec<char> = text_line.chars().collect();
        if chars.is_empty() {
            push_visual_line(
                &mut visual_lines,
                &mut cursor_line,
                &mut cursor_col,
                &mut cursor_char_in_line,
                &mut cursor_set,
                chars_seen,
                &[],
                &[],
                cursor_char,
                true,
                prompt_col,
            );
            chars_seen += 1;
            continue;
        }

        let mut line_chars: Vec<char> = Vec::new();
        let mut line_kinds: Vec<SpanKind> = Vec::new();
        let mut line_width = 0usize;
        let mut line_start = chars_seen;
        let mut last_break: Option<usize> = None;
        let mut i = 0usize;

        while i < chars.len() {
            let ch = chars[i];
            let kind = char_kinds
                .get(chars_seen + i)
                .copied()
                .unwrap_or(SpanKind::Plain);
            let ch_width = display_char_width(ch, prompt_col + line_width);

            if !line_chars.is_empty() && line_width + ch_width > max_col {
                if let Some(break_idx) = last_break {
                    let carry_chars = line_chars.split_off(break_idx);
                    let carry_kinds = line_kinds.split_off(break_idx);
                    push_visual_line(
                        &mut visual_lines,
                        &mut cursor_line,
                        &mut cursor_col,
                        &mut cursor_char_in_line,
                        &mut cursor_set,
                        line_start,
                        &line_chars,
                        &line_kinds,
                        cursor_char,
                        false,
                        prompt_col,
                    );
                    line_start += break_idx;
                    line_chars = carry_chars;
                    line_kinds = carry_kinds;
                    line_width = display_width(&line_chars, prompt_col);
                    last_break = line_chars
                        .iter()
                        .rposition(|&c| c == ' ')
                        .map(|idx| idx + 1);
                } else {
                    push_visual_line(
                        &mut visual_lines,
                        &mut cursor_line,
                        &mut cursor_col,
                        &mut cursor_char_in_line,
                        &mut cursor_set,
                        line_start,
                        &line_chars,
                        &line_kinds,
                        cursor_char,
                        false,
                        prompt_col,
                    );
                    line_start += line_chars.len();
                    line_chars.clear();
                    line_kinds.clear();
                    line_width = 0;
                    last_break = None;
                }
                continue;
            }

            line_chars.push(ch);
            line_kinds.push(kind);
            line_width += ch_width;
            if ch == ' ' {
                last_break = Some(line_chars.len());
            }
            i += 1;
        }

        push_visual_line(
            &mut visual_lines,
            &mut cursor_line,
            &mut cursor_col,
            &mut cursor_char_in_line,
            &mut cursor_set,
            line_start,
            &line_chars,
            &line_kinds,
            cursor_char,
            true,
            prompt_col,
        );
        chars_seen += chars.len() + 1;
    }
    if visual_lines.is_empty() {
        visual_lines.push((String::new(), Vec::new()));
    }
    (visual_lines, cursor_line, cursor_col, cursor_char_in_line)
}

#[allow(clippy::too_many_arguments)]
fn push_visual_line(
    visual_lines: &mut Vec<(String, Vec<SpanKind>)>,
    cursor_line: &mut usize,
    cursor_col: &mut usize,
    cursor_char_in_line: &mut usize,
    cursor_set: &mut bool,
    start_char: usize,
    line_chars: &[char],
    line_kinds: &[SpanKind],
    cursor_char: usize,
    is_last_chunk: bool,
    prompt_col: usize,
) {
    let end_char = start_char + line_chars.len();
    if !*cursor_set
        && cursor_char >= start_char
        && (cursor_char < end_char || (is_last_chunk && cursor_char == end_char))
    {
        *cursor_line = visual_lines.len();
        *cursor_char_in_line = cursor_char - start_char;
        *cursor_col = display_width(&line_chars[..cursor_char - start_char], prompt_col);
        *cursor_set = true;
    }
    visual_lines.push((line_chars.iter().collect(), line_kinds.to_vec()));
}

fn display_width(chars: &[char], start_col: usize) -> usize {
    let mut col = start_col;
    for &ch in chars {
        col += display_char_width(ch, col);
    }
    col.saturating_sub(start_col)
}

fn display_char_width(ch: char, col: usize) -> usize {
    if ch == '\t' {
        let tab_stop = 8usize;
        tab_stop - (col % tab_stop)
    } else {
        UnicodeWidthChar::width(ch).unwrap_or(0)
    }
}

/// Compute the display-char offset of each visual line.
///
/// The display buffer is the concatenation of spans (with attachments
/// expanded to their labels).  `wrap_and_locate_cursor` splits on `\n`
/// and then further wraps each logical line into visual lines.  The
/// char offsets it uses include +1 for every `\n` consumed by `split`.
/// We replicate that counting here by re-splitting the display buffer
/// and mapping each logical line's visual chunks to offsets.
pub(super) fn compute_visual_line_offsets(
    display_buf: &str,
    visual_lines: &[(String, Vec<SpanKind>)],
) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(visual_lines.len());
    let mut chars_seen: usize = 0;
    let mut vl_idx = 0;
    let newline_count = display_buf.chars().filter(|&c| c == '\n').count();

    for (li, text_line) in display_buf.split('\n').enumerate() {
        let line_chars = text_line.chars().count();
        if line_chars == 0 {
            if vl_idx < visual_lines.len() {
                offsets.push(chars_seen);
                vl_idx += 1;
            }
        } else {
            let mut consumed = 0;
            while vl_idx < visual_lines.len() && consumed < line_chars {
                offsets.push(chars_seen + consumed);
                consumed += visual_lines[vl_idx].0.chars().count();
                vl_idx += 1;
            }
        }
        chars_seen += line_chars;
        if li < newline_count {
            chars_seen += 1;
        }
    }
    while offsets.len() < visual_lines.len() {
        offsets.push(chars_seen);
    }
    offsets
}

pub(crate) enum Span {
    Plain(String),
    Attachment(String),
    AtRef(String),
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum SpanKind {
    Plain,
    Attachment,
    AtRef,
}

pub(super) fn build_char_kinds(spans: &[Span]) -> Vec<SpanKind> {
    let mut kinds = Vec::new();
    for span in spans {
        let (text, kind) = match span {
            Span::Plain(t) => (t.as_str(), SpanKind::Plain),
            Span::Attachment(t) => (t.as_str(), SpanKind::Attachment),
            Span::AtRef(t) => (t.as_str(), SpanKind::AtRef),
        };
        kinds.extend(std::iter::repeat_n(kind, text.chars().count()));
    }
    kinds
}

/// Scan an `@path` or `@"path with spaces"` token starting at position `i`.
/// Returns `(token_string, path_str, end_index)`.
pub(crate) fn scan_at_token(chars: &[char], i: usize) -> Option<(String, String, usize)> {
    if chars[i] != '@' {
        return None;
    }
    if i > 0 && !chars[i - 1].is_whitespace() && chars[i - 1] != '(' {
        return None;
    }

    let quoted = i + 1 < chars.len() && chars[i + 1] == '"';
    let end = if quoted {
        let mut e = i + 2;
        while e < chars.len() && chars[e] != '"' {
            e += 1;
        }
        if e >= chars.len() || e == i + 2 {
            return None;
        }
        e + 1
    } else {
        let mut e = i + 1;
        while e < chars.len() && !chars[e].is_whitespace() {
            e += 1;
        }
        if e <= i + 1 {
            return None;
        }
        e
    };

    let token: String = chars[i..end].iter().collect();
    let path = if quoted {
        token[2..token.len() - 1].to_string()
    } else {
        token[1..].to_string()
    };
    Some((token, path, end))
}

/// Check if position `i` in `chars` starts a valid `@path` reference.
/// Returns `Some((token, end_index))` if the path after `@` exists on disk.
pub(crate) fn try_at_ref(chars: &[char], i: usize) -> Option<(String, usize)> {
    let (token, path, end) = scan_at_token(chars, i)?;
    if std::path::Path::new(&path).exists() {
        return Some((token, end));
    }
    // Strip trailing punctuation and retry
    let trimmed = path.trim_end_matches([',', '.', ')', ';', ':', '!', '?']);
    if trimmed.len() < path.len() && !trimmed.is_empty() && std::path::Path::new(trimmed).exists() {
        let stripped = path.len() - trimmed.len();
        let short_token = token[..token.len() - stripped].to_string();
        Some((short_token, end - stripped))
    } else {
        None
    }
}

pub(crate) fn build_display_spans(
    buf: &str,
    att_ids: &[AttachmentId],
    store: &AttachmentStore,
) -> Vec<Span> {
    let _perf = crate::perf::begin("render:display_spans");
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut att_idx = 0;

    let chars: Vec<char> = buf.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ATTACHMENT_MARKER {
            if !plain.is_empty() {
                spans.push(Span::Plain(std::mem::take(&mut plain)));
            }
            let label = att_ids
                .get(att_idx)
                .map(|&id| store.display_label(id))
                .unwrap_or_else(|| "[?]".into());
            spans.push(Span::Attachment(label));
            att_idx += 1;
            i += 1;
        } else if let Some((token, end)) = try_at_ref(&chars, i) {
            if !plain.is_empty() {
                spans.push(Span::Plain(std::mem::take(&mut plain)));
            }
            spans.push(Span::AtRef(token));
            i = end;
        } else if let Some((token, _, end)) = scan_at_token(&chars, i) {
            if !plain.is_empty() {
                spans.push(Span::Plain(std::mem::take(&mut plain)));
            }
            spans.push(Span::Plain(token));
            i = end;
        } else {
            plain.push(chars[i]);
            i += 1;
        }
    }
    if !plain.is_empty() {
        spans.push(Span::Plain(plain));
    }
    spans
}

pub(crate) fn spans_to_string(spans: &[Span]) -> String {
    let mut s = String::new();
    for span in spans {
        match span {
            Span::Plain(t) | Span::Attachment(t) | Span::AtRef(t) => s.push_str(t),
        }
    }
    s
}

pub(super) fn map_cursor(raw_cursor: usize, raw_buf: &str, spans: &[Span]) -> usize {
    let mut raw_pos = 0;
    let mut display_pos = 0;
    for span in spans {
        match span {
            Span::Plain(t) => {
                let chars = t.chars().count();
                if raw_cursor >= raw_pos && raw_cursor < raw_pos + chars {
                    return display_pos + (raw_cursor - raw_pos);
                }
                raw_pos += chars;
                display_pos += chars;
            }
            Span::Attachment(label) => {
                if raw_cursor == raw_pos {
                    return display_pos;
                }
                raw_pos += 1;
                display_pos += label.chars().count();
            }
            Span::AtRef(token) => {
                let chars = token.chars().count();
                if raw_cursor >= raw_pos && raw_cursor < raw_pos + chars {
                    return display_pos + (raw_cursor - raw_pos);
                }
                raw_pos += chars;
                display_pos += chars;
            }
        }
    }
    let _ = raw_buf;
    display_pos
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vlines(strs: &[&str]) -> Vec<(String, Vec<SpanKind>)> {
        strs.iter()
            .map(|s| (s.to_string(), vec![SpanKind::Plain; s.chars().count()]))
            .collect()
    }

    #[test]
    fn offsets_single_line() {
        let offsets = compute_visual_line_offsets("hello", &vlines(&["hello"]));
        assert_eq!(offsets, vec![0]);
    }

    #[test]
    fn offsets_two_logical_lines() {
        let offsets = compute_visual_line_offsets("aaa\nbbb", &vlines(&["aaa", "bbb"]));
        assert_eq!(offsets, vec![0, 4]);
    }

    #[test]
    fn offsets_three_logical_lines() {
        let offsets = compute_visual_line_offsets("aaa\nbbb\nccc", &vlines(&["aaa", "bbb", "ccc"]));
        assert_eq!(offsets, vec![0, 4, 8]);
    }

    #[test]
    fn offsets_empty_line() {
        let offsets = compute_visual_line_offsets("aaa\n\nccc", &vlines(&["aaa", "", "ccc"]));
        assert_eq!(offsets, vec![0, 4, 5]);
    }

    #[test]
    fn offsets_wrapped_line() {
        let offsets = compute_visual_line_offsets("abcdef", &vlines(&["abc", "def"]));
        assert_eq!(offsets, vec![0, 3]);
    }

    #[test]
    fn offsets_wrapped_multiline() {
        let offsets = compute_visual_line_offsets("abcdef\nxy", &vlines(&["abc", "def", "xy"]));
        assert_eq!(offsets, vec![0, 3, 7]);
    }

    #[test]
    fn offsets_selection_across_wrapped() {
        let offsets = compute_visual_line_offsets("abcdef", &vlines(&["abc", "def"]));
        // Selection chars 1..5 should map to line0:(1,3), line1:(0,2).
        let sel = (1usize, 5usize);
        let l0_s = sel.0.saturating_sub(offsets[0]);
        let l0_e = sel.1.min(offsets[0] + 3) - offsets[0];
        assert_eq!((l0_s, l0_e), (1, 3));
        let l1_s = sel.0.saturating_sub(offsets[1]);
        let l1_e = sel.1.min(offsets[1] + 3) - offsets[1];
        assert_eq!((l1_s, l1_e), (0, 2));
    }

    #[test]
    fn prompt_cursor_uses_tab_display_width() {
        let kinds = vec![SpanKind::Plain; "a\tb".chars().count()];
        let (_, cursor_line, cursor_col, _) = wrap_and_locate_cursor("a\tb", &kinds, 3, 80);
        assert_eq!(cursor_line, 0);
        assert_eq!(cursor_col, 8);
    }

    #[test]
    fn prompt_cursor_tracks_multiple_tabs_from_prompt_column() {
        let kinds = vec![SpanKind::Plain; "\t\tb".chars().count()];
        let (_, cursor_line, cursor_col, _) = wrap_and_locate_cursor("\t\tb", &kinds, 3, 80);
        assert_eq!(cursor_line, 0);
        assert_eq!(cursor_col, 16);
    }

    #[test]
    fn prompt_cursor_uses_wide_char_display_width() {
        let kinds = vec![SpanKind::Plain; "a界b".chars().count()];
        let (_, cursor_line, cursor_col, _) = wrap_and_locate_cursor("a界b", &kinds, 3, 80);
        assert_eq!(cursor_line, 0);
        assert_eq!(cursor_col, 4);
    }

    #[test]
    fn prompt_tabs_respect_prompt_column_without_forced_wrap() {
        let kinds = vec![SpanKind::Plain; "a\tb".chars().count()];
        let (lines, cursor_line, cursor_col, _) = wrap_and_locate_cursor("a\tb", &kinds, 3, 8);
        assert_eq!(
            lines.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
            vec!["a\tb"]
        );
        assert_eq!(cursor_line, 0);
        assert_eq!(cursor_col, 8);
    }
}
