use super::{
    begin_dialog_draw, finish_dialog_frame, render_inline_textarea, DialogResult, TextArea,
};
use crate::render::blocks::wrap_line;
use crate::render::highlight::{count_inline_diff_rows, print_inline_diff, print_syntax_file};
use crate::render::{crlf, draw_bar, ConfirmChoice, RenderOut};
use crate::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use crossterm::style::{Attribute, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::{cursor, terminal, QueueableCommand};
use std::collections::HashMap;

/// Compute preview row count for the confirm dialog.
fn confirm_preview_row_count(tool_name: &str, args: &HashMap<String, serde_json::Value>) -> u16 {
    match tool_name {
        "edit_file" => {
            let old = args
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new = args
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            count_inline_diff_rows(old, new, path, old)
        }
        "write_file" => {
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            content.lines().count() as u16
        }
        _ => 0,
    }
}

/// Render the syntax-highlighted preview for the confirm dialog.
/// Renders at most `viewport` rows starting from `skip` into the full preview.
fn render_confirm_preview(
    out: &mut RenderOut,
    tool_name: &str,
    args: &HashMap<String, serde_json::Value>,
    skip: u16,
    viewport: u16,
) {
    match tool_name {
        "edit_file" => {
            let old = args
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new = args
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            print_inline_diff(out, old, new, path, old, skip, viewport);
        }
        "write_file" => {
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            print_syntax_file(out, content, path, skip, viewport);
        }
        _ => {}
    }
}

struct ConfirmLayout {
    title_rows: u16,
    summary_rows: u16,
    has_preview: bool,
    viewport_rows: u16,
    total_rows: u16,
}

/// Non-blocking confirm dialog state machine.
pub struct ConfirmDialog {
    tool_name: String,
    desc: String,
    summary: Option<String>,
    args: HashMap<String, serde_json::Value>,
    options: Vec<(String, ConfirmChoice)>,
    total_preview: u16,
    preview_scroll: usize,
    selected: usize,
    textarea: TextArea,
    editing: bool,
    dirty: bool,
    request_id: u64,
    /// The anchor row where this dialog is positioned. None on first draw.
    pub anchor_row: Option<u16>,
    /// Row where the options section begins (used for partial redraws).
    options_row: u16,
}

impl ConfirmDialog {
    pub fn new(
        tool_name: &str,
        desc: &str,
        args: &HashMap<String, serde_json::Value>,
        approval_pattern: Option<&str>,
        summary: Option<&str>,
        request_id: u64,
    ) -> Self {
        let mut options: Vec<(String, ConfirmChoice)> = vec![
            ("yes".into(), ConfirmChoice::Yes),
            ("no".into(), ConfirmChoice::No),
        ];
        if let Some(pattern) = approval_pattern {
            let display = pattern.strip_suffix("/*").unwrap_or(pattern);
            let display = display.split("://").nth(1).unwrap_or(display);
            options.push((
                format!("allow {display}"),
                ConfirmChoice::AlwaysPattern(pattern.to_string()),
            ));
        } else {
            options.push(("always allow".into(), ConfirmChoice::Always));
        }

        let total_preview = confirm_preview_row_count(tool_name, args);

        Self {
            tool_name: tool_name.to_string(),
            desc: desc.to_string(),
            summary: summary.map(|s| s.to_string()),
            args: args.clone(),
            options,
            total_preview,
            preview_scroll: 0,
            selected: 0,
            textarea: TextArea::new(),
            editing: false,
            anchor_row: None,
            options_row: 0,
            dirty: true,
            request_id,
        }
    }
}

impl ConfirmDialog {
    fn layout(&self, width: u16, height: u16) -> ConfirmLayout {
        let w = width as usize;
        let ta_visible = self.editing || !self.textarea.is_empty();
        let (selected_label, _) = &self.options[self.selected];
        let digits = format!("{}", self.selected + 1).len();
        let text_indent = (2 + digits + 2 + selected_label.len() + 2) as u16;
        let wrap_w = width.saturating_sub(text_indent) as usize;
        let ta_extra: u16 = if ta_visible {
            self.textarea.visual_row_count(wrap_w).saturating_sub(1)
        } else {
            0
        };

        let prefix_len = 1 + self.tool_name.len() + 2;
        let title_rows = wrap_line(&self.desc, w.saturating_sub(prefix_len)).len() as u16;
        let summary_rows: u16 = self
            .summary
            .as_ref()
            .map(|s| wrap_line(s, w.saturating_sub(1)).len() as u16)
            .unwrap_or(0);
        let has_preview = self.total_preview > 0;
        // bar + title + summary + separators(if preview) +
        // "Allow?" + options + ta_extra + blank + hint
        let fixed_rows: u16 = 1
            + title_rows
            + summary_rows
            + if has_preview { 2 } else { 1 }
            + 1
            + self.options.len() as u16
            + ta_extra
            + 2;

        let viewport_rows: u16 = if has_preview {
            let space = height.saturating_sub(fixed_rows);
            space.max(1).min(self.total_preview)
        } else {
            0
        };

        ConfirmLayout {
            title_rows,
            summary_rows,
            has_preview,
            viewport_rows,
            total_rows: fixed_rows + viewport_rows,
        }
    }
}

impl super::Dialog for ConfirmDialog {
    fn blocks_agent(&self) -> bool {
        true
    }

    fn height(&self) -> u16 {
        let (width, height) = terminal::size().unwrap_or((80, 24));
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

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<DialogResult> {
        self.dirty = true;
        if self.editing {
            match (code, modifiers) {
                (KeyCode::Enter, _) => {
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
                (KeyCode::Esc, _) => {
                    self.editing = false;
                }
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                    if self.textarea.is_empty() {
                        return Some(DialogResult::Confirm {
                            choice: ConfirmChoice::No,
                            message: None,
                            tool_name: self.tool_name.clone(),
                            request_id: self.request_id,
                        });
                    }
                    self.textarea.clear();
                    self.editing = false;
                }
                _ => {
                    self.textarea.handle_key(code, modifiers);
                }
            }
            return None;
        }

        match (code, modifiers) {
            (KeyCode::Enter, _) => {
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
            (KeyCode::Tab, _) => {
                self.editing = true;
            }
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                return Some(DialogResult::Confirm {
                    choice: ConfirmChoice::No,
                    message: None,
                    tool_name: self.tool_name.clone(),
                    request_id: self.request_id,
                });
            }
            (KeyCode::Esc, _) => {
                return Some(DialogResult::Confirm {
                    choice: ConfirmChoice::No,
                    message: None,
                    tool_name: self.tool_name.clone(),
                    request_id: self.request_id,
                });
            }
            // Preview scrolling
            (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                if self.total_preview > 0 {
                    let half = 10usize;
                    self.preview_scroll =
                        (self.preview_scroll + half).min(self.total_preview as usize);
                }
            }
            (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.preview_scroll = self.preview_scroll.saturating_sub(10);
            }
            (KeyCode::PageDown, _) => {
                if self.total_preview > 0 {
                    self.preview_scroll =
                        (self.preview_scroll + 20).min(self.total_preview as usize);
                }
            }
            (KeyCode::PageUp, _) => {
                self.preview_scroll = self.preview_scroll.saturating_sub(20);
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                self.selected = if self.selected == 0 {
                    self.options.len() - 1
                } else {
                    self.selected - 1
                };
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                self.selected = (self.selected + 1) % self.options.len();
            }
            _ => {}
        }
        None
    }

    fn draw(&mut self, start_row: u16) {
        if !self.dirty {
            return;
        }
        self.dirty = false;

        let mut out = RenderOut::scroll();
        let (width, height) = terminal::size().unwrap_or((80, 24));
        let w = width as usize;

        let is_first_draw = self.anchor_row.is_none();

        engine::log::entry(
            engine::log::Level::Debug,
            &format!("ConfirmDialog::draw start_row={start_row} height={height} first={is_first_draw} anchor={:?}", self.anchor_row),
            &"",
        );

        let ly = self.layout(width, height);
        let ta_visible = self.editing || !self.textarea.is_empty();

        // Clamp scroll
        let max_scroll = (self.total_preview as usize).saturating_sub(ly.viewport_rows as usize);
        self.preview_scroll = self.preview_scroll.min(max_scroll);

        let (bar_row, _) = begin_dialog_draw(
            &mut out,
            start_row,
            ly.total_rows,
            height,
            None,
            &mut self.anchor_row,
        );

        engine::log::entry(
            engine::log::Level::Debug,
            &format!(
                "ConfirmDialog: bar_row={bar_row} total_rows={} viewport={} preview={}",
                ly.total_rows, ly.viewport_rows, self.total_preview
            ),
            &"",
        );

        // Where the options section should begin in the current layout.
        let expected_options_row = bar_row
            + 1
            + ly.title_rows
            + ly.summary_rows
            + if ly.has_preview {
                2 + ly.viewport_rows
            } else {
                1
            }
            + 1;

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

            draw_bar(&mut out, w, None, None, theme::accent());
            crlf(&mut out);
            row += 1;

            // title -- wrap long commands with a leading space on continuation lines
            let prefix_len = 1 + self.tool_name.len() + 2; // " tool: "
            let segments = wrap_line(&self.desc, w.saturating_sub(prefix_len));
            for (i, seg) in segments.iter().enumerate() {
                if i == 0 {
                    let _ = out.queue(Print(" "));
                    let _ = out.queue(SetForegroundColor(theme::accent()));
                    let _ = out.queue(Print(&self.tool_name));
                    let _ = out.queue(ResetColor);
                    let _ = out.queue(Print(format!(": {seg}")));
                } else {
                    let _ = out.queue(Print(format!(" {seg}")));
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
                    let _ = out.queue(SetForegroundColor(theme::MUTED));
                    let _ = out.queue(Print(seg));
                    let _ = out.queue(ResetColor);
                    crlf(&mut out);
                    row += 1;
                }
            }

            if ly.has_preview {
                let separator: String = "\u{254c}".repeat(w);
                // Top separator -- show scroll position when clipped
                let _ = out.queue(SetForegroundColor(theme::BAR));
                let _ = out.queue(Print(&separator));
                let _ = out.queue(ResetColor);
                crlf(&mut out);
                row += 1;
                render_confirm_preview(
                    &mut out,
                    &self.tool_name,
                    &self.args,
                    self.preview_scroll as u16,
                    ly.viewport_rows,
                );
                row += ly.viewport_rows;
                // Bottom separator -- show scroll indicator when content is clipped
                let _ = out.queue(SetForegroundColor(theme::BAR));
                if self.total_preview > ly.viewport_rows {
                    let pos = format!(
                        " [{}/{}]",
                        self.preview_scroll + ly.viewport_rows as usize,
                        self.total_preview
                    );
                    let sep_len = w.saturating_sub(pos.len());
                    let _ = out.queue(Print("\u{254c}".repeat(sep_len)));
                    let _ = out.queue(SetForegroundColor(theme::MUTED));
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
            // "Allow?"
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let _ = out.queue(Print(" Allow?"));
            let _ = out.queue(SetAttribute(Attribute::Reset));
            crlf(&mut out);
            row += 1;
        }

        self.options_row = row;

        let mut cursor_pos: Option<(u16, u16)> = None;

        for (i, (label, _)) in self.options.iter().enumerate() {
            let _ = out.queue(Print("  "));
            let highlighted = i == self.selected;
            if highlighted {
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(format!("{}.", i + 1)));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let _ = out.queue(Print(" "));
                let _ = out.queue(SetForegroundColor(theme::accent()));
                let _ = out.queue(Print(label));
                let _ = out.queue(ResetColor);
            } else {
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(format!("{}. ", i + 1)));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let _ = out.queue(Print(label));
            }

            if i == self.selected && ta_visible {
                let digits = format!("{}", i + 1).len();
                let text_col = (2 + digits + 2 + label.len() + 2) as u16;
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
        if self.editing {
            let _ = out.queue(Print(" enter: send  esc: cancel"));
        } else if !self.textarea.is_empty() {
            let _ = out.queue(Print(" enter: confirm with message  tab: edit"));
        } else {
            let _ = out.queue(Print(" enter: confirm  tab: add message  esc: cancel"));
        }
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
