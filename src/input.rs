use crate::completer::{Completer, CompleterKind};
use crate::config;
use crate::render::{self, Screen};
use crate::vim::{self, ViMode, Vim};
use crossterm::{
    cursor,
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers},
    terminal, ExecutableCommand,
};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

/// Object Replacement Character — inline placeholder for large pastes.
pub const PASTE_MARKER: char = '\u{FFFC}';

const PASTE_LINE_THRESHOLD: usize = 4;
const PASTE_CHAR_THRESHOLD: usize = 200;

// ── History ──────────────────────────────────────────────────────────────────

pub struct History {
    entries: Vec<String>,
    cursor: usize,
    draft: String,
    path: PathBuf,
}

const RECORD_SEP: char = '\x1e';

impl History {
    pub fn load() -> Self {
        let path = config::state_dir().join("history");
        let entries = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .split(RECORD_SEP)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect::<Vec<_>>();
        let cursor = entries.len();
        Self { entries, cursor, draft: String::new(), path }
    }

    pub fn push(&mut self, entry: String) {
        if !entry.is_empty() && self.entries.last().is_none_or(|last| *last != entry) {
            self.entries.push(entry.clone());
            self.append_to_file(&entry);
        }
        self.reset();
    }

    fn append_to_file(&self, entry: &str) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = write!(f, "{}{}", entry, RECORD_SEP);
        }
    }

    fn reset(&mut self) {
        self.cursor = self.entries.len();
        self.draft.clear();
    }

    fn up(&mut self, current_buf: &str) -> Option<&str> {
        if self.entries.is_empty() { return None; }
        if self.cursor == self.entries.len() {
            self.draft = current_buf.to_string();
        }
        if self.cursor > 0 {
            self.cursor -= 1;
            Some(&self.entries[self.cursor])
        } else {
            None
        }
    }

    fn down(&mut self) -> Option<&str> {
        if self.cursor >= self.entries.len() { return None; }
        self.cursor += 1;
        if self.cursor == self.entries.len() {
            Some(&self.draft)
        } else {
            Some(&self.entries[self.cursor])
        }
    }
}

// ── Mode ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Normal,
    Apply,
}

impl Mode {
    pub fn toggle(self) -> Self {
        match self {
            Mode::Normal => Mode::Apply,
            Mode::Apply => Mode::Normal,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Apply => "apply",
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
    vim: Option<Vim>,
}

/// What the caller should do after `handle_event`.
pub enum Action {
    Redraw,
    Submit(String),
    Cancel,
    ToggleMode,
    Resize(usize),
    Noop,
}

impl InputState {
    pub fn new() -> Self {
        Self { buf: String::new(), cpos: 0, pastes: Vec::new(), completer: None, vim: None }
    }

    pub fn vim_enabled(&self) -> bool {
        self.vim.is_some()
    }

    pub fn vim_mode(&self) -> Option<ViMode> {
        self.vim.as_ref().map(|v| v.mode())
    }

    /// Returns true if vim is enabled and currently in insert mode.
    pub fn vim_in_insert_mode(&self) -> bool {
        self.vim.as_ref().is_some_and(|v| v.mode() == ViMode::Insert)
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
                        self.completer = None;
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
                            self.cpos = self.buf.len();
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
            Event::Key(KeyEvent { code: KeyCode::BackTab, .. }) => Action::ToggleMode,
            Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
                if self.buf.trim().is_empty() {
                    Action::Noop
                } else {
                    let text = self.expanded_text();
                    self.clear();
                    Action::Submit(text)
                }
            }
            // Ctrl+C / Ctrl+D handled in the input loop (double-tap logic).
            Event::Key(KeyEvent { code: KeyCode::Char('c' | 'd'), modifiers: KeyModifiers::CONTROL, .. }) => {
                Action::Noop
            }
            Event::Key(KeyEvent { code: KeyCode::Char('j'), modifiers: KeyModifiers::CONTROL, .. }) => {
                self.buf.insert(self.cpos, '\n');
                self.cpos += 1;
                self.completer = None;
                Action::Redraw
            }
            Event::Key(KeyEvent { code: KeyCode::Char('a'), modifiers: KeyModifiers::CONTROL, .. }) => {
                let before = &self.buf[..self.cpos];
                self.cpos = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
                self.completer = None;
                Action::Redraw
            }
            Event::Key(KeyEvent { code: KeyCode::Char('e'), modifiers: KeyModifiers::CONTROL, .. }) => {
                let after = &self.buf[self.cpos..];
                self.cpos += after.find('\n').unwrap_or(after.len());
                self.completer = None;
                Action::Redraw
            }
            Event::Key(KeyEvent { code: KeyCode::Char(c), modifiers, .. })
                if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT =>
            {
                self.insert_char(c);
                Action::Redraw
            }
            Event::Key(KeyEvent { code: KeyCode::Backspace, .. }) => {
                self.backspace();
                Action::Redraw
            }
            Event::Key(KeyEvent { code: KeyCode::Left, .. }) => {
                if self.cpos > 0 {
                    let cp = char_pos(&self.buf, self.cpos);
                    self.cpos = byte_of_char(&self.buf, cp - 1);
                    self.completer = None;
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Right, .. }) => {
                if self.cpos < self.buf.len() {
                    let cp = char_pos(&self.buf, self.cpos);
                    self.cpos = byte_of_char(&self.buf, cp + 1);
                    self.completer = None;
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Up, .. }) => {
                if let Some(entry) = history.and_then(|h| h.up(&self.buf)) {
                    self.buf = entry.to_string();
                    self.cpos = self.buf.len();
                    self.sync_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Down, .. }) => {
                if let Some(entry) = history.and_then(|h| h.down()) {
                    self.buf = entry.to_string();
                    self.cpos = self.buf.len();
                    self.sync_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Home, .. }) => {
                let before = &self.buf[..self.cpos];
                self.cpos = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
                self.completer = None;
                Action::Redraw
            }
            Event::Key(KeyEvent { code: KeyCode::End, .. }) => {
                let after = &self.buf[self.cpos..];
                self.cpos += after.find('\n').unwrap_or(after.len());
                self.completer = None;
                Action::Redraw
            }
            Event::Resize(w, _) => Action::Resize(w as usize),
            _ => Action::Noop,
        }
    }

