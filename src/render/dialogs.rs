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
use super::{draw_bar, ConfirmChoice, ResumeEntry};

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

    fn line_count(&self) -> u16 {
        self.lines.len() as u16
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
    text_indent: u16,
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

        let first_label = options.first().map(|(l, _)| l.as_str()).unwrap_or("yes");
        let text_indent = (2 + 1 + 2 + first_label.len() + 2) as u16;

        Self {
            tool_name: tool_name.to_string(),
            desc: desc.to_string(),
            args: args.clone(),
            options,
            total_preview,
            selected: 0,
            textarea: TextArea::new(),
            editing: false,
            text_indent,
        }
    }

    /// Process a key event. Returns `Some(choice)` when the dialog is done.
    pub fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<ConfirmChoice> {
        if self.editing {
            match (code, modifiers) {
                (KeyCode::Esc, _) => {
                    self.editing = false;
                }
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                    if self.textarea.is_empty() {
                        return Some(ConfirmChoice::No);
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
                if !self.textarea.is_empty() {
                    return Some(ConfirmChoice::YesWithMessage(self.textarea.text()));
                }
                return Some(self.options[self.selected].1.clone());
            }
            (KeyCode::Tab, _) => {
                self.editing = true;
            }
            (KeyCode::Char(c @ '1'..='9'), _) => {
                let idx = (c as usize) - ('1' as usize);
                if idx < self.options.len() {
                    return Some(self.options[idx].1.clone());
                }
            }
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                return Some(ConfirmChoice::No);
            }
            (KeyCode::Esc, _) => return Some(ConfirmChoice::No),
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
    pub fn draw(&self) {
        let mut out = io::stdout();
        let (width, height) = terminal::size().unwrap_or((80, 24));
        let w = width.saturating_sub(1) as usize;

        let ta_extra = if self.editing || !self.textarea.is_empty() {
            self.textarea.line_count().saturating_sub(1)
        } else {
            0
        };

        let base_rows: u16 = 6 + self.options.len() as u16 + ta_extra;

        let max_preview = height.saturating_sub(base_rows + 2);
        let preview_rows = self.total_preview.min(max_preview);
        let has_preview = preview_rows > 0;
        let preview_extra = if has_preview {
            preview_rows + u16::from(self.total_preview > max_preview) + 1
        } else {
            0
        };

        let total_rows = base_rows + preview_extra;
        let bar_row = height.saturating_sub(total_rows);

        let _ = out.queue(cursor::MoveTo(0, bar_row));
        let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));

        let mut row = bar_row;

        draw_bar(&mut out, w, None, None, theme::ACCENT);
        let _ = out.queue(Print("\r\n"));
        row += 1;

        // title
        let _ = out.queue(Print(" "));
        let _ = out.queue(SetForegroundColor(theme::ACCENT));
        let _ = out.queue(Print(&self.tool_name));
        let _ = out.queue(ResetColor);
        let _ = out.queue(Print(format!(": {}", self.desc)));
        let _ = out.queue(Print("\r\n"));
        row += 1;

        if has_preview {
            let _ = out.queue(Print("\r\n"));
            row += 1;
            render_confirm_preview(&mut out, &self.tool_name, &self.args, max_preview);
            row += preview_rows;
            if self.total_preview > max_preview {
                row += 1;
            }
        }

        // blank + "Allow?"
        let _ = out.queue(Print("\r\n"));
        row += 1;
        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(Print(" Allow?\r\n"));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        row += 1;

        let mut cursor_pos: Option<(u16, u16)> = None;

        for (i, (label, _)) in self.options.iter().enumerate() {
            let _ = out.queue(Print("  "));
            let highlighted = if self.editing {
                i == 0
            } else {
                i == self.selected
            };
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

            if i == 0 && (self.editing || !self.textarea.is_empty()) {
                let _ = out.queue(Print(", "));
                let _ = out.queue(Print(&self.textarea.lines[0]));
                if self.editing && self.textarea.row == 0 {
                    cursor_pos = Some((self.text_indent + self.textarea.col as u16, row));
                }
                let _ = out.queue(Print("\r\n"));
                row += 1;

                let pad: String = " ".repeat(self.text_indent as usize);
                for li in 1..self.textarea.lines.len() {
                    let _ = out.queue(Print(&pad));
                    let _ = out.queue(Print(&self.textarea.lines[li]));
                    if self.editing && self.textarea.row == li {
                        cursor_pos = Some((self.text_indent + self.textarea.col as u16, row));
                    }
                    let _ = out.queue(Print("\r\n"));
                    row += 1;
                }
            } else {
                let _ = out.queue(Print("\r\n"));
                row += 1;
            }
        }

        // footer
        let _ = out.queue(Print("\r\n"));
        let _ = out.queue(SetAttribute(Attribute::Dim));
        if self.editing {
            let _ = out.queue(Print(" esc: done  enter: newline"));
        } else if !self.textarea.is_empty() {
            let _ = out.queue(Print(" enter: approve with message  tab: edit"));
        }
        let _ = out.queue(SetAttribute(Attribute::Reset));

        if let Some((col, r)) = cursor_pos {
            let _ = out.queue(cursor::MoveTo(col, r));
            let _ = out.queue(cursor::Show);
        } else {
            let _ = out.queue(cursor::Hide);
        }

        let _ = out.flush();
    }

    /// Clear the dialog area and restore cursor.
    pub fn cleanup(&self) {
        let mut out = io::stdout();
        let (_, height) = terminal::size().unwrap_or((80, 24));
        let _ = out.queue(cursor::MoveTo(0, height.saturating_sub(1)));
        let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
        let _ = out.queue(cursor::Show);
        let _ = out.flush();
    }
}

