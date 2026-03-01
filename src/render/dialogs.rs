use crate::session;
use crate::theme;
use crossterm::{
    cursor,
    event::{DisableBracketedPaste, EnableBracketedPaste},
    style::{Attribute, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal, QueueableCommand,
};
use std::collections::HashMap;
use std::io::{self, Write};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

use super::highlight::{count_inline_diff_rows, print_inline_diff, print_syntax_file};
use super::{chunk_line, crlf, draw_bar, ConfirmChoice, ResumeEntry};

// ── TextArea ──────────────────────────────────────────────────────────────────

/// Multi-line text editor used in dialog overlays.
struct TextArea {
    pub lines: Vec<String>,
    pub row: usize,
    pub col: usize, // character index (not byte)
}

impl TextArea {
    fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Total visual rows when wrapping at the given width.
    fn visual_row_count(&self, wrap_w: usize) -> u16 {
        self.lines
            .iter()
            .map(|l| chunk_line(l, wrap_w).len() as u16)
            .sum()
    }

    /// Wrap content into visual lines and compute cursor position.
    fn wrap(&self, wrap_w: usize) -> (Vec<String>, (usize, usize)) {
        let mut visual = Vec::new();
        let mut cursor = (0, 0);

        for (li, line) in self.lines.iter().enumerate() {
            let vis_start = visual.len();
            let chunks = chunk_line(line, wrap_w);
            visual.extend(chunks);

            if li == self.row {
                let char_count = line.chars().count();
                let col = self.col.min(char_count);
                if char_count == 0 || wrap_w == 0 {
                    cursor = (vis_start, col);
                } else {
                    let vis_offset = col / wrap_w;
                    let vis_col = col % wrap_w;
                    let num_vis = visual.len() - vis_start;
                    if vis_offset >= num_vis {
                        cursor = (
                            vis_start + num_vis - 1,
                            visual[vis_start + num_vis - 1].chars().count(),
                        );
                    } else {
                        cursor = (vis_start + vis_offset, vis_col);
                    }
                }
            }
        }

        (visual, cursor)
    }

    fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
    }

    fn insert_char(&mut self, c: char) {
        let byte = char_to_byte(&self.lines[self.row], self.col);
        self.lines[self.row].insert(byte, c);
        self.col += 1;
    }

    fn insert_newline(&mut self) {
        let byte = char_to_byte(&self.lines[self.row], self.col);
        let rest = self.lines[self.row][byte..].to_string();
        self.lines[self.row].truncate(byte);
        self.row += 1;
        self.col = 0;
        self.lines.insert(self.row, rest);
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            self.col -= 1;
            let byte = char_to_byte(&self.lines[self.row], self.col);
            self.lines[self.row].remove(byte);
        } else if self.row > 0 {
            let removed = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
            self.lines[self.row].push_str(&removed);
        }
    }

    fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
        }
    }

    fn move_right(&mut self) {
        let len = self.lines[self.row].chars().count();
        if self.col < len {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.lines[self.row].chars().count());
        }
    }

    fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.lines[self.row].chars().count());
        }
    }

    fn move_home(&mut self) {
        self.col = 0;
    }

    fn move_end(&mut self) {
        self.col = self.lines[self.row].chars().count();
    }

    /// Handle a key event. Returns `true` if the event was consumed.
    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        match (code, modifiers) {
            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => self.insert_char(c),
            (KeyCode::Enter, _) => self.insert_newline(),
            (KeyCode::Backspace, _) => self.backspace(),
            (KeyCode::Left, _) => self.move_left(),
            (KeyCode::Right, _) => self.move_right(),
            (KeyCode::Up, _) => self.move_up(),
            (KeyCode::Down, _) => self.move_down(),
            (KeyCode::Home, _) => self.move_home(),
            (KeyCode::End, _) => self.move_end(),
            _ => return false,
        }
        true
    }
}

fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// Render an inline textarea after an option label, tracking cursor position.
///
/// Prints `, <text>` on the first visual line and pads subsequent wrapped lines
/// to `text_col`. Returns the updated row and optional cursor (col, row) if
/// `editing` is true.
fn render_inline_textarea(
    out: &mut io::Stdout,
    ta: &TextArea,
    editing: bool,
    text_col: u16,
    wrap_w: usize,
    mut row: u16,
) -> (u16, Option<(u16, u16)>) {
    let (vis_lines, vis_cursor) = ta.wrap(wrap_w);
    let pad: String = " ".repeat(text_col as usize);
    let mut cursor_pos = None;
    for (vi, vl) in vis_lines.iter().enumerate() {
        if vi == 0 {
            let _ = out.queue(Print(", "));
        } else {
            let _ = out.queue(Print(&pad));
        }
        let _ = out.queue(Print(vl));
        if editing && vi == vis_cursor.0 {
            cursor_pos = Some((text_col + vis_cursor.1 as u16, row));
        }
        crlf(out);
        row += 1;
    }
    (row, cursor_pos)
}

