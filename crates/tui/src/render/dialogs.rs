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
use std::io::Write;
use std::time::Instant;

use super::blocks::wrap_line;
use super::highlight::{count_inline_diff_rows, print_inline_diff, print_syntax_file};
use super::{chunk_line, crlf, draw_bar, ConfirmChoice, RenderOut, ResumeEntry};

// ── ListState ────────────────────────────────────────────────────────────────

/// Shared state for scrollable list dialogs.
pub(super) struct ListState {
    pub selected: usize,
    pub scroll_offset: usize,
    pub max_visible: usize,
    max_height: Option<u16>,
    overhead: u16,
    pub anchor_row: Option<u16>,
    pub dirty: bool,
}

impl ListState {
    fn new(item_count: usize, max_height: Option<u16>, overhead: u16) -> Self {
        let max_visible = Self::cap(max_height, overhead, item_count);
        Self {
            selected: 0,
            scroll_offset: 0,
            max_visible,
            max_height,
            overhead,
            anchor_row: None,
            dirty: true,
        }
    }

    fn cap(max_height: Option<u16>, overhead: u16, item_count: usize) -> usize {
        max_height
            .map(|h| (h as usize).saturating_sub(overhead as usize))
            .unwrap_or(usize::MAX)
            .min(item_count)
    }

    /// Recompute after the item list changes.
    pub fn set_items(&mut self, count: usize) {
        self.max_visible = Self::cap(self.max_height, self.overhead, count);
        if count == 0 {
            self.selected = 0;
            self.scroll_offset = 0;
        } else {
            self.selected = self.selected.min(count - 1);
            self.scroll_offset = self
                .scroll_offset
                .min(count.saturating_sub(self.max_visible));
        }
        self.dirty = true;
    }

    pub fn handle_resize(&mut self) {
        self.anchor_row = None;
        self.dirty = true;
    }

    pub fn select_prev(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            if self.selected < self.scroll_offset {
                self.scroll_offset = self.selected;
            }
            self.dirty = true;
        }
    }

    pub fn select_next(&mut self, item_count: usize) {
        if self.selected + 1 < item_count {
            self.selected += 1;
            if self.selected >= self.scroll_offset + self.max_visible {
                self.scroll_offset = self.selected + 1 - self.max_visible;
            }
            self.dirty = true;
        }
    }

    pub fn page_up(&mut self) {
        let half = self.max_visible / 2;
        self.selected = self.selected.saturating_sub(half.max(1));
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }
        self.dirty = true;
    }

    pub fn page_down(&mut self, item_count: usize) {
        let half = self.max_visible / 2;
        self.selected = (self.selected + half.max(1)).min(item_count.saturating_sub(1));
        if self.selected >= self.scroll_offset + self.max_visible {
            self.scroll_offset = self.selected + 1 - self.max_visible;
        }
        self.dirty = true;
    }

    /// Begin drawing. Returns `(RenderOut, width, bar_row)` or `None` if not dirty.
    pub fn begin_draw(
        &mut self,
        start_row: u16,
        item_count: usize,
    ) -> Option<(RenderOut, usize, u16)> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;

        let mut out = RenderOut::scroll();
        let (width, height) = terminal::size().unwrap_or((80, 24));

        let wanted_rows = (item_count as u16).saturating_add(self.overhead);
        let (bar_row, granted) = begin_dialog_draw(
            &mut out,
            start_row,
            wanted_rows,
            height,
            self.max_height,
            &mut self.anchor_row,
        );
        self.max_visible = (granted as usize)
            .saturating_sub(self.overhead as usize)
            .min(item_count);
        if item_count == 0 {
            self.selected = 0;
            self.scroll_offset = 0;
        } else {
            self.selected = self.selected.min(item_count - 1);
            self.scroll_offset = self
                .scroll_offset
                .min(item_count.saturating_sub(self.max_visible));
        }

        Some((out, width as usize, bar_row))
    }

    pub fn visible_range(&self, item_count: usize) -> std::ops::Range<usize> {
        let end = (self.scroll_offset + self.max_visible).min(item_count);
        self.scroll_offset..end
    }
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
    out: &mut RenderOut,
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

