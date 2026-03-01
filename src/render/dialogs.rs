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

/// Show confirm prompt as a bottom bar overlay. Returns the user's choice.
pub fn show_confirm(
    tool_name: &str,
    desc: &str,
    args: &HashMap<String, serde_json::Value>,
    approval_pattern: Option<&str>,
) -> ConfirmChoice {
    let mut out = io::stdout();
    let (width, height) = terminal::size().unwrap_or((80, 24));
    let w = width.saturating_sub(1) as usize;

    let mut options_vec: Vec<(String, ConfirmChoice)> = vec![
        ("yes".into(), ConfirmChoice::Yes),
        ("no".into(), ConfirmChoice::No),
    ];
    if let Some(pattern) = approval_pattern {
        let display = pattern.strip_suffix("/*").unwrap_or(pattern);
        let display = display.split("://").nth(1).unwrap_or(display);
        options_vec.push((
            format!("allow {display}"),
            ConfirmChoice::AlwaysPattern(pattern.to_string()),
        ));
    } else {
        options_vec.push(("always allow".into(), ConfirmChoice::Always));
    }
    let options: Vec<(&str, ConfirmChoice)> = options_vec
        .iter()
        .map(|(l, c)| (l.as_str(), c.clone()))
        .collect();

    let total_preview = confirm_preview_row_count(tool_name, args);

    let mut selected: usize = 0;
    let mut textarea = TextArea::new();
    let mut editing = false;

    let _ = out.queue(cursor::Hide);
    let _ = out.flush();

    // The text indent column: "  1. yes, " = 2 + digit + ". " + label + ", "
    let first_label = options.first().map(|(l, _)| *l).unwrap_or("yes");
    // "  1. yes, " → indent = 2 (leading) + 1 (digit) + 2 (". ") + label.len + 2 (", ")
    let text_indent = (2 + 1 + 2 + first_label.len() + 2) as u16;

    // Height is recalculated every draw so the dialog expands with the textarea.
    let draw = |selected: usize, textarea: &TextArea, editing: bool| {
        let mut out = io::stdout();
        let (_, height) = terminal::size().unwrap_or((80, 24));

        // Extra lines from the textarea (lines beyond the first are extra rows on option 1)
        let ta_extra = if editing || !textarea.is_empty() {
            textarea.line_count().saturating_sub(1)
        } else {
            0
        };

        // rows: bar(1) + title(1) + blank(1) + "Allow?"(1) + options(N) + blank(1) + footer(1)
        let base_rows: u16 = 6 + options.len() as u16 + ta_extra;

        // preview
        let max_preview = height.saturating_sub(base_rows + 2);
        let preview_rows = total_preview.min(max_preview);
        let has_preview = preview_rows > 0;
        let preview_extra = if has_preview {
            preview_rows + u16::from(total_preview > max_preview) + 1
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
        let _ = out.queue(Print(tool_name));
        let _ = out.queue(ResetColor);
        let _ = out.queue(Print(format!(": {desc}")));
        let _ = out.queue(Print("\r\n"));
        row += 1;

        if has_preview {
            let _ = out.queue(Print("\r\n"));
            row += 1;
            render_confirm_preview(&mut out, tool_name, args, max_preview);
            row += preview_rows;
            if total_preview > max_preview {
                row += 1; // truncation indicator
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

        for (i, (label, _)) in options.iter().enumerate() {
            let _ = out.queue(Print("  "));
            let highlighted = if editing { i == 0 } else { i == selected };
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
                let _ = out.queue(Print(*label));
            }

            // Inline textarea on option 1
            if i == 0 && (editing || !textarea.is_empty()) {
                let _ = out.queue(Print(", "));
                // First line goes inline
                let _ = out.queue(Print(&textarea.lines[0]));
                if editing && textarea.row == 0 {
                    cursor_pos = Some((text_indent + textarea.col as u16, row));
                }
                let _ = out.queue(Print("\r\n"));
                row += 1;

                // Continuation lines indented to the same column
                let pad: String = " ".repeat(text_indent as usize);
                for li in 1..textarea.lines.len() {
                    let _ = out.queue(Print(&pad));
                    let _ = out.queue(Print(&textarea.lines[li]));
                    if editing && textarea.row == li {
                        cursor_pos = Some((text_indent + textarea.col as u16, row));
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
        if editing {
            let _ = out.queue(Print(" esc: done  enter: newline"));
        } else if !textarea.is_empty() {
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
    };

    draw(selected, &textarea, editing);

    let choice = loop {
        if let Ok(Event::Key(KeyEvent {
            code, modifiers, ..
        })) = event::read()
        {
            if editing {
                match (code, modifiers) {
                    (KeyCode::Esc, _) => {
                        editing = false;
                    }
                    (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                        if textarea.is_empty() {
                            break ConfirmChoice::No;
                        }
                        textarea.clear();
                        editing = false;
                    }
                    _ => {
                        textarea.handle_key(code, modifiers);
                    }
                }
                draw(selected, &textarea, editing);
                continue;
            }

            match (code, modifiers) {
                (KeyCode::Enter, _) => {
                    if !textarea.is_empty() {
                        break ConfirmChoice::YesWithMessage(textarea.text());
                    }
                    break options[selected].1.clone();
                }
                (KeyCode::Tab, _) => {
                    editing = true;
                    draw(selected, &textarea, editing);
                }
                (KeyCode::Char(c @ '1'..='9'), _) => {
                    let idx = (c as usize) - ('1' as usize);
                    if idx < options.len() {
                        break options[idx].1.clone();
                    }
                }
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                    break ConfirmChoice::No;
                }
                (KeyCode::Esc, _) => break ConfirmChoice::No,
                (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                    selected = if selected == 0 {
                        options.len() - 1
                    } else {
                        selected - 1
                    };
                    draw(selected, &textarea, editing);
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                    selected = (selected + 1) % options.len();
                    draw(selected, &textarea, editing);
                }
                _ => {}
            }
        }
    };

    // Clean up: clear the dialog area.
    let _ = out.queue(cursor::MoveTo(0, height.saturating_sub(1)));
    let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
    let _ = out.queue(cursor::Show);
    let _ = out.flush();

    choice
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

/// Show an interactive question overlay with tabs for multiple questions.
pub fn show_ask_question(questions: &[Question]) -> Option<String> {
    if questions.is_empty() {
        return Some("{}".into());
    }

    let mut out = io::stdout();
    let (width, _height) = terminal::size().unwrap_or((80, 24));
    let w = width.saturating_sub(1) as usize;

    let mut active_tab: usize = 0;
    let mut selections: Vec<usize> = questions.iter().map(|_| 0).collect();
    let mut multi_toggles: Vec<Vec<bool>> = questions
        .iter()
        .map(|q| vec![false; q.options.len() + 1])
        .collect();
    let mut other_areas: Vec<TextArea> = questions.iter().map(|_| TextArea::new()).collect();
    let mut editing_other: Vec<bool> = questions.iter().map(|_| false).collect();
    let mut visited: Vec<bool> = questions.iter().map(|_| false).collect();
    let mut answered: Vec<bool> = questions.iter().map(|_| false).collect();

    let max_options = questions.iter().map(|q| q.options.len()).max().unwrap_or(0) + 1;
    let has_tabs = questions.len() > 1;

    let _ = out.queue(cursor::Hide);
    let _ = out.flush();

    let draw = |active_tab: usize,
                selections: &[usize],
                multi_toggles: &[Vec<bool>],
                other_areas: &[TextArea],
                editing_other: &[bool],
                visited: &[bool],
                answered: &[bool]| {
        let mut out = io::stdout();
        let (_, height) = terminal::size().unwrap_or((80, 24));

        let ta = &other_areas[active_tab];
        let ta_visible = editing_other[active_tab] || !ta.is_empty();
        // Extra rows from textarea continuation lines (first line is inline with "Other")
        let ta_extra: u16 = if ta_visible {
            ta.line_count().saturating_sub(1)
        } else {
            0
        };

        // bar(1) + tabs?(1) + blank(1) + question(1) + blank(1) + options(N) + blank(1) + footer(1)
        let fixed_rows = 1 + (has_tabs as u16) + 3 + max_options as u16 + 2 + ta_extra;
        let bar_row = height.saturating_sub(fixed_rows);

        let _ = out.queue(cursor::MoveTo(0, bar_row));
        let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));

        let mut row = bar_row;

        draw_bar(&mut out, w, None, None, theme::ACCENT);
        let _ = out.queue(Print("\r\n"));
        row += 1;

        if questions.len() > 1 {
            let _ = out.queue(Print(" "));
            for (i, q) in questions.iter().enumerate() {
                let bullet = if answered[i] || visited[i] {
                    "■"
                } else {
                    "□"
                };
                if i == active_tab {
                    let _ = out.queue(SetForegroundColor(theme::ACCENT));
                    let _ = out.queue(SetAttribute(Attribute::Bold));
                    let _ = out.queue(Print(format!(" {} {} ", bullet, q.header)));
                    let _ = out.queue(SetAttribute(Attribute::Reset));
                    let _ = out.queue(ResetColor);
                } else if answered[i] {
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

        let q = &questions[active_tab];
        let sel = selections[active_tab];
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
            let is_toggled = is_multi && multi_toggles[active_tab][i];

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
        let is_other_toggled = is_multi && multi_toggles[active_tab][other_idx];

        // Calculate the column where text starts after "Other, "
        let other_text_col: u16 = if is_multi {
            // "  ◉ Other, " = 2 + 2(check+space) + 5(Other) + 2(", ")
            2 + 2 + 5 + 2
        } else {
            // "  N. Other, " = 2 + digits + 2(". ") + 5("Other") + 2(", ")
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
            // First line inline
            let _ = out.queue(Print(&ta.lines[0]));
            if editing_other[active_tab] && ta.row == 0 {
                cursor_pos = Some((other_text_col + ta.col as u16, row));
            }
            let _ = out.queue(Print("\r\n"));
            row += 1;

            // Continuation lines aligned to the same column
            let pad: String = " ".repeat(other_text_col as usize);
            for li in 1..ta.lines.len() {
                let _ = out.queue(Print(&pad));
                let _ = out.queue(Print(&ta.lines[li]));
                if editing_other[active_tab] && ta.row == li {
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
        if editing_other[active_tab] {
            let _ = out.queue(Print(" esc: done  enter: newline"));
        } else if questions.len() > 1 {
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
    };

    let redraw = |at, sel: &[usize], mt: &[Vec<bool>], oa: &[TextArea], eo: &[bool], v: &[bool], a: &[bool]| {
        draw(at, sel, mt, oa, eo, v, a);
    };

    redraw(
        active_tab,
        &selections,
        &multi_toggles,
        &other_areas,
        &editing_other,
        &visited,
        &answered,
    );

    let cancelled = loop {
        if let Ok(Event::Key(KeyEvent {
            code, modifiers, ..
        })) = event::read()
        {
            let q = &questions[active_tab];
            let other_idx = q.options.len();

            if editing_other[active_tab] {
                match (code, modifiers) {
                    (KeyCode::Esc, _) => {
                        editing_other[active_tab] = false;
                    }
                    (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                        if other_areas[active_tab].is_empty() {
                            break true;
                        }
                        other_areas[active_tab].clear();
                        editing_other[active_tab] = false;
                        if q.multi_select {
                            multi_toggles[active_tab][other_idx] = false;
                        }
                    }
                    _ => {
                        other_areas[active_tab].handle_key(code, modifiers);
                    }
                }
                redraw(
                    active_tab,
                    &selections,
                    &multi_toggles,
                    &other_areas,
                    &editing_other,
                    &visited,
                    &answered,
                );
                continue;
            }

            match (code, modifiers) {
                (KeyCode::Esc, _) => break true,
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => break true,
                (KeyCode::Enter, _) => {
                    answered[active_tab] = true;
                    if let Some(next) = (0..questions.len()).find(|&i| !answered[i]) {
                        active_tab = next;
                    } else {
                        break false;
                    }
                }
                (KeyCode::Tab, _) => {
                    if selections[active_tab] == other_idx {
                        editing_other[active_tab] = true;
                        if q.multi_select {
                            multi_toggles[active_tab][other_idx] = true;
                        }
                    }
                }
                (KeyCode::Right, _) | (KeyCode::Char('l'), _) => {
                    if questions.len() > 1 {
                        visited[active_tab] = true;
                        active_tab = (active_tab + 1) % questions.len();
                    }
                }
                (KeyCode::BackTab, _) | (KeyCode::Left, _) | (KeyCode::Char('h'), _) => {
                    if questions.len() > 1 {
                        visited[active_tab] = true;
                        active_tab = if active_tab == 0 {
                            questions.len() - 1
                        } else {
                            active_tab - 1
                        };
                    }
                }
                (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                    selections[active_tab] = if selections[active_tab] == 0 {
                        other_idx
                    } else {
                        selections[active_tab] - 1
                    };
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                    selections[active_tab] = (selections[active_tab] + 1) % (other_idx + 1);
                }
                (KeyCode::Char(' '), _) if q.multi_select => {
                    let idx = selections[active_tab];
                    if idx == other_idx && other_areas[active_tab].is_empty() {
                        editing_other[active_tab] = true;
                    } else {
                        multi_toggles[active_tab][idx] = !multi_toggles[active_tab][idx];
                    }
                }
                (KeyCode::Char(c), _) if c.is_ascii_digit() => {
                    let num = c.to_digit(10).unwrap_or(0) as usize;
                    if num >= 1 && num <= other_idx + 1 {
                        if q.multi_select {
                            multi_toggles[active_tab][num - 1] =
                                !multi_toggles[active_tab][num - 1];
                        } else {
                            selections[active_tab] = num - 1;
                        }
                    }
                }
                _ => {}
            }
            redraw(
                active_tab,
                &selections,
                &multi_toggles,
                &other_areas,
                &editing_other,
                &visited,
                &answered,
            );
        }
    };

    let (_, height) = terminal::size().unwrap_or((80, 24));
    let _ = out.queue(cursor::MoveTo(0, height.saturating_sub(1)));
    let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
    let _ = out.queue(cursor::Show);
    let _ = out.flush();

    if cancelled {
        return None;
    }

    let mut answers = serde_json::Map::new();
    for (i, q) in questions.iter().enumerate() {
        let other_idx = q.options.len();
        let other_text = other_areas[i].text();
        let answer = if q.multi_select {
            let mut selected: Vec<String> = Vec::new();
            for (j, toggled) in multi_toggles[i].iter().enumerate() {
                if *toggled {
                    if j == other_idx {
                        selected.push(format!("Other: {other_text}"));
                    } else {
                        selected.push(q.options[j].label.clone());
                    }
                }
            }
            if selected.is_empty() {
                if selections[i] == other_idx {
                    serde_json::Value::String(format!("Other: {other_text}"))
                } else {
                    serde_json::Value::String(q.options[selections[i]].label.clone())
                }
            } else {
                serde_json::Value::Array(
                    selected
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect(),
                )
            }
        } else if selections[i] == other_idx {
            serde_json::Value::String(format!("Other: {other_text}"))
        } else {
            serde_json::Value::String(q.options[selections[i]].label.clone())
        };
        answers.insert(q.question.clone(), answer);
    }
    Some(serde_json::Value::Object(answers).to_string())
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
