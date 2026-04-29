use super::selection::{
    build_char_kinds, build_display_spans, compute_visual_line_offsets, map_cursor,
    spans_to_string, wrap_and_locate_cursor, wrap_line, SpanKind,
};
use super::status::BarSpan;
use super::window_view::{StyledSegment, WindowRow};
use crate::input::PromptState;
use crate::theme;
use ui::buffer::{Buffer, SpanStyle};
use ui::grid::Style;

use crossterm::style::Color;

pub(crate) struct PromptInput<'a> {
    pub queued: &'a [String],
    pub stash: &'a Option<crate::input::InputSnapshot>,
    pub input: &'a PromptState,
    pub prediction: Option<&'a str>,
    pub width: u16,
    pub height: u16,
    pub has_prompt_cursor: bool,
    pub bar_info: BarInfo,
}

pub(crate) struct BarInfo {
    pub model_label: Option<String>,
    pub reasoning_effort: protocol::ReasoningEffort,
    pub show_tokens: bool,
    pub context_tokens: Option<u32>,
    pub context_window: Option<u32>,
    pub show_cost: bool,
    pub session_cost_usd: f64,
}

pub(crate) struct PromptOutput {
    pub chrome_rows: Vec<WindowRow>,
    pub cursor: Option<(u16, u16)>,
    pub cursor_style: Option<(Style, char)>,
    pub input_viewport: Option<InputViewport>,
}

pub(crate) struct InputViewport {
    pub top_row: u16,
    pub rows: u16,
    pub content_width: u16,
    pub total_rows: u16,
    pub scroll_top: u16,
}

fn cursor_style() -> (Color, Color) {
    if theme::is_light() {
        (Color::White, Color::Black)
    } else {
        (Color::Black, Color::White)
    }
}

pub(crate) fn compute_prompt(input: &mut PromptInput<'_>, input_buf: &mut Buffer) -> PromptOutput {
    let width = input.width as usize;
    let usable = width.saturating_sub(2);
    let mut chrome_rows: Vec<WindowRow> = Vec::new();
    let mut row_offset: u16 = 0;

    // ── Queued messages ──
    let queued_rows = queued_message_rows(input.queued, usable);
    row_offset += queued_rows.len() as u16;
    chrome_rows.extend(queued_rows);

    // ── Stash indicator ──
    if input.stash.is_some() {
        chrome_rows.push(stash_row(usable));
        row_offset += 1;
    }

    // ── Top bar ──
    let top_bar_right = build_top_bar_right(&input.bar_info);
    chrome_rows.push(bar_row(
        width,
        None,
        if top_bar_right.is_empty() {
            None
        } else {
            Some(&top_bar_right)
        },
    ));
    row_offset += 1;

    // ── Input area ──
    let input_area_start = row_offset;
    let input_area = compute_input_area(input, usable, row_offset, input_buf);
    let input_row_count = input_area.visible_rows;
    for _ in 0..input_row_count {
        chrome_rows.push(WindowRow::styled(Vec::new()));
    }
    row_offset += input_row_count;

    // ── Bottom bar ──
    chrome_rows.push(bar_row(width, None, None));
    row_offset += 1;

    // ── Status line ──
    // Status line is rendered as a separate component (StatusBar), not as a WindowRow.
    // The caller handles it separately.
    let _ = row_offset;

    PromptOutput {
        chrome_rows,
        cursor: input_area.cursor_info.cursor_pos,
        cursor_style: input_area.cursor_info.cursor_style,
        input_viewport: if input_row_count > 0 {
            Some(InputViewport {
                top_row: input_area_start,
                rows: input_row_count,
                content_width: usable as u16,
                total_rows: input_area.scroll_info.total_content_rows as u16,
                scroll_top: input_area.scroll_info.scroll_offset as u16,
            })
        } else {
            None
        },
    }
}

// ── Queued messages ──

