mod history;
mod settings;

pub use history::History;
pub use settings::{Menu, MenuAction, MenuKind, MenuResult, MenuState};

use crate::completer::{Completer, CompleterKind};
use crate::render;
use crate::vim::{self, ViMode, Vim};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

/// Object Replacement Character — inline placeholder for large pastes.
pub const PASTE_MARKER: char = '\u{FFFC}';

const PASTE_LINE_THRESHOLD: usize = 12;

// ── Mode ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Normal,
    Plan,
    Apply,
    Yolo,
}

impl Mode {
    pub fn toggle(self) -> Self {
        match self {
            Mode::Normal => Mode::Plan,
            Mode::Plan => Mode::Apply,
            Mode::Apply => Mode::Yolo,
            Mode::Yolo => Mode::Normal,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Plan => "plan",
            Mode::Apply => "apply",
            Mode::Yolo => "yolo",
        }
    }
}

// ── Shared input state ───────────────────────────────────────────────────────

/// Unified input buffer with paste tokens and file completer.
/// Used by both the prompt loop and the agent-mode type-ahead.
pub struct InputState {
    pub buf: String,
    pub cpos: usize,
    pub pastes: Vec<String>,
    pub completer: Option<Completer>,
    pub menu: Option<MenuState>,
    vim: Option<Vim>,
    /// Saved buffer before history search, restored on cancel.
    history_saved_buf: Option<(String, usize)>,
    /// Stashed prompt: (buf, cpos, pastes). Ctrl+S toggles.
    pub stash: Option<(String, usize, Vec<String>)>,
}

/// What the caller should do after `handle_event`.
pub enum Action {
    Redraw,
    Submit(String),
    MenuResult(MenuResult),
    ToggleMode,
    Resize { width: usize, height: usize },
    Noop,
}

impl Default for InputState {
    fn default() -> Self {
        Self::new()
    }
}

