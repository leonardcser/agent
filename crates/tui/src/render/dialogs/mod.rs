mod agents;
mod confirm;
mod export;
mod help;
mod permissions;
mod ps;
mod question;
mod resume;
mod rewind;

pub use agents::{AgentSnapshot, AgentsDialog, SharedSnapshots};
pub use confirm::ConfirmDialog;
pub use export::{ExportDialog, ExportTarget};
pub use help::HelpDialog;
pub use permissions::{PermissionEntry, PermissionsDialog};
pub use ps::PsDialog;
pub use question::{parse_questions, Question, QuestionDialog, QuestionOption};
pub use resume::ResumeDialog;
pub use rewind::RewindDialog;

use crossterm::event::{KeyCode, KeyModifiers};
use crossterm::{cursor, terminal, QueueableCommand};

use super::{draw_soft_cursor, truncate_str, wrap_line, ConfirmChoice, RenderOut};

pub enum DialogResult {
    Dismissed,
    Confirm {
        choice: ConfirmChoice,
        message: Option<String>,
        tool_name: String,
        request_id: u64,
    },
    Question {
        answer: Option<String>,
        request_id: u64,
    },
    Rewind {
        block_idx: Option<usize>,
        restore_vim_insert: bool,
    },
    Resume {
        session_id: Option<String>,
    },
    Export {
        target: Option<ExportTarget>,
    },
    PsClosed,
    PermissionsClosed {
        session_remaining: Vec<PermissionEntry>,
        workspace_remaining: Vec<crate::workspace_permissions::Rule>,
    },
    AgentsClosed,
}

pub trait Dialog {
    /// Whether the agent is blocked on a reply for this dialog.
    fn blocks_agent(&self) -> bool {
        false
    }
    fn height(&self) -> u16;
    fn mark_dirty(&mut self);
    /// Render the dialog. `granted_rows` is the exact row budget from
    /// draw_frame — the dialog must not exceed `start_row + granted_rows`.
    fn draw(&mut self, out: &mut RenderOut, start_row: u16, width: u16, granted_rows: u16);
    fn handle_resize(&mut self);
    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Option<DialogResult>;

    /// Whether the layout engine should apply a dynamic height cap
    /// (`max(h/2, natural_space)`) to limit scroll-up.  List-based
    /// dialogs return true; confirm/question dialogs return false.
    fn constrain_height(&self) -> bool {
        false
    }

    /// Seed the dialog's kill ring from the main input's kill ring.
    fn set_kill_ring(&mut self, _contents: String) {}
    /// Retrieve the dialog's kill ring so the main input can sync it back.
    fn kill_ring(&self) -> Option<&str> {
        None
    }
}

pub(crate) struct ListState {
    pub selected: usize,
    pub scroll_offset: usize,
    pub max_visible: usize,
    pub dirty: bool,
}

impl ListState {
    pub fn new(item_count: usize) -> Self {
        Self {
            selected: 0,
            scroll_offset: 0,
            max_visible: item_count,
            dirty: true,
        }
    }

    /// Report the dialog's total desired height.  `chrome` is the
    /// number of non-item rows the dialog will render (bar, header,
    /// hints, etc.) — computed by the caller, not stored.  The layout
    /// engine caps this to the actually available space.
    pub fn height(&self, item_count: usize, chrome: u16) -> u16 {
        (item_count as u16).saturating_add(chrome)
    }

    pub fn set_items(&mut self, count: usize) {
        self.max_visible = count;
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
        self.dirty = true;
    }