/// Finish a dialog frame: optionally show cursor, end synchronized update, flush.
fn finish_dialog_frame(out: &mut io::Stdout, cursor_pos: Option<(u16, u16)>, editing: bool) {
    if editing {
        if let Some((col, r)) = cursor_pos {
            let _ = out.queue(cursor::MoveTo(col, r));
        }
        let _ = out.queue(cursor::Show);
    }
    let _ = out.queue(terminal::EndSynchronizedUpdate);
    let _ = out.flush();
}

/// Clear a dialog area and restore the cursor.
fn dialog_cleanup(last_bar_row: u16) {
    let mut out = io::stdout();
    let (_, height) = terminal::size().unwrap_or((80, 24));
    let clear_from = last_bar_row.min(height.saturating_sub(1));
    let _ = out.queue(cursor::MoveTo(0, clear_from));
    let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
    let _ = out.queue(cursor::Show);
    let _ = out.flush();
}

// ── Dialog types ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Clone)]
pub struct Question {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
}

/// Parse questions from tool call args JSON.
pub fn parse_questions(args: &HashMap<String, serde_json::Value>) -> Vec<Question> {
    let Some(qs) = args.get("questions").and_then(|v| v.as_array()) else {
        return vec![];
    };
    qs.iter()
        .filter_map(|q| {
            let question = q.get("question")?.as_str()?.to_string();
            let header = q.get("header")?.as_str()?.to_string();
            let multi_select = q
                .get("multiSelect")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let options = q
                .get("options")?
                .as_array()?
                .iter()
                .filter_map(|o| {
                    Some(QuestionOption {
                        label: o.get("label")?.as_str()?.to_string(),
                        description: o.get("description")?.as_str()?.to_string(),
                    })
                })
                .collect();
            Some(Question {
                question,
                header,
                options,
                multi_select,
            })
        })
        .collect()
}

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
fn render_confirm_preview(
    out: &mut io::Stdout,
    tool_name: &str,
    args: &HashMap<String, serde_json::Value>,
    max_rows: u16,
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
            print_inline_diff(out, old, new, path, old, max_rows);
        }
        "write_file" => {
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            print_syntax_file(out, content, path, max_rows);
        }
        _ => {}
    }
}

/// Non-blocking confirm dialog state machine.
pub struct ConfirmDialog {
    tool_name: String,
    desc: String,
    args: HashMap<String, serde_json::Value>,
    options: Vec<(String, ConfirmChoice)>,
    total_preview: u16,
    selected: usize,
    textarea: TextArea,
    editing: bool,
    dirty: bool,
    last_bar_row: u16,
    /// Row where the options section begins (used for partial redraws).
    options_row: u16,
    /// Total dialog rows from the previous frame (used to detect height changes).
    last_total_rows: u16,
}