fn queued_message_rows(queued: &[String], usable: usize) -> Vec<WindowRow> {
    let indent = 1usize;
    let text_w = usable.saturating_sub(indent + 1).max(1);
    let mut rows = Vec::new();
    let user_bg = theme::user_bg();

    for msg in queued {
        let is_command = crate::completer::Completer::is_command(msg.trim());
        let geom = crate::app::transcript_present::UserBlockGeometry::new(msg, text_w);
        for line in &geom.lines {
            if line.is_empty() {
                let fill_w = if geom.block_w > 0 {
                    geom.block_w + 1
                } else {
                    2
                };
                let mut segs = vec![StyledSegment {
                    text: " ".repeat(indent),
                    style: Style::default(),
                }];
                segs.push(StyledSegment {
                    text: " ".repeat(fill_w),
                    style: Style::bg(user_bg),
                });
                rows.push(WindowRow::styled(segs));
                continue;
            }
            let chunks = wrap_line(line, text_w);
            for chunk in &chunks {
                let chunk_w = super::layout_out::display_width(chunk);
                let trailing = if geom.block_w > 0 {
                    geom.block_w.saturating_sub(chunk_w)
                } else {
                    1
                };
                let bg_style = Style {
                    bg: Some(user_bg),
                    bold: true,
                    ..Style::default()
                };

                let mut segs = vec![StyledSegment {
                    text: " ".repeat(indent),
                    style: Style::default(),
                }];
                segs.push(StyledSegment {
                    text: " ".into(),
                    style: bg_style,
                });

                // Build styled segments for the chunk content
                let chunk_segs = user_highlight_segments(chunk, is_command, bg_style);
                segs.extend(chunk_segs);

                segs.push(StyledSegment {
                    text: " ".repeat(trailing),
                    style: bg_style,
                });
                rows.push(WindowRow::styled(segs));
            }
        }
    }
    rows
}

fn user_highlight_segments(text: &str, is_command: bool, base_style: Style) -> Vec<StyledSegment> {
    if is_command {
        return vec![StyledSegment {
            text: text.into(),
            style: Style {
                fg: Some(theme::accent()),
                ..base_style
            },
        }];
    }

    // Simple path: no @ref highlighting for now — just plain text with base style.
    // The full `print_user_highlights` does @path detection + image labels,
    // but for queued messages the simple path covers the common case.
    vec![StyledSegment {
        text: text.into(),
        style: base_style,
    }]
}

// ── Stash ──

fn stash_row(_usable: usize) -> WindowRow {
    let text = "› Stashed (ctrl+s to unstash)";
    let display: String = text.chars().take(_usable).collect();
    WindowRow::styled(vec![
        StyledSegment {
            text: "  ".into(),
            style: Style::default(),
        },
        StyledSegment {
            text: display,
            style: Style {
                fg: Some(theme::muted()),
                dim: true,
                ..Style::default()
            },
        },
    ])
}

// ── Bar (horizontal rule with optional spans) ──

