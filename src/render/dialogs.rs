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

use super::highlight::{count_inline_diff_rows, print_inline_diff, print_syntax_file};
use super::{draw_bar, ConfirmChoice, ResumeEntry};

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
) -> ConfirmChoice {
    let mut out = io::stdout();
    let (width, height) = terminal::size().unwrap_or((80, 24));
    let w = width.saturating_sub(1) as usize;
    let options: &[(&str, ConfirmChoice)] = &[
        ("yes", ConfirmChoice::Yes),
        ("no", ConfirmChoice::No),
        ("always allow", ConfirmChoice::Always),
    ];

    let total_preview = confirm_preview_row_count(tool_name, args);
    let fixed_rows: u16 = 11;
    let max_preview = height.saturating_sub(fixed_rows + 2);
    let preview_rows = total_preview.min(max_preview);
    let has_preview = preview_rows > 0;
    let extra = if has_preview {
        preview_rows + if total_preview > max_preview { 1 } else { 0 } + 1
    } else {
        0
    };
    let total_rows = fixed_rows + extra + 1;
    let bar_row = height.saturating_sub(total_rows);
    let mut selected: usize = 0;
    let mut message = String::new();
    let mut editing = false;

    let _ = out.queue(cursor::Hide);
    let _ = out.flush();

    let draw = |selected: usize, message: &str, editing: bool| {
        let mut out = io::stdout();
        let _ = out.queue(cursor::MoveTo(0, bar_row));
        let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));

        draw_bar(&mut out, w, None, None, theme::ACCENT);
        let _ = out.queue(Print("\r\n"));

        let _ = out.queue(Print(" "));
        let _ = out.queue(SetForegroundColor(theme::ACCENT));
        let _ = out.queue(Print(tool_name));
        let _ = out.queue(ResetColor);
        let _ = out.queue(Print(format!(": {}", desc)));
        let _ = out.queue(Print("\r\n"));

        if has_preview {
            let _ = out.queue(Print("\r\n"));
            render_confirm_preview(&mut out, tool_name, args, max_preview);
        }

        let _ = out.queue(Print("\r\n"));

        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(Print(" Allow?\r\n"));

        for (i, (label, _)) in options.iter().enumerate() {
            let _ = out.queue(Print("  "));
            if !editing && i == selected {
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
                let _ = out.queue(Print(label.to_string()));
            }
            let _ = out.queue(Print("\r\n"));
        }

        let _ = out.queue(Print("\r\n"));
        if editing {
            let _ = out.queue(Print("  "));
            let _ = out.queue(SetForegroundColor(theme::ACCENT));
            let _ = out.queue(Print("> "));
            let _ = out.queue(ResetColor);
            let _ = out.queue(Print(message));
            let _ = out.flush();
            let (cursor_col, cursor_row) = cursor::position().unwrap_or((0, 0));
            let _ = out.queue(Print("\r\n\r\n"));
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let _ = out.queue(Print(" enter: approve with message  esc: back"));
            let _ = out.queue(SetAttribute(Attribute::Reset));
            let _ = out.queue(cursor::MoveTo(cursor_col, cursor_row));
            let _ = out.queue(cursor::Show);
        } else if !message.is_empty() {
            let _ = out.queue(Print("  "));
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let _ = out.queue(Print("> "));
            let _ = out.queue(SetAttribute(Attribute::Reset));
            let _ = out.queue(Print(message));
            let _ = out.queue(Print("\r\n\r\n"));
            let _ = out.queue(cursor::Hide);
        } else {
            let _ = out.queue(Print("\r\n\r\n"));
            let _ = out.queue(cursor::Hide);
        }

        let _ = out.flush();
    };

    draw(selected, &message, editing);

    use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

    let choice = loop {
        if let Ok(Event::Key(KeyEvent {
            code, modifiers, ..
        })) = event::read()
        {
            if editing {
                match (code, modifiers) {
                    (KeyCode::Enter, _) => {
                        break ConfirmChoice::YesWithMessage(message.clone());
                    }
                    (KeyCode::Esc, _) => {
                        editing = false;
                        draw(selected, &message, editing);
                    }
                    (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                        message.clear();
                        editing = false;
                        draw(selected, &message, editing);
                    }
                    (KeyCode::Backspace, _) => {
                        message.pop();
                        draw(selected, &message, editing);
                    }
                    (KeyCode::Char(c), _) => {
                        message.push(c);
                        draw(selected, &message, editing);
                    }
                    _ => {}
                }
                continue;
            }

            match (code, modifiers) {
                (KeyCode::Enter, _) => {
                    if !message.is_empty() {
                        break ConfirmChoice::YesWithMessage(message.clone());
                    }
                    break options[selected].1.clone();
                }
                (KeyCode::Tab, _) => {
                    editing = true;
                    draw(selected, &message, editing);
                }
                (KeyCode::Char('1'), _) => break ConfirmChoice::Yes,
                (KeyCode::Char('2'), _) => break ConfirmChoice::No,
                (KeyCode::Char('3'), _) => break ConfirmChoice::Always,
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                    break ConfirmChoice::No
                }
                (KeyCode::Esc, _) => break ConfirmChoice::No,
                (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                    selected = if selected == 0 {
                        options.len() - 1
                    } else {
                        selected - 1
                    };
                    draw(selected, &message, editing);
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                    selected = (selected + 1) % options.len();
                    draw(selected, &message, editing);
                }
                _ => {}
            }
        }
    };

    let _ = out.queue(cursor::MoveTo(0, bar_row));
    let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
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

    use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

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

    use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

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
    let (width, height) = terminal::size().unwrap_or((80, 24));
    let w = width.saturating_sub(1) as usize;

    let mut active_tab: usize = 0;
    let mut selections: Vec<usize> = questions.iter().map(|_| 0).collect();
    let mut multi_toggles: Vec<Vec<bool>> = questions
        .iter()
        .map(|q| vec![false; q.options.len() + 1])
        .collect();
    let mut other_texts: Vec<String> = questions.iter().map(|_| String::new()).collect();
    let mut editing_other: Vec<bool> = questions.iter().map(|_| false).collect();
    let mut visited: Vec<bool> = questions.iter().map(|_| false).collect();
    let mut answered: Vec<bool> = questions.iter().map(|_| false).collect();

    let max_options = questions.iter().map(|q| q.options.len()).max().unwrap_or(0) + 1;
    let has_tabs = questions.len() > 1;
    let fixed_rows = 1 + (has_tabs as usize) + 3 + max_options + 1 + 1;
    let bar_row = height.saturating_sub(fixed_rows as u16);

    let _ = out.queue(cursor::Hide);
    let _ = out.flush();

    let draw = |active_tab: usize,
                selections: &[usize],
                multi_toggles: &[Vec<bool>],
                other_texts: &[String],
                editing_other: &[bool],
                visited: &[bool],
                answered: &[bool]| {
        let mut out = io::stdout();
        let _ = out.queue(cursor::MoveTo(0, bar_row));
        let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));

        let mut row = bar_row;

        let _ = out.queue(cursor::MoveTo(0, row));
        let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
        draw_bar(&mut out, w, None, None, theme::ACCENT);
        row = row.saturating_add(1);

        if questions.len() > 1 {
            let _ = out.queue(cursor::MoveTo(0, row));
            let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
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
            row = row.saturating_add(1);
        }

        let q = &questions[active_tab];
        let sel = selections[active_tab];
        let is_multi = q.multi_select;
        let other_idx = q.options.len();

        let _ = out.queue(cursor::MoveTo(0, row));
        let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
        row = row.saturating_add(1);

        let _ = out.queue(cursor::MoveTo(0, row));
        let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
        let _ = out.queue(Print(" "));
        let _ = out.queue(SetAttribute(Attribute::Bold));
        let _ = out.queue(Print(&q.question));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        if is_multi {
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let _ = out.queue(Print(" (space to toggle)"));
            let _ = out.queue(SetAttribute(Attribute::Reset));
        }
        row = row.saturating_add(1);

        let _ = out.queue(cursor::MoveTo(0, row));
        let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
        row = row.saturating_add(1);

        for (i, opt) in q.options.iter().enumerate() {
            let _ = out.queue(cursor::MoveTo(0, row));
            let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
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
            row = row.saturating_add(1);
        }

        let other_row = row;
        let _ = out.queue(cursor::MoveTo(0, row));
        let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
        let _ = out.queue(Print("  "));
        let is_other_current = sel == other_idx;
        let is_other_toggled = is_multi && multi_toggles[active_tab][other_idx];
        let mut cursor_pos: Option<u16> = None;

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
        if editing_other[active_tab] || !other_texts[active_tab].is_empty() {
            let _ = out.queue(Print(", "));
            let _ = out.queue(Print(&other_texts[active_tab]));
            if editing_other[active_tab] {
                let _ = out.flush();
                cursor_pos = cursor::position().ok().map(|(x, _)| x);
            }
        }
        row = row.saturating_add(1);

        let _ = out.queue(cursor::MoveTo(0, row));
        let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
        row = row.saturating_add(1);

        let _ = out.queue(cursor::MoveTo(0, row));
        let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
        let _ = out.queue(SetAttribute(Attribute::Dim));
        if questions.len() > 1 {
            let _ = out.queue(Print(" tab: next question  enter: confirm"));
        } else {
            let _ = out.queue(Print(" enter: confirm"));
        }
        let _ = out.queue(SetAttribute(Attribute::Reset));

        if let Some(col) = cursor_pos {
            let _ = out.queue(cursor::MoveTo(col, other_row));
            let _ = out.queue(cursor::Show);
        } else {
            let _ = out.queue(cursor::Hide);
        }

        let _ = out.flush();
    };

    draw(
        active_tab,
        &selections,
        &multi_toggles,
        &other_texts,
        &editing_other,
        &visited,
        &answered,
    );

    use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

    let cancelled = loop {
        if let Ok(Event::Key(KeyEvent {
            code, modifiers, ..
        })) = event::read()
        {
            let q = &questions[active_tab];
            let other_idx = q.options.len();

            if editing_other[active_tab] {
                match (code, modifiers) {
                    (KeyCode::Enter, _) => {
                        editing_other[active_tab] = false;
                        if q.multi_select {
                            multi_toggles[active_tab][other_idx] = true;
                            visited[active_tab] = true;
                        } else {
                            answered[active_tab] = true;
                        }
                        if questions.len() == 1 {
                            break false;
                        } else if active_tab + 1 < questions.len() {
                            active_tab += 1;
                        }
                        draw(
                            active_tab,
                            &selections,
                            &multi_toggles,
                            &other_texts,
                            &editing_other,
                            &visited,
                            &answered,
                        );
                    }
                    (KeyCode::Esc, _) => {
                        editing_other[active_tab] = false;
                        draw(
                            active_tab,
                            &selections,
                            &multi_toggles,
                            &other_texts,
                            &editing_other,
                            &visited,
                            &answered,
                        );
                    }
                    (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                        other_texts[active_tab].clear();
                        editing_other[active_tab] = false;
                        if q.multi_select {
                            multi_toggles[active_tab][other_idx] = false;
                        }
                        draw(
                            active_tab,
                            &selections,
                            &multi_toggles,
                            &other_texts,
                            &editing_other,
                            &visited,
                            &answered,
                        );
                    }
                    (KeyCode::Backspace, _) => {
                        other_texts[active_tab].pop();
                        draw(
                            active_tab,
                            &selections,
                            &multi_toggles,
                            &other_texts,
                            &editing_other,
                            &visited,
                            &answered,
                        );
                    }
                    (KeyCode::Char(c), _) => {
                        other_texts[active_tab].push(c);
                        draw(
                            active_tab,
                            &selections,
                            &multi_toggles,
                            &other_texts,
                            &editing_other,
                            &visited,
                            &answered,
                        );
                    }
                    _ => {}
                }
                continue;
            }

            match (code, modifiers) {
                (KeyCode::Esc, _) => break true,
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => break true,
                (KeyCode::Enter, _) => {
                    answered[active_tab] = true;
                    if let Some(next) = (0..questions.len()).find(|&i| !answered[i]) {
                        active_tab = next;
                        draw(
                            active_tab,
                            &selections,
                            &multi_toggles,
                            &other_texts,
                            &editing_other,
                            &visited,
                            &answered,
                        );
                        continue;
                    }
                    break false;
                }
                (KeyCode::Tab, _) => {
                    if selections[active_tab] == other_idx {
                        editing_other[active_tab] = true;
                        if q.multi_select {
                            multi_toggles[active_tab][other_idx] = true;
                        }
                        draw(
                            active_tab,
                            &selections,
                            &multi_toggles,
                            &other_texts,
                            &editing_other,
                            &visited,
                            &answered,
                        );
                    }
                }
                (KeyCode::Right, _) | (KeyCode::Char('l'), _) => {
                    if questions.len() > 1 {
                        visited[active_tab] = true;
                        active_tab = (active_tab + 1) % questions.len();
                        draw(
                            active_tab,
                            &selections,
                            &multi_toggles,
                            &other_texts,
                            &editing_other,
                            &visited,
                            &answered,
                        );
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
                        draw(
                            active_tab,
                            &selections,
                            &multi_toggles,
                            &other_texts,
                            &editing_other,
                            &visited,
                            &answered,
                        );
                    }
                }
                (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                    selections[active_tab] = if selections[active_tab] == 0 {
                        other_idx
                    } else {
                        selections[active_tab] - 1
                    };
                    draw(
                        active_tab,
                        &selections,
                        &multi_toggles,
                        &other_texts,
                        &editing_other,
                        &visited,
                        &answered,
                    );
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                    selections[active_tab] = (selections[active_tab] + 1) % (other_idx + 1);
                    draw(
                        active_tab,
                        &selections,
                        &multi_toggles,
                        &other_texts,
                        &editing_other,
                        &visited,
                        &answered,
                    );
                }
                (KeyCode::Char(' '), _) if q.multi_select => {
                    let idx = selections[active_tab];
                    if idx == other_idx && other_texts[active_tab].is_empty() {
                        editing_other[active_tab] = true;
                    } else {
                        multi_toggles[active_tab][idx] = !multi_toggles[active_tab][idx];
                    }
                    draw(
                        active_tab,
                        &selections,
                        &multi_toggles,
                        &other_texts,
                        &editing_other,
                        &visited,
                        &answered,
                    );
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
                        draw(
                            active_tab,
                            &selections,
                            &multi_toggles,
                            &other_texts,
                            &editing_other,
                            &visited,
                            &answered,
                        );
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

    if cancelled {
        return None;
    }

    let mut answers = serde_json::Map::new();
    for (i, q) in questions.iter().enumerate() {
        let other_idx = q.options.len();
        let answer = if q.multi_select {
            let mut selected: Vec<String> = Vec::new();
            for (j, toggled) in multi_toggles[i].iter().enumerate() {
                if *toggled {
                    if j == other_idx {
                        selected.push(format!("Other: {}", other_texts[i]));
                    } else {
                        selected.push(q.options[j].label.clone());
                    }
                }
            }
            if selected.is_empty() {
                if selections[i] == other_idx {
                    serde_json::Value::String(format!("Other: {}", other_texts[i]))
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
            serde_json::Value::String(format!("Other: {}", other_texts[i]))
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
