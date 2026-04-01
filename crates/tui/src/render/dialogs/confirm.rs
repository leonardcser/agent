use super::{
    begin_dialog_draw, finish_dialog_frame, render_inline_textarea, wrap_line, DialogResult,
    TextArea,
};
use crate::keymap::{hints, nav_lookup, NavAction};
use crate::render::highlight::{
    count_inline_diff_rows, print_inline_diff, print_syntax_file, BashHighlighter,
};
use crate::render::{crlf, draw_bar, ConfirmChoice, RenderOut, TerminalBackend};
use crate::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use crossterm::style::{Attribute, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::{cursor, terminal, QueueableCommand};
use std::collections::HashMap;
use std::io::Write;

/// Tool-specific scrollable preview content for the confirm dialog.
enum ConfirmPreview {
    /// No preview — simple tool calls.
    None,
    /// Inline diff preview for edit_file.
    Diff {
        old: String,
        new: String,
        path: String,
    },
    /// Syntax-highlighted file content for write_file.
    FileContent { content: String, path: String },
    /// Remaining lines of a multiline bash command (after the first line).
    BashBody {
        /// The full command — first line is rendered in the title, rest here.
        full_command: String,
    },
    /// Plan summary rendered as markdown for exit_plan_mode.
    PlanContent { summary: String },
}

impl ConfirmPreview {
    fn from_tool(tool_name: &str, desc: &str, args: &HashMap<String, serde_json::Value>) -> Self {
        match tool_name {
            "edit_file" => {
                let old = args
                    .get("old_string")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let new = args
                    .get("new_string")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let path = args
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                ConfirmPreview::Diff { old, new, path }
            }
            "write_file" => {
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let path = args
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                ConfirmPreview::FileContent { content, path }
            }
            "bash" if desc.lines().count() > 1 => ConfirmPreview::BashBody {
                full_command: desc.to_string(),
            },
            "exit_plan_mode" => {
                let summary = args
                    .get("plan_summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                ConfirmPreview::PlanContent { summary }
            }
            _ => ConfirmPreview::None,
        }
    }

    fn total_rows(&self, width: usize) -> u16 {
        match self {
            ConfirmPreview::None => 0,
            ConfirmPreview::Diff { old, new, path } => count_inline_diff_rows(old, new, path, old),
            ConfirmPreview::FileContent { content, .. } => content.lines().count() as u16,
            ConfirmPreview::BashBody { full_command } => (full_command.lines().count() - 1) as u16,
            ConfirmPreview::PlanContent { summary } => {
                let mut buf = RenderOut::buffer();
                crate::render::blocks::render_markdown_inner(
                    &mut buf, summary, width, " ", false, None,
                );
                let _ = buf.flush();
                let bytes = buf.into_bytes();
                let rendered = String::from_utf8_lossy(&bytes);
                rendered.split("\r\n").count().saturating_sub(1) as u16
            }
        }
    }

    fn is_some(&self) -> bool {
        !matches!(self, ConfirmPreview::None)
    }

    /// Whether to show the top dashed separator before the preview.
    fn has_top_separator(&self) -> bool {
        // Bash preview flows directly from the title line — no separator needed.
        !matches!(self, ConfirmPreview::None | ConfirmPreview::BashBody { .. })
    }

    fn render(&self, out: &mut RenderOut, skip: u16, viewport: u16, width: usize) {
        match self {
            ConfirmPreview::None => {}
            ConfirmPreview::Diff { old, new, path } => {
                print_inline_diff(out, old, new, path, old, skip, viewport);
            }
            ConfirmPreview::FileContent { content, path } => {
                print_syntax_file(out, content, path, skip, viewport);
            }
            ConfirmPreview::BashBody { full_command } => {
                let mut bh = BashHighlighter::new();
                let mut lines = full_command.lines();
                // Advance past first line (rendered in title) to preserve highlighter state.
                if let Some(first) = lines.next() {
                    bh.advance(first);
                }
                let body_lines: Vec<&str> = lines.collect();
                let mut emitted = 0u16;
                for (i, line) in body_lines.iter().enumerate() {
                    if (i as u16) < skip {
                        bh.advance(line);
                        continue;
                    }
                    if emitted >= viewport {
                        break;
                    }
                    let _ = out.queue(Print(" "));
                    bh.print_line(out, line);
                    crlf(out);
                    emitted += 1;
                }
            }
            ConfirmPreview::PlanContent { summary } => {
                let mut buf = RenderOut::buffer();
                crate::render::blocks::render_markdown_inner(
                    &mut buf, summary, width, " ", false, None,
                );
                let _ = buf.flush();
                let bytes: Vec<u8> = buf.into_bytes();
                let rendered = String::from_utf8_lossy(&bytes);
                let lines: Vec<&str> = rendered.split("\r\n").collect();
                let mut emitted = 0u16;
                for (i, line) in lines.iter().enumerate() {
                    if line.is_empty() && i == lines.len() - 1 {
                        break; // skip trailing empty from split
                    }
                    if (i as u16) < skip {
                        continue;
                    }
                    if emitted >= viewport {
                        break;
                    }
                    let _ = out.queue(Print(*line));
                    crlf(out);
                    emitted += 1;
                }
            }
        }
    }
}

struct ConfirmLayout {
    title_rows: u16,
    summary_rows: u16,
    has_preview: bool,
    viewport_rows: u16,
    total_preview: u16,
    total_rows: u16,
}

/// Non-blocking confirm dialog state machine.
pub struct ConfirmDialog {
    tool_name: String,
    display_name: String,
    desc: String,
    summary: Option<String>,
    preview: ConfirmPreview,
    options: Vec<(String, ConfirmChoice)>,
    preview_scroll: usize,
    selected: usize,
    textarea: TextArea,
    kill_ring: String,
    editing: bool,
    dirty: bool,
    request_id: u64,
    /// The anchor row where this dialog is positioned. None on first draw.
    pub anchor_row: Option<u16>,
    /// Row where the options section begins (used for partial redraws).
    options_row: u16,
    vim_enabled: bool,
    /// Cached terminal size, updated each draw().
    term_size: (u16, u16),
}

impl ConfirmDialog {
    pub fn new(req: &crate::render::ConfirmRequest, vim_enabled: bool) -> Self {
        let is_plan = req.tool_name == "exit_plan_mode";
        let mut options: Vec<(String, ConfirmChoice)> = if is_plan {
            vec![
                ("yes, and auto-apply".into(), ConfirmChoice::YesAutoApply),
                ("yes".into(), ConfirmChoice::Yes),
                ("no".into(), ConfirmChoice::No),
            ]
        } else {
            vec![
                ("yes".into(), ConfirmChoice::Yes),
                ("no".into(), ConfirmChoice::No),
            ]
        };
        if !is_plan {
            use crate::render::ApprovalScope::{Session, Workspace};

            let cwd_label = std::env::current_dir()
                .ok()
                .and_then(|p| {
                    let home = engine::home_dir();
                    if let Ok(rel) = p.strip_prefix(&home) {
                        return Some(format!("~/{}", rel.display()));
                    }
                    p.to_str().map(String::from)
                })
                .unwrap_or_default();

            if let Some(ref dir) = req.outside_dir {
                let dir_str = dir.to_string_lossy().into_owned();
                options.push((
                    format!("allow {dir_str}"),
                    ConfirmChoice::AlwaysDir(dir_str.clone(), Session),
                ));
                options.push((
                    format!("allow {dir_str} in {cwd_label}"),
                    ConfirmChoice::AlwaysDir(dir_str, Workspace),
                ));
            } else if !req.approval_patterns.is_empty() {
                let display: Vec<&str> = req
                    .approval_patterns
                    .iter()
                    .map(|p| {
                        let d = p.strip_suffix("/*").unwrap_or(p);
                        d.split("://").nth(1).unwrap_or(d)
                    })
                    .collect();
                let display_str = display.join(", ");
                options.push((
                    format!("allow {display_str}"),
                    ConfirmChoice::AlwaysPatterns(req.approval_patterns.clone(), Session),
                ));
                options.push((
                    format!("allow {display_str} in {cwd_label}"),
                    ConfirmChoice::AlwaysPatterns(req.approval_patterns.clone(), Workspace),
                ));
            } else {
                options.push(("always allow".into(), ConfirmChoice::Always(Session)));
                options.push((
                    format!("always allow in {cwd_label}"),
                    ConfirmChoice::Always(Workspace),
                ));
            }
        }

        let preview = ConfirmPreview::from_tool(&req.tool_name, &req.desc, &req.args);

        let display_name = if is_plan { "plan" } else { &req.tool_name };

        Self {
            tool_name: req.tool_name.clone(),
            display_name: display_name.to_string(),
            desc: req.desc.clone(),
            summary: req.summary.clone(),
            preview,
            options,
            preview_scroll: 0,
            selected: 0,
            textarea: TextArea::new(),
            kill_ring: String::new(),
            editing: false,
            anchor_row: None,
            options_row: 0,
            dirty: true,
            request_id: req.request_id,
            vim_enabled,
            term_size: terminal::size().unwrap_or((80, 24)),
        }
    }
}

impl ConfirmDialog {
    /// Override the cached terminal size (used by `height()` before the
    /// first `draw()` call).  In production `terminal::size()` matches the
    /// real screen, but test harnesses may need to inject a custom size.
    pub fn set_term_size(&mut self, width: u16, height: u16) {
        self.term_size = (width, height);
    }

    fn preview_total_rows(&self) -> usize {
        self.preview.total_rows(self.term_size.0 as usize) as usize
    }

    fn layout(&self, width: u16, height: u16) -> ConfirmLayout {
        let w = width as usize;
        let ta_visible = self.editing || !self.textarea.is_empty();
        let (selected_label, _) = &self.options[self.selected];
        let digits = format!("{}", self.selected + 1).len();
        let prefix_cols = 2 + digits + 2;
        let avail = w.saturating_sub(prefix_cols);
        let last_line_len = if avail > 0 {
            wrap_line(selected_label, avail)
                .last()
                .map(|l| l.len())
                .unwrap_or(0)
        } else {
            selected_label.len()
        };
        let text_indent = (prefix_cols + last_line_len + 2) as u16;
        let wrap_w = width.saturating_sub(text_indent) as usize;
        let ta_extra: u16 = if ta_visible {
            self.textarea.visual_row_count(wrap_w).saturating_sub(1)
        } else {
            0
        };

        let prefix_len = 1 + self.display_name.len() + 2;
        let title_rows = if matches!(self.preview, ConfirmPreview::BashBody { .. }) {
            // Only the first line goes in the title; the rest is scrollable preview.
            let first_line = self.desc.lines().next().unwrap_or("");
            wrap_line(first_line, w.saturating_sub(prefix_len)).len() as u16
        } else {
            wrap_line(&self.desc, w.saturating_sub(prefix_len)).len() as u16
        };
        let summary_rows: u16 = self
            .summary
            .as_ref()
            .map(|s| wrap_line(s, w.saturating_sub(1)).len() as u16)
            .unwrap_or(0);
        let has_preview = self.preview.is_some();
        // bar + title + summary + separators(if preview) +
        // "Allow?" + options + ta_extra + blank + hint
        let separator_rows = if has_preview {
            if self.preview.has_top_separator() {
                2
            } else {
                1
            }
        } else {
            1 // blank line
        };
        let option_rows: u16 = self
            .options
            .iter()
            .enumerate()
            .map(|(i, (label, _))| {
                let digits = format!("{}", i + 1).len();
                let prefix_cols = 2 + digits + 2; // "  N. "
                let avail = w.saturating_sub(prefix_cols);
                if avail == 0 {
                    1
                } else {
                    wrap_line(label, avail).len() as u16
                }
            })
            .sum();
        let fixed_rows: u16 =
            1 + title_rows + summary_rows + separator_rows + 1 + option_rows + ta_extra + 2;

        let total_preview = self.preview.total_rows(w);
        let viewport_rows: u16 = if has_preview {
            let space = height.saturating_sub(fixed_rows);
            space.max(1).min(total_preview)
        } else {
            0
        };

        ConfirmLayout {
            title_rows,
            summary_rows,
            has_preview,
            viewport_rows,
            total_preview,
            total_rows: fixed_rows + viewport_rows,
        }
    }
}

impl super::Dialog for ConfirmDialog {
    fn blocks_agent(&self) -> bool {
        true
    }

    fn height(&self) -> u16 {
        let (width, height) = self.term_size;
        self.layout(width, height).total_rows
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn handle_resize(&mut self) {
        self.anchor_row = None;
        self.dirty = true;
    }

    fn anchor_row(&self) -> Option<u16> {
        self.anchor_row
    }

    fn set_kill_ring(&mut self, contents: String) {
        self.kill_ring = contents;
    }

    fn kill_ring(&self) -> Option<&str> {
        Some(&self.kill_ring)
    }

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<DialogResult> {
        self.dirty = true;

        // ── Editing mode (textarea active) ──────────────────────────────
        if self.editing {
            match nav_lookup(code, modifiers) {
                Some(NavAction::Confirm) => {
                    let msg = if self.textarea.is_empty() {
                        None
                    } else {
                        Some(self.textarea.text())
                    };
                    return Some(DialogResult::Confirm {
                        choice: self.options[self.selected].1.clone(),
                        message: msg,
                        tool_name: self.tool_name.clone(),
                        request_id: self.request_id,
                    });
                }
                Some(NavAction::Dismiss) => {
                    if code == KeyCode::Esc {
                        // Esc exits editing; Ctrl+C rejects or clears.
                        self.editing = false;
                    } else if self.textarea.is_empty() {
                        return Some(DialogResult::Confirm {
                            choice: ConfirmChoice::No,
                            message: None,
                            tool_name: self.tool_name.clone(),
                            request_id: self.request_id,
                        });
                    } else {
                        self.textarea.clear();
                        self.editing = false;
                    }
                }
                _ => {
                    self.textarea
                        .handle_key_with_kill_ring(code, modifiers, &mut self.kill_ring);
                }
            }
            return None;
        }

        // ── Selection mode ──────────────────────────────────────────────
        let (width, height) = self.term_size;
        let viewport = self.layout(width, height).viewport_rows as usize;
        let scroll_step = (viewport / 2).max(1);
        match nav_lookup(code, modifiers) {
            Some(NavAction::Confirm) => {
                let msg = if self.textarea.is_empty() {
                    None
                } else {
                    Some(self.textarea.text())
                };
                Some(DialogResult::Confirm {
                    choice: self.options[self.selected].1.clone(),
                    message: msg,
                    tool_name: self.tool_name.clone(),
                    request_id: self.request_id,
                })
            }
            Some(NavAction::Dismiss) => Some(DialogResult::Confirm {
                choice: ConfirmChoice::No,
                message: None,
                tool_name: self.tool_name.clone(),
                request_id: self.request_id,
            }),
            Some(NavAction::Edit) => {
                self.editing = true;
                None
            }
            Some(NavAction::Up) => {
                self.selected = if self.selected == 0 {
                    self.options.len() - 1
                } else {
                    self.selected - 1
                };
                None
            }
            Some(NavAction::Down) => {
                self.selected = (self.selected + 1) % self.options.len();
                None
            }
            Some(NavAction::PageUp) => {
                self.preview_scroll = self.preview_scroll.saturating_sub(scroll_step);
                None
            }
            Some(NavAction::PageDown) => {
                let tp = self.preview_total_rows();
                if tp > 0 {
                    self.preview_scroll = (self.preview_scroll + scroll_step).min(tp);
                }
                None
            }
            _ => None,
        }
    }

    fn draw(&mut self, start_row: u16, sync_started: bool, backend: &dyn TerminalBackend) {
        if !self.dirty {
            return;
        }
        self.dirty = false;

        let mut out = backend.make_output();
        let (width, height) = backend.size();
        self.term_size = (width, height);
        let w = width as usize;

        let ly = self.layout(width, height);
        let ta_visible = self.editing || !self.textarea.is_empty();

        // Clamp scroll
        let max_scroll = (ly.total_preview as usize).saturating_sub(ly.viewport_rows as usize);
        self.preview_scroll = self.preview_scroll.min(max_scroll);

        let (bar_row, _) = begin_dialog_draw(
            &mut out,
            start_row,
            ly.total_rows,
            height,
            None,
            &mut self.anchor_row,
            sync_started,
        );

        // Where the options section should begin in the current layout.
        let preview_section = if ly.has_preview {
            let seps = if self.preview.has_top_separator() {
                2
            } else {
                1
            };
            seps + ly.viewport_rows
        } else {
            1 // blank line
        };
        let expected_options_row =
            bar_row + 1 + ly.title_rows + ly.summary_rows + preview_section + 1;

        // Partial redraw: when editing and the layout above the options
        // hasn't shifted, skip re-rendering bar/title/preview/"Allow?" and
        // only redraw from options_row down.
        let partial = self.editing
            && self.options_row == expected_options_row
            && self.options_row > 0
            && self.options_row >= bar_row;

        let mut row;
        if partial {
            row = self.options_row;
            out.row = Some(row);
            let _ = out.queue(cursor::MoveTo(0, row));
        } else {
            row = bar_row;

            let is_plan = matches!(self.preview, ConfirmPreview::PlanContent { .. });
            let title_color = if is_plan {
                theme::PLAN
            } else {
                theme::accent()
            };
            draw_bar(&mut out, w, None, None, title_color);
            crlf(&mut out);
            row += 1;

            // Title
            let prefix_len = 1 + self.display_name.len() + 2;
            let title_desc = if matches!(self.preview, ConfirmPreview::BashBody { .. }) {
                self.desc.lines().next().unwrap_or("")
            } else {
                &self.desc
            };
            let segments = wrap_line(title_desc, w.saturating_sub(prefix_len));
            let is_bash =
                matches!(self.preview, ConfirmPreview::BashBody { .. }) || self.tool_name == "bash";
            let mut bh = if is_bash {
                Some(BashHighlighter::new())
            } else {
                None
            };
            for (i, seg) in segments.iter().enumerate() {
                if i == 0 {
                    let _ = out.queue(Print(" "));
                    let _ = out.queue(SetForegroundColor(title_color));
                    let _ = out.queue(Print(&self.display_name));
                    let _ = out.queue(ResetColor);
                    let _ = out.queue(Print(": "));
                } else {
                    let _ = out.queue(Print(" "));
                }
                if let Some(ref mut h) = bh {
                    h.print_line(&mut out, seg);
                } else {
                    let _ = out.queue(Print(seg));
                }
                crlf(&mut out);
                row += 1;
            }

            // summary
            if let Some(ref summary) = self.summary {
                let max_cols = w.saturating_sub(1);
                let segments = wrap_line(summary, max_cols);
                for seg in &segments {
                    let _ = out.queue(Print(" "));
                    let _ = out.queue(SetForegroundColor(theme::muted()));
                    let _ = out.queue(Print(seg));
                    let _ = out.queue(ResetColor);
                    crlf(&mut out);
                    row += 1;
                }
            }

            if ly.has_preview {
                let separator: String = "\u{254c}".repeat(w);
                // Top separator (only for tools that request it)
                if self.preview.has_top_separator() {
                    let _ = out.queue(SetForegroundColor(theme::bar()));
                    let _ = out.queue(Print(&separator));
                    let _ = out.queue(ResetColor);
                    crlf(&mut out);
                    row += 1;
                }
                self.preview
                    .render(&mut out, self.preview_scroll as u16, ly.viewport_rows, w);
                row += ly.viewport_rows;
                // Bottom separator -- show scroll indicator when content is clipped
                let _ = out.queue(SetForegroundColor(theme::bar()));
                if ly.total_preview > ly.viewport_rows {
                    let pos = format!(
                        " [{}/{}]",
                        self.preview_scroll + ly.viewport_rows as usize,
                        ly.total_preview
                    );
                    let sep_len = w.saturating_sub(pos.len());
                    let _ = out.queue(Print("\u{254c}".repeat(sep_len)));
                    let _ = out.queue(SetForegroundColor(theme::muted()));
                    let _ = out.queue(Print(&pos));
                } else {
                    let _ = out.queue(Print(&separator));
                }
                let _ = out.queue(SetAttribute(Attribute::Reset));
                crlf(&mut out);
                row += 1;
            }

            if !ly.has_preview {
                crlf(&mut out);
                row += 1;
            }
            // Action prompt
            let is_plan = matches!(self.preview, ConfirmPreview::PlanContent { .. });
            let prompt_text = if is_plan { " Implement?" } else { " Allow?" };
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let _ = out.queue(Print(prompt_text));
            let _ = out.queue(SetAttribute(Attribute::Reset));
            crlf(&mut out);
            row += 1;
        }

        self.options_row = row;

        let mut cursor_pos: Option<(u16, u16)> = None;

        for (i, (label, _)) in self.options.iter().enumerate() {
            let digits = format!("{}", i + 1).len();
            let prefix_cols = 2 + digits + 2; // "  N. "
            let avail = w.saturating_sub(prefix_cols);
            let lines = if avail > 0 {
                wrap_line(label, avail)
            } else {
                vec![label.clone()]
            };
            let highlighted = i == self.selected;

            for (li, line) in lines.iter().enumerate() {
                if li == 0 {
                    let _ = out.queue(Print("  "));
                    if highlighted {
                        let _ = out.queue(SetAttribute(Attribute::Dim));
                        let _ = out.queue(Print(format!("{}.", i + 1)));
                        let _ = out.queue(SetAttribute(Attribute::Reset));
                        let _ = out.queue(Print(" "));
                    } else {
                        let _ = out.queue(SetAttribute(Attribute::Dim));
                        let _ = out.queue(Print(format!("{}. ", i + 1)));
                        let _ = out.queue(SetAttribute(Attribute::Reset));
                    }
                } else {
                    let _ = out.queue(Print(" ".repeat(prefix_cols)));
                }
                if highlighted {
                    let _ = out.queue(SetForegroundColor(theme::accent()));
                    let _ = out.queue(Print(line));
                    let _ = out.queue(ResetColor);
                } else {
                    let _ = out.queue(Print(line));
                }
                if li < lines.len() - 1 {
                    crlf(&mut out);
                    row += 1;
                }
            }

            if i == self.selected && ta_visible {
                let last_line_len = lines.last().map(|l| l.len()).unwrap_or(0);
                let text_col = (prefix_cols + last_line_len + 2) as u16;
                let wrap_w = (w as u16).saturating_sub(text_col) as usize;
                let (new_row, cpos) = render_inline_textarea(
                    &mut out,
                    &self.textarea,
                    self.editing,
                    text_col,
                    wrap_w,
                    row,
                );
                row = new_row;
                cursor_pos = cpos;
            } else {
                crlf(&mut out);
                row += 1;
            }
        }

        // footer: blank line + hint
        crlf(&mut out);
        crlf(&mut out);
        let _ = out.queue(SetAttribute(Attribute::Dim));
        let hint = if self.editing {
            hints::join(&[hints::SEND, hints::CANCEL])
        } else if !self.textarea.is_empty() {
            if ly.total_preview > 0 {
                hints::join(&[
                    hints::CONFIRM_WITH_MSG,
                    hints::EDIT_MSG,
                    hints::scroll(self.vim_enabled),
                ])
            } else {
                hints::join(&[hints::CONFIRM_WITH_MSG, hints::EDIT_MSG])
            }
        } else if ly.total_preview > 0 {
            hints::join(&[
                hints::CONFIRM,
                hints::ADD_MSG,
                hints::scroll(self.vim_enabled),
                hints::CANCEL,
            ])
        } else {
            hints::join(&[hints::CONFIRM, hints::ADD_MSG, hints::CANCEL])
        };
        let _ = out.queue(Print(&hint));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        // Only clear below the dialog if there's actually viewport space left.
        // When the dialog fills the full terminal, the cursor is at or past
        // the bottom row -- clearing there wipes the last visible option.
        if out.row.is_some_and(|r| r < height) {
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
        }

        finish_dialog_frame(&mut out, cursor_pos, self.editing);
    }
}
