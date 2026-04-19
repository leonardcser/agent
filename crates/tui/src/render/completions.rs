//! Completion dropdown and menu (stats/cost) painters.
//!
//! Drives the list that appears under the prompt when the user is
//! typing a slash command, path, or navigating a picker (theme, color,
//! settings). Also handles `/stats` and `/cost` side-by-side views.

use super::{term_width, RenderOut};
use crate::theme;
use crossterm::{style::Color, terminal, QueueableCommand};

pub(super) fn completion_layout(completer: &crate::completer::Completer) -> (usize, usize, usize) {
    let show_hints = completer.kind == crate::completer::CompleterKind::Settings;
    let hint_rows = usize::from(show_hints) * 2;
    let empty_gap = usize::from(show_hints);
    let list_rows = completer.max_visible_rows();
    (list_rows, hint_rows, empty_gap)
}

/// Rows that `draw_completions` will paint. Used to anchor the
/// overlay flush against the prompt/status bar, growing upward.
pub(super) fn completion_actual_rows(completer: Option<&crate::completer::Completer>) -> usize {
    let Some(comp) = completer else {
        return 0;
    };
    if comp.results.is_empty() {
        if comp.is_picker() {
            let (_, hint_rows, empty_gap) = completion_layout(comp);
            return 1 + hint_rows + empty_gap;
        }
        return 0;
    }
    let (list_rows, hint_rows, empty_gap) = completion_layout(comp);
    if list_rows == 0 {
        return 0;
    }
    let visible = list_rows.min(comp.results.len());
    visible + hint_rows + empty_gap
}

/// Caller-side presentation overrides for `draw_completions`.
pub(super) struct CompletionStyle<'a> {
    pub prefix: Option<&'a str>,
    pub left_indent: u16,
}

impl Default for CompletionStyle<'_> {
    fn default() -> Self {
        Self {
            prefix: None,
            left_indent: 1,
        }
    }
}

pub(super) fn draw_completions(
    out: &mut RenderOut,
    completer: Option<&crate::completer::Completer>,
    max_rows: usize,
    vim_enabled: bool,
    style: &CompletionStyle<'_>,
) -> usize {
    use crate::completer::CompleterKind;

    let Some(comp) = completer else {
        return 0;
    };
    if max_rows == 0 {
        return 0;
    }

    let indent = " ".repeat(style.left_indent as usize);

    let (_, hint_rows, empty_gap) = completion_layout(comp);
    let show_hints = hint_rows > 0;
    let list_rows = max_rows.saturating_sub(hint_rows + empty_gap);

    if comp.results.is_empty() {
        if comp.is_picker() {
            out.push_dim();
            out.print(&indent);
            out.print("no results");
            out.pop_style();
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            if show_hints && max_rows > hint_rows + empty_gap {
                out.newline();
                draw_settings_hints(out, vim_enabled);
                return 1 + empty_gap + hint_rows;
            }
            return 1;
        }
        return 0;
    }
    if list_rows == 0 {
        return 0;
    }
    let total = comp.results.len();
    let visible_rows = list_rows.min(total);
    let mut start = 0;
    if total > visible_rows {
        let half = visible_rows / 2;
        start = comp.selected.saturating_sub(half);
        if start + visible_rows > total {
            start = total - visible_rows;
        }
    }
    let end = start + visible_rows;

    let is_color_picker = matches!(comp.kind, CompleterKind::Theme | CompleterKind::Color);

    let prefix = style.prefix.unwrap_or(match comp.kind {
        CompleterKind::Command => "/",
        CompleterKind::File => "./",
        _ => "",
    });
    let max_label = comp
        .results
        .iter()
        .map(|i| prefix.len() + i.label.len())
        .max()
        .unwrap_or(0);
    let avail = term_width().saturating_sub(style.left_indent as usize + 2);

    let mut drawn = 0;

    if show_hints {
        draw_settings_hints(out, vim_enabled);
        out.newline();
        drawn += hint_rows + empty_gap;
    }

    let slice = &comp.results[start..end];
    for (i, item) in slice.iter().enumerate().rev() {
        let idx = start + i;
        let selected = idx == comp.selected;
        let raw = format!("{}{}", prefix, item.label);
        let label: String = raw.chars().take(avail).collect();

        if is_color_picker {
            out.print(&indent);
            if selected {
                let ansi = item.ansi_color.unwrap_or(theme::accent_value());
                out.push_fg(Color::AnsiValue(ansi));
                out.print(&format!("● {}", label));
                out.pop_style();
            } else {
                out.push_dim();
                out.print(&format!("  {}", label));
                out.pop_style();
            }
            if let Some(ref desc) = item.description {
                let pad = (max_label + 2).saturating_sub(label.len());
                out.push_dim();
                out.print(&format!("{:>width$}{}", "", desc, width = pad));
                out.pop_style();
            }
        } else {
            out.print(&indent);
            if selected {
                out.push_fg(theme::accent());
                out.print(&label);
                if let Some(ref desc) = item.description {
                    let pad = max_label - label.len() + 2;
                    out.pop_style();
                    out.push_dim();
                    out.print(&format!("{:>width$}{}", "", desc, width = pad));
                    out.pop_style();
                } else {
                    out.pop_style();
                }
            } else {
                out.push_dim();
                out.print(&label);
                if let Some(ref desc) = item.description {
                    let pad = max_label - label.len() + 2;
                    out.print(&format!("{:>width$}{}", "", desc, width = pad));
                }
                out.pop_style();
            }
        }

        drawn += 1;
        if i > 0 {
            out.newline();
        } else {
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        }
    }

    drawn
}