    pub fn select_prev(&mut self, item_count: usize) {
        if item_count == 0 || self.selected == 0 {
            return;
        }
        self.selected -= 1;
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }
        self.dirty = true;
    }

    pub fn select_next(&mut self, item_count: usize) {
        if item_count == 0 || self.selected + 1 >= item_count {
            return;
        }
        self.selected += 1;
        if self.selected >= self.scroll_offset + self.max_visible {
            self.scroll_offset = self.selected + 1 - self.max_visible;
        }
        self.dirty = true;
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

    /// Handle the four navigation arrows (Up/Down/PageUp/PageDown) uniformly.
    /// Returns `true` if the action was consumed, `false` otherwise so callers
    /// can fall through to dialog-specific handling (Dismiss, Confirm, Edit).
    pub fn handle_nav(&mut self, action: crate::keymap::NavAction, item_count: usize) -> bool {
        use crate::keymap::NavAction;
        match action {
            NavAction::Up => self.select_prev(item_count),
            NavAction::Down => self.select_next(item_count),
            NavAction::PageUp => self.page_up(),
            NavAction::PageDown => self.page_down(item_count),
            _ => return false,
        }
        true
    }

    /// Begin drawing the dialog.  `chrome` is the number of non-item
    /// rows the caller will render (bar, header, blanks, hints).
    /// `max_visible` is recomputed from `granted_rows - chrome`.
    pub fn begin_draw(
        &mut self,
        out: &mut RenderOut,
        start_row: u16,
        item_count: usize,
        width: u16,
        granted_rows: u16,
        chrome: u16,
    ) -> Option<usize> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;

        begin_dialog_draw(out, start_row);
        self.max_visible = (granted_rows as usize)
            .saturating_sub(chrome as usize)
            .min(item_count);
        if item_count == 0 {
            self.selected = 0;
            self.scroll_offset = 0;
        } else {
            self.selected = self.selected.min(item_count - 1);
            self.scroll_offset = self
                .scroll_offset
                .min(item_count.saturating_sub(self.max_visible));
            // Ensure the selected item stays visible after recalculation.
            if self.max_visible > 0 && self.selected >= self.scroll_offset + self.max_visible {
                self.scroll_offset = self.selected + 1 - self.max_visible;
            }
        }

        Some(width as usize)
    }

    pub fn visible_range(&self, item_count: usize) -> std::ops::Range<usize> {
        let end = (self.scroll_offset + self.max_visible).min(item_count);
        self.scroll_offset..end
    }
}

pub(crate) struct TextArea {
    pub lines: Vec<String>,
    pub row: usize,
    pub col: usize,
}

impl TextArea {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn visual_row_count(&self, wrap_w: usize) -> u16 {
        self.lines
            .iter()
            .map(|l| wrap_line(l, wrap_w).len() as u16)
            .sum()
    }

    pub fn wrap(&self, wrap_w: usize) -> (Vec<String>, (usize, usize)) {
        let mut visual = Vec::new();
        let mut cursor = (0, 0);

        for (li, line) in self.lines.iter().enumerate() {
            let vis_start = visual.len();
            let chunks = wrap_line(line, wrap_w);
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

    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
    }

    pub fn insert_char(&mut self, c: char) {
        let byte = char_to_byte(&self.lines[self.row], self.col);
        self.lines[self.row].insert(byte, c);
        self.col += 1;
    }

    pub fn insert_newline(&mut self) {
        let byte = char_to_byte(&self.lines[self.row], self.col);
        let rest = self.lines[self.row][byte..].to_string();
        self.lines[self.row].truncate(byte);
        self.row += 1;
        self.col = 0;
        self.lines.insert(self.row, rest);
    }

    pub fn backspace(&mut self) {
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

    pub fn delete_word_backward(&mut self) {
        if self.col == 0 {
            return;
        }
        let line = &self.lines[self.row];
        let byte_pos = char_to_byte(line, self.col);
        let target = crate::text_utils::word_backward_pos(
            line,
            byte_pos,
            crate::text_utils::CharClass::Word,
        );
        let target_col = line[..target].chars().count();
        let end_byte = char_to_byte(&self.lines[self.row], self.col);
        self.lines[self.row].drain(target..end_byte);
        self.col = target_col;
    }

    pub fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
        }
    }