impl ConfirmDialog {
    pub fn new(
        tool_name: &str,
        desc: &str,
        args: &HashMap<String, serde_json::Value>,
        approval_pattern: Option<&str>,
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
            args: args.clone(),
            options,
            total_preview,
            selected: 0,
            textarea: TextArea::new(),
            editing: false,
            last_bar_row: u16::MAX,
            options_row: 0,
            last_total_rows: 0,
            dirty: true,
        }
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Process a key event. Returns `Some((choice, optional_message))` when done.
    pub fn handle_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<(ConfirmChoice, Option<String>)> {
        self.dirty = true;
        if self.editing {
            match (code, modifiers) {
                (KeyCode::Esc, _) => {
                    self.editing = false;
                }
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                    if self.textarea.is_empty() {
                        return Some((ConfirmChoice::No, None));
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
                return Some((self.options[self.selected].1.clone(), msg));
            }
            (KeyCode::Tab, _) => {
                self.editing = true;
            }
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                return Some((ConfirmChoice::No, None));
            }
            (KeyCode::Esc, _) => return Some((ConfirmChoice::No, None)),
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

    /// Render the dialog overlay at the bottom of the terminal.
    pub fn draw(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;

        let mut out = io::stdout();
        let _ = out.queue(terminal::BeginSynchronizedUpdate);
        let _ = out.queue(cursor::Hide);
        let (width, height) = terminal::size().unwrap_or((80, 24));
        let w = width as usize;

        let ta_visible = self.editing || !self.textarea.is_empty();
        // Pre-compute text indent for the selected option to get wrap width
        let (selected_label, _) = &self.options[self.selected];
        let digits = format!("{}", self.selected + 1).len();
        let text_indent = (2 + digits + 2 + selected_label.len() + 2) as u16;
        let wrap_w = width.saturating_sub(text_indent) as usize;
        let ta_extra: u16 = if ta_visible {
            self.textarea.visual_row_count(wrap_w).saturating_sub(1)
        } else {
            0
        };

        let base_rows: u16 = 5 + self.options.len() as u16 + ta_extra;

        let max_preview = height.saturating_sub(base_rows + 5);
        let preview_rows = self.total_preview.min(max_preview);
        let has_preview = preview_rows > 0;
        let preview_extra = if has_preview {
            // top separator + content + bottom separator
            preview_rows + 2
        } else {
            0
        };

        let total_rows = base_rows + preview_extra;
        let bar_row = height.saturating_sub(total_rows);

        // Partial redraw: when editing and the dialog height hasn't changed,
        // skip re-rendering the static top portion (bar, title, preview, header)
        // and only redraw from the options row down.
        let partial = self.editing
            && self.last_total_rows == total_rows
            && self.options_row > 0
            && self.options_row >= bar_row;

        let mut row;
        if partial {
            row = self.options_row;
            let _ = out.queue(cursor::MoveTo(0, row));
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
        } else {
            let clear_from = bar_row.min(self.last_bar_row);
            self.last_bar_row = bar_row;

            let _ = out.queue(cursor::MoveTo(0, clear_from));
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
            let _ = out.queue(cursor::MoveTo(0, bar_row));

            row = bar_row;

            draw_bar(&mut out, w, None, None, theme::ACCENT);
            crlf(&mut out);
            row += 1;

            // title
            let _ = out.queue(Print(" "));
            let _ = out.queue(SetForegroundColor(theme::ACCENT));
            let _ = out.queue(Print(&self.tool_name));
            let _ = out.queue(ResetColor);
            let _ = out.queue(Print(format!(": {}", self.desc)));
            crlf(&mut out);
            row += 1;

            if has_preview {
                let separator: String = "╌".repeat(w);
                let _ = out.queue(SetForegroundColor(theme::BAR));
                let _ = out.queue(Print(&separator));
                let _ = out.queue(ResetColor);
                crlf(&mut out);
                row += 1;
                render_confirm_preview(&mut out, &self.tool_name, &self.args, max_preview);
                row += preview_rows;
                let _ = out.queue(SetForegroundColor(theme::BAR));
                let _ = out.queue(Print(&separator));
                let _ = out.queue(SetAttribute(Attribute::Reset));
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
        self.last_total_rows = total_rows;

        let mut cursor_pos: Option<(u16, u16)> = None;

        for (i, (label, _)) in self.options.iter().enumerate() {
            let _ = out.queue(Print("  "));
            let highlighted = i == self.selected;
            if highlighted {
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(format!("{}.", i + 1)));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let _ = out.queue(Print(" "));
                let _ = out.queue(SetForegroundColor(theme::ACCENT));
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

        // footer
        crlf(&mut out);
        let _ = out.queue(SetAttribute(Attribute::Dim));
        if self.editing {
            let _ = out.queue(Print(" esc: done  enter: newline"));
        } else if !self.textarea.is_empty() {
            let _ = out.queue(Print(" enter: confirm with message  tab: edit"));
        }
        let _ = out.queue(SetAttribute(Attribute::Reset));
        let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));

        finish_dialog_frame(&mut out, cursor_pos, self.editing);
    }

    /// Clear the dialog area and restore cursor.
    pub fn cleanup(&self) {
        dialog_cleanup(self.last_bar_row);
    }
}

/// Show a rewind menu listing user turns. Returns the block index to rewind to.
pub fn show_rewind(turns: &[(usize, String)]) -> Option<usize> {
    if turns.is_empty() {
        return None;
    }

    let mut out = io::stdout();
    let (_, height) = terminal::size().unwrap_or((80, 24));

    let mut max_visible = (height as usize).saturating_sub(6).min(turns.len());
    let mut total_rows = (max_visible + 5) as u16;
    let mut bar_row = height.saturating_sub(total_rows);
    let mut last_bar_row = bar_row;
    let mut selected: usize = turns.len() - 1;
    let mut scroll_offset: usize = turns.len().saturating_sub(max_visible);

    let _ = out.flush();
    let _ = out.queue(cursor::Hide);
    let _ = out.flush();

    let draw = |bar_row: u16,
                last_bar_row: u16,
                max_visible: usize,
                selected: usize,
                scroll_offset: usize| {
        let mut out = io::stdout();
        let (width, _) = terminal::size().unwrap_or((80, 24));
        let w = width as usize;
        let _ = out.queue(terminal::BeginSynchronizedUpdate);
        let clear_from = bar_row.min(last_bar_row);
        let _ = out.queue(cursor::MoveTo(0, clear_from));
        let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
        let _ = out.queue(cursor::MoveTo(0, bar_row));

        let mut row = bar_row;

        let _ = out.queue(cursor::MoveTo(0, row));
        let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
        draw_bar(&mut out, w, None, None, theme::ACCENT);
        row = row.saturating_add(1);

        let _ = out.queue(cursor::MoveTo(0, row));
        let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(Print(" Rewind to:"));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        row = row.saturating_add(1);

        let end = (scroll_offset + max_visible).min(turns.len());
        for (i, (_, ref full_text)) in turns.iter().enumerate().take(end).skip(scroll_offset) {
            let label = full_text.lines().next().unwrap_or("");
            let num = i + 1;
            let max_label = w.saturating_sub(8);
            let truncated = if label.chars().count() > max_label {
                format!(
                    "{}…",
                    &label[..label
                        .char_indices()
                        .nth(max_label)
                        .map(|(j, _)| j)
                        .unwrap_or(label.len())]
                )
            } else {
                label.to_string()
            };

            let _ = out.queue(cursor::MoveTo(0, row));
            let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
            let _ = out.queue(Print("  "));
            if i == selected {
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(format!("{}.", num)));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let _ = out.queue(Print(" "));
                let _ = out.queue(SetForegroundColor(theme::ACCENT));
                let _ = out.queue(Print(&truncated));
                let _ = out.queue(ResetColor);
            } else {
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(format!("{}. ", num)));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let _ = out.queue(Print(&truncated));
            }
            row = row.saturating_add(1);
        }

        for _ in 0..3 {
            let _ = out.queue(cursor::MoveTo(0, row));
            let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
            row = row.saturating_add(1);
        }

        let _ = out.queue(terminal::EndSynchronizedUpdate);
        let _ = out.flush();
    };

    draw(bar_row, last_bar_row, max_visible, selected, scroll_offset);
    last_bar_row = bar_row;

    let result = loop {
        match event::read() {
            Ok(Event::Resize(_, h)) => {
                max_visible = (h as usize).saturating_sub(6).min(turns.len());
                total_rows = (max_visible + 5) as u16;
                bar_row = h.saturating_sub(total_rows);
                scroll_offset = scroll_offset.min(turns.len().saturating_sub(max_visible));
                draw(bar_row, last_bar_row, max_visible, selected, scroll_offset);
                last_bar_row = bar_row;
            }
            Ok(Event::Key(KeyEvent {
                code, modifiers, ..
            })) => match (code, modifiers) {
                (KeyCode::Enter, _) => break Some(turns[selected].0),
                (KeyCode::Esc, _) => break None,
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => break None,
                (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                    if selected > 0 {
                        selected -= 1;
                        if selected < scroll_offset {
                            scroll_offset = selected;
                        }
                        draw(bar_row, last_bar_row, max_visible, selected, scroll_offset);
                    }
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                    if selected + 1 < turns.len() {
                        selected += 1;
                        if selected >= scroll_offset + max_visible {
                            scroll_offset = selected + 1 - max_visible;
                        }
                        draw(bar_row, last_bar_row, max_visible, selected, scroll_offset);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    };

    dialog_cleanup(last_bar_row);

    result
}

/// Show a resume menu listing saved sessions. Returns the selected session id.
pub fn show_resume(entries: &[ResumeEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }

    let mut out = io::stdout();
    let (_, height) = terminal::size().unwrap_or((80, 24));
    let mut max_visible = (height as usize)
        .saturating_sub(7)
        .min(entries.len().max(1));
    let mut total_rows = (max_visible + 6) as u16;
    let mut bar_row = height.saturating_sub(total_rows);

    let mut query = String::new();
    let mut filtered = filter_resume_entries(entries, &query);
    let mut selected: usize = 0;
    let mut scroll_offset: usize = 0;

    let _ = out.flush();
    let _ = out.queue(cursor::Hide);
    let _ = out.flush();

    let draw = |bar_row: u16,
                max_visible: usize,
                selected: usize,
                scroll_offset: usize,
                query: &str,
                filtered: &[ResumeEntry]| {
        let mut out = io::stdout();
        let (width, _) = terminal::size().unwrap_or((80, 24));
        let w = width as usize;
        let _ = out.queue(terminal::BeginSynchronizedUpdate);
        let _ = out.queue(cursor::MoveTo(0, 0));
        let _ = out.queue(terminal::Clear(terminal::ClearType::All));
        let _ = out.queue(cursor::MoveTo(0, bar_row));

        let mut row = bar_row;
        let now_ms = session::now_ms();

        let _ = out.queue(cursor::MoveTo(0, row));
        let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
        draw_bar(&mut out, w, None, None, theme::ACCENT);
        row = row.saturating_add(1);

        let _ = out.queue(cursor::MoveTo(0, row));
        let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(Print(" Resume:"));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        let _ = out.queue(Print(" "));
        let _ = out.queue(Print(query));
        row = row.saturating_add(1);

        if filtered.is_empty() {
            let _ = out.queue(cursor::MoveTo(0, row));
            let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let _ = out.queue(Print("  No matches"));
            let _ = out.queue(SetAttribute(Attribute::Reset));
            row = row.saturating_add(1);
        } else {
            let end = (scroll_offset + max_visible).min(filtered.len());
            let num_width = end.to_string().len();
            for (i, entry) in filtered.iter().enumerate().take(end).skip(scroll_offset) {
                let title = resume_title(entry);
                let time_ago = session::time_ago(resume_ts(entry), now_ms);
                let time_len = time_ago.chars().count() + 1;
                let prefix_len = 2 + num_width + 2;
                let max_label = w.saturating_sub(time_len + prefix_len);
                let truncated = truncate_str_local(&title, max_label);
                let num_str = format!("{:>width$}. ", i + 1, width = num_width);

                let _ = out.queue(cursor::MoveTo(0, row));
                let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
                let _ = out.queue(Print("  "));
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(&num_str));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                if i == selected {
                    let _ = out.queue(SetForegroundColor(theme::ACCENT));
                    let _ = out.queue(Print(&truncated));
                    let _ = out.queue(ResetColor);
                } else {
                    let _ = out.queue(Print(&truncated));
                }

                let _ = out.queue(Print(" "));
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(&time_ago));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                row = row.saturating_add(1);
            }
        }

        for _ in 0..3 {
            let _ = out.queue(cursor::MoveTo(0, row));
            let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
            row = row.saturating_add(1);
        }

        let _ = out.queue(terminal::EndSynchronizedUpdate);
        let _ = out.flush();
    };

    draw(
        bar_row,
        max_visible,
        selected,
        scroll_offset,
        &query,
        &filtered,
    );

    terminal::enable_raw_mode().ok();
    let _ = out.queue(EnableBracketedPaste);
    let _ = out.flush();

    let result = loop {
        let has_event = event::poll(Duration::from_millis(500)).unwrap_or(false);
        if has_event {
            match event::read() {
                Ok(Event::Resize(_, h)) => {
                    max_visible = (h as usize).saturating_sub(7).min(entries.len().max(1));
                    total_rows = (max_visible + 6) as u16;
                    bar_row = h.saturating_sub(total_rows);
                    if filtered.is_empty() {
                        selected = 0;
                        scroll_offset = 0;
                    } else {
                        selected = selected.min(filtered.len().saturating_sub(1));
                        scroll_offset =
                            scroll_offset.min(filtered.len().saturating_sub(max_visible));
                    }
                }
                Ok(Event::Key(KeyEvent {
                    code, modifiers, ..
                })) => {
                    match (code, modifiers) {
                        (KeyCode::Enter, _) => {
                            if let Some(entry) = filtered.get(selected) {
                                break Some(entry.id.clone());
                            }
                        }
                        (KeyCode::Esc, _) => break None,
                        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => break None,
                        (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
                            query.clear();
                        }
                        (KeyCode::Backspace, _) => {
                            query.pop();
                        }
                        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                            if selected > 0 {
                                selected -= 1;
                                if selected < scroll_offset {
                                    scroll_offset = selected;
                                }
                            }
                        }
                        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                            if selected + 1 < filtered.len() {
                                selected += 1;
                                if selected >= scroll_offset + max_visible {
                                    scroll_offset = selected + 1 - max_visible;
                                }
                            }
                        }
                        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                            query.push(c);
                        }
                        _ => {}
                    }

                    filtered = filter_resume_entries(entries, &query);
                    if filtered.is_empty() {
                        selected = 0;
                        scroll_offset = 0;
                    } else {
                        selected = selected.min(filtered.len().saturating_sub(1));
                        scroll_offset =
                            scroll_offset.min(filtered.len().saturating_sub(max_visible));
                    }
                }
                _ => {}
            }
        }
        draw(
            bar_row,
            max_visible,
            selected,
            scroll_offset,
            &query,
            &filtered,
        );
    };