/// Begin a dialog frame: compute where the dialog starts, position the
/// cursor there, and switch to overlay mode.
///
/// `content_rows` is the number of rows the dialog actually needs.
/// `max_rows` is an optional cap (e.g. `Some(height/2)` for scrollable list
/// dialogs).  When `None`, the dialog uses its full content height.
///
/// If the (possibly capped) height fits below `start_row`, the dialog draws
/// at the prompt row.  Otherwise lines are pushed into scrollback via
/// `ScrollUp` to make room.
///
/// Returns `(bar_row, granted_rows)` — the row where the dialog starts and
/// how many rows it may use.
fn begin_dialog_draw(
    out: &mut RenderOut,
    start_row: u16,
    content_rows: u16,
    height: u16,
    max_rows: Option<u16>,
    anchor_row: &mut Option<u16>,
) -> (u16, u16) {
    let _ = out.queue(terminal::BeginSynchronizedUpdate);
    let _ = out.queue(cursor::Hide);

    // Apply cap.
    let granted = if let Some(cap) = max_rows {
        content_rows.min(cap)
    } else {
        content_rows
    };
    // Never exceed terminal height.
    let granted = granted.min(height);

    let bar_row = if let Some(anchor) = *anchor_row {
        anchor
    } else {
        // First draw: compute where the dialog starts.
        let available = height.saturating_sub(start_row);
        let row = if granted <= available {
            // Dialog fits below prompt — draw at prompt row.
            start_row
        } else {
            // Doesn't fit: push lines into scrollback to make room.
            let deficit = granted.saturating_sub(available);
            let _ = out.queue(terminal::ScrollUp(deficit));
            height.saturating_sub(granted)
        };
        *anchor_row = Some(row);
        row
    };

    let _ = out.queue(cursor::MoveTo(0, bar_row));
    out.row = Some(bar_row);
    (bar_row, granted)
}

/// End a dialog frame: clear remainder, end sync update, flush.
fn end_dialog_draw(out: &mut RenderOut) {
    let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
    let _ = out.queue(terminal::EndSynchronizedUpdate);
    let _ = out.flush();
}

/// Finish a dialog frame: optionally show cursor, end synchronized update, flush.
fn finish_dialog_frame(out: &mut RenderOut, cursor_pos: Option<(u16, u16)>, editing: bool) {
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

    pub fn draw(&mut self, start_row: u16) {
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
        let has_preview = self.total_preview > 0;
        // Fixed rows: bar + title + summary + separators(if preview) +
        //             "Allow?" + options + ta_extra + blank + hint
        let fixed_rows: u16 = 1
            + title_rows
            + summary_rows
            + if has_preview { 2 } else { 0 }
            + 1
            + self.options.len() as u16
            + ta_extra
            + 2;

        // When there is preview content, allow the dialog to use the full
        // terminal height by scrolling conversation content into scrollback.
        let viewport_rows: u16 = if has_preview {
            let space = height.saturating_sub(fixed_rows);
            space.max(1).min(self.total_preview)
        } else {
            0
        };

        // Clamp scroll
        let max_scroll = (self.total_preview as usize).saturating_sub(viewport_rows as usize);
        self.preview_scroll = self.preview_scroll.min(max_scroll);

        let total_rows = fixed_rows + viewport_rows;

        let (bar_row, _) = begin_dialog_draw(
            &mut out,
            start_row,
            total_rows,
            height,
            None,
            &mut self.anchor_row,
        );

        engine::log::entry(
            engine::log::Level::Debug,
            &format!(
                "ConfirmDialog: bar_row={bar_row} total_rows={total_rows} fixed={fixed_rows} viewport={viewport_rows} preview={}"
                , self.total_preview
            ),
            &"",
        );

        // Where the options section should begin in the current layout.
        let expected_options_row = bar_row
            + 1
            + title_rows
            + summary_rows
            + if has_preview { 2 + viewport_rows } else { 0 }
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

            // title — wrap long commands with a leading space on continuation lines
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

            if has_preview {
                let separator: String = "╌".repeat(w);
                // Top separator — show scroll position when clipped
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
                    viewport_rows,
                );
                row += viewport_rows;
                // Bottom separator — show scroll indicator when content is clipped
                let _ = out.queue(SetForegroundColor(theme::BAR));
                if self.total_preview > viewport_rows {
                    let pos = format!(
                        " [{}/{}]",
                        self.preview_scroll + viewport_rows as usize,
                        self.total_preview
                    );
                    let sep_len = w.saturating_sub(pos.len());
                    let _ = out.queue(Print("╌".repeat(sep_len)));
                    let _ = out.queue(SetForegroundColor(theme::MUTED));
                    let _ = out.queue(Print(&pos));
                } else {
                    let _ = out.queue(Print(&separator));
                }
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
        // the bottom row — clearing there wipes the last visible option.
        if out.row.is_some_and(|r| r < height) {
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
        }

        finish_dialog_frame(&mut out, cursor_pos, self.editing);
    }
}