fn draw_settings_hints(out: &mut RenderOut, vim_enabled: bool) {
    out.newline();
    out.push_dim();
    out.print(&crate::keymap::hints::join(&[
        crate::keymap::hints::picker_nav(vim_enabled),
        "enter/space: toggle",
        crate::keymap::hints::CANCEL,
    ]));
    out.pop_style();
    let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
}

pub(super) fn draw_menu(
    out: &mut RenderOut,
    ms: &crate::input::MenuState,
    max_rows: usize,
) -> usize {
    if max_rows == 0 {
        return 0;
    }
    if let crate::input::MenuKind::Stats { left, right } = &ms.kind {
        return draw_stats(out, left, right, max_rows);
    }
    if let crate::input::MenuKind::Cost { lines } = &ms.kind {
        return draw_stats_sequential(out, lines, 0, max_rows);
    }
    0
}

/// Heat intensity colors: dim → accent, 4 levels.
const HEAT_COLORS: [Color; 4] = [
    Color::AnsiValue(238), // very dim
    Color::AnsiValue(103), // muted lavender
    Color::AnsiValue(141), // medium lavender
    Color::AnsiValue(147), // bright accent
];
const HEAT_CHAR: &str = "█";
const HEAT_EMPTY: &str = "·";

use crate::metrics::{label_col_width, stats_line_visual_width as stats_line_width};

fn draw_stats_line(out: &mut RenderOut, line: &crate::metrics::StatsLine, label_col: usize) {
    use crate::metrics::StatsLine;
    match line {
        StatsLine::Kv { label, value } => {
            out.push_dim();
            out.print(label);
            out.pop_style();
            let col = label_col.max(label.len() + 2);
            let padding = " ".repeat(col.saturating_sub(label.len()));
            out.print(&padding);
            out.print(value);
        }
        StatsLine::Heading(text) | StatsLine::SparklineLegend(text) => {
            out.push_dim();
            out.print(text);
            out.pop_style();
        }
        StatsLine::SparklineBars(bars) => {
            out.push_fg(theme::accent());
            out.print(bars);
            out.pop_style();
        }
        StatsLine::HeatRow { label, cells } => {
            out.push_dim();
            out.print(&format!("{label} "));
            out.pop_style();
            for cell in cells {
                match cell {
                    crate::metrics::HeatCell::Empty => {
                        out.push_fg(Color::AnsiValue(238));
                        out.print(&format!("{HEAT_EMPTY} "));
                        out.pop_style();
                    }
                    crate::metrics::HeatCell::Level(lvl) => {
                        let color = HEAT_COLORS[(*lvl as usize).min(3)];
                        out.push_fg(color);
                        out.print(&format!("{HEAT_CHAR} "));
                        out.pop_style();
                    }
                }
            }
        }
        StatsLine::Blank => {}
    }
}

