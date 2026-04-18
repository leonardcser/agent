mod buffer;
mod completer_bridge;
mod history;
mod kill_ring;
mod menu_bridge;
mod settings;
mod vim_bridge;

pub use history::History;
pub use kill_ring::KillRing;
pub use settings::{Menu, MenuAction, MenuKind, MenuResult, MenuState, SettingsState};

use crate::attachment::{Attachment, AttachmentId, AttachmentStore};
use crate::completer::{Completer, CompleterKind};
use crate::keymap::{self, KeyAction, KeyContext};
use crate::render;
use crate::vim::{ViMode, Vim};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use protocol::Content;
use vim_bridge::VimBridgeResult;

pub const ATTACHMENT_MARKER: char = '\u{FFFC}';

/// Back-reference emitted in place of repeated paste expansions so the model
/// sees the placement without paying tokens for the duplicate body.
const PASTE_STUB: &str = "[see earlier pasted content]";

const PASTE_LINE_THRESHOLD: usize = 12;

/// Snapshot of the input buffer state (used for Ctrl+S stash).
/// Owns its attachment data so it survives store clears across sessions.
#[derive(Clone, Debug)]
pub struct InputSnapshot {
    pub buf: String,
    pub cpos: usize,
    pub attachments: Vec<Attachment>,
    from_paste: bool,
}

// ── Shared input state ───────────────────────────────────────────────────────

/// Unified input buffer with attachment markers and file completer.
/// Used by both the prompt loop and the agent-mode type-ahead.
pub struct InputState {
    /// The underlying editable buffer. Owns `buf`, `cpos`, `vim`,
    /// `kill_ring`, `history`, `attachment_ids`, and `selection_anchor`.
    /// Accessible directly (e.g. `input.buf`, `input.cpos`) via
    /// `Deref<Target = Buffer>` so existing call sites don't need to
    /// know the prompt uses the same `Buffer` type as the content pane.
    pub buffer: crate::buffer::Buffer,
    pub store: AttachmentStore,
    pub completer: Option<Completer>,
    pub menu: Option<MenuState>,
    /// Saved buffer before history search, restored on cancel.
    pub(super) history_saved_buf: Option<(String, usize)>,
    pub stash: Option<InputSnapshot>,
    /// Tracks whether the current buffer content originated from a paste.
    /// Cleared on any manual character input.
    pub(super) from_paste: bool,
    /// Chord state: true after Ctrl+X, waiting for second key.
    pending_ctrl_x: bool,
    /// Completable arguments for commands like `/model`, `/theme`, `/color`.
    /// Each entry is `("/cmd", vec!["arg1", "arg2", ...])`.
    pub command_arg_sources: Vec<(String, Vec<String>)>,
}

impl std::ops::Deref for InputState {
    type Target = crate::buffer::Buffer;
    fn deref(&self) -> &crate::buffer::Buffer {
        &self.buffer
    }
}

impl std::ops::DerefMut for InputState {
    fn deref_mut(&mut self) -> &mut crate::buffer::Buffer {
        &mut self.buffer
    }
}