// ── RewindDialog ─────────────────────────────────────────────────────────────

pub struct RewindDialog {
    turns: Vec<(usize, String)>,
    list: ListState,
    pub restore_vim_insert: bool,
}

impl RewindDialog {
    pub fn new(
        turns: Vec<(usize, String)>,
        restore_vim_insert: bool,
        max_height: Option<u16>,
    ) -> Self {
        let mut list = ListState::new(turns.len(), max_height, 4);
        list.selected = turns.len().saturating_sub(1);
        list.scroll_offset = turns.len().saturating_sub(list.max_visible);
        Self {
            turns,
            list,
            restore_vim_insert,
        }
    }

    pub fn mark_dirty(&mut self) {
        self.list.dirty = true;
    }

    pub fn anchor_row(&self) -> Option<u16> {
        self.list.anchor_row
    }

    pub fn handle_resize(&mut self) {
        self.list.handle_resize();
    }

    /// Returns `Some(Some(idx))` when the user selects an entry, `Some(None)` on cancel.
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Option<Option<usize>> {
        match code {
            KeyCode::Enter => return Some(Some(self.turns[self.list.selected].0)),
            KeyCode::Esc => return Some(None),
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => return Some(None),
            KeyCode::Up | KeyCode::Char('k') => self.list.select_prev(),
            KeyCode::Down | KeyCode::Char('j') => self.list.select_next(self.turns.len()),
            _ => {}
        }
        None
    }

    pub fn draw(&mut self, start_row: u16) {
        let Some((mut out, w, _)) = self.list.begin_draw(start_row, self.turns.len()) else {
            return;
        };

        draw_bar(&mut out, w, None, None, theme::accent());
        crlf(&mut out);

        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(Print(" Rewind to:"));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        crlf(&mut out);

        for (i, (_, ref full_text)) in self
            .turns
            .iter()
            .enumerate()
            .take(self.list.visible_range(self.turns.len()).end)
            .skip(self.list.visible_range(self.turns.len()).start)
        {
            let label = full_text.lines().next().unwrap_or("");
            let num = i + 1;
            let max_label = w.saturating_sub(8);
            let truncated = truncate_str_local(label, max_label);
            let _ = out.queue(Print("  "));
            if i == self.list.selected {
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(format!("{}.", num)));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let _ = out.queue(Print(" "));
                let _ = out.queue(SetForegroundColor(theme::accent()));
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
        end_dialog_draw(&mut out);
    }
}

// ── ResumeDialog ──────────────────────────────────────────────────────────────

pub struct ResumeDialog {
    entries: Vec<ResumeEntry>,
    current_cwd: String,
    query: String,
    workspace_only: bool,
    filtered: Vec<ResumeEntry>,
    list: ListState,
    pending_d: bool,
    last_drawn: Instant,
}

impl ResumeDialog {
    pub fn new(entries: Vec<ResumeEntry>, current_cwd: String, max_height: Option<u16>) -> Self {
        let filtered = filter_resume_entries(&entries, "", true, &current_cwd);
        let list = ListState::new(filtered.len().max(1), max_height, 4);
        Self {
            entries,
            current_cwd,
            query: String::new(),
            workspace_only: true,
            filtered,
            list,
            pending_d: false,
            last_drawn: Instant::now(),
        }
    }

    pub fn mark_dirty(&mut self) {
        self.list.dirty = true;
    }