    pub fn move_right(&mut self) {
        let len = self.lines[self.row].chars().count();
        if self.col < len {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.lines[self.row].chars().count());
        }
    }

    pub fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.lines[self.row].chars().count());
        }
    }

    pub fn move_home(&mut self) {
        self.col = 0;
    }

    pub fn move_end(&mut self) {
        self.col = self.lines[self.row].chars().count();
    }

    pub fn move_word_forward(&mut self) {
        let line = &self.lines[self.row];
        let len = line.chars().count();
        if self.col >= len {
            if self.row + 1 < self.lines.len() {
                self.row += 1;
                self.col = 0;
            }
            return;
        }
        let byte = char_to_byte(line, self.col);
        let target =
            crate::text_utils::word_forward_pos(line, byte, crate::text_utils::CharClass::Word);
        self.col = line[..target].chars().count();
    }

    pub fn move_word_backward(&mut self) {
        if self.col == 0 {
            if self.row > 0 {
                self.row -= 1;
                self.col = self.lines[self.row].chars().count();
            }
            return;
        }
        let line = &self.lines[self.row];
        let byte = char_to_byte(line, self.col);
        let target =
            crate::text_utils::word_backward_pos(line, byte, crate::text_utils::CharClass::Word);
        self.col = line[..target].chars().count();
    }

    pub fn delete_char_forward(&mut self) {
        let len = self.lines[self.row].chars().count();
        if self.col < len {
            let byte = char_to_byte(&self.lines[self.row], self.col);
            self.lines[self.row].remove(byte);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    pub fn delete_word_forward(&mut self) {
        let line = &self.lines[self.row];
        let len = line.chars().count();
        if self.col >= len {
            return;
        }
        let byte = char_to_byte(line, self.col);
        let target =
            crate::text_utils::word_forward_pos(line, byte, crate::text_utils::CharClass::Word);
        self.lines[self.row].drain(byte..target);
    }

    pub fn kill_to_end_of_line(&mut self, kill_ring: &mut String) {
        let line = &self.lines[self.row];
        let byte = char_to_byte(line, self.col);
        *kill_ring = line[byte..].to_string();
        self.lines[self.row].truncate(byte);
    }

    pub fn kill_to_start_of_line(&mut self, kill_ring: &mut String) {
        let byte = char_to_byte(&self.lines[self.row], self.col);
        *kill_ring = self.lines[self.row][..byte].to_string();
        self.lines[self.row].drain(..byte);
        self.col = 0;
    }

    pub fn delete_to_start_of_line(&mut self) {
        let byte = char_to_byte(&self.lines[self.row], self.col);
        self.lines[self.row].drain(..byte);
        self.col = 0;
    }

    pub fn yank(&mut self, kill_ring: &str) {
        if !kill_ring.is_empty() {
            let byte = char_to_byte(&self.lines[self.row], self.col);
            self.lines[self.row].insert_str(byte, kill_ring);
            self.col += kill_ring.chars().count();
        }
    }

    pub fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        // Note: kill/yank need external kill_ring; handled via handle_key_with_kill_ring.
        match (code, modifiers) {
            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => self.insert_char(c),
            (KeyCode::Enter, _) => self.insert_newline(),
            // Ctrl+A: start of line
            (KeyCode::Char('a'), m) if m.contains(KeyModifiers::CONTROL) => self.move_home(),
            // Ctrl+E: end of line
            (KeyCode::Char('e'), m) if m.contains(KeyModifiers::CONTROL) => self.move_end(),
            // Ctrl+F: char forward
            (KeyCode::Char('f'), m) if m.contains(KeyModifiers::CONTROL) => self.move_right(),
            // Ctrl+B: char backward
            (KeyCode::Char('b'), m) if m.contains(KeyModifiers::CONTROL) => self.move_left(),
            // Alt+F: word forward
            (KeyCode::Char('f'), m) if m.contains(KeyModifiers::ALT) => self.move_word_forward(),
            // Alt+B: word backward
            (KeyCode::Char('b'), m) if m.contains(KeyModifiers::ALT) => self.move_word_backward(),
            // Ctrl+D: delete char forward
            (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.delete_char_forward()
            }
            // Alt+D: delete word forward
            (KeyCode::Char('d'), m) if m.contains(KeyModifiers::ALT) => self.delete_word_forward(),
            // Ctrl+W: delete word backward
            (KeyCode::Char('w'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.delete_word_backward()
            }
            (KeyCode::Backspace, m)
                if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
            {
                self.delete_word_backward()
            }
            // Cmd+Backspace: delete to start of line
            (KeyCode::Backspace, m) if m.contains(KeyModifiers::SUPER) => {
                self.delete_to_start_of_line()
            }
            (KeyCode::Backspace, _) => self.backspace(),
            // Delete (forward delete key)
            (KeyCode::Delete, m) if m.contains(KeyModifiers::ALT) => self.delete_word_forward(),
            (KeyCode::Delete, _) => self.delete_char_forward(),
            // Alt+Left: word backward
            (KeyCode::Left, m) if m.contains(KeyModifiers::ALT) => self.move_word_backward(),
            // Cmd+Left: start of line
            (KeyCode::Left, m) if m.contains(KeyModifiers::SUPER) => self.move_home(),
            (KeyCode::Left, _) => self.move_left(),
            // Alt+Right: word forward
            (KeyCode::Right, m) if m.contains(KeyModifiers::ALT) => self.move_word_forward(),
            // Cmd+Right: end of line
            (KeyCode::Right, m) if m.contains(KeyModifiers::SUPER) => self.move_end(),
            (KeyCode::Right, _) => self.move_right(),
            // Cmd+Up: start of buffer
            (KeyCode::Up, m) if m.contains(KeyModifiers::SUPER) => {
                self.row = 0;
                self.col = 0;
            }
            (KeyCode::Up, _) => self.move_up(),
            // Cmd+Down: end of buffer
            (KeyCode::Down, m) if m.contains(KeyModifiers::SUPER) => {
                self.row = self.lines.len() - 1;
                self.col = self.lines[self.row].chars().count();
            }
            (KeyCode::Down, _) => self.move_down(),
            (KeyCode::Home, _) => self.move_home(),
            (KeyCode::End, _) => self.move_end(),
            _ => return false,
        }
        true
    }

    /// Like `handle_key` but with kill ring support for Ctrl+K/U/Y.
    pub fn handle_key_with_kill_ring(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        kill_ring: &mut String,
    ) -> bool {
        match (code, modifiers) {
            (KeyCode::Char('k'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.kill_to_end_of_line(kill_ring);
                true
            }
            (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.kill_to_start_of_line(kill_ring);
                true
            }
            (KeyCode::Char('y'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.yank(kill_ring);
                true
            }
            _ => self.handle_key(code, modifiers),
        }
    }
}

pub(crate) fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

pub(crate) fn render_inline_textarea(
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
            out.print(", ");
        } else {
            out.print(&pad);
        }
        out.print(vl);
        if editing && vi == vis_cursor.0 {
            cursor_pos = Some((text_col + vis_cursor.1 as u16, row));
        }
        out.newline();
        row += 1;
    }
    (row, cursor_pos)
}

pub(crate) fn begin_dialog_draw(out: &mut RenderOut, start_row: u16) {
    // Reset styling to a clean state before the dialog paints.
    // Use the tracked reset (not force) to avoid emitting unnecessary
    // SGR codes that can flash on screen.
    out.reset_style();
    let _ = out.queue(cursor::MoveTo(0, start_row));
    out.row = Some(start_row);
}

pub(crate) fn end_dialog_draw(out: &mut RenderOut) {
    let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
}

pub(crate) fn finish_dialog_frame(
    out: &mut RenderOut,
    cursor_pos: Option<(u16, u16)>,
    editing: bool,
) {
    if editing {
        if let Some((col, r)) = cursor_pos {
            draw_soft_cursor(out, col, r, " ");
        }
    }
}