impl InputState {
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            cpos: 0,
            pastes: Vec::new(),
            completer: None,
            menu: None,
            vim: None,
            history_saved_buf: None,
            stash: None,
        }
    }

    pub fn vim_enabled(&self) -> bool {
        self.vim.is_some()
    }

    pub fn vim_mode(&self) -> Option<ViMode> {
        self.vim.as_ref().map(|v| v.mode())
    }

    /// Returns true if vim is enabled and currently in insert mode.
    pub fn vim_in_insert_mode(&self) -> bool {
        self.vim
            .as_ref()
            .is_some_and(|v| v.mode() == ViMode::Insert)
    }

    pub fn set_vim_enabled(&mut self, enabled: bool) {
        if enabled {
            if self.vim.is_none() {
                self.vim = Some(Vim::new());
            }
        } else {
            self.vim = None;
        }
    }

    /// Restore vim to a specific mode (used after double-Esc cancel).
    pub fn set_vim_mode(&mut self, mode: ViMode) {
        if let Some(ref mut vim) = self.vim {
            vim.set_mode(mode);
        }
    }

    pub fn take_buffer(&mut self) -> (String, usize) {
        let buf = std::mem::take(&mut self.buf);
        let cpos = std::mem::replace(&mut self.cpos, 0);
        (buf, cpos)
    }

    pub fn set_buffer(&mut self, buf: String, cpos: usize) {
        self.buf = buf;
        self.cpos = cpos.min(self.buf.len());
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.cpos = 0;
        self.pastes.clear();
        self.completer = None;
        self.menu = None;
        self.history_saved_buf = None;
        // Note: stash is intentionally NOT cleared here.
    }

    /// Toggle stash: if no stash, save current buf and clear; if stashed, restore.
    pub fn toggle_stash(&mut self) {
        if let Some((buf, cpos, pastes)) = self.stash.take() {
            // Unstash: restore stashed content
            self.buf = buf;
            self.cpos = cpos;
            self.pastes = pastes;
            self.completer = None;
        } else if !self.buf.is_empty() {
            // Stash: save current content and clear
            self.stash = Some((
                std::mem::take(&mut self.buf),
                std::mem::replace(&mut self.cpos, 0),
                std::mem::take(&mut self.pastes),
            ));
            self.completer = None;
        }
    }

    /// Restore stash into the buffer (called after submit/command completes).
    pub fn restore_stash(&mut self) {
        if let Some((buf, cpos, pastes)) = self.stash.take() {
            self.buf = buf;
            self.cpos = cpos;
            self.pastes = pastes;
        }
    }

    pub fn open_settings(&mut self, vim_enabled: bool, auto_compact: bool) {
        self.completer = None;
        self.menu = Some(MenuState {
            nav: Menu {
                selected: 0,
                len: 2,
                select_on_enter: false,
            },
            kind: MenuKind::Settings {
                vim_enabled,
                auto_compact,
            },
        });
    }

    pub fn open_model_picker(&mut self, models: Vec<(String, String, String)>) {
        let len = models.len();
        self.completer = None;
        self.menu = Some(MenuState {
            nav: Menu {
                selected: 0,
                len,
                select_on_enter: true,
            },
            kind: MenuKind::Model { models },
        });
    }

    pub fn has_modal(&self) -> bool {
        self.menu.is_some()
    }

    /// Number of rows the current menu needs (0 if no menu).
    pub fn menu_rows(&self) -> usize {
        match &self.menu {
            Some(ms) => match &ms.kind {
                MenuKind::Settings { .. } => 2,
                MenuKind::Model { models } => models.len().min(10),
            },
            None => 0,
        }
    }

    /// Returns the history search query if a history completer is active.
    pub fn history_search_query(&self) -> Option<&str> {
        self.completer.as_ref().and_then(|c| {
            if c.kind == CompleterKind::History {
                Some(c.query.as_str())
            } else {
                None
            }
        })
    }

    /// Open history fuzzy search using the completer component.
    pub fn open_history_search(&mut self, history: &History) {
        self.history_saved_buf = Some((self.buf.clone(), self.cpos));
        // Keep buf as-is so the current content becomes the initial search query.
        let mut comp = Completer::history(history.entries());
        comp.update_query(self.buf.clone());
        self.completer = Some(comp);
    }

    pub fn cursor_char(&self) -> usize {
        char_pos(&self.buf, self.cpos)
    }

    /// Expand paste markers and return the final text.
    pub fn expanded_text(&self) -> String {
        expand_pastes(&self.buf, &self.pastes)
    }

    /// Process a terminal event. Returns what the caller should do next.
    pub fn handle_event(&mut self, ev: Event, mut history: Option<&mut History>) -> Action {
        // Menu intercepts all keys when open
        if self.menu.is_some() {
            return self.handle_menu_event(&ev);
        }

        // Completer intercepts navigation keys when active
        if self.completer.is_some() {
            if let Some(action) = self.handle_completer_event(&ev) {
                return action;
            }
        }

        // Vim mode intercepts key events.
        if let Some(ref mut vim) = self.vim {
            if let Event::Key(key_ev) = ev {
                match vim.handle_key(key_ev, &mut self.buf, &mut self.cpos) {
                    vim::Action::Consumed => {
                        self.recompute_completer();
                        return Action::Redraw;
                    }
                    vim::Action::Submit => {
                        let text = self.expanded_text();
                        self.buf.clear();
                        self.cpos = 0;
                        self.pastes.clear();
                        self.completer = None;
                        return Action::Submit(text);
                    }
                    vim::Action::HistoryPrev => {
                        if let Some(entry) = history.as_deref_mut().and_then(|h| h.up(&self.buf)) {
                            self.buf = entry.to_string();
                            self.cpos = 0;
                            self.sync_completer();
                        }
                        return Action::Redraw;
                    }
                    vim::Action::HistoryNext => {
                        if let Some(entry) = history.as_deref_mut().and_then(|h| h.down()) {
                            self.buf = entry.to_string();
                            self.cpos = self.buf.len();
                            self.sync_completer();
                        }
                        return Action::Redraw;
                    }
                    vim::Action::Passthrough => {
                        // Fall through to normal handling below.
                    }
                }
            }
        }

        match ev {
            Event::Paste(data) => {
                self.insert_paste(data);
                Action::Redraw
            }
            Event::Key(KeyEvent {
                code: KeyCode::BackTab,
                ..
            }) => Action::ToggleMode,
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                ..
            }) => {
                if self.buf.trim().is_empty() {
                    Action::Noop
                } else {
                    let text = self.expanded_text();
                    self.clear();
                    Action::Submit(text)
                }
            }
            // Ctrl+C: handled by the app event loop (double-tap logic).
            Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => Action::Noop,
            // Ctrl+U / Ctrl+D: half-page up/down in vim normal mode.
            Event::Key(KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) if self
                .vim
                .as_ref()
                .is_some_and(|v| v.mode() == ViMode::Normal) =>
            {
                let half = render::term_height() / 2;
                let line = current_line(&self.buf, self.cpos);
                let target = line.saturating_sub(half);
                self.move_to_line(target);
                Action::Redraw
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) if self
                .vim
                .as_ref()
                .is_some_and(|v| v.mode() == ViMode::Normal) =>
            {
                let half = render::term_height() / 2;
                let line = current_line(&self.buf, self.cpos);
                let total = self.buf.chars().filter(|&c| c == '\n').count() + 1;
                let target = (line + half).min(total - 1);
                self.move_to_line(target);
                Action::Redraw
            }
            // Ctrl+R: open history fuzzy search.
            Event::Key(KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => Action::Noop, // handled by the app event loop
            Event::Key(KeyEvent {
                code: KeyCode::Char('j'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                self.buf.insert(self.cpos, '\n');
                self.cpos += 1;
                self.completer = None;
                Action::Redraw
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char('a'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                let before = &self.buf[..self.cpos];
                self.cpos = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
                self.recompute_completer();
                Action::Redraw
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char('e'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                let after = &self.buf[self.cpos..];
                self.cpos += after.find('\n').unwrap_or(after.len());
                self.recompute_completer();
                Action::Redraw
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                ..
            }) if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT => {
                self.insert_char(c);
                Action::Redraw
            }
            Event::Key(KeyEvent {
                code: KeyCode::Backspace,
                ..
            }) => {
                self.backspace();
                Action::Redraw
            }
            Event::Key(KeyEvent {
                code: KeyCode::Left,
                ..
            }) => {
                if self.cpos > 0 {
                    let cp = char_pos(&self.buf, self.cpos);
                    self.cpos = byte_of_char(&self.buf, cp - 1);
                    self.recompute_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Right,
                ..
            }) => {
                if self.cpos < self.buf.len() {
                    let cp = char_pos(&self.buf, self.cpos);
                    self.cpos = byte_of_char(&self.buf, cp + 1);
                    self.recompute_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Up, ..
            }) => {
                if let Some(entry) = history.and_then(|h| h.up(&self.buf)) {
                    self.buf = entry.to_string();
                    self.cpos = self.buf.len();
                    self.sync_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Down,
                ..
            }) => {
                if let Some(entry) = history.and_then(|h| h.down()) {
                    self.buf = entry.to_string();
                    self.cpos = self.buf.len();
                    self.sync_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Home,
                ..
            }) => {
                let before = &self.buf[..self.cpos];
                self.cpos = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
                self.recompute_completer();
                Action::Redraw
            }
            Event::Key(KeyEvent {
                code: KeyCode::End, ..
            }) => {
                let after = &self.buf[self.cpos..];
                self.cpos += after.find('\n').unwrap_or(after.len());
                self.recompute_completer();
                Action::Redraw
            }
            Event::Resize(w, h) => Action::Resize {
                width: w as usize,
                height: h as usize,
            },
            _ => Action::Noop,
        }
    }

    // ── Completer ────────────────────────────────────────────────────────

    fn handle_menu_event(&mut self, ev: &Event) -> Action {
        let ms = self.menu.as_mut().unwrap();
        match ms.nav.handle_event(ev) {
            MenuAction::Toggle(idx) => {
                if let MenuKind::Settings {
                    ref mut vim_enabled,
                    ref mut auto_compact,
                } = ms.kind
                {
                    match idx {
                        0 => *vim_enabled ^= true,
                        1 => *auto_compact ^= true,
                        _ => {}
                    }
                }
                Action::Redraw
            }
            MenuAction::Select(idx) => {
                let ms = self.menu.take().unwrap();
                match ms.kind {
                    MenuKind::Model { ref models } => {
                        if let Some((key, _, _)) = models.get(idx) {
                            Action::MenuResult(MenuResult::ModelSelect(key.clone()))
                        } else {
                            Action::Redraw
                        }
                    }
                    _ => Action::Redraw,
                }
            }
            MenuAction::Dismiss => {
                let ms = self.menu.take().unwrap();
                match ms.kind {
                    MenuKind::Settings {
                        vim_enabled,
                        auto_compact,
                    } => Action::MenuResult(MenuResult::Settings {
                        vim: vim_enabled,
                        auto_compact,
                    }),
                    MenuKind::Model { .. } => Action::MenuResult(MenuResult::Dismissed),
                }
            }
            MenuAction::Redraw => Action::Redraw,
            MenuAction::Noop => Action::Noop,
        }
    }

    /// Try to handle the event as a completer navigation. Returns Some if consumed.
    fn handle_completer_event(&mut self, ev: &Event) -> Option<Action> {
        let is_history = self
            .completer
            .as_ref()
            .is_some_and(|c| c.kind == CompleterKind::History);

        match ev {
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                ..
            }) => {
                if is_history {
                    let comp = self.completer.take().unwrap();
                    if let Some(label) = comp.accept() {
                        self.buf = label.to_string();
                        self.cpos = self.buf.len();
                    }
                    self.history_saved_buf = None;
                    Some(Action::Redraw)
                } else {
                    let comp = self.completer.take().unwrap();
                    let kind = comp.kind;
                    self.accept_completion(&comp);
                    if kind == CompleterKind::Command {
                        let text = self.expanded_text();
                        self.clear();
                        Some(Action::Submit(text))
                    } else {
                        // File: accept and keep editing
                        Some(Action::Redraw)
                    }
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Esc, ..
            }) => {
                if is_history {
                    if let Some((buf, cpos)) = self.history_saved_buf.take() {
                        self.buf = buf;
                        self.cpos = cpos;
                    }
                }
                self.completer = None;
                Some(Action::Redraw)
            }
            // Ctrl+R cycles forward through history matches
            Event::Key(KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) if is_history => {
                let comp = self.completer.as_mut().unwrap();
                comp.move_down();
                Some(Action::Redraw)
            }
            Event::Key(KeyEvent {
                code: KeyCode::Up, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('k'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                let comp = self.completer.as_mut().unwrap();
                if comp.results.len() <= 1 {
                    return None; // let history handle it
                }
                comp.move_up();
                Some(Action::Redraw)
            }
            Event::Key(KeyEvent {
                code: KeyCode::Down,
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('j'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                let comp = self.completer.as_mut().unwrap();
                if comp.results.len() <= 1 {
                    return None; // let history handle it
                }
                comp.move_down();
                Some(Action::Redraw)
            }
            Event::Key(KeyEvent {
                code: KeyCode::Tab, ..
            }) => {
                let comp = self.completer.take().unwrap();
                if comp.kind == CompleterKind::History {
                    if let Some(label) = comp.accept() {
                        self.buf = label.to_string();
                        self.cpos = self.buf.len();
                    }
                    self.history_saved_buf = None;
                } else {
                    self.accept_completion(&comp);
                }
                Some(Action::Redraw)
            }
            _ => None,
        }
    }

    fn accept_completion(&mut self, comp: &Completer) {
        if let Some(label) = comp.accept() {
            let end = self.cpos;
            let start = comp.anchor;
            let trigger = &self.buf[start..start + 1];
            let replacement = if trigger == "/" {
                format!("/{}", label)
            } else {
                format!("@{} ", label)
            };
            self.buf.replace_range(start..end, &replacement);
            self.cpos = start + replacement.len();
        }
    }

    /// Activate completer if the buffer looks like a command or file ref.
    fn sync_completer(&mut self) {
        if find_slash_anchor(&self.buf, self.cpos).is_some() {
            let mut comp = Completer::commands(0);
            comp.update_query(self.buf[1..self.cpos].to_string());
            self.completer = Some(comp);
        } else {
            self.completer = None;
        }
    }

    /// Recompute the completer based on where the cursor currently sits.
    /// Shows the file or command picker if the cursor is inside an @/slash zone,
    /// hides it otherwise. Never touches a history completer.
    fn recompute_completer(&mut self) {
        if self
            .completer
            .as_ref()
            .is_some_and(|c| c.kind == CompleterKind::History)
        {
            let query = self.buf.clone();
            self.completer.as_mut().unwrap().update_query(query);
            return;
        }
        if let Some(at_pos) = cursor_in_at_zone(&self.buf, self.cpos) {
            let query = if self.cpos > at_pos + 1 {
                self.buf[at_pos + 1..self.cpos].to_string()
            } else {
                String::new()
            };
            if self
                .completer
                .as_ref()
                .is_some_and(|c| c.kind == CompleterKind::File && c.anchor == at_pos)
            {
                self.completer.as_mut().unwrap().update_query(query);
            } else {
                let mut comp = Completer::files(at_pos);
                comp.update_query(query);
                self.completer = Some(comp);
            }
        } else if find_slash_anchor(&self.buf, self.cpos).is_some()
            || (self.cpos == 0 && self.buf.starts_with('/'))
        {
            let end = self.cpos.max(1);
            let query = self.buf[1..end].to_string();
            if self
                .completer
                .as_ref()
                .is_some_and(|c| c.kind == CompleterKind::Command)
            {
                self.completer.as_mut().unwrap().update_query(query);
            } else {
                let mut comp = Completer::commands(0);
                comp.update_query(query);
                self.completer = Some(comp);
            }
        } else {
            self.completer = None;
        }
    }

    /// Move cursor to the beginning of the given line number (0-indexed).
    fn move_to_line(&mut self, target_line: usize) {
        let mut line = 0;
        let mut pos = 0;
        for (i, c) in self.buf.char_indices() {
            if line == target_line {
                pos = i;
                break;
            }
            if c == '\n' {
                line += 1;
                if line == target_line {
                    pos = i + 1;
                    break;
                }
            }
        }
        if line < target_line {
            // target beyond end, go to last line start
            pos = self.buf.rfind('\n').map(|i| i + 1).unwrap_or(0);
        }
        self.cpos = pos;
        self.recompute_completer();
    }

    // ── Editing primitives ───────────────────────────────────────────────

    fn insert_char(&mut self, c: char) {
        self.buf.insert(self.cpos, c);
        self.cpos += c.len_utf8();
        self.recompute_completer();
    }

    fn backspace(&mut self) {
        if self.cpos == 0 {
            return;
        }
        let prev = self.buf[..self.cpos]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.maybe_remove_paste(prev);
        self.buf.drain(prev..self.cpos);
        self.cpos = prev;
        self.recompute_completer();
    }

    fn insert_paste(&mut self, data: String) {
        let lines = data.lines().count();
        let char_threshold = PASTE_LINE_THRESHOLD * (crate::render::term_width().saturating_sub(1));
        if lines >= PASTE_LINE_THRESHOLD || data.len() >= char_threshold {
            let idx = self.buf[..self.cpos]
                .chars()
                .filter(|&c| c == PASTE_MARKER)
                .count();
            self.pastes.insert(idx, data);
            self.buf.insert(self.cpos, PASTE_MARKER);
            self.cpos += PASTE_MARKER.len_utf8();
        } else {
            self.buf.insert_str(self.cpos, &data);
            self.cpos += data.len();
        }
    }

    fn maybe_remove_paste(&mut self, byte_pos: usize) {
        if self.buf[byte_pos..].starts_with(PASTE_MARKER) {
            let idx = self.buf[..byte_pos]
                .chars()
                .filter(|&c| c == PASTE_MARKER)
                .count();
            if idx < self.pastes.len() {
                self.pastes.remove(idx);
            }
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

pub fn char_pos(s: &str, byte_idx: usize) -> usize {
    s[..byte_idx].chars().count()
}

pub fn byte_of_char(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len())
}

fn expand_pastes(buf: &str, pastes: &[String]) -> String {
    let mut result = String::new();
    let mut idx = 0;
    for c in buf.chars() {
        if c == PASTE_MARKER {
            if let Some(content) = pastes.get(idx) {
                result.push_str(content);
            }
            idx += 1;
        } else {
            result.push(c);
        }
    }
    result
}

fn current_line(buf: &str, cpos: usize) -> usize {
    let end = if buf.is_char_boundary(cpos) {
        cpos
    } else {
        buf.len()
    };
    buf[..end].chars().filter(|&c| c == '\n').count()
}

/// Like find_at_anchor but also matches when the cursor is ON the '@' itself.
fn cursor_in_at_zone(buf: &str, cpos: usize) -> Option<usize> {
    if !buf.is_char_boundary(cpos) {
        return None;
    }
    // Include the char at cpos so the cursor-on-@ case works.
    // Find the end of the character at cpos (next char boundary after cpos).
    let search_end = buf[cpos..]
        .char_indices()
        .nth(1)
        .map(|(i, _)| cpos + i)
        .unwrap_or(buf.len());
    let at_pos = buf[..search_end].rfind('@')?;
    // @ must be at start or preceded by whitespace.
    if at_pos > 0 && !buf[..at_pos].ends_with(char::is_whitespace) {
        return None;
    }
    // No whitespace between @ and cpos.
    if at_pos < cpos && buf[at_pos + 1..cpos].contains(char::is_whitespace) {
        return None;
    }
    Some(at_pos)
}

fn find_slash_anchor(buf: &str, cpos: usize) -> Option<usize> {
    // Only valid when `/` is at position 0 and no whitespace in the query.
    if !buf.starts_with('/') || !buf.is_char_boundary(cpos) {
        return None;
    }
    if cpos < 1 || buf[1..cpos].contains(char::is_whitespace) {
        return None;
    }
    Some(0)
}

// ── Agent-mode Esc resolution ────────────────────────────────────────────────

/// Result of pressing Esc during agent processing.
#[derive(Debug, PartialEq)]
pub enum EscAction {
    /// Vim was in insert mode — switch to normal, double-Esc timer started.
    VimToNormal,
    /// Unqueue messages back into the input buffer.
    Unqueue,
    /// Double-Esc cancel. Contains the vim mode to restore (if vim enabled).
    Cancel { restore_vim: Option<ViMode> },
    /// First Esc in normal/no-vim mode — timer started.
    StartTimer,
}

/// Pure logic for Esc key handling during agent processing.
///
/// `vim_mode_at_first_esc` tracks the vim mode before the Esc sequence started,
/// so that a double-Esc cancel can restore it (the first Esc may have switched
/// vim from insert → normal).
pub fn resolve_agent_esc(
    vim_mode: Option<ViMode>,
    has_queued: bool,
    last_esc: &mut Option<std::time::Instant>,
    vim_mode_at_first_esc: &mut Option<ViMode>,
) -> EscAction {
    use std::time::{Duration, Instant};

    // Vim insert mode: switch to normal AND start the double-Esc timer so that
    // a second Esc within 500ms cancels (only two presses total, not three).
    if vim_mode == Some(ViMode::Insert) {
        *vim_mode_at_first_esc = Some(ViMode::Insert);
        *last_esc = Some(Instant::now());
        return EscAction::VimToNormal;
    }

    // Unqueue if there are queued messages.
    if has_queued {
        *last_esc = None;
        *vim_mode_at_first_esc = None;
        return EscAction::Unqueue;
    }

    // Double-Esc: cancel agent, return mode to restore.
    if let Some(prev) = *last_esc {
        if prev.elapsed() < Duration::from_millis(500) {
            let restore = vim_mode_at_first_esc.take();
            *last_esc = None;
            return EscAction::Cancel {
                restore_vim: restore,
            };
        }
    }

    // First Esc (vim normal or vim disabled) — start timer.
    *vim_mode_at_first_esc = vim_mode;
    *last_esc = Some(Instant::now());
    EscAction::StartTimer
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Vim-mode Esc behavior ───────────────────────────────────────────

    #[test]
    fn vim_esc_in_insert_switches_to_normal() {
        // Single Esc while vim is in insert mode → VimToNormal.
        let mut last_esc = None;
        let mut saved_mode = None;
        let action = resolve_agent_esc(Some(ViMode::Insert), false, &mut last_esc, &mut saved_mode);
        assert_eq!(action, EscAction::VimToNormal);
        // Timer should be started so a second Esc can cancel.
        assert!(last_esc.is_some());
        // The insert mode should be saved for restoration on cancel.
        assert_eq!(saved_mode, Some(ViMode::Insert));
    }

    #[test]
    fn vim_esc_in_normal_unqueues_if_queued() {
        // Esc in vim normal mode with queued messages → Unqueue.
        let mut last_esc = None;
        let mut saved_mode = None;
        let action = resolve_agent_esc(Some(ViMode::Normal), true, &mut last_esc, &mut saved_mode);
        assert_eq!(action, EscAction::Unqueue);
    }

    #[test]
    fn vim_double_esc_from_insert_cancels_and_restores_insert() {
        // First Esc: vim insert → normal, timer starts.
        let mut last_esc = None;
        let mut saved_mode = None;
        let action1 =
            resolve_agent_esc(Some(ViMode::Insert), false, &mut last_esc, &mut saved_mode);
        assert_eq!(action1, EscAction::VimToNormal);

        // Second Esc: now in normal mode (vim switched), timer active → Cancel.
        // Restore mode should be Insert (the mode before the sequence started).
        let action2 =
            resolve_agent_esc(Some(ViMode::Normal), false, &mut last_esc, &mut saved_mode);
        assert_eq!(
            action2,
            EscAction::Cancel {
                restore_vim: Some(ViMode::Insert)
            }
        );
    }

    #[test]
    fn vim_double_esc_from_normal_cancels_and_stays_normal() {
        // First Esc: vim already in normal, no queue → StartTimer.
        let mut last_esc = None;
        let mut saved_mode = None;
        let action1 =
            resolve_agent_esc(Some(ViMode::Normal), false, &mut last_esc, &mut saved_mode);
        assert_eq!(action1, EscAction::StartTimer);
        assert_eq!(saved_mode, Some(ViMode::Normal));

        // Second Esc within 500ms → Cancel, restore to Normal.
        let action2 =
            resolve_agent_esc(Some(ViMode::Normal), false, &mut last_esc, &mut saved_mode);
        assert_eq!(
            action2,
            EscAction::Cancel {
                restore_vim: Some(ViMode::Normal)
            }
        );
    }

    // ── No-vim Esc behavior ─────────────────────────────────────────────

    #[test]
    fn no_vim_esc_unqueues_if_queued() {
        let mut last_esc = None;
        let mut saved_mode = None;
        let action = resolve_agent_esc(
            None, // vim disabled
            true,
            &mut last_esc,
            &mut saved_mode,
        );
        assert_eq!(action, EscAction::Unqueue);
    }

    #[test]
    fn no_vim_double_esc_cancels() {
        let mut last_esc = None;
        let mut saved_mode = None;

        // First Esc → StartTimer.
        let action1 = resolve_agent_esc(None, false, &mut last_esc, &mut saved_mode);
        assert_eq!(action1, EscAction::StartTimer);

        // Second Esc within 500ms → Cancel with no vim mode to restore.
        let action2 = resolve_agent_esc(None, false, &mut last_esc, &mut saved_mode);
        assert_eq!(action2, EscAction::Cancel { restore_vim: None });
    }
}
