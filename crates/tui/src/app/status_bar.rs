//! Status bar composition: builds the bottom-row ui::StatusBar layer
//! from the app's live state (mode, vim, throbber, permissions, procs,
//! agents, cursor position).

use super::*;

impl App {
    fn compute_status_position(&mut self) -> Option<render::StatusPosition> {
        match self.app_focus {
            crate::app::AppFocus::Prompt => {
                use crate::text_utils::byte_to_cell;
                let buf = &self.input.buf;
                let cpos = self.input.win.cpos.min(buf.len());
                let line_idx = buf[..cpos].bytes().filter(|&b| b == b'\n').count();
                let line_start = buf[..cpos].rfind('\n').map(|i| i + 1).unwrap_or(0);
                let col_cells = byte_to_cell(&buf[line_start..], cpos - line_start);
                let total_lines = buf.bytes().filter(|&b| b == b'\n').count() + 1;
                let pct = if total_lines <= 1 {
                    100
                } else {
                    ((line_idx as u64 * 100) / (total_lines.saturating_sub(1) as u64)) as u8
                };
                Some(render::StatusPosition {
                    line: (line_idx as u32) + 1,
                    col: col_cells as u32 + 1,
                    scroll_pct: pct.min(100),
                })
            }
            crate::app::AppFocus::Content => {
                let total_lines = self
                    .full_transcript_display_text(self.settings.show_thinking)
                    .len();
                if total_lines == 0 {
                    return None;
                }
                let line_idx = self.transcript_window.cursor_abs_row();
                let pct = if total_lines <= 1 {
                    100
                } else {
                    ((line_idx as u64 * 100) / (total_lines.saturating_sub(1) as u64)) as u8
                };
                Some(render::StatusPosition {
                    line: (line_idx as u32) + 1,
                    col: self.transcript_window.cursor_col as u32 + 1,
                    scroll_pct: pct.min(100),
                })
            }
        }
    }

    pub(super) fn refresh_status_bar(&mut self) {
        use crossterm::style::Color;
        use render::status::{spans_to_segments, StatusSpan};
        use ui::grid::Style;

        let (term_w, _) = self.ui.terminal_size();
        let width = term_w as usize;
        let status_bg = Color::AnsiValue(233);

        let mut spans: Vec<StatusSpan> = Vec::with_capacity(16);

        // Slug pill: spinner + label.
        let is_compacting = self.working.is_compacting();
        let pill_bg = if is_compacting {
            Color::White
        } else {
            crate::theme::slug_color()
        };
        let pill_style = render::StyleState {
            fg: Some(Color::Black),
            bg: Some(pill_bg),
            ..render::StyleState::default()
        };

        let spinner_char = self.working.spinner_char();
        if let Some(sp) = spinner_char {
            spans.push(StatusSpan {
                text: format!(" {sp} "),
                style: pill_style.clone(),
                priority: 0,
                ..StatusSpan::default()
            });
            let label = if is_compacting {
                "compacting ".into()
            } else if self.settings.show_slug {
                self.task_label
                    .as_deref()
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
        } else if self.settings.show_slug {
            if let Some(label) = self.task_label.as_deref() {
                spans.push(StatusSpan {
                    text: format!(" {label} "),
                    style: pill_style,
                    priority: 5,
                    truncatable: true,
                    ..StatusSpan::default()
                });
            }
        }

        // Vim mode.
        let (vim_enabled, vim_mode) = match self.app_focus {
            crate::app::AppFocus::Content => (
                self.transcript_window.vim.is_some(),
                self.transcript_window.vim.as_ref().map(|v| v.mode()),
            ),
            crate::app::AppFocus::Prompt => {
                let mut mode = self.input.vim_mode();
                if self.mouse_drag_active {
                    mode = Some(crate::vim::ViMode::Visual);
                }
                (self.input.vim_enabled() || self.mouse_drag_active, mode)
            }
        };
        if vim_enabled {
            let vim_label = render::status::vim_mode_label(vim_mode).unwrap_or("NORMAL");
            let vim_fg = match vim_mode {
                Some(crate::vim::ViMode::Insert) => Color::AnsiValue(78),
                Some(crate::vim::ViMode::Visual) | Some(crate::vim::ViMode::VisualLine) => {
                    Color::AnsiValue(176)
                }
                _ => Color::AnsiValue(74),
            };
            spans.push(StatusSpan {
                text: format!(" {vim_label} "),
                style: render::StyleState {
                    fg: Some(vim_fg),
                    bg: Some(Color::AnsiValue(236)),
                    ..render::StyleState::default()
                },
                priority: 3,
                ..StatusSpan::default()
            });
        }

        // Mode indicator.
        let mode = self.mode;
        let (mode_icon, mode_name, mode_fg) = match mode {
            protocol::Mode::Plan => ("◇ ", "plan", crate::theme::PLAN),
            protocol::Mode::Apply => ("→ ", "apply", crate::theme::APPLY),
            protocol::Mode::Yolo => ("⚡", "yolo", crate::theme::YOLO),
            protocol::Mode::Normal => ("○ ", "normal", crate::theme::muted()),
        };
        spans.push(StatusSpan {
            text: format!(" {mode_icon}{mode_name} "),
            style: render::StyleState {
                fg: Some(mode_fg),
                bg: Some(Color::AnsiValue(234)),
                ..render::StyleState::default()
            },
            priority: 1,
            ..StatusSpan::default()
        });

        // Throbber spans (timer, tok/s, etc.).
        let throbber_spans = self.working.throbber_spans(self.settings.show_tps);
        // Live-turn spans lead with the spinner glyph (already included
        // as a separate left-aligned span via `spinner_char`); skip it
        // here to avoid duplicating the glyph in the right-aligned area.
        let skip = if self.working.is_animating() && !throbber_spans.is_empty() {
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
                style: render::StyleState {
                    fg: Some(bar_span.color),
                    bg: Some(status_bg),
                    bold: bar_span.bold,
                    dim: bar_span.dim,
                    ..render::StyleState::default()
                },
                priority,
                ..StatusSpan::default()
            });
        }