fn bar_row(width: usize, left: Option<&[BarSpan]>, right: Option<&[BarSpan]>) -> WindowRow {
    let dash = "\u{2500}";
    let bar_color = theme::bar();
    let min_dashes = 4;

    let max_priority = left
        .into_iter()
        .chain(right)
        .flat_map(|spans| spans.iter().map(|s| s.priority))
        .max()
        .unwrap_or(0);

    let mut drop_above = max_priority + 1;
    loop {
        let left_chars: usize = left
            .map(|spans| {
                let inner: usize = spans
                    .iter()
                    .filter(|s| s.priority < drop_above)
                    .map(|s| super::layout_out::display_width(&s.text))
                    .sum();
                if inner > 0 {
                    inner + 1
                } else {
                    0
                }
            })
            .unwrap_or(0);
        let right_chars: usize = right
            .map(|spans| {
                let inner: usize = spans
                    .iter()
                    .filter(|s| s.priority < drop_above)
                    .map(|s| super::layout_out::display_width(&s.text))
                    .sum();
                if inner > 0 {
                    inner + 2
                } else {
                    0
                }
            })
            .unwrap_or(0);
        let total = left_chars + min_dashes + right_chars;
        if total <= width || drop_above == 1 {
            break;
        }
        drop_above -= 1;
    }

    let left_filtered: Vec<&BarSpan> = left
        .map(|spans| spans.iter().filter(|s| s.priority < drop_above).collect())
        .unwrap_or_default();
    let right_filtered: Vec<&BarSpan> = right
        .map(|spans| spans.iter().filter(|s| s.priority < drop_above).collect())
        .unwrap_or_default();

    let left_len: usize = if left_filtered.is_empty() {
        0
    } else {
        left_filtered
            .iter()
            .map(|s| super::layout_out::display_width(&s.text))
            .sum::<usize>()
            + 1
    };
    let right_len: usize = if right_filtered.is_empty() {
        0
    } else {
        right_filtered
            .iter()
            .map(|s| super::layout_out::display_width(&s.text))
            .sum::<usize>()
            + 2
    };
    let bar_len = width.saturating_sub(left_len + right_len);

    let mut segs: Vec<StyledSegment> = Vec::new();

    for span in &left_filtered {
        segs.push(StyledSegment {
            text: span.text.clone(),
            style: Style {
                fg: Some(span.color),
                bg: span.bg,
                bold: span.bold,
                dim: span.dim,
                ..Style::default()
            },
        });
    }
    if !left_filtered.is_empty() {
        segs.push(StyledSegment {
            text: " ".into(),
            style: Style::default(),
        });
    }

    segs.push(StyledSegment {
        text: dash.repeat(bar_len),
        style: Style::fg(bar_color),
    });

    if !right_filtered.is_empty() {
        for span in &right_filtered {
            segs.push(StyledSegment {
                text: span.text.clone(),
                style: Style {
                    fg: Some(span.color),
                    bg: span.bg,
                    bold: span.bold,
                    dim: span.dim,
                    ..Style::default()
                },
            });
        }
        segs.push(StyledSegment {
            text: " ".into(),
            style: Style::default(),
        });
        segs.push(StyledSegment {
            text: dash.into(),
            style: Style::fg(bar_color),
        });
    }

    WindowRow::styled(segs)
}

fn build_top_bar_right(info: &BarInfo) -> Vec<BarSpan> {
    let mut spans = Vec::new();
    if let Some(ref model) = info.model_label {
        spans.push(BarSpan {
            text: format!(" {}", model),
            color: theme::muted(),
            bg: None,
            bold: false,
            dim: false,
            priority: 2,
        });
        if info.reasoning_effort != protocol::ReasoningEffort::Off {
            let effort = info.reasoning_effort;
            spans.push(BarSpan {
                text: format!(" {}", effort.label()),
                color: super::reasoning_color(effort),
                bg: None,
                bold: false,
                dim: false,
                priority: 2,
            });
        }
    }
    if info.show_tokens {
        if let Some(tokens) = info.context_tokens {
            if !spans.is_empty() {
                spans.push(BarSpan {
                    text: " ·".into(),
                    color: theme::bar(),
                    bg: None,
                    bold: false,
                    dim: false,
                    priority: 2,
                });
            }
            let token_text = if let Some(window) = info.context_window {
                if window > 0 {
                    let pct = (tokens as f64 / window as f64 * 100.0) as u32;
                    format!(" {} ({}%)", super::format_tokens(tokens), pct)
                } else {
                    format!(" {}", super::format_tokens(tokens))
                }
            } else {
                format!(" {}", super::format_tokens(tokens))
            };
            spans.push(BarSpan {
                text: token_text,
                color: theme::muted(),
                bg: None,
                bold: false,
                dim: false,
                priority: 1,
            });
        }
    }
    if info.show_cost && info.session_cost_usd > 0.0 {
        if !spans.is_empty() {
            spans.push(BarSpan {
                text: " ·".into(),
                color: theme::bar(),
                bg: None,
                bold: false,
                dim: false,
                priority: 2,
            });
        }
        spans.push(BarSpan {
            text: format!(" {}", crate::metrics::format_cost(info.session_cost_usd)),
            color: theme::muted(),
            bg: None,
            bold: false,
            dim: false,
            priority: 1,
        });
    }
    spans
}

// ── Input area ──