fn draw_stats_sequential(
    out: &mut RenderOut,
    lines: &[crate::metrics::StatsLine],
    already_drawn: usize,
    max_rows: usize,
) -> usize {
    let lc = label_col_width(lines);
    let mut count = 0;
    for line in lines {
        if already_drawn + count >= max_rows {
            break;
        }
        if already_drawn + count > 0 {
            out.newline();
        }
        out.print("  ");
        draw_stats_line(out, line, lc);
        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        count += 1;
    }
    count
}

fn draw_stats(
    out: &mut RenderOut,
    left: &[crate::metrics::StatsLine],
    right: &[crate::metrics::StatsLine],
    max_rows: usize,
) -> usize {
    let left_lc = label_col_width(left);
    let right_lc = label_col_width(right);

    let left_col_width = left
        .iter()
        .map(|l| 2 + stats_line_width(l, left_lc))
        .max()
        .unwrap_or(0);

    let right_width: usize = right
        .iter()
        .map(|l| stats_line_width(l, right_lc))
        .max()
        .unwrap_or(0);
    let term_width = terminal::size().map(|(w, _)| w as usize).unwrap_or(80);
    let gap = 5;
    let side_by_side = !right.is_empty() && left_col_width + gap + right_width + 2 <= term_width;

    if !side_by_side {
        let mut drawn = draw_stats_sequential(out, left, 0, max_rows);
        if !right.is_empty() && drawn < max_rows {
            out.newline();
            drawn += 1;
            drawn += draw_stats_sequential(out, right, drawn, max_rows);
        }
        return drawn;
    }

    let total = left.len().max(right.len());
    let right_col = left_col_width + gap;
    let mut drawn = 0;

    for i in 0..total {
        if drawn >= max_rows {
            break;
        }
        if drawn > 0 {
            out.newline();
        }

        let lw = if i < left.len() {
            out.print("  ");
            draw_stats_line(out, &left[i], left_lc);
            2 + stats_line_width(&left[i], left_lc)
        } else {
            0
        };

        if i < right.len() {
            let pad = right_col.saturating_sub(lw);
            out.print(&" ".repeat(pad));
            draw_stats_line(out, &right[i], right_lc);
        }

        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        drawn += 1;
    }
    drawn
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_completer_with_no_results_paints_no_rows() {
        let mut comp = crate::completer::Completer::files(0);
        comp.update_query("zzzzzz".into());

        assert!(comp.results.is_empty());
        assert!(!comp.is_picker());
        assert_eq!(completion_actual_rows(Some(&comp)), 0);
    }

    #[test]
    fn settings_empty_results_leave_blank_line_before_hints() {
        let state = crate::input::SettingsState {
            vim: true,
            auto_compact: false,
            show_tps: true,
            show_tokens: true,
            show_cost: true,
            show_prediction: true,
            show_slug: true,
            show_thinking: true,
            restrict_to_workspace: false,
            redact_secrets: true,
        };
        let mut comp = crate::completer::Completer::settings(&state);
        comp.update_query("zzzzzz".into());
        let mut out = RenderOut::buffer();
        let rows = draw_completions(
            &mut out,
            Some(&comp),
            completion_actual_rows(Some(&comp)),
            true,
            &Default::default(),
        );
        let rendered = String::from_utf8(out.into_bytes()).unwrap();
        assert!(rows >= 4);
        assert!(
            rendered.contains("no results")
                && rendered.contains("\r\n\u{1b}[K\r\n")
                && rendered.contains("ctrl+j/k: navigate"),
            "rendered: {rendered:?}"
        );
    }
}
