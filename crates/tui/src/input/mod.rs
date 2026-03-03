mod history;
mod settings;

pub use history::History;
pub use settings::{Menu, MenuAction, MenuKind, MenuResult, MenuState};

use crate::completer::{Completer, CompleterKind};
use crate::render;
use crate::vim::{self, ViMode, Vim};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use protocol::Content;

pub const ATTACHMENT_MARKER: char = '\u{FFFC}';
const PASTE_LINE_THRESHOLD: usize = 12;

// ── Attachment types ─────────────────────────────────────────────────────────

/// A single attachment embedded in the input buffer.
/// Each `ATTACHMENT_MARKER` char in `buf` corresponds to one entry in
/// `attachments`, matched by counting markers left-to-right.
#[derive(Clone, Debug)]
pub enum Attachment {
    /// A large paste collapsed to a single marker char.
    Paste { content: String },
    /// An image with a display label and base64 data URL.
    Image { label: String, data_url: String },
}

impl Attachment {
    /// Display label shown in the input buffer.
    pub fn display_label(&self) -> String {
        match self {
            Attachment::Paste { content } => {
                let lines = content.lines().count().max(1);
                format!("[pasted {lines} lines]")
            }
            Attachment::Image { label, .. } => format!("[{label}]"),
        }
    }

    /// Text to expand into the final submitted text.
    /// Pastes expand to their full content; images expand to nothing.
    fn expanded_text(&self) -> &str {
        match self {
            Attachment::Paste { content } => content.as_str(),
            Attachment::Image { .. } => "",
        }
    }
}

/// Snapshot of the input buffer state (used for Ctrl+S stash).
#[derive(Clone, Debug)]
pub struct InputSnapshot {
    pub buf: String,
    pub cpos: usize,
    pub attachments: Vec<Attachment>,
}

// ── Shared input state ───────────────────────────────────────────────────────

/// Unified input buffer with attachment markers and file completer.
/// Used by both the prompt loop and the agent-mode type-ahead.
pub struct InputState {
    pub buf: String,
    pub cpos: usize,
    pub attachments: Vec<Attachment>,
    pub completer: Option<Completer>,
    pub menu: Option<MenuState>,
    vim: Option<Vim>,
    /// Saved buffer before history search, restored on cancel.
    history_saved_buf: Option<(String, usize)>,
    pub stash: Option<InputSnapshot>,
}