struct CursorInfo {
    cursor_pos: Option<(u16, u16)>,
    cursor_style: Option<(Style, char)>,
}

struct ScrollInfo {
    scroll_offset: usize,
    total_content_rows: usize,
}

struct InputArea {
    cursor_info: CursorInfo,
    scroll_info: ScrollInfo,
    visible_rows: u16,
}

fn compute_input_area(
    input: &PromptInput<'_>,
    usable: usize,
    row_offset: u16,
    buf: &mut Buffer,
) -> InputArea {
    let height = input.height as usize;
    let state = input.input;
    let prediction = input.prediction;

    let spans = build_display_spans(&state.buf, &state.attachment_ids, &state.store);
    let display_buf = spans_to_string(&spans);
    let char_kinds = build_char_kinds(&spans);
    let display_cursor = map_cursor(state.cursor_char(), &state.buf, &spans);
    let display_selection = state.display_selection_range().map(|(start, end)| {
        let raw_start_char = crate::input::char_pos(&state.buf, start);
        let raw_end_char = crate::input::char_pos(&state.buf, end);
        let ds = map_cursor(raw_start_char, &state.buf, &spans);
        let de = map_cursor(raw_end_char, &state.buf, &spans);
        (ds, de)
    });
    let (visual_lines, cursor_line, _, cursor_char_in_line) =
        wrap_and_locate_cursor(&display_buf, &char_kinds, display_cursor, usable);
    // Slash commands and `!exec` are single-line by design. Multi-line
    // buffers render as plain text.
    let single_line = !state.buf.contains('\n');
    let plain_only = !single_line;
    let is_command = !plain_only && crate::completer::Completer::is_command(state.buf.trim());
    let is_exec =
        !plain_only && matches!(state.buf.as_bytes(), [b'!', c, ..] if !c.is_ascii_whitespace());
    let is_exec_invalid = !plain_only && state.buf == "!";
    let total_content_rows = visual_lines.len();

    let fixed = row_offset as usize + 1 + 1; // bottom bar + status line
    let max_content_rows = height.saturating_sub(fixed).max(1);
    let content_rows = total_content_rows.min(max_content_rows);

    let scroll_offset = if total_content_rows > content_rows {
        let max_off = total_content_rows.saturating_sub(content_rows);
        let cursor_moved = state.win.last_render_cpos != Some(state.win.cpos);
        let raw = if state.win.pending_recenter {
            cursor_line.saturating_sub(content_rows / 2)
        } else if cursor_moved {
            // Cursor moved since last render → ensure it's visible.
            // tmux copy-mode: wheel/scrollbar panning leaves cpos
            // unchanged, so this branch doesn't fire and scroll_top
            // stays where the user pinned it.
            let mut s = state.win.scroll_top as usize;
            if cursor_line < s {
                s = cursor_line;
            } else if cursor_line >= s + content_rows {
                s = cursor_line + 1 - content_rows;
            }
            s
        } else {
            state.win.scroll_top as usize
        };
        raw.min(max_off)
    } else {
        0
    };

    let show_prediction = prediction.is_some() && state.buf.is_empty();
    let mut cursor_info = CursorInfo {
        cursor_pos: None,
        cursor_style: None,
    };

    if show_prediction {
        let pred = prediction.unwrap();
        let first_line = pred.lines().next().unwrap_or(pred);
        let max_chars = usable.saturating_sub(1);
        let mut chars_iter = first_line.chars().take(max_chars);
        let line = if let Some(first) = chars_iter.next() {
            let rest: String = chars_iter.collect();
            if input.has_prompt_cursor {
                let (fg, bg) = cursor_style();
                let cursor_char_style = Style {
                    fg: Some(fg),
                    bg: Some(bg),
                    ..Style::default()
                };
                cursor_info.cursor_pos = Some((1, 0));
                cursor_info.cursor_style = Some((cursor_char_style, first));
                format!(" {first}{rest}")
            } else {
                format!(" {}{}", first, rest)
            }
        } else {
            if input.has_prompt_cursor {
                let (fg, bg) = cursor_style();
                cursor_info.cursor_pos = Some((1, 0));
                cursor_info.cursor_style = Some((
                    Style {
                        fg: Some(fg),
                        bg: Some(bg),
                        ..Style::default()
                    },
                    ' ',
                ));
            }
            " ".to_string()
        };
        buf.set_all_lines(vec![line]);
        if let Some((first, rest_start)) = first_prediction_split(first_line, max_chars) {
            if !input.has_prompt_cursor {
                let end = 1 + first.chars().count() as u16 + rest_start.chars().count() as u16;
                buf.add_highlight(0, 1, end, SpanStyle::dim());
            } else if !rest_start.is_empty() {
                let start = 2;
                let end = start + rest_start.chars().count() as u16;
                buf.add_highlight(0, start, end, SpanStyle::dim());
            }
        }
        return InputArea {
            cursor_info,
            scroll_info: ScrollInfo {
                scroll_offset: 0,
                total_content_rows: 0,
            },
            visible_rows: 1,
        };
    }

    let visible_lines: Vec<String> = visual_lines
        .iter()
        .skip(scroll_offset)
        .take(content_rows)
        .map(|(line, _)| format!(" {line}"))
        .collect();
    buf.set_all_lines(if visible_lines.is_empty() {
        vec![" ".into()]
    } else {
        visible_lines
    });

    let line_char_offsets = compute_visual_line_offsets(&display_buf, &visual_lines);

    for (li, (line, kinds)) in visual_lines
        .iter()
        .skip(scroll_offset)
        .take(content_rows)
        .enumerate()
    {
        let abs_idx = scroll_offset + li;

        let line_sel = display_selection.and_then(|(sel_start, sel_end)| {
            let line_start = line_char_offsets[abs_idx];
            let line_len = line.chars().count();
            let line_end = line_start + line_len;
            if line_len == 0 && sel_start <= line_start && sel_end > line_start {
                Some((0, 1))
            } else if sel_end <= line_start || sel_start >= line_end {
                None
            } else {
                let s = sel_start.saturating_sub(line_start);
                let e = sel_end.min(line_end) - line_start;
                Some((s, e))
            }
        });

        let line_cursor = if abs_idx == cursor_line && input.has_prompt_cursor {
            Some(cursor_char_in_line)
        } else {
            None
        };

        if is_command {
            let line_chars = line.chars().count();
            let prefix_chars = if abs_idx == 0 {
                line.char_indices()
                    .find(|(_, c)| c.is_whitespace())
                    .map(|(i, _)| line[..i].chars().count())
                    .unwrap_or(line_chars)
            } else {
                0
            };
            let mut cmd_kinds = vec![SpanKind::AtRef; prefix_chars];
            cmd_kinds.resize(line_chars, SpanKind::Plain);
            add_segments_to_buffer(
                buf,
                li,
                &styled_char_segments(line, &cmd_kinds, line_sel, line_cursor),
            );
        } else if (is_exec || is_exec_invalid) && abs_idx == 0 && line.starts_with('!') {
            add_segments_to_buffer(
                buf,
                li,
                &exec_bang_segments(line, kinds, line_sel, line_cursor),
            );
        } else {
            add_segments_to_buffer(
                buf,
                li,
                &styled_char_segments(line, kinds, line_sel, line_cursor),
            );
        }

        if line_cursor.is_some() {
            let cursor_col = 1 + cursor_char_in_line as u16;
            cursor_info.cursor_pos = Some((cursor_col, li as u16));
        }
    }

    if cursor_line >= total_content_rows && input.has_prompt_cursor && !show_prediction {
        let (fg, bg) = cursor_style();
        cursor_info.cursor_pos = Some((1, content_rows.saturating_sub(1) as u16));
        cursor_info.cursor_style = Some((
            Style {
                fg: Some(fg),
                bg: Some(bg),
                ..Style::default()
            },
            ' ',
        ));
    }

    InputArea {
        cursor_info,
        scroll_info: ScrollInfo {
            scroll_offset,
            total_content_rows,
        },
        visible_rows: content_rows as u16,
    }
}