        // Permission pending (no Confirm float is showing yet).
        if self.pending_dialog && !self.focused_float_blocks_agent() {
            spans.push(StatusSpan {
                text: "permission pending".into(),
                style: render::StyleState {
                    fg: Some(crate::theme::accent()),
                    bg: Some(status_bg),
                    bold: true,
                    ..render::StyleState::default()
                },
                priority: 2,
                group: true,
                ..StatusSpan::default()
            });
        }

        // Running procs.
        let running_procs = self.engine.processes.running_count();
        if running_procs > 0 {
            let label = if running_procs == 1 {
                "1 proc".into()
            } else {
                format!("{running_procs} procs")
            };
            spans.push(StatusSpan {
                text: label,
                style: render::StyleState {
                    fg: Some(crate::theme::accent()),
                    bg: Some(status_bg),
                    ..render::StyleState::default()
                },
                priority: 2,
                group: true,
                ..StatusSpan::default()
            });
        }

        // Running agents.
        let running_agents = self.agents.len();
        if running_agents > 0 {
            let label = if running_agents == 1 {
                "1 agent".into()
            } else {
                format!("{running_agents} agents")
            };
            spans.push(StatusSpan {
                text: label,
                style: render::StyleState {
                    fg: Some(crate::theme::AGENT),
                    bg: Some(status_bg),
                    ..render::StyleState::default()
                },
                priority: 2,
                group: true,
                ..StatusSpan::default()
            });
        }

        // Right-aligned position.
        let position = self.compute_status_position();
        if let Some(p) = position {
            spans.push(StatusSpan {
                text: p.render(),
                style: render::StyleState {
                    fg: Some(crate::theme::muted()),
                    bg: Some(status_bg),
                    ..render::StyleState::default()
                },
                priority: 3,
                align_right: true,
                ..StatusSpan::default()
            });
        }

        // Append Lua-registered statusline sources at the end. Priority
        // and align_right on each item determine final placement.
        for item in &self.custom_status_items {
            spans.push(item.to_span(status_bg));
        }

        let (left, right) = spans_to_segments(&mut spans, width, status_bg);
        if let Some(bar) = self.ui.layer_mut::<ui::StatusBar>("status") {
            *bar = ui::StatusBar::new().with_bg(Style::bg(status_bg));
            bar.set_left(left);
            bar.set_right(right);
        }
    }
}
