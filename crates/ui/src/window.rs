use crate::edit_buffer::EditBuffer;
use crate::kill_ring::KillRing;
use crate::layout::{Anchor, Border, Constraint, FloatRelative, Gutters};
use crate::text::{byte_to_cell, cell_to_byte};
use crate::vim::{Action, ViMode, Vim, VimContext};
use crate::window_cursor::WindowCursor;
use crate::{BufId, WinId};
use crossterm::event::{KeyCode, KeyEvent};

#[derive(Clone, Debug)]
pub struct SplitConfig {
    pub region: String,
    pub gutters: Gutters,
}

#[derive(Clone, Debug)]
pub struct FloatConfig {
    pub relative: FloatRelative,
    pub anchor: Anchor,
    pub row: i32,
    pub col: i32,
    pub width: Constraint,
    pub height: Constraint,
    pub border: Border,
    pub title: Option<String>,
    pub zindex: u16,
}

impl Default for FloatConfig {
    fn default() -> Self {
        Self {
            relative: FloatRelative::Editor,
            anchor: Anchor::NW,
            row: 0,
            col: 0,
            width: Constraint::Pct(80),
            height: Constraint::Pct(50),
            border: Border::Single,
            title: None,
            zindex: 50,
        }
    }
}

#[derive(Clone, Debug)]
pub enum WinConfig {
    Split(SplitConfig),
    Float(FloatConfig),
}

pub struct Window {
    pub(crate) id: WinId,
    pub buf: BufId,
    pub config: WinConfig,
    pub focusable: bool,

    pub edit_buf: EditBuffer,
    pub cpos: usize,
    pub vim: Option<Vim>,
    pub win_cursor: WindowCursor,
    pub kill_ring: KillRing,
    pub scroll_top: u16,
    pub cursor_line: u16,
    pub cursor_col: u16,
    pub pinned_last_total: Option<u16>,
    pub selection_anchor: Option<(usize, usize)>,
    pub cursor_positioned: bool,
}

impl Window {
    pub fn new(id: WinId, buf: BufId, config: WinConfig) -> Self {
        Self {
            id,
            buf,
            config,
            focusable: true,
            edit_buf: EditBuffer::readonly(),
            cpos: 0,
            vim: None,
            win_cursor: WindowCursor::new(),
            kill_ring: KillRing::new(),
            scroll_top: 0,
            cursor_line: 0,
            cursor_col: 0,
            pinned_last_total: None,
            selection_anchor: None,
            cursor_positioned: false,
        }
    }

    pub fn id(&self) -> WinId {
        self.id
    }

    pub fn is_float(&self) -> bool {
        matches!(self.config, WinConfig::Float(_))
    }

    pub fn is_split(&self) -> bool {
        matches!(self.config, WinConfig::Split(_))
    }

    pub fn zindex(&self) -> u16 {
        match &self.config {
            WinConfig::Float(f) => f.zindex,
            WinConfig::Split(_) => 0,
        }
    }

    pub fn title(&self) -> Option<&str> {
        match &self.config {
            WinConfig::Float(f) => f.title.as_deref(),
            WinConfig::Split(_) => None,
        }
    }

    pub fn set_title(&mut self, title: Option<String>) {
        if let WinConfig::Float(ref mut f) = self.config {
            f.title = title;
        }
    }

    // ── Vim ────────────────────────────────────────────────────────────

    pub fn set_vim_enabled(&mut self, enabled: bool) {
        if enabled {
            if self.vim.is_none() {
                self.vim = Some(Vim::new());
            }
        } else {
            self.vim = None;
            self.selection_anchor = None;
        }
    }

    pub fn vim_enabled(&self) -> bool {
        self.vim.is_some()
    }

    // ── Cursor ─────────────────────────────────────────────────────────

    pub fn cursor_abs_row(&self) -> usize {
        self.scroll_top as usize + self.cursor_line as usize
    }

