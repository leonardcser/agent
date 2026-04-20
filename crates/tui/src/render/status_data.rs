use super::status::{vim_mode_label, BarSpan, StatusPosition, StatusSpan};
use crate::theme;
use crossterm::style::Color;
use ui::grid::Style;
use ui::StatusSegment;

pub(crate) struct StatusInput {
    pub width: u16,
    pub working: WorkingSnapshot,
    pub vim_enabled: bool,
    pub vim_mode: Option<crate::vim::ViMode>,
    pub mode: protocol::Mode,
    pub pending_dialog: bool,
    pub dialog_open: bool,
    pub running_procs: usize,
    pub running_agents: usize,
    pub position: Option<StatusPosition>,
    pub custom_items: Option<Vec<super::StatusItem>>,
    pub show_slug: bool,
}

pub(crate) struct WorkingSnapshot {
    pub spinner_char: Option<String>,
    pub is_compacting: bool,
    pub task_label: Option<String>,
    pub throbber_spans: Vec<BarSpan>,
    pub is_active: bool,
}

pub(crate) struct StatusOutput {
    pub left: Vec<StatusSegment>,
    pub right: Vec<StatusSegment>,
    pub bg: Style,
}

pub(crate) fn compute_status(input: &StatusInput) -> StatusOutput {
    let status_bg = Color::AnsiValue(233);
    let width = input.width as usize;

    if let Some(ref items) = input.custom_items {
        let mut spans: Vec<StatusSpan> = items.iter().map(|i| i.to_span(status_bg)).collect();
        let (left_segs, right_segs) = spans_to_segments(&mut spans, width, status_bg);
        return StatusOutput {
            left: left_segs,
            right: right_segs,
            bg: Style::bg(status_bg),
        };
    }

    let mut spans: Vec<StatusSpan> = Vec::with_capacity(16);

    // Slug pill
    let pill_bg = if input.working.is_compacting {
        Color::White
    } else {
        theme::slug_color()
    };
    let pill_style = super::StyleState {
        fg: Some(Color::Black),
        bg: Some(pill_bg),
        ..super::StyleState::default()
    };

    if let Some(ref sp) = input.working.spinner_char {
        spans.push(StatusSpan {
            text: format!(" {sp} "),
            style: pill_style.clone(),
            priority: 0,
            ..StatusSpan::default()
        });
        let label = if input.working.is_compacting {
            "compacting ".into()
        } else if input.show_slug {
            input
                .working
                .task_label
                .as_ref()
                .map(|l| format!("{l} "))
                .unwrap_or_else(|| "working ".into())
        } else {
            "working ".into()
        };
        spans.push(StatusSpan {
            text: label,
            style: pill_style,
            priority: 5,
            truncatable: true,
            ..StatusSpan::default()
        });
    } else if input.show_slug {
        if let Some(ref label) = input.working.task_label {
            spans.push(StatusSpan {
                text: format!(" {label} "),
                style: pill_style,
                priority: 5,
                truncatable: true,
                ..StatusSpan::default()
            });
        }
    }

    // Vim mode
    if input.vim_enabled {
        let vim_label = vim_mode_label(input.vim_mode).unwrap_or("NORMAL");
        let vim_fg = match input.vim_mode {
            Some(crate::vim::ViMode::Insert) => Color::AnsiValue(78),
            Some(crate::vim::ViMode::Visual) | Some(crate::vim::ViMode::VisualLine) => {
                Color::AnsiValue(176)
            }
            _ => Color::AnsiValue(74),
        };
        spans.push(StatusSpan {
            text: format!(" {vim_label} "),
            style: super::StyleState {
                fg: Some(vim_fg),
                bg: Some(Color::AnsiValue(236)),
                ..super::StyleState::default()
            },
            priority: 3,
            ..StatusSpan::default()
        });
    }

    // Mode indicator
    let (mode_icon, mode_name, mode_fg) = match input.mode {
        protocol::Mode::Plan => ("◇ ", "plan", theme::PLAN),
        protocol::Mode::Apply => ("→ ", "apply", theme::APPLY),
        protocol::Mode::Yolo => ("⚡", "yolo", theme::YOLO),
        protocol::Mode::Normal => ("○ ", "normal", theme::muted()),
    };
    spans.push(StatusSpan {
        text: format!(" {mode_icon}{mode_name} "),
        style: super::StyleState {
            fg: Some(mode_fg),
            bg: Some(Color::AnsiValue(234)),
            ..super::StyleState::default()
        },
        priority: 1,
        ..StatusSpan::default()
    });

    // Throbber spans (timer, tok/s, etc.)
    let throbber_spans = &input.working.throbber_spans;
    let skip = if input.working.is_active && !throbber_spans.is_empty() {
        1
    } else {
        0
    };
    for bar_span in throbber_spans.iter().skip(skip) {
        let priority = match bar_span.priority {
            0 => 4,
            3 => 6,
            p => p,
        };
        spans.push(StatusSpan {
            text: bar_span.text.clone(),
            style: super::StyleState {
                fg: Some(bar_span.color),
                bg: Some(status_bg),
                bold: bar_span.bold,
                dim: bar_span.dim,
                ..super::StyleState::default()
            },
            priority,
            ..StatusSpan::default()
        });
    }

    // Permission pending
    if input.pending_dialog && !input.dialog_open {
        spans.push(StatusSpan {
            text: "permission pending".into(),
            style: super::StyleState {
                fg: Some(theme::accent()),
                bg: Some(status_bg),
                bold: true,
                ..super::StyleState::default()
            },
            priority: 2,
            group: true,
            ..StatusSpan::default()
        });
    }

    // Running procs
    if input.running_procs > 0 {
        let label = if input.running_procs == 1 {
            "1 proc".into()
        } else {
            format!("{} procs", input.running_procs)
        };
        spans.push(StatusSpan {
            text: label,
            style: super::StyleState {
                fg: Some(theme::accent()),
                bg: Some(status_bg),
                ..super::StyleState::default()
            },
            priority: 2,
            group: true,
            ..StatusSpan::default()
        });
    }

    // Running agents
    if input.running_agents > 0 {
        let label = if input.running_agents == 1 {
            "1 agent".into()
        } else {
            format!("{} agents", input.running_agents)
        };
        spans.push(StatusSpan {
            text: label,
            style: super::StyleState {
                fg: Some(theme::AGENT),
                bg: Some(status_bg),
                ..super::StyleState::default()
            },
            priority: 2,
            group: true,
            ..StatusSpan::default()
        });
    }

    // Right-aligned position
    if let Some(p) = input.position {
        spans.push(StatusSpan {
            text: p.render(),
            style: super::StyleState {
                fg: Some(theme::muted()),
                bg: Some(status_bg),
                ..super::StyleState::default()
            },
            priority: 3,
            truncatable: true,
            align_right: true,
            ..StatusSpan::default()
        });
    }

    let (left_segs, right_segs) = spans_to_segments(&mut spans, width, status_bg);
    StatusOutput {
        left: left_segs,
        right: right_segs,
        bg: Style::bg(status_bg),
    }
}