/// Show a rewind menu listing user turns. Returns the block index to rewind to.
pub fn show_rewind(turns: &[(usize, String)]) -> Option<usize> {
    if turns.is_empty() {
        return None;
    }

    let mut out = io::stdout();
    let (width, height) = terminal::size().unwrap_or((80, 24));
    let w = width.saturating_sub(1) as usize;

    let max_visible = (height as usize).saturating_sub(6).min(turns.len());
    let total_rows = (max_visible + 5) as u16;
    let bar_row = height.saturating_sub(total_rows);
    let mut selected: usize = turns.len() - 1;
    let mut scroll_offset: usize = turns.len().saturating_sub(max_visible);

    let _ = out.flush();
    let _ = out.queue(cursor::Hide);
    let _ = out.flush();

    let draw = |selected: usize, scroll_offset: usize| {
        let mut out = io::stdout();
        let _ = out.queue(cursor::MoveTo(0, bar_row));
        let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));

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

        let _ = out.flush();
    };

    draw(selected, scroll_offset);

    let result = loop {
        if let Ok(Event::Key(KeyEvent {
            code, modifiers, ..
        })) = event::read()
        {
            match (code, modifiers) {
                (KeyCode::Enter, _) => break Some(turns[selected].0),
                (KeyCode::Esc, _) => break None,
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => break None,
                (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                    if selected > 0 {
                        selected -= 1;
                        if selected < scroll_offset {
                            scroll_offset = selected;
                        }
                        draw(selected, scroll_offset);
                    }
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                    if selected + 1 < turns.len() {
                        selected += 1;
                        if selected >= scroll_offset + max_visible {
                            scroll_offset = selected + 1 - max_visible;
                        }
                        draw(selected, scroll_offset);
                    }
                }
                _ => {}
            }
        }
    };

    let _ = out.queue(cursor::MoveTo(0, bar_row));
    let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
    let _ = out.queue(cursor::Show);
    let _ = out.flush();

    result
}