/// What the caller should do after `handle_event`.
pub enum Action {
    Redraw,
    Submit { content: Content, display: String },
    MenuResult(MenuResult),
    ToggleMode,
    CycleReasoning,
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
            attachments: Vec::new(),
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
        self.attachments.clear();
        self.completer = None;
        self.menu = None;
        self.history_saved_buf = None;
        // Note: stash is intentionally NOT cleared here.
    }

    /// Toggle stash: if no stash, save current buf and clear; if stashed, restore.
    pub fn toggle_stash(&mut self) {
        if let Some(snap) = self.stash.take() {
            self.buf = snap.buf;
            self.cpos = snap.cpos;
            self.attachments = snap.attachments;
            self.completer = None;
        } else if !self.buf.is_empty() || !self.attachments.is_empty() {
            self.stash = Some(InputSnapshot {
                buf: std::mem::take(&mut self.buf),
                cpos: std::mem::replace(&mut self.cpos, 0),
                attachments: std::mem::take(&mut self.attachments),
            });
            self.completer = None;
        }
    }

    /// Restore stash into the buffer (called after submit/command completes).
    pub fn restore_stash(&mut self) {
        if let Some(snap) = self.stash.take() {
            self.buf = snap.buf;
            self.cpos = snap.cpos;
            self.attachments = snap.attachments;
        }
    }

    /// Restore input from a rewind. The text has pastes expanded and image
    /// labels inline as `[label]`. Replace each `[label]` with an attachment
    /// marker so images become editable attachments again.
    pub fn restore_from_rewind(&mut self, mut text: String, images: Vec<Attachment>) {
        let mut attachments = Vec::new();
        for img in images {
            let label = img.display_label(); // "[filename.png]"
            if let Some(pos) = text.find(&label) {
                text.replace_range(pos..pos + label.len(), &ATTACHMENT_MARKER.to_string());
                attachments.push(img);
            }
            // If label not found in text (shouldn't happen), drop silently.
        }
        self.buf = text;
        self.cpos = self.buf.len();
        self.attachments = attachments;
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

    pub fn open_stats(&mut self, lines: Vec<crate::metrics::StatsLine>) {
        self.completer = None;
        self.menu = Some(MenuState {
            nav: Menu {
                selected: 0,
                len: 0,
                select_on_enter: false,
            },
            kind: MenuKind::Stats { lines },
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

    /// Dismiss the current menu, returning the appropriate result.
    pub fn dismiss_menu(&mut self) -> Option<MenuResult> {
        let ms = self.menu.take()?;
        Some(match ms.kind {
            MenuKind::Settings {
                vim_enabled,
                auto_compact,
            } => MenuResult::Settings {
                vim: vim_enabled,
                auto_compact,
            },
            MenuKind::Model { .. } => MenuResult::Dismissed,
            MenuKind::Stats { .. } => MenuResult::Stats,
        })
    }

    /// Number of rows the current menu needs (0 if no menu).
    pub fn menu_rows(&self) -> usize {
        match &self.menu {
            Some(ms) => match &ms.kind {
                MenuKind::Settings { .. } => 2,
                MenuKind::Model { models } => (models.len() + 2).min(12),
                MenuKind::Stats { lines } => lines
                    .iter()
                    .map(|l| match l {
                        crate::metrics::StatsLine::Sparkline { .. } => 2,
                        _ => 1,
                    })
                    .sum(),
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

    /// Expand attachment markers and return the final text for submission.
    /// Pastes are inlined; image markers are stripped (images go via Content::Parts).
    pub fn expanded_text(&self) -> String {
        let mut result = String::new();
        let mut att_idx = 0;
        for c in self.buf.chars() {
            if c == ATTACHMENT_MARKER {
                if let Some(att) = self.attachments.get(att_idx) {
                    result.push_str(att.expanded_text());
                }
                att_idx += 1;
            } else {
                result.push(c);
            }
        }
        result
    }

    /// Text for the user message block: pastes expanded, images shown as `[label]`.
    pub fn message_display_text(&self) -> String {
        let mut result = String::new();
        let mut att_idx = 0;
        for c in self.buf.chars() {
            if c == ATTACHMENT_MARKER {
                if let Some(att) = self.attachments.get(att_idx) {
                    match att {
                        Attachment::Paste { content } => result.push_str(content),
                        Attachment::Image { label, .. } => {
                            result.push_str(&format!("[{label}]"));
                        }
                    }
                }
                att_idx += 1;
            } else {
                result.push(c);
            }
        }
        result
    }

    pub fn image_count(&self) -> usize {
        self.attachments
            .iter()
            .filter(|a| matches!(a, Attachment::Image { .. }))
            .count()
    }

    /// Attach an image at the current cursor position.
    pub fn insert_image(&mut self, label: String, data_url: String) {
        self.insert_attachment(Attachment::Image { label, data_url });
    }

    /// Build the message content combining text and any attached images.
    pub fn build_content(&self) -> Content {
        let text = self.expanded_text();
        let images: Vec<(String, String)> = self
            .attachments
            .iter()
            .filter_map(|a| match a {
                Attachment::Image { label, data_url } => Some((label.clone(), data_url.clone())),
                _ => None,
            })
            .collect();
        Content::with_images(text, images)
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
                match vim.handle_key(key_ev, &mut self.buf, &mut self.cpos, &mut self.attachments) {
                    vim::Action::Consumed => {
                        self.recompute_completer();
                        return Action::Redraw;
                    }
                    vim::Action::Submit => {
                        let display = self.message_display_text();
                        let content = self.build_content();
                        self.buf.clear();
                        self.cpos = 0;
                        self.attachments.clear();
                        self.completer = None;
                        return Action::Submit { content, display };
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
                // Save undo state before pasting if vim is enabled.
                if let Some(ref mut vim) = self.vim {
                    vim.save_undo(&self.buf, self.cpos, &self.attachments);
                }
                // Try to detect an image file path (drag-and-drop).
                if let Some(path) = engine::image::normalize_pasted_path(&data) {
                    if engine::image::is_image_file(&path) && std::path::Path::new(&path).exists() {
                        let label = engine::image::image_label_from_path(&path);
                        if let Ok(url) = engine::image::read_image_as_data_url(&path) {
                            self.insert_image(label, url);
                            return Action::Redraw;
                        }
                    }
                }
                // Empty paste (e.g. Cmd+V with image-only clipboard) → try clipboard image.
                if data.trim().is_empty() {
                    if let Some(url) = clipboard_image_to_data_url() {
                        if let Some(ref mut vim) = self.vim {
                            vim.save_undo(&self.buf, self.cpos, &self.attachments);
                        }
                        self.insert_image("clipboard.png".into(), url);
                        return Action::Redraw;
                    }
                }
                self.insert_paste(data);
                Action::Redraw
            }
            // Ctrl+V / Cmd+V: read image from clipboard.
            Event::Key(KeyEvent {
                code: KeyCode::Char('v'),
                modifiers,
                ..
            }) if modifiers.contains(KeyModifiers::CONTROL)
                || modifiers.contains(KeyModifiers::SUPER) =>
            {
                if let Some(url) = clipboard_image_to_data_url() {
                    if let Some(ref mut vim) = self.vim {
                        vim.save_undo(&self.buf, self.cpos, &self.attachments);
                    }
                    self.insert_image("clipboard.png".into(), url);
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::BackTab,
                ..
            }) => Action::ToggleMode,
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                ..
            }) => {
                if self.buf.is_empty() && self.attachments.is_empty() {
                    Action::Noop
                } else {
                    let display = self.message_display_text();
                    let content = self.build_content();
                    self.clear();
                    Action::Submit { content, display }
                }
            }
            // Ctrl+T: cycle reasoning effort.
            Event::Key(KeyEvent {
                code: KeyCode::Char('t'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => Action::CycleReasoning,
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
            // Alt+Backspace (macOS) / Ctrl+Backspace (Linux/Windows): delete word backward.
            Event::Key(KeyEvent {
                code: KeyCode::Backspace,
                modifiers,
                ..
            }) if modifiers.contains(KeyModifiers::ALT)
                || modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.delete_word_backward();
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
            MenuAction::Tab => {
                if matches!(ms.kind, MenuKind::Model { .. }) {
                    Action::CycleReasoning
                } else {
                    Action::Redraw
                }
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
            MenuAction::Dismiss => Action::MenuResult(self.dismiss_menu().unwrap()),
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
                        let display = self.message_display_text();
                        let content = self.build_content();
                        self.clear();
                        Some(Action::Submit { content, display })
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
        self.maybe_remove_attachment(prev);
        self.buf.drain(prev..self.cpos);
        self.cpos = prev;
        self.recompute_completer();
    }

    fn delete_word_backward(&mut self) {
        if self.cpos == 0 {
            return;
        }
        let target = vim::word_backward_pos(&self.buf, self.cpos, vim::CharClass::Word);
        // Count attachment markers in the drained range and remove them
        // (iterate in reverse so indices stay valid).
        let markers_before = self.buf[..target]
            .chars()
            .filter(|&c| c == ATTACHMENT_MARKER)
            .count();
        let markers_in_range = self.buf[target..self.cpos]
            .chars()
            .filter(|&c| c == ATTACHMENT_MARKER)
            .count();
        for i in (0..markers_in_range).rev() {
            let idx = markers_before + i;
            if idx < self.attachments.len() {
                self.attachments.remove(idx);
            }
        }
        self.buf.drain(target..self.cpos);
        self.cpos = target;
        self.recompute_completer();
    }

    fn insert_paste(&mut self, data: String) {
        // Normalize line endings: terminals (especially macOS) send \r for
        // newlines in bracketed paste mode.  Convert \r\n and lone \r to \n
        // so that line counting and display work correctly.
        let data = data.replace("\r\n", "\n").replace('\r', "\n");
        let lines = data.lines().count();
        let char_threshold = PASTE_LINE_THRESHOLD * (crate::render::term_width().saturating_sub(1));
        if lines >= PASTE_LINE_THRESHOLD || data.len() >= char_threshold {
            self.insert_attachment(Attachment::Paste { content: data });
        } else {
            self.buf.insert_str(self.cpos, &data);
            self.cpos += data.len();
        }
    }

    fn insert_attachment(&mut self, att: Attachment) {
        let idx = self.buf[..self.cpos]
            .chars()
            .filter(|&c| c == ATTACHMENT_MARKER)
            .count();
        self.attachments.insert(idx, att);
        self.buf.insert(self.cpos, ATTACHMENT_MARKER);
        self.cpos += ATTACHMENT_MARKER.len_utf8();
    }

    fn maybe_remove_attachment(&mut self, byte_pos: usize) {
        if self.buf[byte_pos..].starts_with(ATTACHMENT_MARKER) {
            let idx = self.buf[..byte_pos]
                .chars()
                .filter(|&c| c == ATTACHMENT_MARKER)
                .count();
            if idx < self.attachments.len() {
                self.attachments.remove(idx);
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

/// Read image data from the system clipboard, encode as PNG, and return a data URL.
fn clipboard_image_to_data_url() -> Option<String> {
    use base64::Engine;
    use image::{ImageBuffer, ImageFormat, RgbaImage};

    let mut clipboard = arboard::Clipboard::new().ok()?;
    let img_data = clipboard.get_image().ok()?;
    let rgba: RgbaImage = ImageBuffer::from_raw(
        img_data.width as u32,
        img_data.height as u32,
        img_data.bytes.into_owned(),
    )?;
    let mut buf = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut buf);
    rgba.write_to(&mut cursor, ImageFormat::Png).ok()?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
    Some(format!("data:image/png;base64,{b64}"))
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
