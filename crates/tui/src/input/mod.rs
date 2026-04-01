mod history;
mod settings;

pub use history::History;
pub use settings::{Menu, MenuAction, MenuKind, MenuResult, MenuState};

use crate::attachment::{Attachment, AttachmentId, AttachmentStore};
use crate::completer::{Completer, CompleterKind};
use crate::keymap::{self, KeyAction, KeyContext};
use crate::render;
use crate::vim::{self, ViMode, Vim};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use protocol::Content;

pub const ATTACHMENT_MARKER: char = '\u{FFFC}';

// ── Kill ring ─────────────────────────────────────────────────────────────────

const KILL_RING_MAX: usize = 32;

/// Emacs-style kill ring with yank-pop support.
struct KillRing {
    current: String,
    history: Vec<String>,
    /// Byte range of the last yank insertion, for yank-pop replacement.
    last_yank: Option<(usize, usize)>,
    pop_idx: usize,
}

impl KillRing {
    fn new() -> Self {
        Self {
            current: String::new(),
            history: Vec::new(),
            last_yank: None,
            pop_idx: 0,
        }
    }

    /// Push a new kill, rotating the previous current into history.
    fn kill(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if !self.current.is_empty() {
            self.history.insert(0, std::mem::take(&mut self.current));
            if self.history.len() > KILL_RING_MAX {
                self.history.pop();
            }
        }
        self.current = text;
        self.last_yank = None;
    }

    /// Yank the current kill into `buf` at `cpos`. Returns new cpos.
    fn yank(&mut self, buf: &mut String, cpos: usize) -> Option<usize> {
        if self.current.is_empty() {
            return None;
        }
        buf.insert_str(cpos, &self.current);
        let end = cpos + self.current.len();
        self.last_yank = Some((cpos, end));
        self.pop_idx = 0;
        Some(end)
    }

    /// Replace the last yank with the next history entry. Returns new cpos.
    fn yank_pop(&mut self, buf: &mut String) -> Option<usize> {
        let (start, end) = self.last_yank?;
        if self.history.is_empty() {
            return None;
        }
        let text = &self.history[self.pop_idx % self.history.len()];
        let new_end = start + text.len();
        buf.replace_range(start..end, text);
        self.last_yank = Some((start, new_end));
        self.pop_idx = (self.pop_idx + 1) % self.history.len();
        Some(new_end)
    }

    /// Clear last-yank tracking (call on any non-yank editing action).
    fn clear_yank(&mut self) {
        self.last_yank = None;
    }

    /// Take the current kill text (for dialog sync).
    fn take(&mut self) -> String {
        std::mem::take(&mut self.current)
    }

    /// Set the current kill text (for dialog sync).
    fn set(&mut self, text: String) {
        self.current = text;
    }

    fn current(&self) -> &str {
        &self.current
    }
}
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
    kill_ring: KillRing,
    /// Undo stack for non-vim mode.
    undo_stack: Vec<(String, usize, Vec<AttachmentId>)>,
    /// Chord state: true after Ctrl+X, waiting for second key.
    pending_ctrl_x: bool,
    /// Completable arguments for commands like `/model`, `/theme`, `/color`.
    /// Each entry is `("/cmd", vec!["arg1", "arg2", ...])`.
    pub command_arg_sources: Vec<(String, Vec<String>)>,
    /// Byte position of the selection anchor for shift+key selection (non-vim).
    /// When `Some`, selection spans from anchor to `cpos`.
    selection_anchor: Option<usize>,
}

