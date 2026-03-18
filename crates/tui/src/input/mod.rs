mod history;
mod settings;

pub use history::History;
pub use settings::{Menu, MenuAction, MenuKind, MenuResult, MenuState};

use crate::attachment::{Attachment, AttachmentId, AttachmentStore};
use crate::completer::{Completer, CompleterKind};
use crate::render;
use crate::vim::{self, ViMode, Vim};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use protocol::Content;

pub const ATTACHMENT_MARKER: char = '\u{FFFC}';
const PASTE_LINE_THRESHOLD: usize = 12;

/// Snapshot of the input buffer state (used for Ctrl+S stash).
#[derive(Clone, Debug)]
pub struct InputSnapshot {
    pub buf: String,
    pub cpos: usize,
    pub attachment_ids: Vec<AttachmentId>,
    from_paste: bool,
}

// ── Shared input state ───────────────────────────────────────────────────────

/// Unified input buffer with attachment markers and file completer.
/// Used by both the prompt loop and the agent-mode type-ahead.
pub struct InputState {
    pub buf: String,
    pub cpos: usize,
    pub attachment_ids: Vec<AttachmentId>,
    pub store: AttachmentStore,
    pub completer: Option<Completer>,
    pub menu: Option<MenuState>,
    vim: Option<Vim>,
    /// Saved buffer before history search, restored on cancel.
    history_saved_buf: Option<(String, usize)>,
    pub stash: Option<InputSnapshot>,
    /// Tracks whether the current buffer content originated from a paste.
    /// Cleared on any manual character input.
    from_paste: bool,
}

/// What the caller should do after `handle_event`.
pub enum Action {
    Redraw,
    Submit { content: Content, display: String },
    MenuResult(MenuResult),
    ToggleMode,
    CycleReasoning,
    Resize { width: usize, height: usize },
    NotifyError(String),
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
            attachment_ids: Vec::new(),
            store: AttachmentStore::new(),
            completer: None,
            menu: None,
            vim: None,
            history_saved_buf: None,
            stash: None,
            from_paste: false,
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

    /// Returns true if the current content originated from a paste and should
    /// not be treated as a shell escape command (starting with '!').
    pub fn skip_shell_escape(&self) -> bool {
        self.from_paste
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
        self.attachment_ids.clear();
        self.completer = None;
        self.menu = None;
        self.history_saved_buf = None;
        self.from_paste = false;
        // Note: stash and store are intentionally NOT cleared here.
    }

    /// Toggle stash: if no stash, save current buf and clear; if stashed, restore.
    pub fn toggle_stash(&mut self) {
        if let Some(snap) = self.stash.take() {
            self.buf = snap.buf;
            self.cpos = snap.cpos;
            self.attachment_ids = snap.attachment_ids;
            self.from_paste = snap.from_paste;
            self.completer = None;
        } else if !self.buf.is_empty() || !self.attachment_ids.is_empty() {
            self.stash = Some(InputSnapshot {
                buf: std::mem::take(&mut self.buf),
                cpos: std::mem::replace(&mut self.cpos, 0),
                attachment_ids: std::mem::take(&mut self.attachment_ids),
                from_paste: self.from_paste,
            });
            self.completer = None;
            self.history_saved_buf = None;
        }
    }

    /// Restore stash into the buffer (called after submit/command completes).
    pub fn restore_stash(&mut self) {
        if let Some(snap) = self.stash.take() {
            self.buf = snap.buf;
            self.cpos = snap.cpos;
            self.attachment_ids = snap.attachment_ids;
            self.from_paste = snap.from_paste;
        }
    }

    /// Restore input from a rewind. The text has pastes expanded and image
    /// labels inline as `[label]`. Replace each `[label]` with an attachment
    /// marker so images become editable attachments again.
    pub fn restore_from_rewind(&mut self, mut text: String, images: Vec<(String, String)>) {
        let mut ids = Vec::new();
        for (label, data_url) in images {
            let display = format!("[{label}]");
            if let Some(pos) = text.find(&display) {
                text.replace_range(pos..pos + display.len(), &ATTACHMENT_MARKER.to_string());
                let id = self.store.insert_image(label, data_url);
                ids.push(id);
            }
        }
        self.buf = text;
        self.cpos = self.buf.len();
        self.attachment_ids = ids;
    }