    pub fn anchor_row(&self) -> Option<u16> {
        self.list.anchor_row
    }

    fn refilter(&mut self) {
        self.filtered = filter_resume_entries(
            &self.entries,
            &self.query,
            self.workspace_only,
            &self.current_cwd,
        );
        self.list.set_items(self.filtered.len().max(1));
    }

    pub fn handle_resize(&mut self) {
        self.list.handle_resize();
        self.refilter();
    }

    /// Returns `Some(Some(id))` on selection, `Some(None)` on cancel.
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Option<Option<String>> {
        // Check for DD completion before anything else.
        if self.pending_d {
            self.pending_d = false;
            if code == KeyCode::Char('d') && mods == KeyModifiers::NONE {
                self.delete_selected();
                return None;
            }
            // 'd' followed by something else: insert both as query chars.
            self.query.push('d');
            // Fall through to handle the current key normally.
        }

        match (code, mods) {
            (KeyCode::Enter, _) => {
                return Some(self.filtered.get(self.list.selected).map(|e| e.id.clone()));
            }
            (KeyCode::Esc, _) => return Some(None),
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => return Some(None),
            (KeyCode::Char('w'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.workspace_only = !self.workspace_only;
                self.refilter();
            }
            (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
                if !self.filtered.is_empty() {
                    self.list.page_up();
                }
            }
            (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                if !self.filtered.is_empty() {
                    self.list.page_down(self.filtered.len());
                }
            }
            (KeyCode::Backspace, m)
                if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
            {
                if self.query.is_empty() {
                    self.delete_selected();
                } else {
                    let len = self.query.len();
                    let target = crate::vim::word_backward_pos(
                        &self.query,
                        len,
                        crate::vim::CharClass::Word,
                    );
                    self.query.truncate(target);
                    self.refilter();
                }
            }
            (KeyCode::Backspace, _) => {
                if self.query.is_empty() {
                    self.delete_selected();
                } else {
                    self.query.pop();
                    self.refilter();
                }
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                self.list.select_prev();
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                self.list.select_next(self.filtered.len());
            }
            (KeyCode::Char('d'), KeyModifiers::NONE) if self.query.is_empty() => {
                self.pending_d = true;
                self.list.dirty = true;
            }
            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                self.query.push(c);
                self.refilter();
            }
            _ => {}
        }
        None
    }

    fn delete_selected(&mut self) {
        if let Some(entry) = self.filtered.get(self.list.selected) {
            let id = entry.id.clone();
            session::delete(&id);
            self.entries.retain(|e| e.id != id);
            self.refilter();
        }
    }

    pub fn draw(&mut self, start_row: u16) {
        if !self.list.dirty {
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
                self.list.dirty = true;
            }
        }
        if !self.list.dirty {
            return;
        }
        self.last_drawn = Instant::now();

        let Some((mut out, w, _)) = self.list.begin_draw(start_row, self.filtered.len().max(1))
        else {
            return;
        };

        let now_ms = session::now_ms();

        draw_bar(&mut out, w, None, None, theme::accent());
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
            let range = self.list.visible_range(self.filtered.len());
            for (i, entry) in self
                .filtered
                .iter()
                .enumerate()
                .take(range.end)
                .skip(range.start)
            {
                let title = resume_title(entry);
                let time_ago = session::time_ago(resume_ts(entry), now_ms);
                let time_len = time_ago.chars().count() + 1;
                let indent = 2 + entry.depth * 2;
                let indent_str = " ".repeat(indent);
                let max_label = w.saturating_sub(time_len + indent + 2);
                let truncated = truncate_str_local(&title, max_label);
                if i == self.list.selected {
                    let _ = out.queue(Print(&indent_str));
                    let _ = out.queue(SetForegroundColor(theme::accent()));
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
        end_dialog_draw(&mut out);
    }
}

// ── PsDialog ──────────────────────────────────────────────────────────────────

pub struct PsDialog {
    registry: engine::tools::ProcessRegistry,
    procs: Vec<ProcessInfo>,
    list: ListState,
    killed: Vec<String>,
}

impl PsDialog {
    pub fn new(registry: engine::tools::ProcessRegistry, max_height: Option<u16>) -> Self {
        let procs = Self::fetch_procs(&registry, &[]);
        let list = ListState::new(procs.len().max(1), max_height, 4);
        Self {
            registry,
            procs,
            list,
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
        self.list.dirty = true;
    }

    pub fn anchor_row(&self) -> Option<u16> {
        self.list.anchor_row
    }

    pub fn handle_resize(&mut self) {
        self.list.handle_resize();
    }

    /// Returns `Some(killed_ids)` when the user closes the dialog (may be empty).
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Option<Vec<String>> {
        match (code, mods) {
            (KeyCode::Esc, _) => return Some(self.killed.clone()),
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                return Some(self.killed.clone())
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                self.list.select_prev();
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                self.list.select_next(self.procs.len());
            }
            (KeyCode::Backspace, _) => {
                if let Some(p) = self.procs.get(self.list.selected) {
                    self.killed.push(p.id.clone());
                    self.procs = Self::fetch_procs(&self.registry, &self.killed);
                    self.list.set_items(self.procs.len().max(1));
                }
            }
            _ => {}
        }
        None
    }

