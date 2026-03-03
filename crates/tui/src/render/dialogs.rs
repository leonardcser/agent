use crate::session;
use crate::theme;
use crate::utils::format_duration;
use crossterm::event::{KeyCode, KeyModifiers};
use crossterm::{
    cursor,
    style::{Attribute, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal, QueueableCommand,
};
use engine::tools::ProcessInfo;
use std::collections::HashMap;
use std::io::{self, Write};
use std::time::Instant;

use super::blocks::wrap_line;
use super::highlight::{count_inline_diff_rows, print_inline_diff, print_syntax_file};
use super::{chunk_line, crlf, draw_bar, ConfirmChoice, ResumeEntry};

/// Maximum number of scrollable list items in a dialog.
///
/// The dialog starts at `start_row` and can push content into scrollback
/// by up to `height / 2` rows. This gives a max dialog height of
/// `(height - start_row) + min(start_row, height / 2)`, minus overhead.
fn max_dialog_items(start_row: u16, height: u16) -> usize {
    max_dialog_height(start_row, height).saturating_sub(4)
}

/// Maximum total rows a dialog may occupy, accounting for push-up into
/// scrollback (capped at half the terminal height).
fn max_dialog_height(start_row: u16, height: u16) -> usize {
    let space_below = height.saturating_sub(start_row) as usize;
    let max_push = start_row.min(height / 2) as usize;
    (space_below + max_push).min(height as usize)
}

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

    fn delete_word_backward(&mut self) {
        if self.col == 0 {
            return;
        }
        let line = &self.lines[self.row];
        let byte_pos = char_to_byte(line, self.col);
        let target = crate::vim::word_backward_pos(line, byte_pos, crate::vim::CharClass::Word);
        let target_col = line[..target].chars().count();
        let end_byte = char_to_byte(&self.lines[self.row], self.col);
        self.lines[self.row].drain(target..end_byte);
        self.col = target_col;
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
            (KeyCode::Backspace, m)
                if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
            {
                self.delete_word_backward()
            }
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

/// Begin a dialog frame: synchronized update, hide cursor, move to start, clear.
fn begin_dialog_draw(out: &mut io::Stdout, start_row: u16) {
    let _ = out.queue(terminal::BeginSynchronizedUpdate);
    let _ = out.queue(cursor::Hide);
    let _ = out.queue(cursor::MoveTo(0, start_row));
    let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
}

/// End a dialog frame: clear remainder, end sync update, flush, return scroll.
fn end_dialog_draw(out: &mut io::Stdout, start_row: u16, total_rows: u16, height: u16) -> u16 {
    let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
    let _ = out.queue(terminal::EndSynchronizedUpdate);
    let _ = out.flush();
    (start_row + total_rows).saturating_sub(height)
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
    summary: Option<String>,
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
        summary: Option<&str>,
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
                (KeyCode::Enter, _) => {
                    let msg = if self.textarea.is_empty() {
                        None
                    } else {
                        Some(self.textarea.text())
                    };
                    return Some((self.options[self.selected].1.clone(), msg));
                }
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

    /// Render the dialog inline at `start_row`, pushing content into
    /// scrollback if needed. Returns how many rows the terminal scrolled.
    pub fn draw(&mut self, start_row: u16) -> u16 {
        if !self.dirty {
            return 0;
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

        let prefix_len = 1 + self.tool_name.len() + 2; // " tool: "
        let title_rows = wrap_line(&self.desc, w.saturating_sub(prefix_len)).len() as u16;
        let summary_rows: u16 = self
            .summary
            .as_ref()
            .map(|s| wrap_line(s, w.saturating_sub(1)).len() as u16)
            .unwrap_or(0);
        // 4 = bar + blank-before-allow + allow-label + bottom-pad; title_rows replaces the old fixed 1
        let base_rows: u16 =
            4 + title_rows + summary_rows + 1 + self.options.len() as u16 + ta_extra;

        let max_avail = max_dialog_height(start_row, height) as u16;
        let max_preview = max_avail.saturating_sub(base_rows + 2);
        let preview_rows = self.total_preview.min(max_preview);
        let has_preview = preview_rows > 0;
        let preview_extra = if has_preview {
            // top separator + content + bottom separator
            preview_rows + 2
        } else {
            0
        };

        let total_rows = base_rows + preview_extra;
        let bar_row = start_row;

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

            // title — wrap long commands with a leading space on continuation lines
            let prefix_len = 1 + self.tool_name.len() + 2; // " tool: "
            let segments = wrap_line(&self.desc, w.saturating_sub(prefix_len));
            for (i, seg) in segments.iter().enumerate() {
                if i == 0 {
                    let _ = out.queue(Print(" "));
                    let _ = out.queue(SetForegroundColor(theme::ACCENT));
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

            // blank line before "Allow?"
            crlf(&mut out);
            row += 1;

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
            let _ = out.queue(Print(" enter: send  esc: cancel"));
        } else if !self.textarea.is_empty() {
            let _ = out.queue(Print(" enter: confirm with message  tab: edit"));
        }
        let _ = out.queue(SetAttribute(Attribute::Reset));
        let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));

        finish_dialog_frame(&mut out, cursor_pos, self.editing);

        // Compute how much the terminal scrolled and adjust stored positions.
        let scroll = (bar_row + total_rows).saturating_sub(height);
        if scroll > 0 {
            self.last_bar_row = self.last_bar_row.saturating_sub(scroll);
            self.options_row = self.options_row.saturating_sub(scroll);
        }
        scroll
    }
}

// ── RewindDialog ─────────────────────────────────────────────────────────────

pub struct RewindDialog {
    turns: Vec<(usize, String)>,
    selected: usize,
    scroll_offset: usize,
    max_visible: usize,
    dirty: bool,
    pub restore_vim_insert: bool,
}

impl RewindDialog {
    pub fn new(turns: Vec<(usize, String)>, restore_vim_insert: bool) -> Self {
        let (_, height) = terminal::size().unwrap_or((80, 24));
        let max_visible = max_dialog_items(height, height).min(turns.len());
        let selected = turns.len().saturating_sub(1);
        let scroll_offset = turns.len().saturating_sub(max_visible);
        Self {
            turns,
            selected,
            scroll_offset,
            max_visible,
            dirty: true,
            restore_vim_insert,
        }
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn handle_resize(&mut self, h: u16) {
        self.max_visible = max_dialog_items(h, h).min(self.turns.len());
        self.scroll_offset = self
            .scroll_offset
            .min(self.turns.len().saturating_sub(self.max_visible));
        self.dirty = true;
    }

    /// Returns `Some(Some(idx))` when the user selects an entry, `Some(None)` on cancel.
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Option<Option<usize>> {
        match code {
            KeyCode::Enter => return Some(Some(self.turns[self.selected].0)),
            KeyCode::Esc => return Some(None),
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => return Some(None),
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected > 0 {
                    self.selected -= 1;
                    if self.selected < self.scroll_offset {
                        self.scroll_offset = self.selected;
                    }
                    self.dirty = true;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.turns.len() {
                    self.selected += 1;
                    if self.selected >= self.scroll_offset + self.max_visible {
                        self.scroll_offset = self.selected + 1 - self.max_visible;
                    }
                    self.dirty = true;
                }
            }
            _ => {}
        }
        None
    }

    pub fn draw(&mut self, start_row: u16) -> u16 {
        if !self.dirty {
            return 0;
        }
        self.dirty = false;

        let mut out = io::stdout();
        let (width, height) = terminal::size().unwrap_or((80, 24));
        let w = width as usize;

        self.max_visible = max_dialog_items(start_row, height).min(self.turns.len());
        self.scroll_offset = self
            .scroll_offset
            .min(self.turns.len().saturating_sub(self.max_visible));

        // total_rows = bar(1) + title(1) + items + blank(1) + footer(1)
        let total_rows = (self.max_visible as u16) + 4;

        begin_dialog_draw(&mut out, start_row);

        draw_bar(&mut out, w, None, None, theme::ACCENT);
        crlf(&mut out);

        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(Print(" Rewind to:"));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        crlf(&mut out);

        let end = (self.scroll_offset + self.max_visible).min(self.turns.len());
        for (i, (_, ref full_text)) in self
            .turns
            .iter()
            .enumerate()
            .take(end)
            .skip(self.scroll_offset)
        {
            let label = full_text.lines().next().unwrap_or("");
            let num = i + 1;
            let max_label = w.saturating_sub(8);
            let truncated = truncate_str_local(label, max_label);
            let _ = out.queue(Print("  "));
            if i == self.selected {
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
            crlf(&mut out);
        }

        crlf(&mut out);
        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(Print(" enter: select  esc: cancel"));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        end_dialog_draw(&mut out, start_row, total_rows, height)
    }
}

// ── ResumeDialog ──────────────────────────────────────────────────────────────

pub struct ResumeDialog {
    entries: Vec<ResumeEntry>,
    current_cwd: String,
    query: String,
    workspace_only: bool,
    filtered: Vec<ResumeEntry>,
    selected: usize,
    scroll_offset: usize,
    max_visible: usize,
    dirty: bool,
    pending_d: bool,
    last_drawn: Instant,
}

impl ResumeDialog {
    pub fn new(entries: Vec<ResumeEntry>, current_cwd: String) -> Self {
        let (_, height) = terminal::size().unwrap_or((80, 24));
        let filtered = filter_resume_entries(&entries, "", true, &current_cwd);
        let max_visible = max_dialog_items(height, height).min(filtered.len().max(1));
        Self {
            entries,
            current_cwd,
            query: String::new(),
            workspace_only: true,
            filtered,
            selected: 0,
            scroll_offset: 0,
            max_visible,
            dirty: true,
            pending_d: false,
            last_drawn: Instant::now(),
        }
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn recalculate_layout(&mut self, height: u16) {
        self.filtered = filter_resume_entries(
            &self.entries,
            &self.query,
            self.workspace_only,
            &self.current_cwd,
        );
        self.max_visible = max_dialog_items(height, height).min(self.filtered.len().max(1));
        if self.filtered.is_empty() {
            self.selected = 0;
            self.scroll_offset = 0;
        } else {
            self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
            self.scroll_offset = self
                .scroll_offset
                .min(self.filtered.len().saturating_sub(self.max_visible));
        }
        self.dirty = true;
    }

    pub fn handle_resize(&mut self, h: u16) {
        self.recalculate_layout(h);
    }

    /// Returns `Some(Some(id))` on selection, `Some(None)` on cancel.
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Option<Option<String>> {
        let (_, height) = terminal::size().unwrap_or((80, 24));

        // Check for DD completion before anything else.
        if self.pending_d {
            self.pending_d = false;
            if code == KeyCode::Char('d') && mods == KeyModifiers::NONE {
                self.delete_selected(height);
                return None;
            }
            // 'd' followed by something else: insert both as query chars.
            self.query.push('d');
            // Fall through to handle the current key normally.
        }

        match (code, mods) {
            (KeyCode::Enter, _) => {
                return Some(self.filtered.get(self.selected).map(|e| e.id.clone()));
            }
            (KeyCode::Esc, _) => return Some(None),
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => return Some(None),
            (KeyCode::Char('w'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.workspace_only = !self.workspace_only;
                self.recalculate_layout(height);
            }
            // Ctrl+U: half-page up
            (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
                if !self.filtered.is_empty() {
                    let half = self.max_visible / 2;
                    self.selected = self.selected.saturating_sub(half.max(1));
                    if self.selected < self.scroll_offset {
                        self.scroll_offset = self.selected;
                    }
                    self.dirty = true;
                }
            }
            // Ctrl+D: half-page down
            (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                if !self.filtered.is_empty() {
                    let half = self.max_visible / 2;
                    self.selected =
                        (self.selected + half.max(1)).min(self.filtered.len().saturating_sub(1));
                    if self.selected >= self.scroll_offset + self.max_visible {
                        self.scroll_offset = self.selected + 1 - self.max_visible;
                    }
                    self.dirty = true;
                }
            }
            (KeyCode::Backspace, m)
                if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
            {
                if self.query.is_empty() {
                    self.delete_selected(height);
                } else {
                    let len = self.query.len();
                    let target = crate::vim::word_backward_pos(
                        &self.query,
                        len,
                        crate::vim::CharClass::Word,
                    );
                    self.query.truncate(target);
                    self.recalculate_layout(height);
                }
            }
            (KeyCode::Backspace, _) => {
                if self.query.is_empty() {
                    self.delete_selected(height);
                } else {
                    self.query.pop();
                    self.recalculate_layout(height);
                }
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                if self.selected > 0 {
                    self.selected -= 1;
                    if self.selected < self.scroll_offset {
                        self.scroll_offset = self.selected;
                    }
                    self.dirty = true;
                }
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                if self.selected + 1 < self.filtered.len() {
                    self.selected += 1;
                    if self.selected >= self.scroll_offset + self.max_visible {
                        self.scroll_offset = self.selected + 1 - self.max_visible;
                    }
                    self.dirty = true;
                }
            }
            (KeyCode::Char('d'), KeyModifiers::NONE) if self.query.is_empty() => {
                self.pending_d = true;
                self.dirty = true;
            }
            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                self.query.push(c);
                self.recalculate_layout(height);
            }
            _ => {}
        }
        None
    }

    fn delete_selected(&mut self, height: u16) {
        if let Some(entry) = self.filtered.get(self.selected) {
            let id = entry.id.clone();
            session::delete(&id);
            self.entries.retain(|e| e.id != id);
            self.recalculate_layout(height);
        }
    }

    pub fn draw(&mut self, start_row: u16) -> u16 {
        if !self.dirty {
            let freshest = self.filtered.iter().map(resume_ts).max().unwrap_or(0);
            let age_s = session::now_ms().saturating_sub(freshest) / 1000;
            let interval = if age_s < 60 {
                1
            } else if age_s < 3600 {
                30
            } else {
                60
            };
            if self.last_drawn.elapsed().as_secs() >= interval {
                self.dirty = true;
            }
        }
        if !self.dirty {
            return 0;
        }
        self.dirty = false;
        self.last_drawn = Instant::now();

        let mut out = io::stdout();
        let (width, height) = terminal::size().unwrap_or((80, 24));
        let w = width as usize;

        self.max_visible = max_dialog_items(start_row, height).min(self.filtered.len().max(1));
        if self.filtered.is_empty() {
            self.selected = 0;
            self.scroll_offset = 0;
        } else {
            self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
            self.scroll_offset = self
                .scroll_offset
                .min(self.filtered.len().saturating_sub(self.max_visible));
        }

        // total_rows = bar(1) + title(1) + items + blank(1) + footer(1)
        let item_rows = if self.filtered.is_empty() {
            1
        } else {
            self.max_visible as u16
        };
        let total_rows = item_rows + 4;

        begin_dialog_draw(&mut out, start_row);

        let now_ms = session::now_ms();

        draw_bar(&mut out, w, None, None, theme::ACCENT);
        crlf(&mut out);

        let _ = out.queue(SetAttribute(Attribute::Dim));
        if self.workspace_only {
            let _ = out.queue(Print(" Resume (workspace):"));
        } else {
            let _ = out.queue(Print(" Resume (all):"));
        }
        let _ = out.queue(SetAttribute(Attribute::Reset));
        let _ = out.queue(Print(" "));
        let _ = out.queue(Print(&self.query));
        crlf(&mut out);

        if self.filtered.is_empty() {
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let _ = out.queue(Print("  No matches"));
            let _ = out.queue(SetAttribute(Attribute::Reset));
            crlf(&mut out);
        } else {
            let end = (self.scroll_offset + self.max_visible).min(self.filtered.len());
            for (i, entry) in self
                .filtered
                .iter()
                .enumerate()
                .take(end)
                .skip(self.scroll_offset)
            {
                let title = resume_title(entry);
                let time_ago = session::time_ago(resume_ts(entry), now_ms);
                let time_len = time_ago.chars().count() + 1;
                let indent = 2 + entry.depth * 2;
                let indent_str = " ".repeat(indent);
                let max_label = w.saturating_sub(time_len + indent + 2);
                let truncated = truncate_str_local(&title, max_label);
                if i == self.selected {
                    let _ = out.queue(Print(&indent_str));
                    let _ = out.queue(SetForegroundColor(theme::ACCENT));
                    let _ = out.queue(Print(&truncated));
                    let _ = out.queue(ResetColor);
                } else {
                    let _ = out.queue(Print(&indent_str));
                    let _ = out.queue(Print(&truncated));
                }
                let _ = out.queue(Print(" "));
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(&time_ago));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                crlf(&mut out);
            }
        }

        crlf(&mut out);
        let _ = out.queue(SetAttribute(Attribute::Dim));
        if self.workspace_only {
            let _ = out.queue(Print(
                " enter: select  dd/bs: delete  esc: cancel  ctrl+w: all sessions",
            ));
        } else {
            let _ = out.queue(Print(
                " enter: select  dd/bs: delete  esc: cancel  ctrl+w: this workspace",
            ));
        }
        let _ = out.queue(SetAttribute(Attribute::Reset));
        end_dialog_draw(&mut out, start_row, total_rows, height)
    }
}

// ── PsDialog ──────────────────────────────────────────────────────────────────

pub struct PsDialog {
    registry: engine::tools::ProcessRegistry,
    procs: Vec<ProcessInfo>,
    selected: usize,
    scroll_offset: usize,
    max_visible: usize,
    dirty: bool,
    killed: Vec<String>,
}

impl PsDialog {
    pub fn new(registry: engine::tools::ProcessRegistry) -> Self {
        let procs = Self::fetch_procs(&registry, &[]);
        let (_, height) = terminal::size().unwrap_or((80, 24));
        let max_visible = max_dialog_items(height, height).min(procs.len().max(1));
        Self {
            registry,
            procs,
            selected: 0,
            scroll_offset: 0,
            max_visible,
            dirty: true,
            killed: Vec::new(),
        }
    }

    fn fetch_procs(
        registry: &engine::tools::ProcessRegistry,
        killed: &[String],
    ) -> Vec<ProcessInfo> {
        registry
            .list()
            .into_iter()
            .filter(|p| !killed.contains(&p.id))
            .collect()
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn handle_resize(&mut self, h: u16) {
        self.max_visible = max_dialog_items(h, h).min(self.procs.len().max(1));
        if self.procs.is_empty() {
            self.selected = 0;
            self.scroll_offset = 0;
        } else {
            self.selected = self.selected.min(self.procs.len().saturating_sub(1));
            self.scroll_offset = self
                .scroll_offset
                .min(self.procs.len().saturating_sub(self.max_visible));
        }
        self.dirty = true;
    }

    /// Returns `Some(killed_ids)` when the user closes the dialog (may be empty).
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Option<Vec<String>> {
        match (code, mods) {
            (KeyCode::Esc, _) => return Some(self.killed.clone()),
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                return Some(self.killed.clone())
            }
            (KeyCode::Up, _) => {
                if self.selected > 0 {
                    self.selected -= 1;
                    if self.selected < self.scroll_offset {
                        self.scroll_offset = self.selected;
                    }
                    self.dirty = true;
                }
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                if self.selected + 1 < self.procs.len() {
                    self.selected += 1;
                    if self.selected >= self.scroll_offset + self.max_visible {
                        self.scroll_offset = self.selected + 1 - self.max_visible;
                    }
                    self.dirty = true;
                }
            }
            (KeyCode::Char('k'), KeyModifiers::NONE) => {
                if let Some(p) = self.procs.get(self.selected) {
                    self.killed.push(p.id.clone());
                    self.procs = Self::fetch_procs(&self.registry, &self.killed);
                    if !self.procs.is_empty() {
                        self.selected = self.selected.min(self.procs.len().saturating_sub(1));
                    } else {
                        self.selected = 0;
                    }
                    let (_, h) = terminal::size().unwrap_or((80, 24));
                    self.max_visible = max_dialog_items(h, h).min(self.procs.len().max(1));
                    self.dirty = true;
                }
            }
            _ => {}
        }
        None
    }

    pub fn draw(&mut self, start_row: u16) -> u16 {
        if !self.dirty {
            return 0;
        }
        self.dirty = false;

        self.procs = Self::fetch_procs(&self.registry, &self.killed);
        let now = std::time::Instant::now();

        let mut out = io::stdout();
        let (width, height) = terminal::size().unwrap_or((80, 24));
        let w = width as usize;

        self.max_visible = max_dialog_items(start_row, height).min(self.procs.len().max(1));
        if self.procs.is_empty() {
            self.selected = 0;
            self.scroll_offset = 0;
        } else {
            self.selected = self.selected.min(self.procs.len().saturating_sub(1));
            self.scroll_offset = self
                .scroll_offset
                .min(self.procs.len().saturating_sub(self.max_visible));
        }

        // total_rows = bar(1) + title(1) + items + blank(1) + footer(1)
        let item_rows = if self.procs.is_empty() {
            1
        } else {
            self.max_visible as u16
        };
        let total_rows = item_rows + 4;

        begin_dialog_draw(&mut out, start_row);

        draw_bar(&mut out, w, None, None, theme::ACCENT);
        crlf(&mut out);

        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(Print(" Background Processes"));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        crlf(&mut out);

        if self.procs.is_empty() {
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let _ = out.queue(Print("  No processes"));
            let _ = out.queue(SetAttribute(Attribute::Reset));
            crlf(&mut out);
        } else {
            let end = (self.scroll_offset + self.max_visible).min(self.procs.len());
            for (i, proc) in self
                .procs
                .iter()
                .enumerate()
                .take(end)
                .skip(self.scroll_offset)
            {
                let time = format_duration(now.duration_since(proc.started_at).as_secs());
                let meta = format!(" {time} {}", proc.id);
                let meta_len = meta.chars().count() + 1;
                let max_cmd = w.saturating_sub(meta_len + 4);
                let cmd_display = truncate_str_local(&proc.command, max_cmd);
                if i == self.selected {
                    let _ = out.queue(Print("  "));
                    let _ = out.queue(SetForegroundColor(theme::ACCENT));
                    let _ = out.queue(Print(&cmd_display));
                    let _ = out.queue(ResetColor);
                } else {
                    let _ = out.queue(Print("  "));
                    let _ = out.queue(Print(&cmd_display));
                }
                let _ = out.queue(Print(" "));
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(format!("{time} {}", proc.id)));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                crlf(&mut out);
            }
        }

        crlf(&mut out);
        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(Print(" esc: close  k: kill selected"));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        end_dialog_draw(&mut out, start_row, total_rows, height)
    }
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

    /// Render the dialog inline at `start_row`, pushing content into
    /// scrollback if needed. Returns how many rows the terminal scrolled.
    pub fn draw(&mut self, start_row: u16) -> u16 {
        if !self.dirty {
            return 0;
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

        let q = &self.questions[self.active_tab];
        let suffix_len = if q.multi_select {
            " (space to toggle)".len()
        } else {
            0
        };
        let q_rows = wrap_line(&q.question, w.saturating_sub(1 + suffix_len)).len() as u16;
        // 1=bar, 1=blank, q_rows=question, 1=blank, 2=other+bottom
        let fixed_rows =
            1 + (self.has_tabs as u16) + 1 + q_rows + 1 + self.max_options as u16 + 2 + ta_extra;
        let bar_row = start_row;

        let _ = out.queue(cursor::MoveTo(0, bar_row));
        let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));

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

        let sel = self.selections[self.active_tab];
        let is_multi = q.multi_select;
        let other_idx = q.options.len();

        crlf(&mut out);
        row += 1;

        let suffix = if is_multi { " (space to toggle)" } else { "" };
        let q_max = w.saturating_sub(1 + suffix.len());
        let segments = wrap_line(&q.question, q_max);
        for (i, seg) in segments.iter().enumerate() {
            let _ = out.queue(Print(" "));
            let _ = out.queue(SetAttribute(Attribute::Bold));
            let _ = out.queue(Print(seg));
            let _ = out.queue(SetAttribute(Attribute::Reset));
            if i == 0 && !suffix.is_empty() {
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(suffix));
                let _ = out.queue(SetAttribute(Attribute::Reset));
            }
            crlf(&mut out);
            row += 1;
        }

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

            if is_current && !opt.description.is_empty() {
                let prefix_len = if is_multi {
                    2 + 2 // "  ◉ "
                } else {
                    2 + format!("{}.", i + 1).len() + 1 // "  N. "
                };
                let used = prefix_len + opt.label.chars().count() + 2; // "  " gap
                let remaining = w.saturating_sub(used);
                if remaining > 3 {
                    let desc: String = opt.description.chars().take(remaining).collect();
                    let _ = out.queue(SetAttribute(Attribute::Dim));
                    let _ = out.queue(Print(format!("  {desc}")));
                    let _ = out.queue(SetAttribute(Attribute::Reset));
                }
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

        (bar_row + fixed_rows).saturating_sub(height)
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
    let raw = if !is_junk_title(&entry.title) {
        &entry.title
    } else if let Some(ref sub) = entry.subtitle {
        if !is_junk_title(sub) {
            sub
        } else {
            return "Untitled".into();
        }
    } else {
        return "Untitled".into();
    };
    raw.lines().next().unwrap_or("Untitled").trim().to_string()
}

fn resume_ts(entry: &ResumeEntry) -> u64 {
    if entry.updated_at_ms > 0 {
        entry.updated_at_ms
    } else {
        entry.created_at_ms
    }
}

fn filter_resume_entries(
    entries: &[ResumeEntry],
    query: &str,
    workspace_only: bool,
    current_cwd: &str,
) -> Vec<ResumeEntry> {
    let q = query.to_lowercase();
    entries
        .iter()
        .filter(|e| {
            if workspace_only {
                match e.cwd {
                    Some(ref cwd) => cwd == current_cwd,
                    None => false,
                }
            } else {
                true
            }
        })
        .filter(|e| {
            if q.is_empty() {
                return true;
            }
            let mut hay = resume_title(e);
            if let Some(ref subtitle) = e.subtitle {
                hay.push(' ');
                hay.push_str(subtitle);
            }
            crate::fuzzy::fuzzy_match(&hay, &q)
        })
        .cloned()
        .collect()
}

fn truncate_str_local(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut truncated: String = s.chars().take(max.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

// ── HelpDialog ────────────────────────────────────────────────────────────────

pub struct HelpDialog {
    dirty: bool,
}

impl HelpDialog {
    pub fn new() -> Self {
        Self { dirty: true }
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn handle_resize(&mut self) {
        self.dirty = true;
    }

    /// Returns true when the dialog should close.
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            return true;
        }
        matches!(
            code,
            KeyCode::Esc
                | KeyCode::Enter
                | KeyCode::Char('q')
                | KeyCode::Char('?')
        )
    }

    pub fn draw(&mut self, start_row: u16) -> u16 {
        if !self.dirty {
            return 0;
        }
        self.dirty = false;

        let mut out = io::stdout();
        let (width, height) = terminal::size().unwrap_or((80, 24));
        let w = width as usize;

        // Each section renders as a heading row followed by entry rows.
        // Entries use the same draw_menu_row style as the model/settings pickers:
        //   "  label<padding>dim detail"
        let sections: &[(&str, &[(&str, &str)])] = &[
            (
                "prefixes",
                &[
                    ("/command", "slash commands  (try /resume, /compact, /fork, /ps, /vim…)"),
                    ("@<path>", "attach a file or URL"),
                    ("!<cmd>", "run a shell command"),
                ],
            ),
            (
                "keys",
                &[
                    ("enter", "send message"),
                    ("ctrl+j  shift+enter", "insert newline"),
                    ("ctrl+c", "cancel / interrupt"),
                    ("ctrl+r", "search input history"),
                    ("ctrl+t", "cycle reasoning effort"),
                    ("shift+tab", "cycle mode  (normal → plan → apply → yolo)"),
                    ("ctrl+u / ctrl+d", "scroll up / down"),
                    ("ctrl+a / ctrl+e", "line start / end"),
                    ("ctrl+w  alt+bs", "delete word backward"),
                    ("tab", "autocomplete"),
                    ("esc  ctrl+c", "cancel / close dialog"),
                ],
            ),
        ];

        // Compute label column width (same as draw_menu_row: label + 4 spaces min gap).
        let label_col = sections
            .iter()
            .flat_map(|(_, entries)| entries.iter().map(|(k, _)| k.len()))
            .max()
            .unwrap_or(0)
            + 4;

        // Count total display rows: per section = entries + 1 blank.
        let content_rows: usize = sections
            .iter()
            .map(|(_, entries)| entries.len() + 1)
            .sum::<usize>()
            .saturating_sub(1); // no trailing blank after last section

        // bar(1) + title(1) + blank(1) + content
        let total_rows = (1 + 1 + 1 + content_rows) as u16;

        begin_dialog_draw(&mut out, start_row);

        draw_bar(&mut out, w, None, None, super::theme::ACCENT);
        crlf(&mut out);

        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(Print(" help"));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        crlf(&mut out);
        crlf(&mut out);

        for (si, (_, entries)) in sections.iter().enumerate() {
            for (label, detail) in entries.iter() {
                let _ = out.queue(Print("  "));
                let _ = out.queue(SetForegroundColor(super::theme::MUTED));
                let _ = out.queue(Print(label));
                let _ = out.queue(ResetColor);
                let padding = " ".repeat(label_col.saturating_sub(label.len()));
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(format!("{padding}{detail}")));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
                crlf(&mut out);
            }

            // blank line between sections (not after last)
            if si + 1 < sections.len() {
                crlf(&mut out);
            }
        }

        end_dialog_draw(&mut out, start_row, total_rows, height)
    }
}