    let _ = out.queue(cursor::MoveTo(0, bar_row));
    let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
    let _ = out.queue(cursor::Show);
    let _ = out.queue(DisableBracketedPaste);
    let _ = out.flush();
    terminal::disable_raw_mode().ok();

    result
}

/// Non-blocking question dialog state machine.
pub struct QuestionDialog {
    questions: Vec<Question>,
    has_tabs: bool,
    max_options: usize,
    active_tab: usize,
    selections: Vec<usize>,
    multi_toggles: Vec<Vec<bool>>,
    other_areas: Vec<TextArea>,
    editing_other: Vec<bool>,
    visited: Vec<bool>,
    answered: Vec<bool>,
    dirty: bool,
    last_bar_row: u16,
}

impl QuestionDialog {
    pub fn new(questions: Vec<Question>) -> Self {
        let n = questions.len();
        let max_options = questions.iter().map(|q| q.options.len()).max().unwrap_or(0) + 1;
        let has_tabs = n > 1;
        Self {
            multi_toggles: questions
                .iter()
                .map(|q| vec![false; q.options.len() + 1])
                .collect(),
            questions,
            has_tabs,
            max_options,
            active_tab: 0,
            selections: vec![0; n],
            other_areas: (0..n).map(|_| TextArea::new()).collect(),
            editing_other: vec![false; n],
            visited: vec![false; n],
            answered: vec![false; n],
            dirty: true,
            last_bar_row: u16::MAX,
        }
    }

