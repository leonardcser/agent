//! Responsive status line + top/bottom bar painters.
//!
//! `StatusSpan` drives the bottom status line (slug pill, mode,
//! throbber, permission/proc/agent indicators). `BarSpan` drives the
//! top-of-prompt and bottom-of-prompt horizontal rules drawn by
//! `draw_bar`. Both collapse on narrow terminals by dropping the
//! highest-priority (least-important) spans first.

use super::{layout_out::display_width, selection::truncate_str};
use crossterm::style::Color;

/// Foreground / background / attribute snapshot for a status-line
/// span. The status bar is the only consumer; it converts to
/// `ui::grid::Style` at paint time via `style_to_grid` below.
#[derive(Clone, Default, PartialEq)]
pub struct StyleState {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub crossedout: bool,
    pub underline: bool,
}

/// A structured status item that Lua (or internal code) provides.
/// Rust owns width fitting, priority dropping, and truncation.
#[derive(Clone, Debug)]
pub struct StatusItem {
    pub text: String,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub priority: u8,
    pub align_right: bool,
    pub truncatable: bool,
    pub group: bool,
}

impl StatusItem {
    pub(crate) fn to_span(&self, fill_bg: Color) -> StatusSpan {
        StatusSpan {
            text: self.text.clone(),
            style: StyleState {
                fg: self.fg,
                bg: Some(self.bg.unwrap_or(fill_bg)),
                bold: self.bold,
                ..StyleState::default()
            },
            priority: self.priority,
            align_right: self.align_right,
            truncatable: self.truncatable,
            group: self.group,
        }
    }
}

/// Buffer-agnostic cursor + scroll snapshot for the status bar.
/// One record covers every focused window (prompt, transcript). Set
/// once per tick by `App::tick_prompt` so the status bar stays in
/// sync with the actual focused window instead of a cached viewport
/// field that only some code paths update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusPosition {
    /// 1-indexed logical line of the cursor.
    pub line: u32,
    /// 1-indexed display column of the cursor.
    pub col: u32,
    /// Percent of the buffer scrolled past the top of the viewport
    /// (0 = top, 100 = bottom). Always clamped to `0..=100`.
    pub scroll_pct: u8,
}

impl StatusPosition {
    /// Format the way the status bar shows it: `<line>:<col> <pct>%`.
    pub fn render(&self) -> String {
        format!("{}:{} {}%", self.line, self.col, self.scroll_pct)
    }
}

pub(crate) fn vim_mode_label(mode: Option<crate::vim::ViMode>) -> Option<&'static str> {
    match mode {
        Some(crate::vim::ViMode::Insert) => Some("INSERT"),
        Some(crate::vim::ViMode::Visual) => Some("VISUAL"),
        Some(crate::vim::ViMode::VisualLine) => Some("VISUAL LINE"),
        _ => None,
    }
}

pub(crate) struct BarSpan {
    pub(crate) text: String,
    pub(crate) color: Color,
    pub(crate) bg: Option<Color>,
    pub(crate) bold: bool,
    pub(crate) dim: bool,
    /// Priority for responsive dropping. 0 = always show, higher = drop first.
    pub(crate) priority: u8,
}

/// A span in the status line with responsive priority support.
#[derive(Default)]
pub(crate) struct StatusSpan {
    pub(crate) text: String,
    pub(crate) style: StyleState,
    /// Priority for responsive dropping. 0 = always show, higher = drop first.
    pub(crate) priority: u8,
    /// If true, a " · " separator is inserted before this span during rendering.
    pub(crate) group: bool,
    /// If true, the text can be truncated with "…" before being fully dropped.
    pub(crate) truncatable: bool,
    /// If true, the span is pulled to the right edge of the status bar
    /// with a one-cell gap before the terminal edge. Right-aligned
    /// spans render after every left-aligned span and don't accept
    /// group separators.
    pub(crate) align_right: bool,
}

/// Separator inserted between groups in the status line.
const STATUS_SEP: &str = " \u{00b7} "; // " · "
const STATUS_SEP_LEN: usize = 3;

pub(crate) fn spans_to_segments(
    spans: &mut Vec<StatusSpan>,
    width: usize,
    fill_bg: Color,
) -> (Vec<ui::StatusSegment>, Vec<ui::StatusSegment>) {
    const RIGHT_EDGE_GAP: usize = 1;

    let span_cols = |spans: &[StatusSpan], right: bool| -> usize {
        let mut w = 0;
        let mut first = true;
        for s in spans.iter().filter(|s| s.align_right == right) {
            if s.group && !first {
                w += STATUS_SEP_LEN;
            }
            w += display_width(&s.text);
            first = false;
        }
        w
    };
    let total_width = |spans: &[StatusSpan]| -> usize {
        let left = span_cols(spans, false);
        let right = span_cols(spans, true);
        let gap = if right > 0 { RIGHT_EDGE_GAP } else { 0 };
        left + right + gap
    };

    while total_width(spans) > width && !spans.is_empty() {
        let max_pri = spans.iter().map(|s| s.priority).max().unwrap_or(0);
        if max_pri == 0 {
            break;
        }
        let trunc_idx = spans
            .iter()
            .rposition(|s| s.priority == max_pri && s.truncatable);
        if let Some(idx) = trunc_idx {
            let available =
                width.saturating_sub(total_width(spans) - display_width(&spans[idx].text));
            if available >= 2 {
                spans[idx].text = truncate_str(&spans[idx].text, available);
                continue;
            }
        }
        spans.retain(|s| s.priority != max_pri);
    }

    let sep_style = ui::grid::Style {
        fg: Some(crate::theme::muted()),
        bg: Some(fill_bg),
        dim: true,
        ..ui::grid::Style::default()
    };

    let style_to_grid = |ss: &StyleState| -> ui::grid::Style {
        ui::grid::Style {
            fg: ss.fg,
            bg: ss.bg,
            bold: ss.bold,
            dim: ss.dim,
            italic: ss.italic,
            underline: ss.underline,
            crossedout: ss.crossedout,
        }
    };

    let mut left = Vec::new();
    let mut right = Vec::new();

    let mut first_left = true;
    for s in spans.iter().filter(|s| !s.align_right) {
        if s.group && !first_left {
            left.push(ui::StatusSegment::styled(STATUS_SEP, sep_style));
        }
        left.push(ui::StatusSegment::styled(&s.text, style_to_grid(&s.style)));
        first_left = false;
    }

    let mut first_right = true;
    for s in spans.iter().filter(|s| s.align_right) {
        if s.group && !first_right {
            right.push(ui::StatusSegment::styled(STATUS_SEP, sep_style));
        }
        right.push(ui::StatusSegment::styled(&s.text, style_to_grid(&s.style)));
        first_right = false;
    }

    (left, right)
}