/// What the caller should do after `handle_event`.
pub enum Action {
    Redraw,
    PurgeRedraw,
    Submit { content: Content, display: String },
    SubmitEmpty,
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
            buffer: crate::buffer::Buffer::writable(),
            store: AttachmentStore::new(),
            completer: None,
            menu: None,
            history_saved_buf: None,
            stash: None,
            from_paste: false,
            pending_ctrl_x: false,
            command_arg_sources: Vec::new(),
        }
    }

    /// Returns the current selection range as (start_byte, end_byte), ordered.
    /// Works for both vim visual modes and shift+key selection.
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        // Vim visual mode takes priority.
        if let Some(ref vim) = self.buffer.vim {
            if let Some(range) = vim.visual_range(&self.buffer.buf, self.buffer.cpos) {
                return Some(range);
            }
        }
        // Non-vim shift+key selection.
        let anchor = self.buffer.selection_anchor?;
        let (a, b) = if anchor <= self.buffer.cpos {
            (anchor, self.buffer.cpos)
        } else {
            (self.buffer.cpos, anchor)
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
    pub fn clear_selection(&mut self) {
        self.buffer.selection_anchor = None;
    }

    /// Start or extend selection at current cursor position (non-vim shift+key).
    fn extend_selection(&mut self) {
        if self.buffer.selection_anchor.is_none() {
            self.buffer.selection_anchor = Some(self.buffer.cpos);
        }
    }

    /// Delete the currently selected text, returning it. Handles attachment cleanup.
    fn delete_selection(&mut self) -> Option<String> {
        let (start, end) = self.selection_range()?;
        let deleted = self.buffer.buf[start..end].to_string();
        self.remove_attachments_in_range(start, end);
        self.buffer.buf.drain(start..end);
        self.buffer.cpos = start;
        self.buffer.selection_anchor = None;
        Some(deleted)
    }

    pub fn vim_enabled(&self) -> bool {
        self.buffer.vim.is_some()
    }

    pub fn vim_mode(&self) -> Option<ViMode> {
        self.buffer.vim.as_ref().map(|v| v.mode())
    }

    /// Returns true if vim is enabled and currently in insert mode.
    pub fn vim_in_insert_mode(&self) -> bool {
        self.buffer.vim
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
            if self.buffer.vim.is_none() {
                self.buffer.vim = Some(Vim::new());
            }
        } else {
            self.buffer.vim = None;
        }
    }

    /// Restore vim to a specific mode (used after double-Esc cancel).
    pub fn set_vim_mode(&mut self, mode: ViMode) {
        if let Some(ref mut vim) = self.buffer.vim {
            vim.set_mode(mode);
        }
    }

    pub fn take_buffer(&mut self) -> (String, usize) {
        let buf = std::mem::take(&mut self.buffer.buf);
        let cpos = std::mem::replace(&mut self.buffer.cpos, 0);
        (buf, cpos)
    }

    pub fn set_buffer(&mut self, buf: String, cpos: usize) {
        self.buffer.buf = buf;
        self.buffer.cpos = cpos.min(self.buffer.buf.len());
    }

    /// Take the kill ring contents (moves ownership, leaves empty).
    pub fn take_kill_ring(&mut self) -> String {
        self.buffer.kill_ring.take()
    }

    /// Set the kill ring contents (used to sync back from dialogs).
    pub fn set_kill_ring(&mut self, contents: String) {
        self.buffer.kill_ring.set(contents);
    }

    pub fn clear(&mut self) {
        self.buffer.buf.clear();
        self.buffer.cpos = 0;
        self.buffer.attachment_ids.clear();
        self.completer = None;
        self.menu = None;
        self.history_saved_buf = None;
        self.from_paste = false;
        self.buffer.selection_anchor = None;
        // Note: stash and store are intentionally NOT cleared here.
    }

    /// Toggle stash: if no stash, save current buf and clear; if stashed, restore.
    /// Attachments are cloned out of the store so the stash survives store clears.
    pub fn toggle_stash(&mut self) {
        if let Some(snap) = self.stash.take() {
            self.buffer.buf = snap.buf;
            self.buffer.cpos = snap.cpos;
            self.buffer.attachment_ids = snap
                .attachments
                .into_iter()
                .map(|a| self.store.insert(a))
                .collect();
            self.from_paste = snap.from_paste;
            self.completer = None;
        } else if !self.buffer.buf.is_empty() || !self.buffer.attachment_ids.is_empty() {
            let attachments = std::mem::take(&mut self.buffer.attachment_ids)
                .into_iter()
                .filter_map(|id| self.store.get(id).cloned())
                .collect();
            self.stash = Some(InputSnapshot {
                buf: std::mem::take(&mut self.buffer.buf),
                cpos: std::mem::replace(&mut self.buffer.cpos, 0),
                attachments,
                from_paste: self.from_paste,
            });
            self.completer = None;
            self.history_saved_buf = None;
        }
    }

    /// Restore stash into the buffer (called after submit/command completes).
    pub fn restore_stash(&mut self) {
        if let Some(snap) = self.stash.take() {
            self.buffer.buf = snap.buf;
            self.buffer.cpos = snap.cpos;
            self.buffer.attachment_ids = snap
                .attachments
                .into_iter()
                .map(|a| self.store.insert(a))
                .collect();
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
        self.buffer.buf = text;
        self.buffer.cpos = self.buffer.buf.len();
        self.buffer.attachment_ids = ids;
    }

    pub fn has_modal(&self) -> bool {
        self.menu.is_some()
            || self.completer.as_ref().is_some_and(|c| {
                matches!(
                    c.kind,
                    CompleterKind::Model
                        | CompleterKind::Theme
                        | CompleterKind::Color
                        | CompleterKind::Settings
                        | CompleterKind::History
                )
            })
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

    pub fn cursor_char(&self) -> usize {
        char_pos(&self.buffer.buf, self.buffer.cpos)
    }

    /// Expand attachment markers and return the final text for submission.
    /// Pastes are inlined; image markers are stripped (images go via Content::Parts).
    ///
    /// When the same attachment id appears more than once in the buffer we
    /// expand it only on the first occurrence. Subsequent occurrences emit a
    /// short back-reference so the model still sees the placement without
    /// spending tokens on duplicate content. The back-reference is omitted
    /// for images entirely — their content is carried via `Content::Parts`,
    /// not inline text, so repeating the (empty) image expansion is a no-op.
    pub fn expanded_text(&self) -> String {
        let mut result = String::new();
        let mut att_idx = 0;
        let mut seen: std::collections::HashSet<AttachmentId> = std::collections::HashSet::new();
        for c in self.buffer.buf.chars() {
            if c == ATTACHMENT_MARKER {
                if let Some(&id) = self.buffer.attachment_ids.get(att_idx) {
                    if seen.insert(id) {
                        result.push_str(self.store.expanded_text(id));
                    } else if matches!(self.store.get(id), Some(Attachment::Paste { .. })) {
                        result.push_str(PASTE_STUB);
                    }
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
        for c in self.buffer.buf.chars() {
            if c == ATTACHMENT_MARKER {
                if let Some(&id) = self.buffer.attachment_ids.get(att_idx) {
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
        self.buffer.attachment_ids
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
    ///
    /// Images referenced multiple times in the buffer are emitted only once
    /// in `Content::Parts` — the payload is a base64 data URL (large), and
    /// the model gets nothing extra from seeing it twice.
    pub fn build_content(&self) -> Content {
        let text = self.expanded_text();
        let mut seen: std::collections::HashSet<AttachmentId> = std::collections::HashSet::new();
        let images: Vec<(String, String)> = self
            .attachment_ids
            .iter()
            .filter(|&&id| seen.insert(id))
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
            buf_empty: self.buffer.buf.is_empty() && self.buffer.attachment_ids.is_empty(),
            vim_non_insert: self.buffer.vim.as_ref().is_some_and(|v| {
                matches!(
                    v.mode(),
                    ViMode::Normal | ViMode::Visual | ViMode::VisualLine
                )
            }),
            vim_enabled: self.buffer.vim.is_some(),
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
            self.buffer.kill_ring.clear_yank();
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
                if self.buffer.buf.is_empty() && self.buffer.attachment_ids.is_empty() {
                    Action::SubmitEmpty
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
                self.buffer.buf.insert(self.buffer.cpos, '\n');
                self.buffer.cpos += 1;
                self.completer = None;
                Action::Redraw
            }

            // ── Navigation ──────────────────────────────────────────────
            KeyAction::MoveLeft => {
                if self.buffer.cpos > 0 {
                    let cp = char_pos(&self.buffer.buf, self.buffer.cpos);
                    self.buffer.cpos = byte_of_char(&self.buffer.buf, cp - 1);
                    self.recompute_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveRight => {
                if self.buffer.cpos < self.buffer.buf.len() {
                    let cp = char_pos(&self.buffer.buf, self.buffer.cpos);
                    self.buffer.cpos = byte_of_char(&self.buffer.buf, cp + 1);
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
                let new_pos = crate::vim::move_up(&self.buffer.buf, self.buffer.cpos);
                if new_pos != self.buffer.cpos {
                    self.buffer.cpos = new_pos;
                    self.recompute_completer();
                    Action::Redraw
                } else if let Some(entry) = history.and_then(|h| h.up(&self.buffer.buf)) {
                    self.buffer.buf = entry.to_string();
                    self.buffer.cpos = 0;
                    self.sync_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveDown => {
                let new_pos = crate::vim::move_down(&self.buffer.buf, self.buffer.cpos);
                if new_pos != self.buffer.cpos {
                    self.buffer.cpos = new_pos;
                    self.recompute_completer();
                    Action::Redraw
                } else if let Some(entry) = history.and_then(|h| h.down()) {
                    self.buffer.buf = entry.to_string();
                    self.buffer.cpos = self.buffer.buf.len();
                    self.sync_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveStartOfLine => {
                self.buffer.cpos = crate::text_utils::line_start(&self.buffer.buf, self.buffer.cpos);
                self.recompute_completer();
                Action::Redraw
            }
            KeyAction::MoveEndOfLine => {
                self.buffer.cpos = crate::text_utils::line_end(&self.buffer.buf, self.buffer.cpos);
                self.recompute_completer();
                Action::Redraw
            }
            KeyAction::MoveStartOfBuffer => {
                self.buffer.cpos = 0;
                self.recompute_completer();
                Action::Redraw
            }
            KeyAction::MoveEndOfBuffer => {
                self.buffer.cpos = self.buffer.buf.len();
                self.recompute_completer();
                Action::Redraw
            }
            KeyAction::HistoryPrev => {
                if let Some(entry) = history.and_then(|h| h.up(&self.buffer.buf)) {
                    self.buffer.buf = entry.to_string();
                    self.buffer.cpos = 0;
                    self.sync_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::HistoryNext => {
                if let Some(entry) = history.and_then(|h| h.down()) {
                    self.buffer.buf = entry.to_string();
                    self.buffer.cpos = self.buffer.buf.len();
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
                if let Some(new_cpos) = self.buffer.kill_ring.yank(&mut self.buffer.buf, self.buffer.cpos) {
                    self.buffer.cpos = new_cpos;
                    self.recompute_completer();
                }
                Action::Redraw
            }
            KeyAction::YankPop => {
                if let Some(new_cpos) = self.buffer.kill_ring.yank_pop(&mut self.buffer.buf) {
                    self.buffer.cpos = new_cpos;
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
                let line = current_line(&self.buffer.buf, self.buffer.cpos);
                let target = line.saturating_sub(half);
                self.move_to_line(target);
                Action::Redraw
            }
            KeyAction::VimHalfPageDown => {
                let half = render::term_height() / 2;
                let line = current_line(&self.buffer.buf, self.buffer.cpos);
                let total = self.buffer.buf.chars().filter(|&c| c == '\n').count() + 1;
                let target = (line + half).min(total - 1);
                self.move_to_line(target);
                Action::Redraw
            }

            // ── Clipboard ───────────────────────────────────────────────
            KeyAction::CopySelection => {
                if let Some((start, end)) = self.selection_range() {
                    let text = self.buffer.buf[start..end].to_string();
                    let _ = crate::app::copy_to_clipboard(&text);
                    self.buffer.kill_ring.set(text);
                }
                Action::Noop
            }
            KeyAction::CutSelection => {
                if self.selection_range().is_some() {
                    self.save_undo();
                    if let Some(text) = self.delete_selection() {
                        let _ = crate::app::copy_to_clipboard(&text);
                        self.buffer.kill_ring.set(text);
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
                if self.buffer.cpos > 0 {
                    let cp = char_pos(&self.buffer.buf, self.buffer.cpos);
                    self.buffer.cpos = byte_of_char(&self.buffer.buf, cp - 1);
                }
                Action::Redraw
            }
            KeyAction::SelectRight => {
                self.extend_selection();
                if self.buffer.cpos < self.buffer.buf.len() {
                    let cp = char_pos(&self.buffer.buf, self.buffer.cpos);
                    self.buffer.cpos = byte_of_char(&self.buffer.buf, cp + 1);
                }
                Action::Redraw
            }
            KeyAction::SelectWordForward => {
                self.extend_selection();
                self.buffer.cpos = crate::text_utils::word_forward_pos(
                    &self.buffer.buf,
                    self.buffer.cpos,
                    crate::text_utils::CharClass::Word,
                );
                Action::Redraw
            }
            KeyAction::SelectWordBackward => {
                self.extend_selection();
                self.buffer.cpos = crate::text_utils::word_backward_pos(
                    &self.buffer.buf,
                    self.buffer.cpos,
                    crate::text_utils::CharClass::Word,
                );
                Action::Redraw
            }
            KeyAction::SelectStartOfLine => {
                self.extend_selection();
                self.buffer.cpos = crate::text_utils::line_start(&self.buffer.buf, self.buffer.cpos);
                Action::Redraw
            }
            KeyAction::SelectEndOfLine => {
                self.extend_selection();
                self.buffer.cpos = crate::text_utils::line_end(&self.buffer.buf, self.buffer.cpos);
                Action::Redraw
            }
        }
    }

    /// Process a terminal event. Returns what the caller should do next.
    ///
    /// Priority ladder: menu → completer → vim → paste → resize → keymap → insert.
    pub fn handle_event(&mut self, ev: Event, mut history: Option<&mut History>) -> Action {
        // 1. Menu intercepts all keys when open.
        if self.menu.is_some() {
            return self.handle_menu_event(&ev);
        }

        // 2. Completer intercepts navigation keys when active.
        if self.completer.is_some() {
            if let Some(action) = self.handle_completer_event(&ev) {
                return action;
            }
        }

        // 3. Vim mode intercepts key events.
        match self.dispatch_vim(&ev, &mut history) {
            VimBridgeResult::Handled(action) => return action,
            VimBridgeResult::Passthrough => {
                // Fall through to keymap / char insert below.
            }
            VimBridgeResult::NotAKey => {
                // Not a key event or vim disabled — handle paste/resize/key below.
            }
        }

        // 4. Paste events.
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

        // 5. Resize events.
        if let Event::Resize(w, h) = ev {
            return Action::Resize {
                width: w as usize,
                height: h as usize,
            };
        }

        // 6. Key events — look up in the keymap.
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
                buf_empty: self.buffer.buf.is_empty() && self.buffer.attachment_ids.is_empty(),
                vim_non_insert: self.buffer.vim.as_ref().is_some_and(|v| {
                    matches!(
                        v.mode(),
                        ViMode::Normal | ViMode::Visual | ViMode::VisualLine
                    )
                }),
                vim_enabled: self.buffer.vim.is_some(),
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
pub(super) fn cursor_in_at_zone(buf: &str, cpos: usize) -> Option<usize> {
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

pub(super) fn find_slash_anchor(buf: &str, cpos: usize) -> Option<usize> {
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

    // ── Attachment dedup within a single message ───────────────────────

    /// Place two markers in the buffer that both point at `id`.
    fn buf_with_two_markers(input: &mut InputState, id: AttachmentId) {
        input.buf = format!("pre{m}mid{m}post", m = ATTACHMENT_MARKER);
        input.cpos = input.buf.len();
        input.attachment_ids = vec![id, id];
    }

    #[test]
    fn expanded_text_inlines_paste_once_for_duplicate_ids() {
        let mut input = InputState::new();
        let body = "secret pasted block".to_string();
        let id = input.store.insert_paste(body.clone());
        buf_with_two_markers(&mut input, id);

        let text = input.expanded_text();
        assert_eq!(
            text.matches(&body).count(),
            1,
            "paste body should appear exactly once"
        );
        assert!(text.contains(PASTE_STUB));
    }

    #[test]
    fn expanded_text_distinct_pastes_both_expand() {
        let mut input = InputState::new();
        let id1 = input.store.insert_paste("alpha body long enough".into());
        let id2 = input.store.insert_paste("beta body different".into());
        input.buf = format!("{m}and{m}", m = ATTACHMENT_MARKER);
        input.cpos = input.buf.len();
        input.attachment_ids = vec![id1, id2];
        let text = input.expanded_text();
        assert!(text.contains("alpha body long enough"));
        assert!(text.contains("beta body different"));
        assert!(!text.contains("[see earlier"));
    }

    #[test]
    fn expanded_text_three_identical_ids_emits_one_body_two_stubs() {
        let mut input = InputState::new();
        let body = "repeated content".to_string();
        let id = input.store.insert_paste(body.clone());
        input.buf = format!("{m}a{m}b{m}", m = ATTACHMENT_MARKER);
        input.cpos = input.buf.len();
        input.attachment_ids = vec![id, id, id];
        let text = input.expanded_text();
        assert_eq!(text.matches(&body).count(), 1);
        assert_eq!(text.matches(PASTE_STUB).count(), 2);
    }

    #[test]
    fn build_content_dedups_repeated_image_in_parts() {
        let mut input = InputState::new();
        let id = input
            .store
            .insert_image("img.png".into(), "data:image/png;base64,AAA".into());
        buf_with_two_markers(&mut input, id);
        let content = input.build_content();
        assert_eq!(
            content.image_count(),
            1,
            "repeated image should appear once in Content::Parts"
        );
    }

    #[test]
    fn build_content_preserves_distinct_images() {
        let mut input = InputState::new();
        let id1 = input
            .store
            .insert_image("a.png".into(), "data:image/png;base64,AAA".into());
        let id2 = input
            .store
            .insert_image("b.png".into(), "data:image/png;base64,BBB".into());
        input.buf = format!("{m}{m}", m = ATTACHMENT_MARKER);
        input.cpos = input.buf.len();
        input.attachment_ids = vec![id1, id2];
        let content = input.build_content();
        assert_eq!(content.image_count(), 2);
    }

    #[test]
    fn build_content_dedups_interleaved_image_references() {
        // Pattern: img A, img B, img A again. Parts should be [A, B].
        let mut input = InputState::new();
        let id_a = input
            .store
            .insert_image("a.png".into(), "data:image/png;base64,AAA".into());
        let id_b = input
            .store
            .insert_image("b.png".into(), "data:image/png;base64,BBB".into());
        input.buf = format!("{m}x{m}y{m}", m = ATTACHMENT_MARKER);
        input.cpos = input.buf.len();
        input.attachment_ids = vec![id_a, id_b, id_a];
        let content = input.build_content();
        assert_eq!(content.image_count(), 2);
    }

    use crate::keymap::KeyAction;
}