    /// Process a key event. Returns `Some(answer_json)` on confirm, `None` to keep going.
    /// Returns `Some(None)` on cancel (Esc/Ctrl+C).
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<Option<String>> {
        self.dirty = true;
        let q = &self.questions[self.active_tab];
        let other_idx = q.options.len();

        if self.editing_other[self.active_tab] {
            match (code, modifiers) {
                (KeyCode::Esc, _) => {
                    self.editing_other[self.active_tab] = false;
                }
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                    if self.other_areas[self.active_tab].is_empty() {
                        return Some(None); // cancel
                    }
                    self.other_areas[self.active_tab].clear();
                    self.editing_other[self.active_tab] = false;
                    if q.multi_select {
                        self.multi_toggles[self.active_tab][other_idx] = false;
                    }
                }
                _ => {
                    self.other_areas[self.active_tab].handle_key(code, modifiers);
                }
            }
            return None;
        }

        match (code, modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                return Some(None); // cancel
            }
            (KeyCode::Enter, _) => {
                self.answered[self.active_tab] = true;
                if let Some(next) = (0..self.questions.len()).find(|&i| !self.answered[i]) {
                    self.active_tab = next;
                } else {
                    return Some(Some(self.build_answer()));
                }
            }
            (KeyCode::Tab, _) => {
                if self.selections[self.active_tab] == other_idx {
                    self.editing_other[self.active_tab] = true;
                    if q.multi_select {
                        self.multi_toggles[self.active_tab][other_idx] = true;
                    }
                }
            }
            (KeyCode::Right, _) | (KeyCode::Char('l'), _) => {
                if self.has_tabs {
                    self.visited[self.active_tab] = true;
                    self.active_tab = (self.active_tab + 1) % self.questions.len();
                }
            }
            (KeyCode::BackTab, _) | (KeyCode::Left, _) | (KeyCode::Char('h'), _) => {
                if self.has_tabs {
                    self.visited[self.active_tab] = true;
                    self.active_tab = if self.active_tab == 0 {
                        self.questions.len() - 1
                    } else {
                        self.active_tab - 1
                    };
                }
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                self.selections[self.active_tab] = if self.selections[self.active_tab] == 0 {
                    other_idx
                } else {
                    self.selections[self.active_tab] - 1
                };
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                self.selections[self.active_tab] =
                    (self.selections[self.active_tab] + 1) % (other_idx + 1);
            }
            (KeyCode::Char(' '), _) if q.multi_select => {
                let idx = self.selections[self.active_tab];
                if idx == other_idx && self.other_areas[self.active_tab].is_empty() {
                    self.editing_other[self.active_tab] = true;
                } else {
                    self.multi_toggles[self.active_tab][idx] =
                        !self.multi_toggles[self.active_tab][idx];
                }
            }
            (KeyCode::Char(c), _) if c.is_ascii_digit() => {
                let num = c.to_digit(10).unwrap_or(0) as usize;
                if num >= 1 && num <= other_idx + 1 {
                    if q.multi_select {
                        self.multi_toggles[self.active_tab][num - 1] =
                            !self.multi_toggles[self.active_tab][num - 1];
                    } else {
                        self.selections[self.active_tab] = num - 1;
                    }
                }
            }
            _ => {}
        }
        None
    }

    /// Render the dialog overlay at the bottom of the terminal.
    pub fn draw(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;

        let mut out = io::stdout();
        let _ = out.queue(terminal::BeginSynchronizedUpdate);
        let _ = out.queue(cursor::Hide);
        let (width, height) = terminal::size().unwrap_or((80, 24));
        let w = width as usize;

        let ta = &self.other_areas[self.active_tab];
        let ta_visible = self.editing_other[self.active_tab] || !ta.is_empty();
        let q_other_idx = self.questions[self.active_tab].options.len();
        let other_text_col: u16 = if self.questions[self.active_tab].multi_select {
            2 + 2 + 5 + 2
        } else {
            let digits = format!("{}", q_other_idx + 1).len();
            (2 + digits + 2 + 5 + 2) as u16
        };
        let other_wrap_w = width.saturating_sub(other_text_col) as usize;
        let ta_extra: u16 = if ta_visible {
            ta.visual_row_count(other_wrap_w).saturating_sub(1)
        } else {
            0
        };

        let fixed_rows = 1 + (self.has_tabs as u16) + 3 + self.max_options as u16 + 2 + ta_extra;
        let bar_row = height.saturating_sub(fixed_rows);
        let clear_from = bar_row.min(self.last_bar_row);
        self.last_bar_row = bar_row;

        let _ = out.queue(cursor::MoveTo(0, clear_from));
        let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
        let _ = out.queue(cursor::MoveTo(0, bar_row));

        let mut row = bar_row;

        draw_bar(&mut out, w, None, None, theme::ACCENT);
        crlf(&mut out);
        row += 1;

        if self.has_tabs {
            let _ = out.queue(Print(" "));
            for (i, q) in self.questions.iter().enumerate() {
                let bullet = if self.answered[i] || self.visited[i] {
                    "■"
                } else {
                    "□"
                };
                if i == self.active_tab {
                    let _ = out.queue(SetForegroundColor(theme::ACCENT));
                    let _ = out.queue(SetAttribute(Attribute::Bold));
                    let _ = out.queue(Print(format!(" {} {} ", bullet, q.header)));
                    let _ = out.queue(SetAttribute(Attribute::Reset));
                    let _ = out.queue(ResetColor);
                } else if self.answered[i] {
                    let _ = out.queue(SetForegroundColor(theme::SUCCESS));
                    let _ = out.queue(Print(format!(" {}", bullet)));
                    let _ = out.queue(ResetColor);
                    let _ = out.queue(SetAttribute(Attribute::Dim));
                    let _ = out.queue(Print(format!(" {} ", q.header)));
                    let _ = out.queue(SetAttribute(Attribute::Reset));
                } else {
                    let _ = out.queue(SetAttribute(Attribute::Dim));
                    let _ = out.queue(Print(format!(" {} {} ", bullet, q.header)));
                    let _ = out.queue(SetAttribute(Attribute::Reset));
                }
            }
            crlf(&mut out);
            row += 1;
        }

        let q = &self.questions[self.active_tab];
        let sel = self.selections[self.active_tab];
        let is_multi = q.multi_select;
        let other_idx = q.options.len();

        crlf(&mut out);
        row += 1;

        let _ = out.queue(Print(" "));
        let _ = out.queue(SetAttribute(Attribute::Bold));
        let _ = out.queue(Print(&q.question));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        if is_multi {
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let _ = out.queue(Print(" (space to toggle)"));
            let _ = out.queue(SetAttribute(Attribute::Reset));
        }
        crlf(&mut out);
        row += 1;

        crlf(&mut out);
        row += 1;

        for (i, opt) in q.options.iter().enumerate() {
            let _ = out.queue(Print("  "));
            let is_current = sel == i;
            let is_toggled = is_multi && self.multi_toggles[self.active_tab][i];

            if is_multi {
                let check = if is_toggled { "◉" } else { "○" };
                if is_current {
                    let _ = out.queue(SetForegroundColor(theme::ACCENT));
                    let _ = out.queue(Print(format!("{} ", check)));
                    let _ = out.queue(Print(&opt.label));
                    let _ = out.queue(ResetColor);
                } else {
                    let _ = out.queue(SetAttribute(Attribute::Dim));
                    let _ = out.queue(Print(format!("{} ", check)));
                    let _ = out.queue(SetAttribute(Attribute::Reset));
                    let _ = out.queue(Print(&opt.label));
                }
            } else {
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(format!("{}.", i + 1)));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let _ = out.queue(Print(" "));
                if is_current {
                    let _ = out.queue(SetForegroundColor(theme::ACCENT));
                    let _ = out.queue(Print(&opt.label));
                    let _ = out.queue(ResetColor);
                } else {
                    let _ = out.queue(Print(&opt.label));
                }
            }

            if is_current {
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(format!("  {}", opt.description)));
                let _ = out.queue(SetAttribute(Attribute::Reset));
            }
            crlf(&mut out);
            row += 1;
        }

        // "Other" option with inline textarea
        let _ = out.queue(Print("  "));
        let is_other_current = sel == other_idx;
        let is_other_toggled = is_multi && self.multi_toggles[self.active_tab][other_idx];

        if is_multi {
            let check = if is_other_toggled { "◉" } else { "○" };
            if is_other_current {
                let _ = out.queue(SetForegroundColor(theme::ACCENT));
                let _ = out.queue(Print(format!("{} Other", check)));
                let _ = out.queue(ResetColor);
            } else {
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(format!("{} ", check)));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let _ = out.queue(Print("Other"));
            }
        } else {
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let _ = out.queue(Print(format!("{}.", other_idx + 1)));
            let _ = out.queue(SetAttribute(Attribute::Reset));
            let _ = out.queue(Print(" "));
            if is_other_current {
                let _ = out.queue(SetForegroundColor(theme::ACCENT));
                let _ = out.queue(Print("Other"));
                let _ = out.queue(ResetColor);
            } else {
                let _ = out.queue(Print("Other"));
            }
        }

        let editing = self.editing_other[self.active_tab];
        let mut cursor_pos = None;
        if ta_visible {
            let (new_row, cpos) =
                render_inline_textarea(&mut out, ta, editing, other_text_col, other_wrap_w, row);
            row = new_row;
            cursor_pos = cpos;
        } else {
            crlf(&mut out);
        }
        let _ = row;

        // Footer
        crlf(&mut out);
        let _ = out.queue(SetAttribute(Attribute::Dim));
        if editing {
            let _ = out.queue(Print(" esc: done  enter: newline"));
        } else if self.has_tabs {
            let _ = out.queue(Print(" tab: next question  enter: confirm"));
        } else {
            let _ = out.queue(Print(" enter: confirm"));
        }
        let _ = out.queue(SetAttribute(Attribute::Reset));
        let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));

        finish_dialog_frame(&mut out, cursor_pos, editing);
    }

    /// Clear the dialog area and restore cursor.
    pub fn cleanup(&self) {
        dialog_cleanup(self.last_bar_row);
    }

    fn build_answer(&self) -> String {
        let mut answers = serde_json::Map::new();
        for (i, q) in self.questions.iter().enumerate() {
            let other_idx = q.options.len();
            let other_text = self.other_areas[i].text();
            let answer = if q.multi_select {
                let mut selected: Vec<String> = Vec::new();
                for (j, toggled) in self.multi_toggles[i].iter().enumerate() {
                    if *toggled {
                        if j == other_idx {
                            selected.push(format!("Other: {other_text}"));
                        } else {
                            selected.push(q.options[j].label.clone());
                        }
                    }
                }
                if selected.is_empty() {
                    if self.selections[i] == other_idx {
                        serde_json::Value::String(format!("Other: {other_text}"))
                    } else {
                        serde_json::Value::String(q.options[self.selections[i]].label.clone())
                    }
                } else {
                    serde_json::Value::Array(
                        selected
                            .into_iter()
                            .map(serde_json::Value::String)
                            .collect(),
                    )
                }
            } else if self.selections[i] == other_idx {
                serde_json::Value::String(format!("Other: {other_text}"))
            } else {
                serde_json::Value::String(q.options[self.selections[i]].label.clone())
            };
            answers.insert(q.question.clone(), answer);
        }
        serde_json::Value::Object(answers).to_string()
    }
}