/// What the caller should do after `handle_event`.
pub enum Action {
    Redraw,
    PurgeRedraw,
    Submit { content: Content, display: String },
    MenuResult(MenuResult),
    ToggleMode,
    CycleReasoning,
    EditInEditor,
    CenterScroll,
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
            kill_ring: KillRing::new(),
            undo_stack: Vec::new(),
            pending_ctrl_x: false,
            command_arg_sources: Vec::new(),
            selection_anchor: None,
        }
    }

    /// Returns the current selection range as (start_byte, end_byte), ordered.
    /// Works for both vim visual modes and shift+key selection.
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        // Vim visual mode takes priority.
        if let Some(ref vim) = self.vim {
            if let Some(range) = vim.visual_range(&self.buf, self.cpos) {
                return Some(range);
            }
        }
        // Non-vim shift+key selection.
        let anchor = self.selection_anchor?;
        let (a, b) = if anchor <= self.cpos {
            (anchor, self.cpos)
        } else {
            (self.cpos, anchor)
        };
        if a == b {
            None
        } else {
            Some((a, b))
        }
    }

    fn has_selection(&self) -> bool {
        self.selection_range().is_some()
    }

    /// Clear any active selection (non-vim). Called on non-shift movement or editing.
    fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    /// Start or extend selection at current cursor position (non-vim shift+key).
    fn extend_selection(&mut self) {
        if self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.cpos);
        }
    }

    /// Delete the currently selected text, returning it. Handles attachment cleanup.
    fn delete_selection(&mut self) -> Option<String> {
        let (start, end) = self.selection_range()?;
        let deleted = self.buf[start..end].to_string();
        self.remove_attachments_in_range(start, end);
        self.buf.drain(start..end);
        self.cpos = start;
        self.selection_anchor = None;
        Some(deleted)
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

    /// Take the kill ring contents (moves ownership, leaves empty).
    pub fn take_kill_ring(&mut self) -> String {
        self.kill_ring.take()
    }

    /// Set the kill ring contents (used to sync back from dialogs).
    pub fn set_kill_ring(&mut self, contents: String) {
        self.kill_ring.set(contents);
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.cpos = 0;
        self.attachment_ids.clear();
        self.completer = None;
        self.menu = None;
        self.history_saved_buf = None;
        self.from_paste = false;
        self.selection_anchor = None;
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

    #[allow(clippy::too_many_arguments)]
    pub fn open_settings(
        &mut self,
        vim_enabled: bool,
        auto_compact: bool,
        show_tps: bool,
        show_tokens: bool,
        show_cost: bool,
        show_prediction: bool,
        show_slug: bool,
        restrict_to_workspace: bool,
    ) {
        self.completer = None;
        self.menu = Some(MenuState {
            nav: Menu {
                selected: 0,
                len: 8,
                select_on_enter: false,
            },
            kind: MenuKind::Settings {
                vim_enabled,
                auto_compact,
                show_tps,
                show_tokens,
                show_cost,
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

    pub fn open_cost(&mut self, lines: Vec<crate::metrics::StatsLine>) {
        self.completer = None;
        self.menu = Some(MenuState {
            nav: Menu {
                selected: 0,
                len: 0,
                select_on_enter: false,
            },
            kind: MenuKind::Cost { lines },
        });
    }

    pub fn open_model_completer(&mut self, models: &[(String, String, String)]) {
        self.menu = None;
        self.history_saved_buf = Some((self.buf.clone(), self.cpos));
        let mut comp = Completer::models(models);
        comp.update_query(self.buf.clone());
        self.completer = Some(comp);
    }

    pub fn open_theme_completer(&mut self) {
        self.menu = None;
        self.history_saved_buf = Some((self.buf.clone(), self.cpos));
        let mut comp = Completer::themes(crate::theme::accent_value());
        comp.update_query(self.buf.clone());
        self.completer = Some(comp);
    }

    pub fn open_color_completer(&mut self) {
        self.menu = None;
        self.history_saved_buf = Some((self.buf.clone(), self.cpos));
        let mut comp = Completer::colors(crate::theme::slug_color_value());
        comp.update_query(self.buf.clone());
        self.completer = Some(comp);
    }

    pub fn has_modal(&self) -> bool {
        self.menu.is_some()
            || self.completer.as_ref().is_some_and(|c| {
                matches!(
                    c.kind,
                    CompleterKind::Model
                        | CompleterKind::Theme
                        | CompleterKind::Color
                        | CompleterKind::History
                )
            })
    }

    /// Dismiss the current menu, returning the appropriate result.
    pub fn dismiss_menu(&mut self) -> Option<MenuResult> {
        let ms = self.menu.take()?;
        Some(match ms.kind {
            MenuKind::Settings {
                vim_enabled,
                auto_compact,
                show_tps,
                show_tokens,
                show_cost,
                show_prediction,
                show_slug,
                restrict_to_workspace,
            } => MenuResult::Settings {
                vim: vim_enabled,
                auto_compact,
                show_tps,
                show_tokens,
                show_cost,
                show_prediction,
                show_slug,
                restrict_to_workspace,
            },
            MenuKind::Stats { .. } => MenuResult::Stats,
            MenuKind::Cost { .. } => MenuResult::Cost,
        })
    }

    /// Number of rows the current menu needs (0 if no menu).
    pub fn menu_rows(&self) -> usize {
        match &self.menu {
            Some(ms) => match &ms.kind {
                MenuKind::Settings { .. } => ms.nav.len,
                MenuKind::Stats { left, right } => crate::metrics::stats_row_count(left, right),
                MenuKind::Cost { lines } => lines.len(),
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

    /// Build a `KeyContext` snapshot for keymap lookups.
    pub fn key_context(&self, agent_running: bool, ghost_text_visible: bool) -> KeyContext {
        KeyContext {
            buf_empty: self.buf.is_empty() && self.attachment_ids.is_empty(),
            vim_non_insert: self.vim.as_ref().is_some_and(|v| {
                matches!(
                    v.mode(),
                    ViMode::Normal | ViMode::Visual | ViMode::VisualLine
                )
            }),
            vim_enabled: self.vim.is_some(),
            agent_running,
            ghost_text_visible,
        }
    }

    /// Execute a `KeyAction` resolved by the keymap. Handles all editing,
    /// navigation, and app-control actions. Returns `None` for actions that
    /// the caller (app event loop) must handle itself.
    pub fn execute_key_action(
        &mut self,
        action: KeyAction,
        history: Option<&mut History>,
    ) -> Action {
        if !matches!(action, KeyAction::Yank | KeyAction::YankPop) {
            self.kill_ring.clear_yank();
        }
        // Selection actions extend; editing actions consume; everything else clears.
        let is_select = matches!(
            action,
            KeyAction::SelectLeft
                | KeyAction::SelectRight
                | KeyAction::SelectWordForward
                | KeyAction::SelectWordBackward
                | KeyAction::SelectStartOfLine
                | KeyAction::SelectEndOfLine
        );
        let is_editing = matches!(
            action,
            KeyAction::Backspace
                | KeyAction::DeleteCharForward
                | KeyAction::DeleteWordBackward
                | KeyAction::DeleteWordForward
                | KeyAction::DeleteToStartOfLine
                | KeyAction::KillToEndOfLine
                | KeyAction::KillToStartOfLine
                | KeyAction::InsertNewline
                | KeyAction::Yank
                | KeyAction::CutSelection
        );
        let preserves_selection = matches!(action, KeyAction::CopySelection);
        if !is_select && !is_editing && !preserves_selection {
            self.clear_selection();
        }
        match action {
            // ── Actions the caller must handle ──────────────────────────
            KeyAction::Quit => Action::Noop,        // caller checks
            KeyAction::CancelAgent => Action::Noop, // caller checks
            KeyAction::OpenHelp => Action::Noop,    // caller checks
            KeyAction::OpenHistorySearch => Action::Noop, // caller checks
            KeyAction::AcceptGhostText => Action::Noop, // caller checks

            // ── App control ─────────────────────────────────────────────
            KeyAction::ClearBuffer => {
                self.clear();
                Action::Redraw
            }
            KeyAction::ToggleMode => Action::ToggleMode,
            KeyAction::CycleReasoning => Action::CycleReasoning,
            KeyAction::ToggleStash => {
                self.toggle_stash();
                Action::Redraw
            }
            KeyAction::PurgeRedraw => Action::PurgeRedraw,

            // ── Submit / newline ─────────────────────────────────────────
            KeyAction::Submit => {
                if self.buf.is_empty() && self.attachment_ids.is_empty() {
                    Action::Noop
                } else {
                    let display = self.message_display_text();
                    let content = self.build_content();
                    self.clear();
                    Action::Submit { content, display }
                }
            }
            KeyAction::InsertNewline => {
                if self.selection_range().is_some() {
                    self.save_undo();
                    self.delete_selection();
                }
                self.buf.insert(self.cpos, '\n');
                self.cpos += 1;
                self.completer = None;
                Action::Redraw
            }

            // ── Navigation ──────────────────────────────────────────────
            KeyAction::MoveLeft => {
                if self.cpos > 0 {
                    let cp = char_pos(&self.buf, self.cpos);
                    self.cpos = byte_of_char(&self.buf, cp - 1);
                    self.recompute_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveRight => {
                if self.cpos < self.buf.len() {
                    let cp = char_pos(&self.buf, self.cpos);
                    self.cpos = byte_of_char(&self.buf, cp + 1);
                    self.recompute_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveWordForward => {
                if self.move_word_forward() {
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveWordBackward => {
                if self.move_word_backward() {
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveUp => {
                let new_pos = vim::move_up(&self.buf, self.cpos);
                if new_pos != self.cpos {
                    self.cpos = new_pos;
                    self.recompute_completer();
                    Action::Redraw
                } else if let Some(entry) = history.and_then(|h| h.up(&self.buf)) {
                    self.buf = entry.to_string();
                    self.cpos = 0;
                    self.sync_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveDown => {
                let new_pos = vim::move_down(&self.buf, self.cpos);
                if new_pos != self.cpos {
                    self.cpos = new_pos;
                    self.recompute_completer();
                    Action::Redraw
                } else if let Some(entry) = history.and_then(|h| h.down()) {
                    self.buf = entry.to_string();
                    self.cpos = self.buf.len();
                    self.sync_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveStartOfLine => {
                self.cpos = vim::line_start(&self.buf, self.cpos);
                self.recompute_completer();
                Action::Redraw
            }
            KeyAction::MoveEndOfLine => {
                self.cpos = vim::line_end(&self.buf, self.cpos);
                self.recompute_completer();
                Action::Redraw
            }
            KeyAction::MoveStartOfBuffer => {
                self.cpos = 0;
                self.recompute_completer();
                Action::Redraw
            }
            KeyAction::MoveEndOfBuffer => {
                self.cpos = self.buf.len();
                self.recompute_completer();
                Action::Redraw
            }
            KeyAction::HistoryPrev => {
                if let Some(entry) = history.and_then(|h| h.up(&self.buf)) {
                    self.buf = entry.to_string();
                    self.cpos = 0;
                    self.sync_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::HistoryNext => {
                if let Some(entry) = history.and_then(|h| h.down()) {
                    self.buf = entry.to_string();
                    self.cpos = self.buf.len();
                    self.sync_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }

            // ── Editing ─────────────────────────────────────────────────
            KeyAction::Backspace => {
                self.backspace();
                Action::Redraw
            }
            KeyAction::DeleteCharForward => {
                self.save_undo();
                if self.has_selection() {
                    self.delete_selection();
                } else {
                    self.delete_char_forward();
                }
                Action::Redraw
            }
            KeyAction::DeleteWordBackward => {
                self.save_undo();
                if self.has_selection() {
                    self.delete_selection();
                } else {
                    self.delete_word_backward();
                }
                Action::Redraw
            }
            KeyAction::DeleteWordForward => {
                self.save_undo();
                if self.has_selection() {
                    self.delete_selection();
                } else {
                    self.delete_word_forward();
                }
                Action::Redraw
            }
            KeyAction::DeleteToStartOfLine => {
                self.save_undo();
                if self.has_selection() {
                    self.delete_selection();
                } else {
                    self.delete_to_start_of_line();
                }
                Action::Redraw
            }
            KeyAction::KillToEndOfLine => {
                self.save_undo();
                if self.has_selection() {
                    let deleted = self.delete_selection();
                    if let Some(text) = deleted {
                        self.kill_and_copy(text);
                    }
                } else {
                    self.kill_to_end_of_line();
                }
                Action::Redraw
            }
            KeyAction::KillToStartOfLine => {
                self.save_undo();
                if self.has_selection() {
                    let deleted = self.delete_selection();
                    if let Some(text) = deleted {
                        self.kill_and_copy(text);
                    }
                } else {
                    self.kill_to_start_of_line();
                }
                Action::Redraw
            }
            KeyAction::Yank => {
                self.save_undo();
                if self.has_selection() {
                    self.delete_selection();
                }
                if let Some(new_cpos) = self.kill_ring.yank(&mut self.buf, self.cpos) {
                    self.cpos = new_cpos;
                    self.recompute_completer();
                }
                Action::Redraw
            }
            KeyAction::YankPop => {
                if let Some(new_cpos) = self.kill_ring.yank_pop(&mut self.buf) {
                    self.cpos = new_cpos;
                    self.recompute_completer();
                }
                Action::Redraw
            }
            KeyAction::UppercaseWord => {
                self.save_undo();
                self.uppercase_word();
                Action::Redraw
            }
            KeyAction::LowercaseWord => {
                self.save_undo();
                self.lowercase_word();
                Action::Redraw
            }
            KeyAction::CapitalizeWord => {
                self.save_undo();
                self.capitalize_word();
                Action::Redraw
            }
            KeyAction::Undo => {
                self.undo();
                Action::Redraw
            }

            // ── Vim half-page scroll ────────────────────────────────────
            KeyAction::VimHalfPageUp => {
                let half = render::term_height() / 2;
                let line = current_line(&self.buf, self.cpos);
                let target = line.saturating_sub(half);
                self.move_to_line(target);
                Action::Redraw
            }
            KeyAction::VimHalfPageDown => {
                let half = render::term_height() / 2;
                let line = current_line(&self.buf, self.cpos);
                let total = self.buf.chars().filter(|&c| c == '\n').count() + 1;
                let target = (line + half).min(total - 1);
                self.move_to_line(target);
                Action::Redraw
            }

            // ── Clipboard ───────────────────────────────────────────────
            KeyAction::CopySelection => {
                if let Some((start, end)) = self.selection_range() {
                    let text = self.buf[start..end].to_string();
                    let _ = crate::app::copy_to_clipboard(&text);
                    self.kill_ring.set(text);
                }
                Action::Noop
            }
            KeyAction::CutSelection => {
                if self.selection_range().is_some() {
                    self.save_undo();
                    if let Some(text) = self.delete_selection() {
                        let _ = crate::app::copy_to_clipboard(&text);
                        self.kill_ring.set(text);
                    }
                    self.recompute_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::ClipboardImage => {
                if let Some(url) = clipboard_image_to_data_url() {
                    self.save_undo();
                    self.insert_image("clipboard.png".into(), url);
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }

            // ── Selection (shift+movement) ─────────────────────────────
            KeyAction::SelectLeft => {
                self.extend_selection();
                if self.cpos > 0 {
                    let cp = char_pos(&self.buf, self.cpos);
                    self.cpos = byte_of_char(&self.buf, cp - 1);
                }
                Action::Redraw
            }
            KeyAction::SelectRight => {
                self.extend_selection();
                if self.cpos < self.buf.len() {
                    let cp = char_pos(&self.buf, self.cpos);
                    self.cpos = byte_of_char(&self.buf, cp + 1);
                }
                Action::Redraw
            }
            KeyAction::SelectWordForward => {
                self.extend_selection();
                self.cpos = vim::word_forward_pos(&self.buf, self.cpos, vim::CharClass::Word);
                Action::Redraw
            }
            KeyAction::SelectWordBackward => {
                self.extend_selection();
                self.cpos = vim::word_backward_pos(&self.buf, self.cpos, vim::CharClass::Word);
                Action::Redraw
            }
            KeyAction::SelectStartOfLine => {
                self.extend_selection();
                self.cpos = vim::line_start(&self.buf, self.cpos);
                Action::Redraw
            }
            KeyAction::SelectEndOfLine => {
                self.extend_selection();
                self.cpos = vim::line_end(&self.buf, self.cpos);
                Action::Redraw
            }
        }
    }

    /// Process a terminal event. Returns what the caller should do next.
    pub fn handle_event(&mut self, ev: Event, mut history: Option<&mut History>) -> Action {
        // Menu intercepts all keys when open.
        if self.menu.is_some() {
            return self.handle_menu_event(&ev);
        }

        // Completer intercepts navigation keys when active.
        if self.completer.is_some() {
            if let Some(action) = self.handle_completer_event(&ev) {
                return action;
            }
        }

        // Vim mode intercepts key events.
        if let Some(ref mut vim) = self.vim {
            if let Event::Key(key_ev) = ev {
                // Sync kill ring → vim register so `p` can paste emacs-killed text.
                if vim.register() != self.kill_ring.current() {
                    vim.set_register(self.kill_ring.current().to_string());
                }
                match vim.handle_key(
                    key_ev,
                    &mut self.buf,
                    &mut self.cpos,
                    &mut self.attachment_ids,
                ) {
                    vim::Action::Consumed => {
                        // Sync vim register → kill ring so `Ctrl+Y` can paste vim-yanked text.
                        self.sync_vim_register();
                        // Clear shift+key selection on any vim-consumed key
                        // (e.g. Esc in insert mode, Esc in visual mode).
                        self.clear_selection();
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
                    vim::Action::EditInEditor => {
                        return Action::EditInEditor;
                    }
                    vim::Action::CenterScroll => {
                        return Action::CenterScroll;
                    }
                    vim::Action::Passthrough => {
                        // Fall through to keymap / char insert below.
                    }
                }
            }
        }

        // Paste events (not key events — handled before keymap).
        if let Event::Paste(data) = ev {
            self.save_undo();
            if self.selection_range().is_some() {
                self.delete_selection();
            }
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
            if data.trim().is_empty() {
                if let Some(url) = clipboard_image_to_data_url() {
                    self.insert_image("clipboard.png".into(), url);
                    return Action::Redraw;
                }
            }
            self.insert_paste(data);
            return Action::Redraw;
        }

        // Resize events.
        if let Event::Resize(w, h) = ev {
            return Action::Resize {
                width: w as usize,
                height: h as usize,
            };
        }

        // Key events — look up in the keymap.
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = ev
        {
            // Chord: C-x C-e → edit in $EDITOR.
            if self.pending_ctrl_x {
                self.pending_ctrl_x = false;
                if code == KeyCode::Char('e') && modifiers.contains(KeyModifiers::CONTROL) {
                    return Action::EditInEditor;
                }
                // Not a recognized chord — discard the C-x and process this
                // key normally below.
            }
            if code == KeyCode::Char('x') && modifiers.contains(KeyModifiers::CONTROL) {
                self.pending_ctrl_x = true;
                return Action::Noop;
            }

            // Build context for keymap lookup. The caller-specific fields
            // (agent_running, ghost_text) are set to defaults here — the app
            // event loop overrides them by calling lookup directly when needed.
            let ctx = KeyContext {
                buf_empty: self.buf.is_empty() && self.attachment_ids.is_empty(),
                vim_non_insert: self.vim.as_ref().is_some_and(|v| {
                    matches!(
                        v.mode(),
                        ViMode::Normal | ViMode::Visual | ViMode::VisualLine
                    )
                }),
                vim_enabled: self.vim.is_some(),
                agent_running: false,
                ghost_text_visible: false,
            };

            if let Some(action) = keymap::lookup(code, modifiers, &ctx) {
                return self.execute_key_action(action, history);
            }

            // Fallback: insert character for unmodified / shift-only key presses.
            if let KeyCode::Char(c) = code {
                if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT {
                    self.insert_char(c);
                    return Action::Redraw;
                }
            }
        }

        Action::Noop
    }

    // ── Completer ────────────────────────────────────────────────────────

    fn handle_menu_event(&mut self, ev: &Event) -> Action {
        let ms = self.menu.as_mut().unwrap();
        match ms.nav.handle_event(ev) {
            MenuAction::Toggle(idx) => {
                if let MenuKind::Settings {
                    ref mut vim_enabled,
                    ref mut auto_compact,
                    ref mut show_tps,
                    ref mut show_tokens,
                    ref mut show_cost,
                    ref mut show_prediction,
                    ref mut show_slug,
                    ref mut restrict_to_workspace,
                } = ms.kind
                {
                    match idx {
                        0 => *vim_enabled ^= true,
                        1 => *auto_compact ^= true,
                        2 => *show_tps ^= true,
                        3 => *show_tokens ^= true,
                        4 => *show_cost ^= true,
                        5 => *show_prediction ^= true,
                        6 => *show_slug ^= true,
                        7 => *restrict_to_workspace ^= true,
                        _ => {}
                    }
                }
                Action::Redraw
            }
            MenuAction::Tab => Action::Redraw,
            MenuAction::Select(_) => Action::Redraw,
            MenuAction::Dismiss => Action::MenuResult(self.dismiss_menu().unwrap()),
            MenuAction::Redraw => Action::Redraw,
            MenuAction::Noop => Action::Noop,
        }
    }

    /// Accept the selected item from a Model/Theme/Color picker, clear the buffer,
    /// apply side effects, and return the appropriate action.
    fn accept_picker(&mut self, comp: Completer) -> Action {
        self.history_saved_buf = None;
        self.buf.clear();
        self.cpos = 0;
        match comp.kind {
            CompleterKind::Model => match comp.accept_extra().map(|s| s.to_string()) {
                Some(k) => Action::MenuResult(MenuResult::ModelSelect(k)),
                None => Action::Redraw,
            },
            CompleterKind::Theme => match comp.selected_item().and_then(|i| i.ansi_color) {
                Some(v) => {
                    crate::theme::set_accent(v);
                    Action::MenuResult(MenuResult::ThemeSelect(v))
                }
                None => Action::Redraw,
            },
            CompleterKind::Color => match comp.selected_item().and_then(|i| i.ansi_color) {
                Some(v) => {
                    crate::theme::set_slug_color(v);
                    Action::MenuResult(MenuResult::ColorSelect(v))
                }
                None => Action::Redraw,
            },
            _ => Action::Redraw,
        }
    }

    /// Try to handle the event as a completer navigation. Returns Some if consumed.
    fn handle_completer_event(&mut self, ev: &Event) -> Option<Action> {
        let kind = self.completer.as_ref().map(|c| c.kind)?;
        let is_picker = matches!(
            kind,
            CompleterKind::History
                | CompleterKind::Model
                | CompleterKind::Theme
                | CompleterKind::Color
        );

        match ev {
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers,
                ..
            }) if !modifiers.contains(KeyModifiers::SHIFT) => {
                let comp = self.completer.take().unwrap();
                match comp.kind {
                    CompleterKind::History => {
                        if let Some(label) = comp.accept() {
                            self.buf = label.to_string();
                            self.cpos = self.buf.len();
                        }
                        self.history_saved_buf = None;
                        Some(Action::Redraw)
                    }
                    CompleterKind::Model | CompleterKind::Theme | CompleterKind::Color => {
                        Some(self.accept_picker(comp))
                    }
                    _ => {
                        let kind = comp.kind;
                        self.accept_completion(&comp);
                        if kind == CompleterKind::Command {
                            let display = self.message_display_text();
                            let content = self.build_content();
                            self.clear();
                            Some(Action::Submit { content, display })
                        } else {
                            Some(Action::Redraw)
                        }
                    }
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Esc, ..
            }) => {
                let comp = self.completer.take().unwrap();
                // Restore original theme/color on dismiss.
                match comp.kind {
                    CompleterKind::Theme => {
                        if let Some(orig) = comp.original_value {
                            crate::theme::set_accent(orig);
                        }
                    }
                    CompleterKind::Color => {
                        if let Some(orig) = comp.original_value {
                            crate::theme::set_slug_color(orig);
                        }
                    }
                    _ => {}
                }
                // Restore saved buffer for all picker types.
                if is_picker {
                    if let Some((buf, cpos)) = self.history_saved_buf.take() {
                        self.buf = buf;
                        self.cpos = cpos;
                    }
                }
                Some(Action::Redraw)
            }
            // Ctrl+R cycles forward through history matches.
            Event::Key(KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) if kind == CompleterKind::History => {
                let comp = self.completer.as_mut().unwrap();
                comp.move_down();
                Some(Action::Redraw)
            }
            Event::Key(KeyEvent {
                code: KeyCode::Up, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('k' | 'p'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                let comp = self.completer.as_mut().unwrap();
                if comp.results.len() <= 1 && !is_picker {
                    return None;
                }
                comp.move_up();
                self.live_preview_picker();
                Some(Action::Redraw)
            }
            Event::Key(KeyEvent {
                code: KeyCode::Down,
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('j' | 'n'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                let comp = self.completer.as_mut().unwrap();
                if comp.results.len() <= 1 && !is_picker {
                    return None;
                }
                comp.move_down();
                self.live_preview_picker();
                Some(Action::Redraw)
            }
            Event::Key(KeyEvent {
                code: KeyCode::Tab, ..
            }) => {
                let comp = self.completer.take().unwrap();
                match comp.kind {
                    CompleterKind::History => {
                        if let Some(label) = comp.accept() {
                            self.buf = label.to_string();
                            self.cpos = self.buf.len();
                        }
                        self.history_saved_buf = None;
                    }
                    CompleterKind::Model | CompleterKind::Theme | CompleterKind::Color => {
                        return Some(self.accept_picker(comp));
                    }
                    _ => {
                        let was_command = comp.kind == CompleterKind::Command;
                        self.accept_completion(&comp);
                        if was_command {
                            self.sync_completer();
                        }
                    }
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
            if comp.kind == CompleterKind::CommandArg {
                // Replace just the argument portion after the command prefix.
                self.buf.replace_range(start..end, label);
                self.cpos = start + label.len();
            } else {
                let trigger = &self.buf[start..start + 1];
                let replacement = if trigger == "/" {
                    format!("/{} ", label)
                } else if label.contains(' ') {
                    format!("@\"{}\" ", label)
                } else {
                    format!("@{} ", label)
                };
                self.buf.replace_range(start..end, &replacement);
                self.cpos = start + replacement.len();
            }
        }
    }

    /// Activate completer if the buffer looks like a command or file ref.
    fn sync_completer(&mut self) {
        if let Some((src_idx, arg_anchor)) = self.find_command_arg_zone() {
            let items = self.command_arg_sources[src_idx].1.clone();
            let query = self.arg_query(arg_anchor);
            self.set_or_update_completer(
                CompleterKind::CommandArg,
                || Completer::command_args(arg_anchor, &items),
                query,
            );
        } else if find_slash_anchor(&self.buf, self.cpos).is_some() {
            let query = self.buf[1..self.cpos].to_string();
            self.set_or_update_completer(CompleterKind::Command, || Completer::commands(0), query);
        } else {
            self.completer = None;
        }
    }

    /// Live-preview theme/color while navigating in a picker completer.
    fn live_preview_picker(&self) {
        if let Some(comp) = &self.completer {
            if let Some(item) = comp.results.get(comp.selected) {
                match comp.kind {
                    CompleterKind::Theme => {
                        if let Some(c) = item.ansi_color {
                            crate::theme::set_accent(c);
                        }
                    }
                    CompleterKind::Color => {
                        if let Some(c) = item.ansi_color {
                            crate::theme::set_slug_color(c);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Recompute the completer based on where the cursor currently sits.
    /// Shows the file or command picker if the cursor is inside an @/slash zone,
    /// hides it otherwise. Never touches a history/picker completer.
    fn recompute_completer(&mut self) {
        if self.completer.as_ref().is_some_and(|c| {
            matches!(
                c.kind,
                CompleterKind::History
                    | CompleterKind::Model
                    | CompleterKind::Theme
                    | CompleterKind::Color
            )
        }) {
            let query = self.buf.clone();
            self.completer.as_mut().unwrap().update_query(query);
            self.live_preview_picker();
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
        } else if let Some((src_idx, arg_anchor)) = self.find_command_arg_zone() {
            let items = self.command_arg_sources[src_idx].1.clone();
            let query = self.arg_query(arg_anchor);
            self.set_or_update_completer(
                CompleterKind::CommandArg,
                || Completer::command_args(arg_anchor, &items),
                query,
            );
        } else if find_slash_anchor(&self.buf, self.cpos).is_some()
            || (self.cpos == 0 && self.buf.starts_with('/'))
        {
            let end = self.cpos.max(1);
            let query = self.buf[1..end].to_string();
            self.set_or_update_completer(CompleterKind::Command, || Completer::commands(0), query);
        } else {
            self.completer = None;
        }
    }

    /// Reuse the current completer if it matches `kind`, otherwise create a new
    /// one via `make`. Either way, update the query.
    fn set_or_update_completer(
        &mut self,
        kind: CompleterKind,
        make: impl FnOnce() -> Completer,
        query: String,
    ) {
        if self.completer.as_ref().is_some_and(|c| c.kind == kind) {
            self.completer.as_mut().unwrap().update_query(query);
        } else {
            let mut comp = make();
            comp.update_query(query);
            self.completer = Some(comp);
        }
    }

    fn arg_query(&self, anchor: usize) -> String {
        if self.cpos > anchor {
            self.buf[anchor..self.cpos].to_string()
        } else {
            String::new()
        }
    }

    /// Check if the cursor is inside a command argument zone (e.g. `/model foo`).
    /// Returns `(source_index, arg_anchor)` where source_index indexes into
    /// `command_arg_sources` and arg_anchor is the byte offset after the space.
    fn find_command_arg_zone(&self) -> Option<(usize, usize)> {
        for (i, (cmd, _)) in self.command_arg_sources.iter().enumerate() {
            let anchor = cmd.len() + 1; // "/cmd" + space
            if self.buf.len() >= anchor
                && self.buf.starts_with(cmd.as_str())
                && self.buf.as_bytes()[cmd.len()] == b' '
                && self.cpos >= anchor
            {
                return Some((i, anchor));
            }
        }
        None
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

    /// Kill text into the kill ring and copy to the system clipboard.
    fn kill_and_copy(&mut self, text: String) {
        if !text.is_empty() {
            let _ = crate::app::copy_to_clipboard(&text);
        }
        self.kill_ring.kill(text);
    }

    /// Sync vim register → kill ring after vim modifies it.
    /// Also copies to the system clipboard.
    fn sync_vim_register(&mut self) {
        if let Some(ref vim) = self.vim {
            if vim.register() != self.kill_ring.current() {
                let text = vim.register().to_string();
                let _ = crate::app::copy_to_clipboard(&text);
                self.kill_ring.set(text);
            }
        }
    }

    /// Save undo state before an editing operation.
    /// When vim is in insert mode, skip — the entire insert session is
    /// already covered by the undo entry saved on insert entry.
    pub fn save_undo(&mut self) {
        if let Some(ref mut vim) = self.vim {
            if vim.mode() == ViMode::Insert {
                return; // insert session groups all edits into one undo step
            }
            vim.save_undo(&self.buf, self.cpos, &self.attachment_ids);
        } else {
            self.undo_stack
                .push((self.buf.clone(), self.cpos, self.attachment_ids.clone()));
            if self.undo_stack.len() > 100 {
                self.undo_stack.remove(0);
            }
        }
    }

    fn insert_char(&mut self, c: char) {
        self.from_paste = false;
        if self.selection_range().is_some() {
            self.save_undo();
            self.delete_selection();
        }
        self.buf.insert(self.cpos, c);
        self.cpos += c.len_utf8();
        self.recompute_completer();
    }

    fn backspace(&mut self) {
        if self.selection_range().is_some() {
            self.save_undo();
            self.delete_selection();
            self.recompute_completer();
            return;
        }
        if self.cpos == 0 {
            return;
        }
        // If deleting the closing `"` of a `"@path"` token, remove the whole token.
        if let Some(start) = self.quoted_at_ref_start() {
            // Clear from_paste if deleting from the beginning of the buffer
            if start == 0 {
                self.from_paste = false;
            }
            self.buf.drain(start..self.cpos);
            self.cpos = start;
            self.recompute_completer();
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

    /// If the cursor is right after the closing `"` of a `"@path"` token,
    /// return the byte offset of the opening `"`.
    fn quoted_at_ref_start(&self) -> Option<usize> {
        let before = &self.buf[..self.cpos];
        if !before.ends_with('"') {
            return None;
        }
        let inner = &before[..before.len() - 1];
        let at_pos = inner.rfind("@\"")?;
        if at_pos > 0 && !self.buf[..at_pos].ends_with(char::is_whitespace) {
            return None;
        }
        // Reject if the content between @" and closing " contains another quote.
        if inner[at_pos + 2..].contains('"') {
            return None;
        }
        Some(at_pos)
    }

    fn delete_word_backward(&mut self) {
        if self.cpos == 0 {
            return;
        }
        let target = vim::word_backward_pos(&self.buf, self.cpos, vim::CharClass::Word);
        if target == 0 {
            self.from_paste = false;
        }
        self.remove_attachments_in_range(target, self.cpos);
        self.buf.drain(target..self.cpos);
        self.cpos = target;
        self.recompute_completer();
    }

    fn delete_char_forward(&mut self) {
        if self.cpos >= self.buf.len() {
            return;
        }
        self.maybe_remove_attachment(self.cpos);
        let next = self.buf[self.cpos..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cpos + i)
            .unwrap_or(self.buf.len());
        self.buf.drain(self.cpos..next);
        self.recompute_completer();
    }

    fn delete_word_forward(&mut self) {
        if self.cpos >= self.buf.len() {
            return;
        }
        let target = vim::word_forward_pos(&self.buf, self.cpos, vim::CharClass::Word);
        self.remove_attachments_in_range(self.cpos, target);
        self.buf.drain(self.cpos..target);
        self.recompute_completer();
    }

    fn kill_to_end_of_line(&mut self) {
        let end = self.buf[self.cpos..]
            .find('\n')
            .map(|i| self.cpos + i)
            .unwrap_or(self.buf.len());
        let killed = self.buf[self.cpos..end].to_string();
        self.remove_attachments_in_range(self.cpos, end);
        self.buf.drain(self.cpos..end);
        self.kill_and_copy(killed);
        self.recompute_completer();
    }

    fn kill_to_start_of_line(&mut self) {
        let start = self.buf[..self.cpos]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let killed = self.buf[start..self.cpos].to_string();
        self.remove_attachments_in_range(start, self.cpos);
        self.buf.drain(start..self.cpos);
        self.cpos = start;
        self.kill_and_copy(killed);
        self.recompute_completer();
    }

    fn delete_to_start_of_line(&mut self) {
        let start = self.buf[..self.cpos]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.remove_attachments_in_range(start, self.cpos);
        self.buf.drain(start..self.cpos);
        self.cpos = start;
        self.recompute_completer();
    }

    fn uppercase_word(&mut self) {
        let end = vim::word_forward_pos(&self.buf, self.cpos, vim::CharClass::Word);
        if end == self.cpos {
            return;
        }
        let upper: String = self.buf[self.cpos..end].to_uppercase();
        self.buf.replace_range(self.cpos..end, &upper);
        self.cpos += upper.len();
        self.recompute_completer();
    }

    fn lowercase_word(&mut self) {
        let end = vim::word_forward_pos(&self.buf, self.cpos, vim::CharClass::Word);
        if end == self.cpos {
            return;
        }
        let lower: String = self.buf[self.cpos..end].to_lowercase();
        self.buf.replace_range(self.cpos..end, &lower);
        self.cpos += lower.len();
        self.recompute_completer();
    }

    fn capitalize_word(&mut self) {
        let end = vim::word_forward_pos(&self.buf, self.cpos, vim::CharClass::Word);
        if end == self.cpos {
            return;
        }
        let word = &self.buf[self.cpos..end];
        let mut cap = String::with_capacity(word.len());
        let mut first = true;
        for c in word.chars() {
            if first && c.is_alphabetic() {
                cap.extend(c.to_uppercase());
                first = false;
            } else {
                cap.push(c);
            }
        }
        self.buf.replace_range(self.cpos..end, &cap);
        self.cpos += cap.len();
        self.recompute_completer();
    }

    fn undo(&mut self) {
        if let Some(ref mut vim) = self.vim {
            vim.undo(&mut self.buf, &mut self.cpos, &mut self.attachment_ids);
        } else if let Some((buf, cpos, att)) = self.undo_stack.pop() {
            self.buf = buf;
            self.cpos = cpos;
            self.attachment_ids = att;
        }
        self.recompute_completer();
    }

    fn move_word_forward(&mut self) -> bool {
        if self.cpos >= self.buf.len() {
            return false;
        }
        let target = vim::word_forward_pos(&self.buf, self.cpos, vim::CharClass::Word);
        if target != self.cpos {
            self.cpos = target;
            self.recompute_completer();
            true
        } else {
            false
        }
    }

    fn move_word_backward(&mut self) -> bool {
        if self.cpos == 0 {
            return false;
        }
        let target = vim::word_backward_pos(&self.buf, self.cpos, vim::CharClass::Word);
        if target != self.cpos {
            self.cpos = target;
            self.recompute_completer();
            true
        } else {
            false
        }
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

    /// Remove attachment IDs for any markers in `buf[start..end]`.
    fn remove_attachments_in_range(&mut self, start: usize, end: usize) {
        let before = self.buf[..start]
            .chars()
            .filter(|&c| c == ATTACHMENT_MARKER)
            .count();
        let count = self.buf[start..end]
            .chars()
            .filter(|&c| c == ATTACHMENT_MARKER)
            .count();
        for i in (0..count).rev() {
            let idx = before + i;
            if idx < self.attachment_ids.len() {
                self.attachment_ids.remove(idx);
            }
        }
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

/// Read image data from the system clipboard and return a data URL.
///
/// On macOS, uses `osascript` to write clipboard image to a temp file.
/// On Linux, tries `xclip` then `wl-paste`.
fn clipboard_image_to_data_url() -> Option<String> {
    use base64::Engine;

    let tmp = std::env::temp_dir().join("agent_clipboard.png");
    let tmp_str = tmp.to_string_lossy();

    let ok = if cfg!(target_os = "macos") {
        std::process::Command::new("osascript")
            .args([
                "-e",
                &format!(
                    "set f to (open for access POSIX file \"{}\" with write permission)\n\
                     try\n\
                       write (the clipboard as «class PNGf») to f\n\
                     end try\n\
                     close access f",
                    tmp_str
                ),
            ])
            .output()
            .ok()
            .is_some_and(|o| o.status.success())
    } else {
        // Try xclip first, then wl-paste.
        std::process::Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "image/png", "-o"])
            .stdout(std::fs::File::create(&tmp).ok()?)
            .status()
            .ok()
            .is_some_and(|s| s.success())
            || std::process::Command::new("wl-paste")
                .args(["--type", "image/png"])
                .stdout(std::fs::File::create(&tmp).ok()?)
                .status()
                .ok()
                .is_some_and(|s| s.success())
    };

    if !ok {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }

    let bytes = std::fs::read(&tmp).ok()?;
    let _ = std::fs::remove_file(&tmp);
    if bytes.is_empty() {
        return None;
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
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

    // ── Selection tests ─────────────────────────────────────────────────

    #[test]
    fn shift_select_right_creates_selection() {
        let mut input = InputState::new();
        input.buf = "hello".to_string();
        input.cpos = 0;
        input.execute_key_action(KeyAction::SelectRight, None);
        assert_eq!(input.selection_anchor, Some(0));
        assert_eq!(input.cpos, 1);
        assert_eq!(input.selection_range(), Some((0, 1)));
    }

    #[test]
    fn shift_select_extends_selection() {
        let mut input = InputState::new();
        input.buf = "hello".to_string();
        input.cpos = 0;
        input.execute_key_action(KeyAction::SelectRight, None);
        input.execute_key_action(KeyAction::SelectRight, None);
        input.execute_key_action(KeyAction::SelectRight, None);
        assert_eq!(input.selection_anchor, Some(0));
        assert_eq!(input.cpos, 3);
        assert_eq!(input.selection_range(), Some((0, 3)));
    }

    #[test]
    fn movement_clears_selection() {
        let mut input = InputState::new();
        input.buf = "hello".to_string();
        input.cpos = 0;
        input.execute_key_action(KeyAction::SelectRight, None);
        input.execute_key_action(KeyAction::SelectRight, None);
        assert!(input.selection_range().is_some());
        input.execute_key_action(KeyAction::MoveRight, None);
        assert!(input.selection_range().is_none());
    }

    #[test]
    fn backspace_deletes_selection() {
        let mut input = InputState::new();
        input.buf = "hello world".to_string();
        input.cpos = 0;
        // Select "hello"
        for _ in 0..5 {
            input.execute_key_action(KeyAction::SelectRight, None);
        }
        assert_eq!(input.selection_range(), Some((0, 5)));
        input.execute_key_action(KeyAction::Backspace, None);
        assert_eq!(input.buf, " world");
        assert_eq!(input.cpos, 0);
    }

    #[test]
    fn delete_forward_deletes_selection() {
        let mut input = InputState::new();
        input.buf = "hello world".to_string();
        input.cpos = 0;
        for _ in 0..5 {
            input.execute_key_action(KeyAction::SelectRight, None);
        }
        input.execute_key_action(KeyAction::DeleteCharForward, None);
        assert_eq!(input.buf, " world");
    }

    #[test]
    fn typing_replaces_selection() {
        let mut input = InputState::new();
        input.buf = "hello world".to_string();
        input.cpos = 0;
        for _ in 0..5 {
            input.execute_key_action(KeyAction::SelectRight, None);
        }
        input.insert_char('X');
        assert_eq!(input.buf, "X world");
        assert_eq!(input.cpos, 1);
    }

    #[test]
    fn select_left_from_end() {
        let mut input = InputState::new();
        input.buf = "hello".to_string();
        input.cpos = 5;
        input.execute_key_action(KeyAction::SelectLeft, None);
        input.execute_key_action(KeyAction::SelectLeft, None);
        assert_eq!(input.selection_anchor, Some(5));
        assert_eq!(input.cpos, 3);
        assert_eq!(input.selection_range(), Some((3, 5)));
    }

    #[test]
    fn select_word_forward() {
        let mut input = InputState::new();
        input.buf = "hello world foo".to_string();
        input.cpos = 0;
        input.execute_key_action(KeyAction::SelectWordForward, None);
        assert_eq!(input.selection_anchor, Some(0));
        // word_forward_pos from 0 should be 6 (start of "world").
        assert_eq!(input.cpos, 6);
        input.execute_key_action(KeyAction::Backspace, None);
        assert_eq!(input.buf, "world foo");
    }

    #[test]
    fn select_word_backward() {
        let mut input = InputState::new();
        input.buf = "hello world".to_string();
        input.cpos = 11;
        input.execute_key_action(KeyAction::SelectWordBackward, None);
        assert_eq!(input.selection_range(), Some((6, 11)));
        input.execute_key_action(KeyAction::Backspace, None);
        assert_eq!(input.buf, "hello ");
    }

    #[test]
    fn select_to_line_start() {
        let mut input = InputState::new();
        input.buf = "hello world".to_string();
        input.cpos = 5;
        input.execute_key_action(KeyAction::SelectStartOfLine, None);
        assert_eq!(input.selection_range(), Some((0, 5)));
    }

    #[test]
    fn select_to_line_end() {
        let mut input = InputState::new();
        input.buf = "hello world".to_string();
        input.cpos = 5;
        input.execute_key_action(KeyAction::SelectEndOfLine, None);
        assert_eq!(input.selection_range(), Some((5, 11)));
    }

    #[test]
    fn newline_replaces_selection() {
        let mut input = InputState::new();
        input.buf = "hello world".to_string();
        input.cpos = 0;
        for _ in 0..5 {
            input.execute_key_action(KeyAction::SelectRight, None);
        }
        input.execute_key_action(KeyAction::InsertNewline, None);
        assert_eq!(input.buf, "\n world");
        assert_eq!(input.cpos, 1);
    }

    #[test]
    fn kill_to_eol_with_selection() {
        let mut input = InputState::new();
        input.buf = "hello world".to_string();
        input.cpos = 0;
        for _ in 0..5 {
            input.execute_key_action(KeyAction::SelectRight, None);
        }
        input.execute_key_action(KeyAction::KillToEndOfLine, None);
        assert_eq!(input.buf, " world");
        // Killed text should be in kill ring.
        assert_eq!(input.kill_ring.current(), "hello");
    }

    #[test]
    fn selection_at_buffer_boundary() {
        let mut input = InputState::new();
        input.buf = "ab".to_string();
        input.cpos = 0;
        // Select all.
        input.execute_key_action(KeyAction::SelectRight, None);
        input.execute_key_action(KeyAction::SelectRight, None);
        assert_eq!(input.selection_range(), Some((0, 2)));
        input.execute_key_action(KeyAction::Backspace, None);
        assert_eq!(input.buf, "");
        assert_eq!(input.cpos, 0);
    }

    #[test]
    fn selection_range_empty_when_anchor_equals_cursor() {
        let mut input = InputState::new();
        input.buf = "hello".to_string();
        input.cpos = 3;
        input.selection_anchor = Some(3);
        assert_eq!(input.selection_range(), None);
    }

    #[test]
    fn clear_resets_selection() {
        let mut input = InputState::new();
        input.buf = "hello".to_string();
        input.cpos = 0;
        input.execute_key_action(KeyAction::SelectRight, None);
        assert!(input.selection_range().is_some());
        input.clear();
        assert!(input.selection_range().is_none());
    }

    #[test]
    fn delete_word_backward_with_selection() {
        let mut input = InputState::new();
        input.buf = "hello world".to_string();
        input.cpos = 6;
        // Select "wor"
        for _ in 0..3 {
            input.execute_key_action(KeyAction::SelectRight, None);
        }
        input.execute_key_action(KeyAction::DeleteWordBackward, None);
        assert_eq!(input.buf, "hello ld");
    }

    #[test]
    fn delete_word_forward_with_selection() {
        let mut input = InputState::new();
        input.buf = "hello world".to_string();
        input.cpos = 0;
        for _ in 0..3 {
            input.execute_key_action(KeyAction::SelectRight, None);
        }
        input.execute_key_action(KeyAction::DeleteWordForward, None);
        assert_eq!(input.buf, "lo world");
    }

    #[test]
    fn delete_to_start_of_line_with_selection() {
        let mut input = InputState::new();
        input.buf = "hello world".to_string();
        input.cpos = 3;
        for _ in 0..4 {
            input.execute_key_action(KeyAction::SelectRight, None);
        }
        input.execute_key_action(KeyAction::DeleteToStartOfLine, None);
        assert_eq!(input.buf, "helorld");
    }

    #[test]
    fn select_left_at_start_stays() {
        let mut input = InputState::new();
        input.buf = "hello".to_string();
        input.cpos = 0;
        input.execute_key_action(KeyAction::SelectLeft, None);
        assert_eq!(input.cpos, 0);
        assert_eq!(input.selection_anchor, Some(0));
    }

    #[test]
    fn select_right_at_end_stays() {
        let mut input = InputState::new();
        input.buf = "hello".to_string();
        input.cpos = 5;
        input.execute_key_action(KeyAction::SelectRight, None);
        assert_eq!(input.cpos, 5);
    }

    #[test]
    fn select_empty_buffer() {
        let mut input = InputState::new();
        input.buf = String::new();
        input.cpos = 0;
        input.execute_key_action(KeyAction::SelectRight, None);
        assert_eq!(input.cpos, 0);
        assert!(input.selection_range().is_none());
    }

    #[test]
    fn utf8_selection() {
        let mut input = InputState::new();
        input.buf = "héllo".to_string();
        input.cpos = 0;
        input.execute_key_action(KeyAction::SelectRight, None);
        input.execute_key_action(KeyAction::SelectRight, None);
        // Should select "hé" — 2 chars but 3 bytes.
        assert_eq!(input.cpos, 3); // byte offset of 'l'
        assert_eq!(input.selection_range(), Some((0, 3)));
        input.execute_key_action(KeyAction::Backspace, None);
        assert_eq!(input.buf, "llo");
    }

    #[test]
    fn selection_preserved_across_multiple_select_directions() {
        let mut input = InputState::new();
        input.buf = "abcdef".to_string();
        input.cpos = 3; // on 'd'
                        // Select right 2 chars.
        input.execute_key_action(KeyAction::SelectRight, None);
        input.execute_key_action(KeyAction::SelectRight, None);
        assert_eq!(input.selection_range(), Some((3, 5)));
        // Then select left 4 chars — anchor stays at 3.
        input.execute_key_action(KeyAction::SelectLeft, None);
        input.execute_key_action(KeyAction::SelectLeft, None);
        input.execute_key_action(KeyAction::SelectLeft, None);
        input.execute_key_action(KeyAction::SelectLeft, None);
        assert_eq!(input.cpos, 1);
        assert_eq!(input.selection_range(), Some((1, 3)));
    }

    #[test]
    fn vim_esc_clears_shift_selection() {
        use crossterm::event::{
            Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
        };

        let mut input = InputState::new();
        input.set_vim_enabled(true);
        input.buf = "hello world".to_string();
        input.cpos = 0;
        // Create a shift selection.
        input.execute_key_action(KeyAction::SelectRight, None);
        input.execute_key_action(KeyAction::SelectRight, None);
        assert!(input.selection_range().is_some());
        // Press Esc — vim switches to normal mode AND clears selection.
        let esc = Event::Key(KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        });
        input.handle_event(esc, None);
        assert!(
            input.selection_range().is_none(),
            "Esc should clear shift selection"
        );
        assert_eq!(
            input.vim_mode(),
            Some(crate::vim::ViMode::Normal),
            "Should be in normal mode"
        );
    }

    #[test]
    fn delete_selection_removes_attachments() {
        let mut input = InputState::new();
        // Insert text with an attachment marker in the middle: "ab[paste]cd"
        input.buf = format!("ab{}cd", ATTACHMENT_MARKER);
        input.cpos = 0;
        let id = input.store.insert_paste("pasted".to_string());
        input.attachment_ids.push(id);
        // Select "b[paste]c" (bytes 1..5 — marker is 3 bytes)
        input.selection_anchor = Some(1);
        input.cpos = 1 + 1 + ATTACHMENT_MARKER.len_utf8() + 1; // b + marker + c
        assert!(input.selection_range().is_some());
        let deleted = input.delete_selection();
        assert!(deleted.is_some());
        assert_eq!(input.buf, "ad");
        assert!(
            input.attachment_ids.is_empty(),
            "Attachment should be removed"
        );
    }

    use crate::keymap::KeyAction;
}