    // ── Completer ────────────────────────────────────────────────────────

    /// Try to handle the event as a completer navigation. Returns Some if consumed.
    fn handle_completer_event(&mut self, ev: &Event) -> Option<Action> {
        match ev {
            Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
                let comp = self.completer.take().unwrap();
                let is_command = comp.kind == CompleterKind::Command;
                self.accept_completion(&comp);
                if is_command {
                    let text = self.expanded_text();
                    self.buf.clear();
                    self.cpos = 0;
                    self.pastes.clear();
                    Some(Action::Submit(text))
                } else {
                    Some(Action::Redraw)
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => {
                self.completer = None;
                Some(Action::Redraw)
            }
            Event::Key(KeyEvent { code: KeyCode::Up, .. })
            | Event::Key(KeyEvent { code: KeyCode::Char('k'), modifiers: KeyModifiers::CONTROL, .. }) => {
                let comp = self.completer.as_mut().unwrap();
                if comp.results.len() <= 1 {
                    return None; // let history handle it
                }
                comp.move_up();
                Some(Action::Redraw)
            }
            Event::Key(KeyEvent { code: KeyCode::Down, .. })
            | Event::Key(KeyEvent { code: KeyCode::Char('j'), modifiers: KeyModifiers::CONTROL, .. }) => {
                let comp = self.completer.as_mut().unwrap();
                if comp.results.len() <= 1 {
                    return None; // let history handle it
                }
                comp.move_down();
                Some(Action::Redraw)
            }
            Event::Key(KeyEvent { code: KeyCode::Tab, .. }) => {
                let comp = self.completer.take().unwrap();
                self.accept_completion(&comp);
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

    fn update_completer(&mut self) {
        if let Some(ref comp) = self.completer {
            let anchor = comp.anchor;
            let trigger = self.buf.as_bytes().get(anchor).copied();
            let valid = match trigger {
                Some(b'@') => find_at_anchor(&self.buf, self.cpos).is_some(),
                Some(b'/') => find_slash_anchor(&self.buf, self.cpos).is_some(),
                _ => false,
            };
            if valid && self.cpos > anchor {
                let query = self.buf[anchor + 1..self.cpos].to_string();
                self.completer.as_mut().unwrap().update_query(query);
            } else {
                self.completer = None;
            }
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

    // ── Editing primitives ───────────────────────────────────────────────

    fn insert_char(&mut self, c: char) {
        self.buf.insert(self.cpos, c);
        self.cpos += c.len_utf8();
        let anchor = self.cpos - c.len_utf8();
        if c == '@' {
            self.completer = Some(Completer::files(anchor));
        } else if c == '/' && anchor == 0 {
            self.completer = Some(Completer::commands(anchor));
        } else {
            self.update_completer();
        }
    }

    fn backspace(&mut self) {
        if self.cpos == 0 { return; }
        let prev = self.buf[..self.cpos].char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
        self.maybe_remove_paste(prev);
        self.buf.drain(prev..self.cpos);
        self.cpos = prev;
        self.update_completer();
    }

    fn insert_paste(&mut self, data: String) {
        let lines = data.lines().count();
        if lines >= PASTE_LINE_THRESHOLD || data.len() >= PASTE_CHAR_THRESHOLD {
            let idx = self.buf[..self.cpos].chars().filter(|&c| c == PASTE_MARKER).count();
            self.pastes.insert(idx, data);
            self.buf.insert(self.cpos, PASTE_MARKER);
            self.cpos += PASTE_MARKER.len_utf8();
        } else {
            self.buf.insert_str(self.cpos, &data);
            self.cpos += data.len();
        }
    }

    fn maybe_remove_paste(&mut self, byte_pos: usize) {
        if self.buf[byte_pos..].chars().next() == Some(PASTE_MARKER) {
            let idx = self.buf[..byte_pos].chars().filter(|&c| c == PASTE_MARKER).count();
            if idx < self.pastes.len() {
                self.pastes.remove(idx);
            }
        }
    }
}

// ── Prompt-mode entry point ──────────────────────────────────────────────────

pub fn read_input(
    screen: &mut Screen,
    mode: &mut Mode,
    history: &mut History,
    state: &mut InputState,
) -> Option<String> {
    let mut out = io::stdout();
    let mut width = render::term_width();

    terminal::enable_raw_mode().ok()?;
    let _ = out.execute(EnableBracketedPaste);
    let _ = out.execute(cursor::Hide);
    screen.draw_prompt(state, *mode, width);
    let _ = out.execute(cursor::Show);

    let mut resize_pending = false;
    let mut last_ctrlc: Option<std::time::Instant> = None;
    let mut last_esc: Option<std::time::Instant> = None;

    loop {
        let ev = if resize_pending {
            match event::poll(Duration::from_millis(150)) {
                Ok(true) => match event::read() {
                    Ok(ev) => ev,
                    Err(_) => continue,
                },
                _ => {
                    resize_pending = false;
                    let _ = out.execute(cursor::Hide);
                    screen.redraw_all();
                    screen.draw_prompt(state, *mode, width);
                    let _ = out.execute(cursor::Show);
                    continue;
                }
            }
        } else {
            match event::read() {
                Ok(ev) => ev,
                Err(_) => continue,
            }
        };

        // Ctrl+C / Ctrl+D: if empty or double-tap, quit; otherwise clear input.
        if matches!(ev, Event::Key(KeyEvent { code: KeyCode::Char('c' | 'd'), modifiers: KeyModifiers::CONTROL, .. })) {
            let double_tap = last_ctrlc.map_or(false, |prev| prev.elapsed() < Duration::from_millis(500));
            if state.buf.is_empty() || double_tap {
                let _ = out.execute(cursor::Hide);
                screen.erase_prompt();
                let _ = out.execute(cursor::Show);
                let _ = out.flush();
                let _ = out.execute(DisableBracketedPaste);
                terminal::disable_raw_mode().ok();
                return None;
            }
            last_ctrlc = Some(std::time::Instant::now());
            state.buf.clear();
            state.cpos = 0;
            state.pastes.clear();
            let _ = out.execute(cursor::Hide);
            screen.draw_prompt(state, *mode, width);
            let _ = out.execute(cursor::Show);
            continue;
        }

        // Double-Esc in idle → show rewind menu
        if matches!(ev, Event::Key(KeyEvent { code: KeyCode::Esc, .. })) {
            let in_normal = !state.vim_enabled() || !state.vim_in_insert_mode();
            if in_normal {
                let double = last_esc.map_or(false, |t| t.elapsed() < Duration::from_millis(500));
                if double {
                    last_esc = None;
                    let turns = screen.user_turns();
                    if !turns.is_empty() {
                        if let Some(block_idx) = render::show_rewind(&turns) {
                            let _ = out.execute(cursor::Hide);
                            screen.erase_prompt();
                            let _ = out.execute(cursor::Show);
                            let _ = out.flush();
                            let _ = out.execute(DisableBracketedPaste);
                            terminal::disable_raw_mode().ok();
                            return Some(format!("\x00rewind:{}", block_idx));
                        }
                        // Cancelled — redraw prompt
                        let _ = out.execute(cursor::Hide);
                        screen.draw_prompt(state, *mode, width);
                        let _ = out.execute(cursor::Show);
                    }
                    continue;
                } else {
                    last_esc = Some(std::time::Instant::now());
                    if !state.vim_enabled() {
                        // No vim — Esc has no other meaning in idle, just track for double
                        continue;
                    }
                }
            } else {
                // vim insert mode — first Esc goes to normal, reset timer
                last_esc = Some(std::time::Instant::now());
            }
        } else {
            last_esc = None;
        }

        match state.handle_event(ev, Some(history)) {
            Action::Submit(text) => {
                let _ = out.execute(cursor::Hide);
                screen.erase_prompt();
                let _ = out.execute(cursor::Show);
                let _ = out.flush();
                let _ = out.execute(DisableBracketedPaste);
                terminal::disable_raw_mode().ok();
                return Some(text);
            }
            Action::Cancel => {
                let _ = out.execute(cursor::Hide);
                screen.erase_prompt();
                let _ = out.execute(cursor::Show);
                let _ = out.flush();
                let _ = out.execute(DisableBracketedPaste);
                terminal::disable_raw_mode().ok();
                return None;
            }
            Action::ToggleMode => {
                *mode = mode.toggle();
                let _ = out.execute(cursor::Hide);
                screen.draw_prompt(state, *mode, width);
                let _ = out.execute(cursor::Show);
            }
            Action::Resize(w) => {
                width = w;
                screen.erase_prompt();
                resize_pending = true;
            }
            Action::Redraw => {
                let _ = out.execute(cursor::Hide);
                screen.draw_prompt(state, *mode, width);
                let _ = out.execute(cursor::Show);
            }
            Action::Noop => {}
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

fn find_at_anchor(buf: &str, cpos: usize) -> Option<usize> {
    let before = &buf[..cpos];
    let at_pos = before.rfind('@')?;
    if buf[at_pos + 1..cpos].contains(char::is_whitespace) {
        return None;
    }
    if at_pos > 0 && !buf[..at_pos].ends_with(char::is_whitespace) {
        return None;
    }
    Some(at_pos)
}

fn find_slash_anchor(buf: &str, cpos: usize) -> Option<usize> {
    // Only valid when `/` is at position 0 and no whitespace in the query.
    if !buf.starts_with('/') { return None; }
    if buf[1..cpos].contains(char::is_whitespace) { return None; }
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
            return EscAction::Cancel { restore_vim: restore };
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
        let action = resolve_agent_esc(
            Some(ViMode::Insert),
            false,
            &mut last_esc,
            &mut saved_mode,
        );
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
        let action = resolve_agent_esc(
            Some(ViMode::Normal),
            true,
            &mut last_esc,
            &mut saved_mode,
        );
        assert_eq!(action, EscAction::Unqueue);
    }

    #[test]
    fn vim_double_esc_from_insert_cancels_and_restores_insert() {
        // First Esc: vim insert → normal, timer starts.
        let mut last_esc = None;
        let mut saved_mode = None;
        let action1 = resolve_agent_esc(
            Some(ViMode::Insert),
            false,
            &mut last_esc,
            &mut saved_mode,
        );
        assert_eq!(action1, EscAction::VimToNormal);

        // Second Esc: now in normal mode (vim switched), timer active → Cancel.
        // Restore mode should be Insert (the mode before the sequence started).
        let action2 = resolve_agent_esc(
            Some(ViMode::Normal),
            false,
            &mut last_esc,
            &mut saved_mode,
        );
        assert_eq!(
            action2,
            EscAction::Cancel { restore_vim: Some(ViMode::Insert) }
        );
    }

    #[test]
    fn vim_double_esc_from_normal_cancels_and_stays_normal() {
        // First Esc: vim already in normal, no queue → StartTimer.
        let mut last_esc = None;
        let mut saved_mode = None;
        let action1 = resolve_agent_esc(
            Some(ViMode::Normal),
            false,
            &mut last_esc,
            &mut saved_mode,
        );
        assert_eq!(action1, EscAction::StartTimer);
        assert_eq!(saved_mode, Some(ViMode::Normal));

        // Second Esc within 500ms → Cancel, restore to Normal.
        let action2 = resolve_agent_esc(
            Some(ViMode::Normal),
            false,
            &mut last_esc,
            &mut saved_mode,
        );
        assert_eq!(
            action2,
            EscAction::Cancel { restore_vim: Some(ViMode::Normal) }
        );
    }

    // ── No-vim Esc behavior ─────────────────────────────────────────────

    #[test]
    fn no_vim_esc_unqueues_if_queued() {
        let mut last_esc = None;
        let mut saved_mode = None;
        let action = resolve_agent_esc(
            None,  // vim disabled
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
        let action1 = resolve_agent_esc(
            None,
            false,
            &mut last_esc,
            &mut saved_mode,
        );
        assert_eq!(action1, EscAction::StartTimer);

        // Second Esc within 500ms → Cancel with no vim mode to restore.
        let action2 = resolve_agent_esc(
            None,
            false,
            &mut last_esc,
            &mut saved_mode,
        );
        assert_eq!(action2, EscAction::Cancel { restore_vim: None });
    }
}