    pub fn draw(&mut self, start_row: u16) {
        let fresh = Self::fetch_procs(&self.registry, &self.killed);
        if fresh.len() != self.procs.len() {
            self.list.set_items(fresh.len().max(1));
        }
        self.procs = fresh;

        let Some((mut out, w, _)) = self.list.begin_draw(start_row, self.procs.len().max(1)) else {
            return;
        };
        let now = std::time::Instant::now();

        draw_bar(&mut out, w, None, None, theme::accent());
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
            let range = self.list.visible_range(self.procs.len());
            for (i, proc) in self
                .procs
                .iter()
                .enumerate()
                .take(range.end)
                .skip(range.start)
            {
                let time = format_duration(now.duration_since(proc.started_at).as_secs());
                let meta = format!(" {time} {}", proc.id);
                let meta_len = meta.chars().count() + 1;
                let max_cmd = w.saturating_sub(meta_len + 4);
                let cmd_display = truncate_str_local(&proc.command, max_cmd);
                if i == self.list.selected {
                    let _ = out.queue(Print("  "));
                    let _ = out.queue(SetForegroundColor(theme::accent()));
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
        let _ = out.queue(Print(" esc: close  backspace: kill selected"));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        end_dialog_draw(&mut out);
    }
}

/// Non-blocking question dialog state machine.
pub struct QuestionDialog {
    questions: Vec<Question>,
    has_tabs: bool,
    active_tab: usize,
    selections: Vec<usize>,
    multi_toggles: Vec<Vec<bool>>,
    other_areas: Vec<TextArea>,
    editing_other: Vec<bool>,
    visited: Vec<bool>,
    answered: Vec<bool>,
    dirty: bool,
    /// The anchor row where this dialog is positioned. None on first draw.
    pub anchor_row: Option<u16>,
}

impl QuestionDialog {
    pub fn new(questions: Vec<Question>) -> Self {
        let n = questions.len();
        let has_tabs = n > 1;
        Self {
            multi_toggles: questions
                .iter()
                .map(|q| vec![false; q.options.len() + 1])
                .collect(),
            questions,
            has_tabs,
            active_tab: 0,
            selections: vec![0; n],
            other_areas: (0..n).map(|_| TextArea::new()).collect(),
            editing_other: vec![false; n],
            visited: vec![false; n],
            answered: vec![false; n],
            dirty: true,
            anchor_row: None,
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

    pub fn draw(&mut self, start_row: u16) {
        if !self.dirty {
            return;
        }
        self.dirty = false;

        let mut out = RenderOut::scroll();
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

        let q = &self.questions[self.active_tab];

        // Compute actual content height.
        let ta_extra: u16 = if ta_visible {
            self.other_areas[self.active_tab]
                .visual_row_count(other_wrap_w)
                .saturating_sub(1)
        } else {
            0
        };
        let q_segments = wrap_line(&q.question, w.saturating_sub(1)).len() as u16;
        // bar(1) + tabs?(1) + blank(1) + question + blank(1) + options + other(1) + ta_extra + blank(1) + footer(1)
        let content_rows: u16 = 1
            + if self.has_tabs { 1 } else { 0 }
            + 1
            + q_segments
            + 1
            + q.options.len() as u16
            + 1
            + ta_extra
            + 1
            + 1;
        let (bar_row, _) = begin_dialog_draw(
            &mut out,
            start_row,
            content_rows,
            height,
            None,
            &mut self.anchor_row,
        );
        let mut row = bar_row;

        draw_bar(&mut out, w, None, None, theme::accent());
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
                    let _ = out.queue(SetForegroundColor(theme::accent()));
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
                    let _ = out.queue(SetForegroundColor(theme::accent()));
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
                    let _ = out.queue(SetForegroundColor(theme::accent()));
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
                let _ = out.queue(SetForegroundColor(theme::accent()));
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
                let _ = out.queue(SetForegroundColor(theme::accent()));
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
        // Only clear below the dialog if there's viewport space left.
        // When the dialog fills the full terminal, clearing here wipes
        // the last visible line.
        if out.row.is_some_and(|r| r < height) {
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
        }

        finish_dialog_frame(&mut out, cursor_pos, editing);
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
    list: ListState,
    /// Total content rows (static, computed once).
    total_rows: usize,
}

impl Default for HelpDialog {
    fn default() -> Self {
        Self::new()
    }
}

impl HelpDialog {
    /// Number of content lines in the help sections (3 prefixes + 11 keys + 1 separator).
    const TOTAL_ROWS: usize = 15;

    pub fn new() -> Self {
        Self {
            list: ListState::new(0, None, 3),
            total_rows: Self::TOTAL_ROWS,
        }
    }

    pub fn mark_dirty(&mut self) {
        self.list.dirty = true;
    }

    pub fn anchor_row(&self) -> Option<u16> {
        self.list.anchor_row
    }

    pub fn handle_resize(&mut self) {
        self.list.handle_resize();
    }

    /// Returns true when the dialog should close.
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            return true;
        }
        match code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?') => true,
            KeyCode::Up | KeyCode::Char('k') => {
                if self.list.scroll_offset > 0 {
                    self.list.scroll_offset -= 1;
                    self.list.dirty = true;
                }
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                // Clamp eagerly; draw() will also clamp but this prevents
                // unbounded accumulation from rapid key presses.
                let max_scroll = self.total_rows.saturating_sub(self.list.max_visible);
                if self.list.scroll_offset < max_scroll {
                    self.list.scroll_offset += 1;
                    self.list.dirty = true;
                }
                false
            }
            _ => false,
        }
    }

    pub fn draw(&mut self, start_row: u16) {
        // Each section renders as a heading row followed by entry rows.
        let sections: &[(&str, &[(&str, &str)])] = &[
            (
                "prefixes",
                &[
                    (
                        "/command",
                        "slash commands  (try /resume, /compact, /fork, /ps, /vim…)",
                    ),
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

        let label_col = sections
            .iter()
            .flat_map(|(_, entries)| entries.iter().map(|(k, _)| k.len()))
            .max()
            .unwrap_or(0)
            + 4;

        // Collect content lines for scrolling
        let mut content_lines: Vec<(&str, &str)> = Vec::new();
        for (si, (_, entries)) in sections.iter().enumerate() {
            for &(label, detail) in *entries {
                content_lines.push((label, detail));
            }
            if si + 1 < sections.len() {
                content_lines.push(("", "")); // blank separator
            }
        }
        let total_content = content_lines.len();

        let Some((mut out, w, _)) = self.list.begin_draw(start_row, total_content) else {
            return;
        };
        let max_visible = self.list.max_visible;

        // Clamp scroll
        let max_scroll = total_content.saturating_sub(max_visible);
        self.list.scroll_offset = self.list.scroll_offset.min(max_scroll);

        draw_bar(&mut out, w, None, None, super::theme::accent());
        crlf(&mut out);

        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(Print(" help"));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        crlf(&mut out);
        crlf(&mut out);

        for &(label, detail) in content_lines
            .iter()
            .skip(self.list.scroll_offset)
            .take(max_visible)
        {
            if label.is_empty() && detail.is_empty() {
                crlf(&mut out);
            } else {
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
        }

        end_dialog_draw(&mut out);
    }
}