fn first_prediction_split(line: &str, max_chars: usize) -> Option<(String, String)> {
    let mut chars_iter = line.chars().take(max_chars);
    let first = chars_iter.next()?;
    let rest: String = chars_iter.collect();
    Some((first.to_string(), rest))
}

fn add_segments_to_buffer(buf: &mut Buffer, line_idx: usize, segments: &[StyledSegment]) {
    let mut col = 1u16;
    for seg in segments {
        let len = seg.text.chars().count() as u16;
        if len > 0 {
            let end = col + len;
            if seg.style != Style::default() {
                buf.add_highlight(line_idx, col, end, span_style(seg.style));
            }
            col = end;
        }
    }
}

fn span_style(style: Style) -> SpanStyle {
    SpanStyle {
        fg: style.fg,
        bg: style.bg,
        bold: style.bold,
        dim: style.dim,
        italic: style.italic,
    }
}

/// Convert a styled line into StyledSegments, applying cursor and selection highlighting.
/// This replaces `render_styled_chars` from the old escape-sequence-based renderer.
fn styled_char_segments(
    line: &str,
    kinds: &[SpanKind],
    selection: Option<(usize, usize)>,
    cursor_pos: Option<usize>,
) -> Vec<StyledSegment> {
    let mut segments: Vec<StyledSegment> = Vec::new();
    let mut current_text = String::new();
    let mut current_style = Style::default();
    let char_count = line.chars().count();

    let (cursor_fg, cursor_bg) = cursor_style();
    let selection_bg = theme::selection_bg();

    for (i, ch) in line.chars().enumerate() {
        let kind = kinds.get(i).copied().unwrap_or(SpanKind::Plain);
        let want_sel = selection.is_some_and(|(s, e)| i >= s && i < e);
        let want_cursor = cursor_pos == Some(i);

        let style = if want_cursor {
            Style {
                fg: Some(cursor_fg),
                bg: Some(cursor_bg),
                ..Style::default()
            }
        } else {
            let fg = match kind {
                SpanKind::AtRef | SpanKind::Attachment => Some(theme::accent()),
                SpanKind::Plain => None,
            };
            let bg = if want_sel { Some(selection_bg) } else { None };
            Style {
                fg,
                bg,
                ..Style::default()
            }
        };

        if style != current_style && !current_text.is_empty() {
            segments.push(StyledSegment {
                text: std::mem::take(&mut current_text),
                style: current_style,
            });
        }
        current_style = style;
        current_text.push(ch);
    }

    // Flush remaining text
    if !current_text.is_empty() {
        segments.push(StyledSegment {
            text: current_text,
            style: current_style,
        });
    }

    // Cursor past end of line
    if cursor_pos == Some(char_count) {
        segments.push(StyledSegment {
            text: " ".into(),
            style: Style {
                fg: Some(cursor_fg),
                bg: Some(cursor_bg),
                ..Style::default()
            },
        });
    } else if let Some((s, e)) = selection {
        if e > char_count && s <= char_count {
            segments.push(StyledSegment {
                text: " ".into(),
                style: Style::bg(selection_bg),
            });
        }
    }

    segments
}