    pub fn selection_range(&self, rows: &[String]) -> Option<(usize, usize)> {
        let cpos = self.compute_cpos(rows);
        if let Some(ref vim) = self.vim {
            if let Some(range) = vim.visual_range(&rows.join("\n"), cpos) {
                return Some(range);
            }
        }
        let (ar, ac) = self.selection_anchor?;
        let offsets = Self::line_start_offsets(rows);
        let anchor_row = ar.min(rows.len().saturating_sub(1));
        let anchor_byte = offsets.get(anchor_row).copied().unwrap_or(0)
            + cell_to_byte(rows.get(anchor_row).map(|s| s.as_str()).unwrap_or(""), ac);
        let (lo, hi) = if anchor_byte <= cpos {
            (anchor_byte, cpos)
        } else {
            (cpos, anchor_byte)
        };
        (lo != hi).then_some((lo, hi))
    }

    pub fn select_word_at(&mut self, rows: &[String], cpos: usize) -> Option<(usize, usize)> {
        let (start, end) = self.edit_buf.word_range_at(cpos)?;
        if let Some(vim) = self.vim.as_mut() {
            self.cpos = end.saturating_sub(1).max(start);
            vim.begin_visual(ViMode::Visual, start);
        } else {
            self.cpos = end;
            let offsets = Self::line_start_offsets(rows);
            let anchor_line = match offsets.binary_search(&start) {
                Ok(i) => i,
                Err(i) => i.saturating_sub(1),
            };
            let byte_col = start.saturating_sub(offsets[anchor_line]);
            let col = byte_to_cell(&rows[anchor_line], byte_col);
            self.selection_anchor = Some((anchor_line, col));
        }
        Some((start, end))
    }

    pub fn resync(&mut self, rows: &[String], viewport_rows: u16) {
        if rows.is_empty() {
            return;
        }
        let offsets = Self::line_start_offsets(rows);
        self.edit_buf.buf = rows.join("\n");
        self.sync_from_cpos(rows, &offsets, viewport_rows);
    }

    pub fn refocus(&mut self, rows: &[String], viewport_rows: u16) {
        if rows.is_empty() {
            self.edit_buf.buf.clear();
            self.cpos = 0;
            self.cursor_line = 0;
            self.cursor_col = 0;
            self.cursor_positioned = false;
            return;
        }
        if let Some(vim) = self.vim.as_mut() {
            if vim.mode() != ViMode::Normal {
                vim.set_mode(ViMode::Normal);
            }
        }
        if !self.cursor_positioned {
            let total = rows.len();
            let last_line = total.saturating_sub(1);
            let offsets = Self::line_start_offsets(rows);
            self.edit_buf.buf = rows.join("\n");
            self.cpos = offsets[last_line];
            self.sync_from_cpos(rows, &offsets, viewport_rows);
            self.cursor_positioned = true;
        } else {
            let offsets = self.mount(rows);
            self.sync_from_cpos(rows, &offsets, viewport_rows);
        }
        if self.win_cursor.curswant().is_none() {
            self.win_cursor.set_curswant(Some(self.cursor_col as usize));
        }
    }

    pub fn reanchor_to_visible_row(&mut self, rows: &[String], viewport_rows: u16) {
        if rows.is_empty() {
            return;
        }
        let offsets = Self::line_start_offsets(rows);
        self.edit_buf.buf = rows.join("\n");
        let total = rows.len() as u16;
        let max = total.saturating_sub(viewport_rows);
        self.scroll_top = self.scroll_top.min(max);
        let cursor_line = self.cursor_line.min(viewport_rows.saturating_sub(1));
        let target_line = (self.scroll_top + cursor_line) as usize;
        let target_line = target_line.min(rows.len() - 1);
        let line = &rows[target_line];
        let want = self
            .win_cursor
            .curswant()
            .unwrap_or(self.cursor_col as usize);
        let col_bytes = cell_to_byte(line, want);
        self.cpos = offsets[target_line] + col_bytes;
        self.cursor_col = byte_to_cell(line, col_bytes) as u16;
        self.cursor_line = cursor_line;
    }

    // ── Pin ────────────────────────────────────────────────────────────

    pub fn pin(&mut self, total_rows: u16) {
        self.pinned_last_total = Some(total_rows);
    }

    pub fn unpin(&mut self) {
        self.pinned_last_total = None;
    }

    pub fn apply_pin(&mut self, total_rows: u16, viewport_rows: u16) {
        if self.pinned_last_total.is_none() {
            return;
        }
        let max = total_rows.saturating_sub(viewport_rows);
        self.scroll_top = self.scroll_top.min(max);
        self.pinned_last_total = Some(total_rows);
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned_last_total.is_some()
    }

    // ── Navigation ─────────────────────────────────────────────────────