fn is_junk_title(s: &str) -> bool {
    let t = s.trim();
    t.is_empty()
        || t.eq_ignore_ascii_case("untitled")
        || t.starts_with('/')
        || t.starts_with('\x00')
}

fn resume_title(entry: &ResumeEntry) -> String {
    if !is_junk_title(&entry.title) {
        return entry.title.clone();
    }
    if let Some(ref sub) = entry.subtitle {
        if !is_junk_title(sub) {
            return sub.clone();
        }
    }
    "Untitled".into()
}

fn resume_ts(entry: &ResumeEntry) -> u64 {
    if entry.updated_at_ms > 0 {
        entry.updated_at_ms
    } else {
        entry.created_at_ms
    }
}

fn filter_resume_entries(entries: &[ResumeEntry], query: &str) -> Vec<ResumeEntry> {
    if query.is_empty() {
        return entries.to_vec();
    }
    let q = query.to_lowercase();
    entries
        .iter()
        .filter(|e| {
            let mut hay = resume_title(e);
            if let Some(ref subtitle) = e.subtitle {
                hay.push(' ');
                hay.push_str(subtitle);
            }
            fuzzy_match(&hay, &q)
        })
        .cloned()
        .collect()
}

fn fuzzy_match(text: &str, query: &str) -> bool {
    let lower = text.to_lowercase();
    let mut hay = lower.chars().peekable();
    for qc in query.chars() {
        loop {
            match hay.next() {
                Some(pc) if pc == qc => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

fn truncate_str_local(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut truncated: String = s.chars().take(max.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}