fn spans_to_segments(
    spans: &mut Vec<StatusSpan>,
    width: usize,
    fill_bg: Color,
) -> (Vec<StatusSegment>, Vec<StatusSegment>) {
    use super::layout_out::display_width;
    use super::selection::truncate_str;

    const SEP: &str = " \u{00b7} ";
    const SEP_LEN: usize = 3;
    const RIGHT_EDGE_GAP: usize = 1;

    let span_cols = |spans: &[StatusSpan], right: bool| -> usize {
        let mut w = 0;
        let mut first = true;
        for s in spans.iter().filter(|s| s.align_right == right) {
            if s.group && !first {
                w += SEP_LEN;
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

    let sep_style = Style {
        fg: Some(theme::muted()),
        bg: Some(fill_bg),
        dim: true,
        ..Style::default()
    };

    let style_state_to_style = |ss: &super::StyleState| -> Style {
        Style {
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
            left.push(StatusSegment::styled(SEP, sep_style));
        }
        left.push(StatusSegment::styled(
            &s.text,
            style_state_to_style(&s.style),
        ));
        first_left = false;
    }

    let mut first_right = true;
    for s in spans.iter().filter(|s| s.align_right) {
        if s.group && !first_right {
            right.push(StatusSegment::styled(SEP, sep_style));
        }
        right.push(StatusSegment::styled(
            &s.text,
            style_state_to_style(&s.style),
        ));
        first_right = false;
    }

    (left, right)
}