    pub fn compute_cpos(&self, rows: &[String]) -> usize {
        let offsets = Self::line_start_offsets(rows);
        self.visible_cpos(rows, &offsets)
    }

    fn line_start_offsets(rows: &[String]) -> Vec<usize> {
        let mut v = Vec::with_capacity(rows.len());
        let mut acc = 0usize;
        for r in rows {
            v.push(acc);
            acc += r.len() + 1;
        }
        v
    }

    fn visible_cpos(&self, rows: &[String], offsets: &[usize]) -> usize {
        let total = rows.len();
        if total == 0 {
            return 0;
        }
        let line_idx = (self.scroll_top as usize + self.cursor_line as usize).min(total - 1);
        offsets[line_idx] + cell_to_byte(&rows[line_idx], self.cursor_col as usize)
    }

    fn sync_from_cpos(&mut self, rows: &[String], offsets: &[usize], viewport_rows: u16) {
        let total = rows.len();
        if total == 0 {
            return;
        }
        let tail_byte = *offsets.last().unwrap() + rows.last().map_or(0, |r| r.len());
        self.cpos = self.cpos.min(tail_byte);
        let line_idx = match offsets.binary_search(&self.cpos) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let line = &rows[line_idx];
        let byte_col = self.cpos.saturating_sub(offsets[line_idx]);
        self.cursor_col = byte_to_cell(line, byte_col) as u16;
        let line_idx = line_idx as u16;
        let viewport_bottom = self
            .scroll_top
            .saturating_add(viewport_rows.saturating_sub(1));
        if line_idx > viewport_bottom {
            self.scroll_top = line_idx.saturating_sub(viewport_rows.saturating_sub(1));
        } else if line_idx < self.scroll_top {
            self.scroll_top = line_idx;
        }
        self.cursor_line = line_idx.saturating_sub(self.scroll_top);
    }

    fn mount(&mut self, rows: &[String]) -> Vec<usize> {
        let offsets = Self::line_start_offsets(rows);
        self.edit_buf.buf = rows.join("\n");
        self.cpos = self.visible_cpos(rows, &offsets);
        offsets
    }

    // ── Key dispatch ───────────────────────────────────────────────────

    pub fn handle_key(
        &mut self,
        k: KeyEvent,
        rows: &[String],
        viewport_rows: u16,
    ) -> Option<Option<String>> {
        if rows.is_empty() {
            return None;
        }
        let offsets = self.mount(rows);
        if !self.dispatch_vim_key(k) {
            return None;
        }
        if let Some(vim) = self.vim.as_mut() {
            if vim.mode() == ViMode::Insert {
                vim.set_mode(ViMode::Normal);
            }
        }
        let yanked = self.kill_ring.current().to_string();
        let yanked = if yanked.is_empty() {
            None
        } else {
            self.kill_ring.set_with_linewise(String::new(), false);
            Some(yanked)
        };
        self.sync_from_cpos(rows, &offsets, viewport_rows);
        Some(yanked)
    }

    fn dispatch_vim_key(&mut self, key: KeyEvent) -> bool {
        let Some(vim) = self.vim.as_mut() else {
            return false;
        };
        let key = match key.code {
            KeyCode::Up => KeyEvent {
                code: KeyCode::Char('k'),
                ..key
            },
            KeyCode::Down => KeyEvent {
                code: KeyCode::Char('j'),
                ..key
            },
            KeyCode::Left => KeyEvent {
                code: KeyCode::Char('h'),
                ..key
            },
            KeyCode::Right => KeyEvent {
                code: KeyCode::Char('l'),
                ..key
            },
            _ => key,
        };
        vim.set_curswant(self.win_cursor.curswant());
        let mut cpos = self.cpos;
        let mut ctx = VimContext {
            buf: &mut self.edit_buf.buf,
            cpos: &mut cpos,
            attachments: &mut self.edit_buf.attachment_ids,
            kill_ring: &mut self.kill_ring,
            history: &mut self.edit_buf.history,
        };
        let action = vim.handle_key(key, &mut ctx);
        self.cpos = cpos;
        self.win_cursor.set_curswant(vim.curswant());
        !matches!(action, Action::Passthrough)
    }