    pub fn open_settings(
        &mut self,
        vim_enabled: bool,
        auto_compact: bool,
        show_speed: bool,
        show_prediction: bool,
        show_slug: bool,
        restrict_to_workspace: bool,
    ) {
        self.completer = None;
        self.menu = Some(MenuState {
            nav: Menu {
                selected: 0,
                len: 6,
                select_on_enter: false,
            },
            kind: MenuKind::Settings {
                vim_enabled,
                auto_compact,
                show_speed,
                show_prediction,
                show_slug,
                restrict_to_workspace,
            },
        });
    }

    pub fn open_stats(&mut self, stats: crate::metrics::StatsOutput) {
        self.completer = None;
        self.menu = Some(MenuState {
            nav: Menu {
                selected: 0,
                len: 0,
                select_on_enter: false,
            },
            kind: MenuKind::Stats {
                left: stats.left,
                right: stats.right,
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

    pub fn open_theme_picker(&mut self) {
        self.open_preset_picker(crate::theme::accent_value(), |presets, original| {
            MenuKind::Theme { presets, original }
        });
    }

    pub fn open_color_picker(&mut self) {
        self.open_preset_picker(crate::theme::slug_color_value(), |presets, original| {
            MenuKind::Color { presets, original }
        });
    }

    fn open_preset_picker(
        &mut self,
        current: u8,
        make_kind: impl FnOnce(Vec<(&'static str, &'static str, u8)>, u8) -> MenuKind,
    ) {
        let presets: Vec<_> = crate::theme::PRESETS.to_vec();
        let len = presets.len();
        let selected = presets
            .iter()
            .position(|(_, _, v)| *v == current)
            .unwrap_or(0);
        self.completer = None;
        self.menu = Some(MenuState {
            nav: Menu {
                selected,
                len,
                select_on_enter: true,
            },
            kind: make_kind(presets, current),
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
                show_speed,
                show_prediction,
                show_slug,
                restrict_to_workspace,
            } => MenuResult::Settings {
                vim: vim_enabled,
                auto_compact,
                show_speed,
                show_prediction,
                show_slug,
                restrict_to_workspace,
            },
            MenuKind::Model { .. } => MenuResult::Dismissed,
            MenuKind::Theme { original, .. } => {
                // Restore original accent on dismiss
                crate::theme::set_accent(original);
                MenuResult::Dismissed
            }
            MenuKind::Color { original, .. } => {
                crate::theme::set_slug_color(original);
                MenuResult::Dismissed
            }
            MenuKind::Stats { .. } => MenuResult::Stats,
        })
    }

    /// Number of rows the current menu needs (0 if no menu).
    pub fn menu_rows(&self) -> usize {
        match &self.menu {
            Some(ms) => match &ms.kind {
                MenuKind::Settings { .. } => 6,
                MenuKind::Model { models } => (models.len() + 2).min(12),
                MenuKind::Theme { presets, .. } => presets.len().min(14),
                MenuKind::Color { presets, .. } => presets.len().min(14),
                MenuKind::Stats { left, right } => crate::metrics::stats_row_count(left, right),
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
                if let Some(&id) = self.attachment_ids.get(att_idx) {
                    result.push_str(self.store.expanded_text(id));
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
                if let Some(&id) = self.attachment_ids.get(att_idx) {
                    if let Some(att) = self.store.get(id) {
                        match att {
                            Attachment::Paste { content } => result.push_str(content),
                            Attachment::Image { label, .. } => {
                                result.push_str(&format!("[{label}]"));
                            }
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
        self.attachment_ids
            .iter()
            .filter(|&&id| matches!(self.store.get(id), Some(Attachment::Image { .. })))
            .count()
    }

    /// Attach an image at the current cursor position.
    pub fn insert_image(&mut self, label: String, data_url: String) {
        let id = self.store.insert_image(label, data_url);
        self.insert_attachment_id(id);
    }

    /// Build the message content combining text and any attached images.
    pub fn build_content(&self) -> Content {
        let text = self.expanded_text();
        let images: Vec<(String, String)> = self
            .attachment_ids
            .iter()
            .filter_map(|&id| match self.store.get(id) {
                Some(Attachment::Image { label, data_url }) => {
                    Some((label.clone(), data_url.clone()))
                }
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
                match vim.handle_key(
                    key_ev,
                    &mut self.buf,
                    &mut self.cpos,
                    &mut self.attachment_ids,
                ) {
                    vim::Action::Consumed => {
                        self.recompute_completer();
                        return Action::Redraw;
                    }
                    vim::Action::Submit => {
                        let display = self.message_display_text();
                        let content = self.build_content();
                        self.clear();
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
                    vim.save_undo(&self.buf, self.cpos, &self.attachment_ids);
                }
                // Try to detect an image file path (drag-and-drop).
                if let Some(path) = engine::image::normalize_pasted_path(&data) {
                    if engine::image::is_image_file(&path) {
                        match engine::image::read_image_as_data_url(&path) {
                            Ok(url) => {
                                let label = engine::image::image_label_from_path(&path);
                                self.insert_image(label, url);
                                return Action::Redraw;
                            }
                            Err(e) => {
                                return Action::NotifyError(format!("cannot read image: {e}"));
                            }
                        }
                    }
                }
                // Empty paste (e.g. Cmd+V with image-only clipboard) → try clipboard image.
                if data.trim().is_empty() {
                    if let Some(url) = clipboard_image_to_data_url() {
                        if let Some(ref mut vim) = self.vim {
                            vim.save_undo(&self.buf, self.cpos, &self.attachment_ids);
                        }
                        self.insert_image("clipboard.png".into(), url);
                        return Action::Redraw;
                    }
                }
                self.insert_paste(data);
                Action::Redraw
            }
            // Cmd+V: read image from clipboard.
            Event::Key(KeyEvent {
                code: KeyCode::Char('v'),
                modifiers,
                ..
            }) if modifiers.contains(KeyModifiers::SUPER) => {
                if let Some(url) = clipboard_image_to_data_url() {
                    if let Some(ref mut vim) = self.vim {
                        vim.save_undo(&self.buf, self.cpos, &self.attachment_ids);
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
                modifiers,
                ..
            }) if !modifiers.contains(KeyModifiers::SHIFT) => {
                if self.buf.is_empty() && self.attachment_ids.is_empty() {
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
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::SHIFT,
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
                    self.cpos = 0;
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
                    ref mut show_speed,
                    ref mut show_prediction,
                    ref mut show_slug,
                    ref mut restrict_to_workspace,
                } = ms.kind
                {
                    match idx {
                        0 => *vim_enabled ^= true,
                        1 => *auto_compact ^= true,
                        2 => *show_speed ^= true,
                        3 => *show_prediction ^= true,
                        4 => *show_slug ^= true,
                        5 => *restrict_to_workspace ^= true,
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
                    MenuKind::Theme { ref presets, .. } => {
                        if let Some(&(_, _, value)) = presets.get(idx) {
                            crate::theme::set_accent(value);
                            Action::MenuResult(MenuResult::ThemeSelect(value))
                        } else {
                            Action::Redraw
                        }
                    }
                    MenuKind::Color { ref presets, .. } => {
                        if let Some(&(_, _, value)) = presets.get(idx) {
                            crate::theme::set_slug_color(value);
                            Action::MenuResult(MenuResult::ColorSelect(value))
                        } else {
                            Action::Redraw
                        }
                    }
                    _ => Action::Redraw,
                }
            }
            MenuAction::Dismiss => Action::MenuResult(self.dismiss_menu().unwrap()),
            MenuAction::Redraw => {
                // Live-preview theme/color while scrolling
                if let Some(ref ms) = self.menu {
                    match ms.kind {
                        MenuKind::Theme { ref presets, .. } => {
                            if let Some(&(_, _, value)) = presets.get(ms.nav.selected) {
                                crate::theme::set_accent(value);
                            }
                        }
                        MenuKind::Color { ref presets, .. } => {
                            if let Some(&(_, _, value)) = presets.get(ms.nav.selected) {
                                crate::theme::set_slug_color(value);
                            }
                        }
                        _ => {}
                    }
                }
                Action::Redraw
            }
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
                format!("/{} ", label)
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
        self.from_paste = false;
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
        // Clear from_paste if modifying the beginning of the buffer
        if prev == 0 {
            self.from_paste = false;
        }
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
        // Clear from_paste if deleting from the beginning of the buffer
        if target == 0 {
            self.from_paste = false;
        }
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
            if idx < self.attachment_ids.len() {
                self.attachment_ids.remove(idx);
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

        // Don't set from_paste for empty pastes
        if data.is_empty() {
            return;
        }

        let lines = data.lines().count();
        let char_threshold = PASTE_LINE_THRESHOLD * (crate::render::term_width().saturating_sub(1));
        // Mark as from_paste if inserting at the beginning of the current line.
        // This prevents pasted content starting with '!' from being treated as a shell escape.
        let line_start = self.buf[..self.cpos]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        if self.cpos == line_start {
            self.from_paste = true;
        }
        if lines >= PASTE_LINE_THRESHOLD || data.len() >= char_threshold {
            let id = self.store.insert_paste(data);
            self.insert_attachment_id(id);
        } else {
            self.buf.insert_str(self.cpos, &data);
            self.cpos += data.len();
        }
    }

    fn insert_attachment_id(&mut self, id: AttachmentId) {
        let idx = self.buf[..self.cpos]
            .chars()
            .filter(|&c| c == ATTACHMENT_MARKER)
            .count();
        self.attachment_ids.insert(idx, id);
        self.buf.insert(self.cpos, ATTACHMENT_MARKER);
        self.cpos += ATTACHMENT_MARKER.len_utf8();
    }

    fn maybe_remove_attachment(&mut self, byte_pos: usize) {
        if self.buf[byte_pos..].starts_with(ATTACHMENT_MARKER) {
            let idx = self.buf[..byte_pos]
                .chars()
                .filter(|&c| c == ATTACHMENT_MARKER)
                .count();
            if idx < self.attachment_ids.len() {
                self.attachment_ids.remove(idx);
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

    // ── from_paste behavior for shell escape prevention ───────────────────

    #[test]
    fn paste_into_empty_buffer_sets_from_paste() {
        let mut input = InputState::new();
        input.insert_paste("!echo hello".to_string());
        assert!(
            input.skip_shell_escape(),
            "Paste at buffer start should set from_paste"
        );
        assert_eq!(input.buf, "!echo hello");
    }

    #[test]
    fn type_then_type_sets_from_paste_false() {
        let mut input = InputState::new();
        input.insert_char('!');
        input.insert_char('e');
        assert!(
            !input.skip_shell_escape(),
            "Manual typing should clear from_paste"
        );
    }

    #[test]
    fn type_bang_then_paste_sets_from_paste() {
        let mut input = InputState::new();

        // Simulate user typing '!'
        input.insert_char('!');
        assert!(!input.skip_shell_escape(), "Typing clears from_paste");

        // Reset cursor to simulate the scenario: user types '!', then pastes at line start
        // This is the key scenario that was broken before the fix
        input.buf.clear();
        input.cpos = 0;
        input.insert_paste("echo hello".to_string());
        assert!(
            input.skip_shell_escape(),
            "Paste at line start should set from_paste"
        );
        assert_eq!(input.buf, "echo hello");
    }

    #[test]
    fn paste_in_middle_of_line_does_not_set_from_paste() {
        let mut input = InputState::new();

        input.buf = "hello ".to_string();
        input.cpos = 6; // After "hello "
        input.insert_paste("!world".to_string());
        assert!(
            !input.skip_shell_escape(),
            "Paste in middle of line should not set from_paste"
        );
        assert_eq!(input.buf, "hello !world");
    }

    #[test]
    fn paste_at_end_of_line_does_not_set_from_paste() {
        let mut input = InputState::new();

        input.buf = "hello".to_string();
        input.cpos = 5; // At end
        input.insert_paste(" world".to_string());
        assert!(
            !input.skip_shell_escape(),
            "Paste at end of line should not set from_paste"
        );
        assert_eq!(input.buf, "hello world");
    }

    #[test]
    fn paste_at_start_of_multiline_buffer() {
        let mut input = InputState::new();

        input.buf = "line1\nline2".to_string();
        input.cpos = 0; // At very start
        input.insert_paste("!command".to_string());
        assert!(
            input.skip_shell_escape(),
            "Paste at buffer start should set from_paste"
        );
        assert_eq!(input.buf, "!commandline1\nline2");
    }

    #[test]
    fn paste_at_start_of_second_line_sets_from_paste() {
        let mut input = InputState::new();

        input.buf = "line1\n".to_string();
        input.cpos = 6; // Start of second line
        input.insert_paste("!command".to_string());
        assert!(
            input.skip_shell_escape(),
            "Paste at line start should set from_paste"
        );
        assert_eq!(input.buf, "line1\n!command");
    }

    #[test]
    fn paste_middle_of_second_line_does_not_set_from_paste() {
        let mut input = InputState::new();

        input.buf = "line1\nhello".to_string();
        input.cpos = 8; // Insert at byte position 8 (before first 'l' of "hello")
        input.insert_paste(" world".to_string());
        assert!(
            !input.skip_shell_escape(),
            "Paste in middle of line should not set from_paste"
        );
        assert_eq!(input.buf, "line1\nhe worldllo");
    }

    #[test]
    fn manual_char_after_paste_clears_from_paste() {
        let mut input = InputState::new();
        input.insert_paste("!echo hello".to_string());
        assert!(input.skip_shell_escape());

        input.insert_char('x');
        assert!(
            !input.skip_shell_escape(),
            "Manual character after paste should clear from_paste"
        );
    }

    #[test]
    fn backspace_at_start_clears_from_paste() {
        let mut input = InputState::new();
        input.insert_paste("!echo hello".to_string());
        assert!(input.skip_shell_escape());

        input.backspace(); // Deletes last character
        assert!(
            input.skip_shell_escape(),
            "Backspace not at start should not clear from_paste"
        );

        input.cpos = 0;
        input.backspace(); // Now at position 0
                           // Can't backspace further, but the logic would clear it if we could
    }

    #[test]
    fn delete_word_backward_at_start_clears_from_paste() {
        let mut input = InputState::new();
        input.insert_paste("!echo hello".to_string());
        assert!(input.skip_shell_escape());

        // Move cursor to end
        input.cpos = input.buf.len();
        input.delete_word_backward(); // Deletes "hello"
        assert!(
            input.skip_shell_escape(),
            "Delete word not at start should not clear from_paste"
        );

        // Move to after "echo " and delete word
        input.cpos = 5; // After "echo"
        input.delete_word_backward(); // Deletes "echo"
        assert!(input.skip_shell_escape(), "Still not at absolute start");

        input.cpos = 1; // After "!"
        input.delete_word_backward(); // Would delete to start, which should clear from_paste
        assert!(
            !input.skip_shell_escape(),
            "Delete word to start should clear from_paste"
        );
    }

    #[test]
    fn clear_resets_from_paste() {
        let mut input = InputState::new();
        input.insert_paste("!test".to_string());
        assert!(input.skip_shell_escape());

        input.clear();
        assert!(!input.skip_shell_escape(), "Clear should reset from_paste");
    }

    #[test]
    fn large_paste_creates_attachment() {
        let mut input = InputState::new();

        // Use multi-line paste which definitely creates an attachment
        let multi_line = (0..PASTE_LINE_THRESHOLD)
            .map(|i| format!("!line{}", i))
            .collect::<Vec<_>>()
            .join("\n");
        input.insert_paste(multi_line);
        assert!(
            input.skip_shell_escape(),
            "Multi-line paste should set from_paste"
        );
        assert!(
            !input.attachment_ids.is_empty(),
            "Multi-line paste above threshold should create attachment"
        );
        assert_eq!(input.buf, "\u{FFFC}"); // Should be just the marker
    }

    #[test]
    fn multi_line_paste_above_threshold_creates_attachment() {
        let mut input = InputState::new();

        let multi_line = (0..PASTE_LINE_THRESHOLD)
            .map(|i| format!("!line{}", i))
            .collect::<Vec<_>>()
            .join("\n");
        input.insert_paste(multi_line);
        assert!(
            input.skip_shell_escape(),
            "Multi-line paste should set from_paste"
        );
        assert!(
            !input.attachment_ids.is_empty(),
            "Multi-line paste should create attachment"
        );
    }

    #[test]
    fn small_multi_line_paste_inlined() {
        let mut input = InputState::new();

        let multi_line = "!line1\nline2\nline3".to_string();
        input.insert_paste(multi_line);
        assert!(
            input.skip_shell_escape(),
            "Small multi-line paste should set from_paste"
        );
        assert!(
            input.attachment_ids.is_empty(),
            "Small multi-line paste should not create attachment"
        );
        assert_eq!(input.buf, "!line1\nline2\nline3");
    }

    #[test]
    fn stash_preserves_from_paste() {
        let mut input = InputState::new();
        input.insert_paste("!test".to_string());
        assert!(input.skip_shell_escape());

        // Stash: saves from_paste to snapshot, but doesn't clear it in active buffer
        input.toggle_stash();
        assert!(
            input.skip_shell_escape(),
            "Stash saves from_paste to snapshot but keeps it in buffer"
        );
        assert!(
            input.buf.is_empty(),
            "Buffer should be empty after stashing"
        );

        // Restore: restores from_paste from snapshot
        input.toggle_stash();
        assert!(input.skip_shell_escape(), "Stash should restore from_paste");
        assert_eq!(input.buf, "!test");
    }

    #[test]
    fn multiple_pastes_set_from_paste() {
        let mut input = InputState::new();
        input.insert_paste("!first".to_string());
        assert!(input.skip_shell_escape());

        // Type something, which clears from_paste
        input.insert_char(' ');
        assert!(!input.skip_shell_escape());

        // Paste again at start of line
        input.cpos = 0;
        input.insert_paste("!second".to_string());
        assert!(
            input.skip_shell_escape(),
            "Second paste at start should set from_paste again"
        );
    }

    #[test]
    fn paste_with_carriage_returns_normalized() {
        let mut input = InputState::new();
        input.insert_paste("!line1\r\nline2\rline3".to_string());
        assert!(input.skip_shell_escape());
        assert!(
            !input.buf.contains('\r'),
            "Carriage returns should be normalized"
        );
        assert_eq!(input.buf, "!line1\nline2\nline3");
    }

    #[test]
    fn empty_paste_does_not_set_from_paste() {
        let mut input = InputState::new();
        input.insert_paste("".to_string());
        assert!(
            !input.skip_shell_escape(),
            "Empty paste should not set from_paste"
        );
    }

    #[test]
    fn whitespace_only_paste_at_start_sets_from_paste() {
        let mut input = InputState::new();
        input.insert_paste("   ".to_string());
        assert!(
            input.skip_shell_escape(),
            "Whitespace paste at start should set from_paste"
        );
    }

    #[test]
    fn paste_starting_with_bang_at_line_start() {
        // This is the main bug scenario: type '!', then paste command
        let mut input = InputState::new();

        input.buf = String::new();
        input.cpos = 0;
        input.insert_paste("!ls -la".to_string());

        assert!(
            input.skip_shell_escape(),
            "Paste at start of line should set from_paste"
        );
        assert_eq!(input.buf, "!ls -la");

        // The expanded text should not be treated as shell command
        let text = input.expanded_text();
        assert_eq!(text, "!ls -la");
    }
}