/// Handle the exec `!` prefix with special styling.
fn exec_bang_segments(
    line: &str,
    kinds: &[SpanKind],
    selection: Option<(usize, usize)>,
    cursor_pos: Option<usize>,
) -> Vec<StyledSegment> {
    let mut segs = Vec::new();

    let bang_cursor = cursor_pos == Some(0);
    let bang_selected = selection.is_some_and(|(s, _)| s == 0);

    if bang_cursor {
        let (fg, bg) = cursor_style();
        segs.push(StyledSegment {
            text: "!".into(),
            style: Style {
                fg: Some(fg),
                bg: Some(bg),
                ..Style::default()
            },
        });
    } else {
        segs.push(StyledSegment {
            text: "!".into(),
            style: Style {
                fg: Some(Color::Red),
                bg: if bang_selected {
                    Some(theme::selection_bg())
                } else {
                    None
                },
                bold: true,
                ..Style::default()
            },
        });
    }

    // Shift selection/cursor for remainder
    let rest_sel = selection.and_then(|(s, e)| {
        let s2 = if s == 0 { 0 } else { s - 1 };
        let e2 = e.saturating_sub(1);
        if s2 < e2 {
            Some((s2, e2))
        } else {
            None
        }
    });
    let rest_cursor = cursor_pos.and_then(|c| c.checked_sub(1));

    if line.len() > 1 {
        segs.extend(styled_char_segments(
            &line[1..],
            if kinds.len() > 1 { &kinds[1..] } else { &[] },
            rest_sel,
            rest_cursor,
        ));
    }

    segs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stash_row_has_muted_style() {
        let row = stash_row(40);
        assert!(row.segments[1].style.dim);
    }

    #[test]
    fn bar_row_fills_with_dashes() {
        let row = bar_row(20, None, None);
        let text: String = row.segments.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("────"));
    }

    #[test]
    fn bar_row_with_right_spans() {
        let right = vec![BarSpan {
            text: " model".into(),
            color: crossterm::style::Color::White,
            bg: None,
            bold: false,
            dim: false,
            priority: 0,
        }];
        let row = bar_row(30, None, Some(&right));
        let text: String = row.segments.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains(" model"));
        assert!(text.contains("────"));
    }

    #[test]
    fn styled_char_segments_plain() {
        let segs = styled_char_segments("hello", &[SpanKind::Plain; 5], None, None);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "hello");
    }

    #[test]
    fn styled_char_segments_with_cursor() {
        let segs = styled_char_segments("hello", &[SpanKind::Plain; 5], None, Some(2));
        // Before cursor, cursor char, after cursor
        assert!(segs.len() >= 3);
        // The cursor char should have inverted colors
        let cursor_seg = &segs[1];
        assert_eq!(cursor_seg.text, "l");
        assert!(cursor_seg.style.bg.is_some());
    }

    #[test]
    fn styled_char_segments_cursor_at_end() {
        let segs = styled_char_segments("hi", &[SpanKind::Plain; 2], None, Some(2));
        let last = segs.last().unwrap();
        assert_eq!(last.text, " ");
        assert!(last.style.bg.is_some());
    }

    #[test]
    fn styled_char_segments_with_selection() {
        let segs = styled_char_segments("hello", &[SpanKind::Plain; 5], Some((1, 4)), None);
        // Should have: unselected "h", selected "ell", unselected "o"
        assert!(segs.len() >= 3);
        assert_eq!(segs[0].text, "h");
        assert!(segs[0].style.bg.is_none());
        assert!(segs[1].style.bg.is_some()); // selected
    }

    #[test]
    fn exec_bang_segments_highlights_bang() {
        let kinds = vec![SpanKind::Plain; 4];
        let segs = exec_bang_segments("!ls", &kinds, None, None);
        assert_eq!(segs[0].text, "!");
        assert_eq!(segs[0].style.fg, Some(crossterm::style::Color::Red));
        assert!(segs[0].style.bold);
    }

    #[test]
    fn compute_prompt_produces_bars_and_status() {
        let input_state = PromptState::default();
        let mut prompt_input = PromptInput {
            queued: &[],
            stash: &None,
            input: &input_state,

            prediction: None,
            width: 80,
            height: 10,
            has_prompt_cursor: true,
            bar_info: BarInfo {
                model_label: None,
                reasoning_effort: protocol::ReasoningEffort::Off,
                show_tokens: false,
                context_tokens: None,
                context_window: None,
                show_cost: false,
                session_cost_usd: 0.0,
            },
        };
        let mut input_buf = Buffer::new(
            ui::BufId(0),
            ui::buffer::BufCreateOpts {
                modifiable: true,
                buftype: ui::buffer::BufType::Prompt,
            },
        );
        let output = compute_prompt(&mut prompt_input, &mut input_buf);
        // Should have at least: top bar + input area + bottom bar
        assert!(output.chrome_rows.len() >= 3);
        // Cursor should be in the input area
        assert!(output.cursor.is_some());
    }
}