    pub fn scroll_by_lines(&mut self, delta: isize, rows: &[String], viewport_rows: u16) {
        if rows.is_empty() || delta == 0 {
            return;
        }
        let offsets = self.mount(rows);
        let new_cpos = self
            .win_cursor
            .move_vertical(&self.edit_buf.buf, self.cpos, delta);
        self.cpos = new_cpos;
        if let Some(vim) = self.vim.as_mut() {
            if vim.mode() == ViMode::Insert {
                vim.set_mode(ViMode::Normal);
            }
        }
        self.sync_from_cpos(rows, &offsets, viewport_rows);
    }

    pub fn jump_to_line_col(
        &mut self,
        rows: &[String],
        line_idx: usize,
        col: usize,
        viewport_rows: u16,
    ) {
        if rows.is_empty() {
            return;
        }
        let line_idx = line_idx.min(rows.len() - 1);
        let offsets = Self::line_start_offsets(rows);
        self.edit_buf.buf = rows.join("\n");
        let line = &rows[line_idx];
        let col_bytes = cell_to_byte(line, col);
        self.cpos = offsets[line_idx] + col_bytes;
        let landed_col = byte_to_cell(line, col_bytes);
        self.win_cursor.set_curswant(Some(landed_col));
        self.sync_from_cpos(rows, &offsets, viewport_rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BufId;

    fn make_win() -> Window {
        Window::new(
            WinId(1),
            BufId(1),
            WinConfig::Split(SplitConfig {
                region: "test".into(),
                gutters: Gutters::default(),
            }),
        )
    }

    fn sample_rows(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("line {i}")).collect()
    }

    #[test]
    fn apply_pin_holds_scroll_top_on_growth() {
        let mut w = make_win();
        w.scroll_top = 5;
        w.pin(100);
        w.apply_pin(103, 20);
        assert_eq!(w.scroll_top, 5);
        assert_eq!(w.pinned_last_total, Some(103));
    }

    #[test]
    fn apply_pin_clamps_on_shrinkage() {
        let mut w = make_win();
        w.scroll_top = 85;
        w.pin(100);
        w.apply_pin(97, 20);
        assert_eq!(w.scroll_top, 77);
        assert_eq!(w.pinned_last_total, Some(97));
    }

    #[test]
    fn apply_pin_clamps_to_zero() {
        let mut w = make_win();
        w.scroll_top = 2;
        w.pin(100);
        w.apply_pin(15, 20);
        assert_eq!(w.scroll_top, 0);
    }

    #[test]
    fn apply_pin_noop_when_unpinned() {
        let mut w = make_win();
        w.scroll_top = 5;
        w.apply_pin(200, 20);
        assert_eq!(w.scroll_top, 5);
    }

    #[test]
    fn scroll_by_lines_moves_cursor_down() {
        let mut w = make_win();
        w.set_vim_enabled(true);
        let rows = sample_rows(30);
        let viewport = 10;
        w.jump_to_line_col(&rows, 0, 0, viewport);
        assert_eq!(w.cursor_line, 0);
        assert_eq!(w.scroll_top, 0);
        w.scroll_by_lines(1, &rows, viewport);
        assert_eq!(w.cursor_line, 1);
        assert_eq!(w.scroll_top, 0);
    }

    #[test]
    fn refocus_on_empty_resets_cursor() {
        let mut w = make_win();
        w.cursor_line = 5;
        w.cursor_col = 3;
        w.refocus(&[], 20);
        assert_eq!(w.cursor_line, 0);
        assert_eq!(w.cursor_col, 0);
    }

    #[test]
    fn jump_to_last_line_scrolls_to_bottom() {
        let mut w = make_win();
        let rows = sample_rows(50);
        let viewport = 10;
        w.jump_to_line_col(&rows, 49, 0, viewport);
        assert_eq!(w.scroll_top, 40);
        assert_eq!(w.cursor_line, 9);
    }

    #[test]
    fn cursor_abs_row_top_relative() {
        let mut w = make_win();
        w.scroll_top = 10;
        w.cursor_line = 5;
        assert_eq!(w.cursor_abs_row(), 15);
    }

    #[test]
    fn unpin_stops_tracking() {
        let mut w = make_win();
        w.pin(100);
        assert!(w.is_pinned());
        w.unpin();
        assert!(!w.is_pinned());
        w.scroll_top = 5;
        w.apply_pin(200, 20);
        assert_eq!(w.scroll_top, 5);
    }
}
