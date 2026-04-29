//! Helpers shared by the inline-emphasis renderer and the markdown table
//! renderer: visible-width measurement (`strip_markdown_markers`),
//! word-break candidate detection (`breakable_positions`), and the
//! recursive `strip_range` over inline syntax characters.

/// Strip inline markdown markers (`**`, `*`, `__`, `_`, `` ` ``, `~~`) and
/// return the visible text content. Used for measuring visual width.
/// Recurses into nested spans so nested emphasis/code is also stripped,
/// keeping this consistent with `print_inline_styled`'s actual output.
pub(crate) fn strip_markdown_markers(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    strip_range(&chars, 0, chars.len())
}

fn strip_range(chars: &[char], start: usize, end: usize) -> String {
    let mut out = String::new();
    let mut i = start;
    while i < end {
        if let Some((content_start, content_end, after)) = skip_inline_span_range(chars, i, end) {
            // Code spans are literal; emphasis/strike recurse for nesting.
            if chars[i] == '`' {
                out.extend(chars[content_start..content_end].iter());
            } else {
                out.push_str(&strip_range(chars, content_start, content_end));
            }
            i = after;
            continue;
        }
        // When an emphasis delimiter run didn't open a span, consume
        // the whole run at once. Otherwise a stray `*` inside e.g.
        // `**text*` could be re-interpreted by the next iteration as
        // an italic opener, producing a stripped string that doesn't
        // match what `print_inline_styled` actually emits.
        if chars[i] == '*' || chars[i] == '_' {
            let run = run_length(chars, i, end, chars[i]);
            for _ in 0..run {
                out.push(chars[i]);
            }
            i += run;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Identify character positions in `text` where line-breaking is allowed.
/// Returns a bool vec parallel to `text.chars()` — `true` at spaces outside
/// inline markdown spans (delimiters are not breakable).
pub(super) fn breakable_positions(text: &str) -> Vec<bool> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut breakable = vec![false; len];
    let mut i = 0;
    while i < len {
        if let Some((_, _, after)) = skip_inline_span_range(&chars, i, len) {
            // Jump past the entire span (delimiters + content) — no breaks inside.
            i = after;
            continue;
        }
        if chars[i] == ' ' {
            breakable[i] = true;
        }
        i += 1;
    }
    breakable
}

/// Try to match an inline markdown span at position `i` within the open
/// range `[0..end)`. Returns `Some((content_start, content_end, after))`
/// if a complete span is found. Uses strict delimiter-run matching so
/// that e.g. `**text*` does not collapse to `*` + italic("text").
pub(super) fn skip_inline_span_range(
    chars: &[char],
    i: usize,
    end: usize,
) -> Option<(usize, usize, usize)> {
    if i >= end {
        return None;
    }

    // `code`: highest precedence.
    if chars[i] == '`' {
        if let Some(close) = find_code_close(chars, i + 1, end) {
            return Some((i + 1, close, close + 1));
        }
    }

    // ~~strikethrough~~
    if i + 1 < end && chars[i] == '~' && chars[i + 1] == '~' {
        if let Some(close) = find_strike_close(chars, i + 2, end) {
            return Some((i + 2, close, close + 2));
        }
    }

    // Emphasis: *italic*, **bold**, ***both*** (and `_` variants).
    if chars[i] == '*' || chars[i] == '_' {
        let marker = chars[i];
        let run = run_length(chars, i, end, marker);
        if (1..=3).contains(&run) && can_open_emphasis(chars, i, run, end, marker) {
            if let Some(close) = find_closing_run(chars, i + run, end, marker, run) {
                return Some((i + run, close, close + run));
            }
        }
    }

    None
}
/// Length of the run of consecutive `marker` chars starting at `i`.
pub(super) fn run_length(chars: &[char], i: usize, end: usize, marker: char) -> usize {
    let mut j = i;
    while j < end && chars[j] == marker {
        j += 1;
    }
    j - i
}

/// Can a delimiter run of `count` `marker` chars at position `i` open
/// emphasis? Rules (simplified CommonMark left-flanking):
/// - The character after the run must exist and not be whitespace.
/// - For `_`: the character before the run must not be alphanumeric.
///   Prevents intraword emphasis like `snake_case` or URLs containing
///   underscores.
pub(super) fn can_open_emphasis(
    chars: &[char],
    i: usize,
    count: usize,
    end: usize,
    marker: char,
) -> bool {
    let after = i + count;
    if after >= end || chars[after].is_whitespace() {
        return false;
    }
    if marker == '_' && i > 0 && chars[i - 1].is_alphanumeric() {
        return false;
    }
    true
}

/// Find a closing delimiter run of **exactly** `count` consecutive
/// `marker` chars in `[start..end)`. Rules:
/// - The character before the run must not be whitespace
///   (right-flanking).
/// - For `_`: the character after the run must not be alphanumeric.
/// - Run length must equal `count` exactly — a run of 1 cannot close an
///   opener of 2, and vice versa.
pub(super) fn find_closing_run(
    chars: &[char],
    start: usize,
    end: usize,
    marker: char,
    count: usize,
) -> Option<usize> {
    let mut j = start;
    while j < end {
        if chars[j] == marker {
            let run = run_length(chars, j, end, marker);
            if run == count && j > 0 && !chars[j - 1].is_whitespace() {
                let after = j + run;
                if marker == '*' || after >= end || !chars[after].is_alphanumeric() {
                    return Some(j);
                }
            }
            j += run;
        } else {
            j += 1;
        }
    }
    None
}

/// Find the closing backtick of a code span starting at `start`.
pub(super) fn find_code_close(chars: &[char], start: usize, end: usize) -> Option<usize> {
    let mut j = start;
    while j < end {
        if chars[j] == '`' {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// Find the closing `~~` of a strikethrough span.
pub(super) fn find_strike_close(chars: &[char], start: usize, end: usize) -> Option<usize> {
    let mut j = start;
    while j + 1 < end {
        if chars[j] == '~' && chars[j + 1] == '~' {
            return Some(j);
        }
        j += 1;
    }
    None
}