/// Show a resume menu listing saved sessions. Returns the selected session id.
pub fn show_resume(entries: &[ResumeEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }

    let mut out = io::stdout();
    let (width, height) = terminal::size().unwrap_or((80, 24));
    let w = width.saturating_sub(1) as usize;
    let max_visible = (height as usize)
        .saturating_sub(7)
        .min(entries.len().max(1));
    let total_rows = (max_visible + 6) as u16;
    let bar_row = height.saturating_sub(total_rows);

    let mut query = String::new();
    let mut filtered = filter_resume_entries(entries, &query);
    let mut selected: usize = 0;
    let mut scroll_offset: usize = 0;

    let _ = out.flush();
    let _ = out.queue(cursor::Hide);
    let _ = out.flush();

    let draw = |selected: usize, scroll_offset: usize, query: &str, filtered: &[ResumeEntry]| {
        let mut out = io::stdout();
        let _ = out.queue(cursor::MoveTo(0, bar_row));
        let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));

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

        let _ = out.flush();
    };

    draw(selected, scroll_offset, &query, &filtered);

    terminal::enable_raw_mode().ok();
    let _ = out.queue(EnableBracketedPaste);
    let _ = out.flush();

    let result = loop {
        let has_event = event::poll(Duration::from_millis(500)).unwrap_or(false);
        if has_event {
            if let Ok(Event::Key(KeyEvent {
                code, modifiers, ..
            })) = event::read()
            {
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
                    scroll_offset = scroll_offset.min(filtered.len().saturating_sub(max_visible));
                }
            }
        }
        draw(selected, scroll_offset, &query, &filtered);
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
        }
    }

    /// Process a key event. Returns `Some(answer_json)` on confirm, `None` to keep going.
    /// Returns `Some(None)` on cancel (Esc/Ctrl+C).
    pub fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<Option<String>> {
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
    pub fn draw(&self) {
        let mut out = io::stdout();
        let (width, height) = terminal::size().unwrap_or((80, 24));
        let w = width.saturating_sub(1) as usize;

        let ta = &self.other_areas[self.active_tab];
        let ta_visible = self.editing_other[self.active_tab] || !ta.is_empty();
        let ta_extra: u16 = if ta_visible {
            ta.line_count().saturating_sub(1)
        } else {
            0
        };

        let fixed_rows = 1 + (self.has_tabs as u16) + 3 + self.max_options as u16 + 2 + ta_extra;
        let bar_row = height.saturating_sub(fixed_rows);

        let _ = out.queue(cursor::MoveTo(0, bar_row));
        let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));

        let mut row = bar_row;

        draw_bar(&mut out, w, None, None, theme::ACCENT);
        let _ = out.queue(Print("\r\n"));
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
            let _ = out.queue(Print("\r\n"));
            row += 1;
        }

        let q = &self.questions[self.active_tab];
        let sel = self.selections[self.active_tab];
        let is_multi = q.multi_select;
        let other_idx = q.options.len();

        let _ = out.queue(Print("\r\n"));
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
        let _ = out.queue(Print("\r\n"));
        row += 1;

        let _ = out.queue(Print("\r\n"));
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
            let _ = out.queue(Print("\r\n"));
            row += 1;
        }

        // "Other" option with inline textarea
        let _ = out.queue(Print("  "));
        let is_other_current = sel == other_idx;
        let is_other_toggled = is_multi && self.multi_toggles[self.active_tab][other_idx];

        let other_text_col: u16 = if is_multi {
            2 + 2 + 5 + 2
        } else {
            let digits = format!("{}", other_idx + 1).len();
            (2 + digits + 2 + 5 + 2) as u16
        };

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

        let mut cursor_pos: Option<(u16, u16)> = None;
        if ta_visible {
            let _ = out.queue(Print(", "));
            let _ = out.queue(Print(&ta.lines[0]));
            if self.editing_other[self.active_tab] && ta.row == 0 {
                cursor_pos = Some((other_text_col + ta.col as u16, row));
            }
            let _ = out.queue(Print("\r\n"));
            row += 1;

            let pad: String = " ".repeat(other_text_col as usize);
            for li in 1..ta.lines.len() {
                let _ = out.queue(Print(&pad));
                let _ = out.queue(Print(&ta.lines[li]));
                if self.editing_other[self.active_tab] && ta.row == li {
                    cursor_pos = Some((other_text_col + ta.col as u16, row));
                }
                let _ = out.queue(Print("\r\n"));
                row += 1;
            }
        } else {
            let _ = out.queue(Print("\r\n"));
        }

        // Footer
        let _ = out.queue(Print("\r\n"));
        let _ = out.queue(SetAttribute(Attribute::Dim));
        if self.editing_other[self.active_tab] {
            let _ = out.queue(Print(" esc: done  enter: newline"));
        } else if self.has_tabs {
            let _ = out.queue(Print(" tab: next question  enter: confirm"));
        } else {
            let _ = out.queue(Print(" enter: confirm"));
        }
        let _ = out.queue(SetAttribute(Attribute::Reset));

        if let Some((col, r)) = cursor_pos {
            let _ = out.queue(cursor::MoveTo(col, r));
            let _ = out.queue(cursor::Show);
        } else {
            let _ = out.queue(cursor::Hide);
        }

        let _ = out.flush();
    }

    /// Clear the dialog area and restore cursor.
    pub fn cleanup(&self) {
        let mut out = io::stdout();
        let (_, height) = terminal::size().unwrap_or((80, 24));
        let _ = out.queue(cursor::MoveTo(0, height.saturating_sub(1)));
        let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
        let _ = out.queue(cursor::Show);
        let _ = out.flush();
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
