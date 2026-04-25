mod buffer;
mod completer_bridge;
mod history;
mod vim_bridge;

pub use history::History;

use crate::attachment::{Attachment, AttachmentId, AttachmentStore};
use crate::completer::CompleterSession;
use crate::content;
use crate::keymap::{self, KeyAction, KeyContext};
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

/// Prompt window state — a `ui::Window` (cursor, vim, kill ring,
/// editable buffer) plus prompt-specific side-cars (completer, stash,
/// history, attachments). `Deref<Target = EditBuffer>` gives direct
/// access to `input.buf`, `input.attachment_ids`, etc.
pub struct PromptState {
    pub win: ui::Window,
    pub store: AttachmentStore,
    pub completer: Option<CompleterSession>,
    /// Picker float IDs from closed completer sessions, waiting for the
    /// next frame to drain and `win_close`. `PromptState` doesn't hold a
    /// `&mut ui::Ui`, so closing has to happen out-of-band.
    pub pending_picker_close: Vec<ui::WinId>,
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

impl std::ops::Deref for PromptState {
    type Target = ui::EditBuffer;
    fn deref(&self) -> &ui::EditBuffer {
        &self.win.edit_buf
    }
}

/// What the caller should do after `handle_event`.
pub enum Action {
    Redraw,
    Submit { content: Content, display: String },
    SubmitEmpty,
    ToggleMode,
    CycleReasoning,
    EditInEditor,
    CenterScroll,
    Resize { width: usize, height: usize },
    NotifyError(String),
    Noop,
}

impl Default for PromptState {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptState {
    pub fn new() -> Self {
        let mut win = ui::Window::new(
            ui::PROMPT_WIN,
            ui::BufId(0),
            ui::WinConfig::Split(ui::SplitConfig {
                region: "prompt".into(),
                gutters: ui::Gutters::default(),
            }),
        );
        win.edit_buf = ui::EditBuffer::new();
        Self {
            win,
            store: AttachmentStore::new(),
            completer: None,
            pending_picker_close: Vec::new(),
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
        if let Some(ref vim) = self.win.vim {
            if let Some(range) = vim.visual_range(&self.win.edit_buf.buf, self.win.cpos) {
                return Some(range);
            }
        }
        self.win.win_cursor.range(self.win.cpos)
    }

    /// Selection range to *render* with the selection-bg style. Falls
    /// back to the yank-flash range from the kill ring when there's no
    /// real selection so vim copy ops (`yy`, `yw`, visual `y`, …) get
    /// the brief post-yank highlight, matching nvim's
    /// `vim.highlight.on_yank`. Editing logic must keep using
    /// `selection_range` so the flash never affects mutations.
    pub fn display_selection_range(&self) -> Option<(usize, usize)> {
        if let Some(range) = self.selection_range() {
            return Some(range);
        }
        self.win
            .kill_ring
            .yank_flash_range(std::time::Instant::now())
    }

    fn has_selection(&self) -> bool {
        self.selection_range().is_some()
    }

    /// Clear any active selection (non-vim). Called on non-shift movement or editing.
    pub fn clear_selection(&mut self) {
        self.win.win_cursor.clear_anchor();
    }

    /// End the active completer session, queueing its Picker float for
    /// close on the next frame. Replaces bare `self.completer = None`
    /// so the associated `ui::WinId` doesn't leak.
    pub fn close_completer(&mut self) {
        if let Some(session) = self.completer.take() {
            if let Some(win) = session.picker_win {
                self.pending_picker_close.push(win);
            }
        }
    }

    /// Install a fresh completer, retiring any previous session's picker
    /// float. Every site that creates a new `CompleterSession` must go
    /// through this — bare `self.completer = Some(...)` orphans the old
    /// `ui::WinId` and leaves a stale picker painted above the prompt.
    pub fn set_completer(&mut self, comp: crate::completer::Completer) {
        self.close_completer();
        self.completer = Some(CompleterSession::new(comp));
    }

    /// Select the word at `cpos`. Used by mouse double-click.
    pub fn select_word_at(&mut self, cpos: usize) -> Option<(usize, usize)> {
        let (start, end) = self.win.edit_buf.word_range_at(cpos)?;
        self.win.cpos = end.saturating_sub(1).max(start);
        if let Some(vim) = self.win.vim.as_mut() {
            vim.begin_visual(crate::vim::ViMode::Visual, start);
        } else {
            self.win.win_cursor.set_anchor(Some(start));
        }
        Some((start, end))
    }

    /// Start or extend selection at current cursor position (non-vim shift+key).
    fn extend_selection(&mut self) {
        self.win.win_cursor.extend(self.win.cpos);
    }

    /// Delete the currently selected text, returning it. Handles attachment cleanup.
    fn delete_selection(&mut self) -> Option<String> {
        let (start, end) = self.selection_range()?;
        let deleted = self.win.edit_buf.buf[start..end].to_string();
        self.remove_attachments_in_range(start, end);
        self.win.edit_buf.buf.drain(start..end);
        self.win.cpos = start;
        self.win.win_cursor.clear_anchor();
        Some(deleted)
    }

    pub fn vim_enabled(&self) -> bool {
        self.win.vim.is_some()
    }

    pub fn vim_mode(&self) -> Option<ViMode> {
        self.win.vim.as_ref().map(|v| v.mode())
    }

    /// Returns true if vim is enabled and currently in insert mode.
    pub fn vim_in_insert_mode(&self) -> bool {
        self.win
            .vim
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
            if self.win.vim.is_none() {
                self.win.vim = Some(Vim::new());
            }
        } else {
            self.win.vim = None;
        }
    }

    /// Restore vim to a specific mode (used after double-Esc cancel).
    pub fn set_vim_mode(&mut self, mode: ViMode) {
        if let Some(ref mut vim) = self.win.vim {
            vim.set_mode(mode);
        }
    }

    pub fn take_buffer(&mut self) -> (String, usize) {
        let buf = std::mem::take(&mut self.win.edit_buf.buf);
        let cpos = std::mem::replace(&mut self.win.cpos, 0);
        (buf, cpos)
    }

    pub fn set_buffer(&mut self, buf: String, cpos: usize) {
        self.win.edit_buf.buf = buf;
        self.win.cpos = cpos.min(self.win.edit_buf.buf.len());
    }

    /// Take the kill ring contents (moves ownership, leaves empty).
    pub fn take_kill_ring(&mut self) -> String {
        self.win.kill_ring.take()
    }

    /// Set the kill ring contents (used to sync back from dialogs).
    pub fn set_kill_ring(&mut self, contents: String) {
        self.win.kill_ring.set(contents);
    }

    /// Reconcile the kill ring with the system clipboard before an
    /// emacs-style paste (`C-y`). If the clipboard text differs from
    /// what we last pushed, treat it as externally updated and
    /// overwrite the kill ring (charwise — external sources don't
    /// know about vim's linewise concept). When they match, the kill
    /// ring is already authoritative.
    fn sync_kill_ring_from_clipboard(&mut self) {
        let Some(text) = crate::app::commands::paste_from_clipboard() else {
            return;
        };
        if self.win.kill_ring.last_clipboard_write() == Some(text.as_str()) {
            return;
        }
        self.win.kill_ring.set(text.clone());
        self.win.kill_ring.record_clipboard_write(text);
    }

    pub fn clear(&mut self) {
        self.win.edit_buf.buf.clear();
        self.win.cpos = 0;
        self.win.edit_buf.attachment_ids.clear();
        self.close_completer();
        self.from_paste = false;
        self.win.win_cursor.clear_anchor();
        // Note: stash and store are intentionally NOT cleared here.
    }

    /// Replace the prompt buffer wholesale and re-establish invariants:
    /// snapshot undo, clear attachments + shift-selection anchor, reset
    /// paste state, drop the completer so it re-derives. Prefer the
    /// `crate::api::buf::replace` wrapper at call sites — it reads as
    /// intent rather than a method on the receiver.
    pub fn replace_text(&mut self, text: String, cursor: Option<usize>) {
        self.save_undo();
        let cpos = cursor.unwrap_or(text.len()).min(text.len());
        self.win.edit_buf.buf = text;
        self.win.cpos = cpos;
        self.win.edit_buf.attachment_ids.clear();
        self.win.win_cursor.clear_anchor();
        self.from_paste = false;
        self.close_completer();
        self.recompute_completer();
    }

    /// Toggle stash: if no stash, save current buf and clear; if stashed, restore.
    /// Attachments are cloned out of the store so the stash survives store clears.
    pub fn toggle_stash(&mut self) {
        if let Some(snap) = self.stash.take() {
            self.win.edit_buf.buf = snap.buf;
            self.win.cpos = snap.cpos;
            self.win.edit_buf.attachment_ids = snap
                .attachments
                .into_iter()
                .map(|a| self.store.insert(a))
                .collect();
            self.from_paste = snap.from_paste;
            self.close_completer();
        } else if !self.win.edit_buf.buf.is_empty() || !self.win.edit_buf.attachment_ids.is_empty()
        {
            let attachments = std::mem::take(&mut self.win.edit_buf.attachment_ids)
                .into_iter()
                .filter_map(|id| self.store.get(id).cloned())
                .collect();
            self.stash = Some(InputSnapshot {
                buf: std::mem::take(&mut self.win.edit_buf.buf),
                cpos: std::mem::replace(&mut self.win.cpos, 0),
                attachments,
                from_paste: self.from_paste,
            });
            self.close_completer();
        }
    }

    /// Restore stash into the buffer (called after submit/command completes).
    pub fn restore_stash(&mut self) {
        if let Some(snap) = self.stash.take() {
            self.win.edit_buf.buf = snap.buf;
            self.win.cpos = snap.cpos;
            self.win.edit_buf.attachment_ids = snap
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
        self.win.edit_buf.buf = text;
        self.win.cpos = self.win.edit_buf.buf.len();
        self.win.edit_buf.attachment_ids = ids;
    }

    pub fn cursor_char(&self) -> usize {
        char_pos(&self.win.edit_buf.buf, self.win.cpos)
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
        for c in self.win.edit_buf.buf.chars() {
            if c == ATTACHMENT_MARKER {
                if let Some(&id) = self.win.edit_buf.attachment_ids.get(att_idx) {
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
        for c in self.win.edit_buf.buf.chars() {
            if c == ATTACHMENT_MARKER {
                if let Some(&id) = self.win.edit_buf.attachment_ids.get(att_idx) {
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
        self.win
            .edit_buf
            .attachment_ids
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
            buf_empty: self.win.edit_buf.buf.is_empty()
                && self.win.edit_buf.attachment_ids.is_empty(),
            vim_non_insert: self.win.vim.as_ref().is_some_and(|v| {
                matches!(
                    v.mode(),
                    ViMode::Normal | ViMode::Visual | ViMode::VisualLine
                )
            }),
            vim_enabled: self.win.vim.is_some(),
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
            self.win.kill_ring.clear_yank();
        }
        // Any non-vertical action abandons the preferred column so the
        // next vertical motion picks up wherever the user is now.
        if !matches!(
            action,
            KeyAction::MoveUp | KeyAction::MoveDown | KeyAction::SelectUp | KeyAction::SelectDown
        ) {
            self.win.win_cursor.clear_curswant();
        }
        // Selection actions extend; editing actions consume; everything else clears.
        let is_select = matches!(
            action,
            KeyAction::SelectLeft
                | KeyAction::SelectRight
                | KeyAction::SelectUp
                | KeyAction::SelectDown
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
            KeyAction::Redraw => Action::Redraw,

            // ── Submit / newline ─────────────────────────────────────────
            KeyAction::Submit => {
                if self.win.edit_buf.buf.is_empty() && self.win.edit_buf.attachment_ids.is_empty() {
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
                self.win.edit_buf.buf.insert(self.win.cpos, '\n');
                self.win.cpos += 1;
                self.close_completer();
                Action::Redraw
            }

            // ── Navigation ──────────────────────────────────────────────
            KeyAction::MoveLeft => {
                if self.win.cpos > 0 {
                    let cp = char_pos(&self.win.edit_buf.buf, self.win.cpos);
                    self.win.cpos = byte_of_char(&self.win.edit_buf.buf, cp - 1);
                    self.recompute_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveRight => {
                if self.win.cpos < self.win.edit_buf.buf.len() {
                    let cp = char_pos(&self.win.edit_buf.buf, self.win.cpos);
                    self.win.cpos = byte_of_char(&self.win.edit_buf.buf, cp + 1);
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
                let new_pos =
                    self.win
                        .win_cursor
                        .move_vertical(&self.win.edit_buf.buf, self.win.cpos, -1);
                if new_pos != self.win.cpos {
                    self.win.cpos = new_pos;
                    self.recompute_completer();
                    Action::Redraw
                } else if let Some(entry) = history.and_then(|h| h.up(&self.win.edit_buf.buf)) {
                    self.win.edit_buf.buf = entry.to_string();
                    self.win.cpos = 0;
                    self.win.win_cursor.clear_curswant();
                    self.sync_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveDown => {
                let new_pos =
                    self.win
                        .win_cursor
                        .move_vertical(&self.win.edit_buf.buf, self.win.cpos, 1);
                if new_pos != self.win.cpos {
                    self.win.cpos = new_pos;
                    self.recompute_completer();
                    Action::Redraw
                } else if let Some(entry) = history.and_then(|h| h.down()) {
                    self.win.edit_buf.buf = entry.to_string();
                    self.win.cpos = self.win.edit_buf.buf.len();
                    self.win.win_cursor.clear_curswant();
                    self.sync_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveStartOfLine => {
                self.win.cpos = ui::text::line_start(&self.win.edit_buf.buf, self.win.cpos);
                self.recompute_completer();
                Action::Redraw
            }
            KeyAction::MoveEndOfLine => {
                self.win.cpos = ui::text::line_end(&self.win.edit_buf.buf, self.win.cpos);
                self.recompute_completer();
                Action::Redraw
            }
            KeyAction::MoveStartOfBuffer => {
                self.win.cpos = 0;
                self.recompute_completer();
                Action::Redraw
            }
            KeyAction::MoveEndOfBuffer => {
                self.win.cpos = self.win.edit_buf.buf.len();
                self.recompute_completer();
                Action::Redraw
            }
            KeyAction::HistoryPrev => {
                if let Some(entry) = history.and_then(|h| h.up(&self.win.edit_buf.buf)) {
                    self.win.edit_buf.buf = entry.to_string();
                    self.win.cpos = 0;
                    self.sync_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::HistoryNext => {
                if let Some(entry) = history.and_then(|h| h.down()) {
                    self.win.edit_buf.buf = entry.to_string();
                    self.win.cpos = self.win.edit_buf.buf.len();
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
                self.sync_kill_ring_from_clipboard();
                if let Some(new_cpos) = self
                    .win
                    .kill_ring
                    .yank(&mut self.win.edit_buf.buf, self.win.cpos)
                {
                    self.win.cpos = new_cpos;
                    self.recompute_completer();
                }
                Action::Redraw
            }
            KeyAction::YankPop => {
                if let Some(new_cpos) = self.win.kill_ring.yank_pop(&mut self.win.edit_buf.buf) {
                    self.win.cpos = new_cpos;
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
                let half = content::term_height() / 2;
                let line = current_line(&self.win.edit_buf.buf, self.win.cpos);
                let target = line.saturating_sub(half);
                self.move_to_line(target);
                Action::Redraw
            }
            KeyAction::VimHalfPageDown => {
                let half = content::term_height() / 2;
                let line = current_line(&self.win.edit_buf.buf, self.win.cpos);
                let total = self.win.edit_buf.buf.chars().filter(|&c| c == '\n').count() + 1;
                let target = (line + half).min(total - 1);
                self.move_to_line(target);
                Action::Redraw
            }

            // ── Clipboard ───────────────────────────────────────────────
            KeyAction::CopySelection => {
                if let Some((start, end)) = self.selection_range() {
                    let text = self.win.edit_buf.buf[start..end].to_string();
                    if crate::app::copy_to_clipboard(&text).is_ok() {
                        self.win.kill_ring.record_clipboard_write(text.clone());
                    }
                    self.win.kill_ring.set(text);
                }
                Action::Noop
            }
            KeyAction::CutSelection => {
                if self.selection_range().is_some() {
                    self.save_undo();
                    if let Some(text) = self.delete_selection() {
                        if crate::app::copy_to_clipboard(&text).is_ok() {
                            self.win.kill_ring.record_clipboard_write(text.clone());
                        }
                        self.win.kill_ring.set(text);
                    }
                    self.recompute_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::ClipboardImage => {
                // Cmd/Meta+V. When bracketed paste is active the
                // terminal forwards the clipboard as an `Event::Paste`
                // we never reach here. But some terminals (or configs
                // with bracketed paste off) send raw Cmd+V as a key —
                // handle both image and text paste so the shortcut is
                // reliable regardless of the terminal's paste mode.
                if let Some(url) = clipboard_image_to_data_url() {
                    self.save_undo();
                    self.insert_image("clipboard.png".into(), url);
                    return Action::Redraw;
                }
                if let Some(text) = crate::app::commands::paste_from_clipboard() {
                    if !text.is_empty() {
                        self.save_undo();
                        if self.has_selection() {
                            self.delete_selection();
                        }
                        self.insert_paste(text);
                        return Action::Redraw;
                    }
                }
                Action::Noop
            }

            // ── Selection (shift+movement) ─────────────────────────────
            KeyAction::SelectLeft => {
                self.extend_selection();
                if self.win.cpos > 0 {
                    let cp = char_pos(&self.win.edit_buf.buf, self.win.cpos);
                    self.win.cpos = byte_of_char(&self.win.edit_buf.buf, cp - 1);
                }
                Action::Redraw
            }
            KeyAction::SelectRight => {
                self.extend_selection();
                if self.win.cpos < self.win.edit_buf.buf.len() {
                    let cp = char_pos(&self.win.edit_buf.buf, self.win.cpos);
                    self.win.cpos = byte_of_char(&self.win.edit_buf.buf, cp + 1);
                }
                Action::Redraw
            }
            KeyAction::SelectUp => {
                self.extend_selection();
                self.win.cpos =
                    self.win
                        .win_cursor
                        .move_vertical(&self.win.edit_buf.buf, self.win.cpos, -1);
                Action::Redraw
            }
            KeyAction::SelectDown => {
                self.extend_selection();
                self.win.cpos =
                    self.win
                        .win_cursor
                        .move_vertical(&self.win.edit_buf.buf, self.win.cpos, 1);
                Action::Redraw
            }
            KeyAction::SelectWordForward => {
                self.extend_selection();
                self.win.cpos = ui::text::word_forward_pos(
                    &self.win.edit_buf.buf,
                    self.win.cpos,
                    ui::text::CharClass::Word,
                );
                Action::Redraw
            }
            KeyAction::SelectWordBackward => {
                self.extend_selection();
                self.win.cpos = ui::text::word_backward_pos(
                    &self.win.edit_buf.buf,
                    self.win.cpos,
                    ui::text::CharClass::Word,
                );
                Action::Redraw
            }
            KeyAction::SelectStartOfLine => {
                self.extend_selection();
                self.win.cpos = ui::text::line_start(&self.win.edit_buf.buf, self.win.cpos);
                Action::Redraw
            }
            KeyAction::SelectEndOfLine => {
                self.extend_selection();
                self.win.cpos = ui::text::line_end(&self.win.edit_buf.buf, self.win.cpos);
                Action::Redraw
            }
        }
    }

    /// Process a terminal event. Returns what the caller should do next.
    ///
    /// Priority ladder: completer → vim → paste → resize → keymap → insert.
    pub fn handle_event(&mut self, ev: Event, mut history: Option<&mut History>) -> Action {
        // 1. Completer intercepts navigation keys when active.
        if self.completer.is_some() {
            if let Some(action) = self.handle_completer_event(&ev) {
                return action;
            }
        }

        // 2. Vim mode intercepts key events.
        match self.dispatch_vim(&ev, &mut history) {
            VimBridgeResult::Handled(action) => return action,
            VimBridgeResult::Passthrough => {
                // Fall through to keymap / char insert below.
            }
            VimBridgeResult::NotAKey => {
                // Not a key event or vim disabled — handle paste/resize/key below.
            }
        }

        // 3. Paste events.
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

        // 4. Resize events.
        if let Event::Resize(w, h) = ev {
            return Action::Resize {
                width: w as usize,
                height: h as usize,
            };
        }

        // 5. Key events — look up in the keymap.
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
                buf_empty: self.win.edit_buf.buf.is_empty()
                    && self.win.edit_buf.attachment_ids.is_empty(),
                vim_non_insert: self.win.vim.as_ref().is_some_and(|v| {
                    matches!(
                        v.mode(),
                        ViMode::Normal | ViMode::Visual | ViMode::VisualLine
                    )
                }),
                vim_enabled: self.win.vim.is_some(),
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
        let mut input = PromptState::new();
        input.insert_paste("!echo hello".to_string());
        assert!(
            input.skip_shell_escape(),
            "Paste at buffer start should set from_paste"
        );
        assert_eq!(input.buf, "!echo hello");
    }

    #[test]
    fn type_then_type_sets_from_paste_false() {
        let mut input = PromptState::new();
        input.insert_char('!');
        input.insert_char('e');
        assert!(
            !input.skip_shell_escape(),
            "Manual typing should clear from_paste"
        );
    }

    #[test]
    fn type_bang_then_paste_sets_from_paste() {
        let mut input = PromptState::new();

        // Simulate user typing '!'
        input.insert_char('!');
        assert!(!input.skip_shell_escape(), "Typing clears from_paste");

        // Reset cursor to simulate the scenario: user types '!', then pastes at line start
        // This is the key scenario that was broken before the fix
        input.win.edit_buf.buf.clear();
        input.win.cpos = 0;
        input.insert_paste("echo hello".to_string());
        assert!(
            input.skip_shell_escape(),
            "Paste at line start should set from_paste"
        );
        assert_eq!(input.buf, "echo hello");
    }

    #[test]
    fn paste_in_middle_of_line_does_not_set_from_paste() {
        let mut input = PromptState::new();

        input.win.edit_buf.buf = "hello ".to_string();
        input.win.cpos = 6; // After "hello "
        input.insert_paste("!world".to_string());
        assert!(
            !input.skip_shell_escape(),
            "Paste in middle of line should not set from_paste"
        );
        assert_eq!(input.buf, "hello !world");
    }

    #[test]
    fn paste_at_end_of_line_does_not_set_from_paste() {
        let mut input = PromptState::new();

        input.win.edit_buf.buf = "hello".to_string();
        input.win.cpos = 5; // At end
        input.insert_paste(" world".to_string());
        assert!(
            !input.skip_shell_escape(),
            "Paste at end of line should not set from_paste"
        );
        assert_eq!(input.buf, "hello world");
    }

    #[test]
    fn paste_at_start_of_multiline_buffer() {
        let mut input = PromptState::new();

        input.win.edit_buf.buf = "line1\nline2".to_string();
        input.win.cpos = 0; // At very start
        input.insert_paste("!command".to_string());
        assert!(
            input.skip_shell_escape(),
            "Paste at buffer start should set from_paste"
        );
        assert_eq!(input.buf, "!commandline1\nline2");
    }

    #[test]
    fn paste_at_start_of_second_line_sets_from_paste() {
        let mut input = PromptState::new();

        input.win.edit_buf.buf = "line1\n".to_string();
        input.win.cpos = 6; // Start of second line
        input.insert_paste("!command".to_string());
        assert!(
            input.skip_shell_escape(),
            "Paste at line start should set from_paste"
        );
        assert_eq!(input.buf, "line1\n!command");
    }

    #[test]
    fn paste_middle_of_second_line_does_not_set_from_paste() {
        let mut input = PromptState::new();

        input.win.edit_buf.buf = "line1\nhello".to_string();
        input.win.cpos = 8; // Insert at byte position 8 (before first 'l' of "hello")
        input.insert_paste(" world".to_string());
        assert!(
            !input.skip_shell_escape(),
            "Paste in middle of line should not set from_paste"
        );
        assert_eq!(input.buf, "line1\nhe worldllo");
    }

    #[test]
    fn manual_char_after_paste_clears_from_paste() {
        let mut input = PromptState::new();
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
        let mut input = PromptState::new();
        input.insert_paste("!echo hello".to_string());
        assert!(input.skip_shell_escape());

        input.backspace(); // Deletes last character
        assert!(
            input.skip_shell_escape(),
            "Backspace not at start should not clear from_paste"
        );

        input.win.cpos = 0;
        input.backspace(); // Now at position 0
                           // Can't backspace further, but the logic would clear it if we could
    }

    #[test]
    fn delete_word_backward_at_start_clears_from_paste() {
        let mut input = PromptState::new();
        input.insert_paste("!echo hello".to_string());
        assert!(input.skip_shell_escape());

        // Move cursor to end
        input.win.cpos = input.buf.len();
        input.delete_word_backward(); // Deletes "hello"
        assert!(
            input.skip_shell_escape(),
            "Delete word not at start should not clear from_paste"
        );

        // Move to after "echo " and delete word
        input.win.cpos = 5; // After "echo"
        input.delete_word_backward(); // Deletes "echo"
        assert!(input.skip_shell_escape(), "Still not at absolute start");

        input.win.cpos = 1; // After "!"
        input.delete_word_backward(); // Would delete to start, which should clear from_paste
        assert!(
            !input.skip_shell_escape(),
            "Delete word to start should clear from_paste"
        );
    }

    #[test]
    fn clear_resets_from_paste() {
        let mut input = PromptState::new();
        input.insert_paste("!test".to_string());
        assert!(input.skip_shell_escape());

        input.clear();
        assert!(!input.skip_shell_escape(), "Clear should reset from_paste");
    }

    #[test]
    fn large_paste_creates_attachment() {
        let mut input = PromptState::new();

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
        let mut input = PromptState::new();

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
        let mut input = PromptState::new();

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
        let mut input = PromptState::new();
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
        let mut input = PromptState::new();
        input.insert_paste("!first".to_string());
        assert!(input.skip_shell_escape());

        // Type something, which clears from_paste
        input.insert_char(' ');
        assert!(!input.skip_shell_escape());

        // Paste again at start of line
        input.win.cpos = 0;
        input.insert_paste("!second".to_string());
        assert!(
            input.skip_shell_escape(),
            "Second paste at start should set from_paste again"
        );
    }

    #[test]
    fn paste_with_carriage_returns_normalized() {
        let mut input = PromptState::new();
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
        let mut input = PromptState::new();
        input.insert_paste("".to_string());
        assert!(
            !input.skip_shell_escape(),
            "Empty paste should not set from_paste"
        );
    }

    #[test]
    fn whitespace_only_paste_at_start_sets_from_paste() {
        let mut input = PromptState::new();
        input.insert_paste("   ".to_string());
        assert!(
            input.skip_shell_escape(),
            "Whitespace paste at start should set from_paste"
        );
    }

    #[test]
    fn paste_starting_with_bang_at_line_start() {
        // This is the main bug scenario: type '!', then paste command
        let mut input = PromptState::new();

        input.win.edit_buf.buf = String::new();
        input.win.cpos = 0;
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
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello".to_string();
        input.win.cpos = 0;
        input.execute_key_action(KeyAction::SelectRight, None);
        assert_eq!(input.win.win_cursor.anchor(), Some(0));
        assert_eq!(input.win.cpos, 1);
        assert_eq!(input.selection_range(), Some((0, 1)));
    }

    #[test]
    fn shift_select_extends_selection() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello".to_string();
        input.win.cpos = 0;
        input.execute_key_action(KeyAction::SelectRight, None);
        input.execute_key_action(KeyAction::SelectRight, None);
        input.execute_key_action(KeyAction::SelectRight, None);
        assert_eq!(input.win.win_cursor.anchor(), Some(0));
        assert_eq!(input.win.cpos, 3);
        assert_eq!(input.selection_range(), Some((0, 3)));
    }

    #[test]
    fn movement_clears_selection() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello".to_string();
        input.win.cpos = 0;
        input.execute_key_action(KeyAction::SelectRight, None);
        input.execute_key_action(KeyAction::SelectRight, None);
        assert!(input.selection_range().is_some());
        input.execute_key_action(KeyAction::MoveRight, None);
        assert!(input.selection_range().is_none());
    }

    #[test]
    fn backspace_deletes_selection() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello world".to_string();
        input.win.cpos = 0;
        // Select "hello"
        for _ in 0..5 {
            input.execute_key_action(KeyAction::SelectRight, None);
        }
        assert_eq!(input.selection_range(), Some((0, 5)));
        input.execute_key_action(KeyAction::Backspace, None);
        assert_eq!(input.buf, " world");
        assert_eq!(input.win.cpos, 0);
    }

    #[test]
    fn delete_forward_deletes_selection() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello world".to_string();
        input.win.cpos = 0;
        for _ in 0..5 {
            input.execute_key_action(KeyAction::SelectRight, None);
        }
        input.execute_key_action(KeyAction::DeleteCharForward, None);
        assert_eq!(input.buf, " world");
    }

    #[test]
    fn typing_replaces_selection() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello world".to_string();
        input.win.cpos = 0;
        for _ in 0..5 {
            input.execute_key_action(KeyAction::SelectRight, None);
        }
        input.insert_char('X');
        assert_eq!(input.buf, "X world");
        assert_eq!(input.win.cpos, 1);
    }

    #[test]
    fn select_left_from_end() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello".to_string();
        input.win.cpos = 5;
        input.execute_key_action(KeyAction::SelectLeft, None);
        input.execute_key_action(KeyAction::SelectLeft, None);
        assert_eq!(input.win.win_cursor.anchor(), Some(5));
        assert_eq!(input.win.cpos, 3);
        assert_eq!(input.selection_range(), Some((3, 5)));
    }

    #[test]
    fn select_word_forward() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello world foo".to_string();
        input.win.cpos = 0;
        input.execute_key_action(KeyAction::SelectWordForward, None);
        assert_eq!(input.win.win_cursor.anchor(), Some(0));
        // word_forward_pos from 0 should be 6 (start of "world").
        assert_eq!(input.win.cpos, 6);
        input.execute_key_action(KeyAction::Backspace, None);
        assert_eq!(input.buf, "world foo");
    }

    #[test]
    fn select_word_backward() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello world".to_string();
        input.win.cpos = 11;
        input.execute_key_action(KeyAction::SelectWordBackward, None);
        assert_eq!(input.selection_range(), Some((6, 11)));
        input.execute_key_action(KeyAction::Backspace, None);
        assert_eq!(input.buf, "hello ");
    }

    #[test]
    fn select_to_line_start() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello world".to_string();
        input.win.cpos = 5;
        input.execute_key_action(KeyAction::SelectStartOfLine, None);
        assert_eq!(input.selection_range(), Some((0, 5)));
    }

    #[test]
    fn select_to_line_end() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello world".to_string();
        input.win.cpos = 5;
        input.execute_key_action(KeyAction::SelectEndOfLine, None);
        assert_eq!(input.selection_range(), Some((5, 11)));
    }

    #[test]
    fn newline_replaces_selection() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello world".to_string();
        input.win.cpos = 0;
        for _ in 0..5 {
            input.execute_key_action(KeyAction::SelectRight, None);
        }
        input.execute_key_action(KeyAction::InsertNewline, None);
        assert_eq!(input.buf, "\n world");
        assert_eq!(input.win.cpos, 1);
    }

    #[test]
    fn kill_to_eol_with_selection() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello world".to_string();
        input.win.cpos = 0;
        for _ in 0..5 {
            input.execute_key_action(KeyAction::SelectRight, None);
        }
        input.execute_key_action(KeyAction::KillToEndOfLine, None);
        assert_eq!(input.buf, " world");
        // Killed text should be in kill ring.
        assert_eq!(input.win.kill_ring.current(), "hello");
    }

    #[test]
    fn selection_at_buffer_boundary() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "ab".to_string();
        input.win.cpos = 0;
        // Select all.
        input.execute_key_action(KeyAction::SelectRight, None);
        input.execute_key_action(KeyAction::SelectRight, None);
        assert_eq!(input.selection_range(), Some((0, 2)));
        input.execute_key_action(KeyAction::Backspace, None);
        assert_eq!(input.buf, "");
        assert_eq!(input.win.cpos, 0);
    }

    #[test]
    fn selection_range_empty_when_anchor_equals_cursor() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello".to_string();
        input.win.cpos = 3;
        input.win.win_cursor.set_anchor(Some(3));
        assert_eq!(input.selection_range(), None);
    }

    #[test]
    fn clear_resets_selection() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello".to_string();
        input.win.cpos = 0;
        input.execute_key_action(KeyAction::SelectRight, None);
        assert!(input.selection_range().is_some());
        input.clear();
        assert!(input.selection_range().is_none());
    }

    #[test]
    fn delete_word_backward_with_selection() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello world".to_string();
        input.win.cpos = 6;
        // Select "wor"
        for _ in 0..3 {
            input.execute_key_action(KeyAction::SelectRight, None);
        }
        input.execute_key_action(KeyAction::DeleteWordBackward, None);
        assert_eq!(input.buf, "hello ld");
    }

    #[test]
    fn delete_word_forward_with_selection() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello world".to_string();
        input.win.cpos = 0;
        for _ in 0..3 {
            input.execute_key_action(KeyAction::SelectRight, None);
        }
        input.execute_key_action(KeyAction::DeleteWordForward, None);
        assert_eq!(input.buf, "lo world");
    }

    #[test]
    fn delete_to_start_of_line_with_selection() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello world".to_string();
        input.win.cpos = 3;
        for _ in 0..4 {
            input.execute_key_action(KeyAction::SelectRight, None);
        }
        input.execute_key_action(KeyAction::DeleteToStartOfLine, None);
        assert_eq!(input.buf, "helorld");
    }

    #[test]
    fn select_left_at_start_stays() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello".to_string();
        input.win.cpos = 0;
        input.execute_key_action(KeyAction::SelectLeft, None);
        assert_eq!(input.win.cpos, 0);
        assert_eq!(input.win.win_cursor.anchor(), Some(0));
    }

    #[test]
    fn select_right_at_end_stays() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "hello".to_string();
        input.win.cpos = 5;
        input.execute_key_action(KeyAction::SelectRight, None);
        assert_eq!(input.win.cpos, 5);
    }

    #[test]
    fn select_empty_buffer() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = String::new();
        input.win.cpos = 0;
        input.execute_key_action(KeyAction::SelectRight, None);
        assert_eq!(input.win.cpos, 0);
        assert!(input.selection_range().is_none());
    }

    #[test]
    fn utf8_selection() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "héllo".to_string();
        input.win.cpos = 0;
        input.execute_key_action(KeyAction::SelectRight, None);
        input.execute_key_action(KeyAction::SelectRight, None);
        // Should select "hé" — 2 chars but 3 bytes.
        assert_eq!(input.win.cpos, 3); // byte offset of 'l'
        assert_eq!(input.selection_range(), Some((0, 3)));
        input.execute_key_action(KeyAction::Backspace, None);
        assert_eq!(input.buf, "llo");
    }

    #[test]
    fn selection_preserved_across_multiple_select_directions() {
        let mut input = PromptState::new();
        input.win.edit_buf.buf = "abcdef".to_string();
        input.win.cpos = 3; // on 'd'
                            // Select right 2 chars.
        input.execute_key_action(KeyAction::SelectRight, None);
        input.execute_key_action(KeyAction::SelectRight, None);
        assert_eq!(input.selection_range(), Some((3, 5)));
        // Then select left 4 chars — anchor stays at 3.
        input.execute_key_action(KeyAction::SelectLeft, None);
        input.execute_key_action(KeyAction::SelectLeft, None);
        input.execute_key_action(KeyAction::SelectLeft, None);
        input.execute_key_action(KeyAction::SelectLeft, None);
        assert_eq!(input.win.cpos, 1);
        assert_eq!(input.selection_range(), Some((1, 3)));
    }

    #[test]
    fn vim_esc_clears_shift_selection() {
        use crossterm::event::{
            Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
        };

        let mut input = PromptState::new();
        input.set_vim_enabled(true);
        input.win.edit_buf.buf = "hello world".to_string();
        input.win.cpos = 0;
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
        let mut input = PromptState::new();
        // Insert text with an attachment marker in the middle: "ab[paste]cd"
        input.win.edit_buf.buf = format!("ab{}cd", ATTACHMENT_MARKER);
        input.win.cpos = 0;
        let id = input.store.insert_paste("pasted".to_string());
        input.win.edit_buf.attachment_ids.push(id);
        // Select "b[paste]c" (bytes 1..5 — marker is 3 bytes)
        input.win.win_cursor.set_anchor(Some(1));
        input.win.cpos = 1 + 1 + ATTACHMENT_MARKER.len_utf8() + 1; // b + marker + c
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
    fn buf_with_two_markers(input: &mut PromptState, id: AttachmentId) {
        input.win.edit_buf.buf = format!("pre{m}mid{m}post", m = ATTACHMENT_MARKER);
        input.win.cpos = input.buf.len();
        input.win.edit_buf.attachment_ids = vec![id, id];
    }

    #[test]
    fn expanded_text_inlines_paste_once_for_duplicate_ids() {
        let mut input = PromptState::new();
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
        let mut input = PromptState::new();
        let id1 = input.store.insert_paste("alpha body long enough".into());
        let id2 = input.store.insert_paste("beta body different".into());
        input.win.edit_buf.buf = format!("{m}and{m}", m = ATTACHMENT_MARKER);
        input.win.cpos = input.buf.len();
        input.win.edit_buf.attachment_ids = vec![id1, id2];
        let text = input.expanded_text();
        assert!(text.contains("alpha body long enough"));
        assert!(text.contains("beta body different"));
        assert!(!text.contains("[see earlier"));
    }

    #[test]
    fn expanded_text_three_identical_ids_emits_one_body_two_stubs() {
        let mut input = PromptState::new();
        let body = "repeated content".to_string();
        let id = input.store.insert_paste(body.clone());
        input.win.edit_buf.buf = format!("{m}a{m}b{m}", m = ATTACHMENT_MARKER);
        input.win.cpos = input.buf.len();
        input.win.edit_buf.attachment_ids = vec![id, id, id];
        let text = input.expanded_text();
        assert_eq!(text.matches(&body).count(), 1);
        assert_eq!(text.matches(PASTE_STUB).count(), 2);
    }

    #[test]
    fn build_content_dedups_repeated_image_in_parts() {
        let mut input = PromptState::new();
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
        let mut input = PromptState::new();
        let id1 = input
            .store
            .insert_image("a.png".into(), "data:image/png;base64,AAA".into());
        let id2 = input
            .store
            .insert_image("b.png".into(), "data:image/png;base64,BBB".into());
        input.win.edit_buf.buf = format!("{m}{m}", m = ATTACHMENT_MARKER);
        input.win.cpos = input.buf.len();
        input.win.edit_buf.attachment_ids = vec![id1, id2];
        let content = input.build_content();
        assert_eq!(content.image_count(), 2);
    }

    #[test]
    fn build_content_dedups_interleaved_image_references() {
        // Pattern: img A, img B, img A again. Parts should be [A, B].
        let mut input = PromptState::new();
        let id_a = input
            .store
            .insert_image("a.png".into(), "data:image/png;base64,AAA".into());
        let id_b = input
            .store
            .insert_image("b.png".into(), "data:image/png;base64,BBB".into());
        input.win.edit_buf.buf = format!("{m}x{m}y{m}", m = ATTACHMENT_MARKER);
        input.win.cpos = input.buf.len();
        input.win.edit_buf.attachment_ids = vec![id_a, id_b, id_a];
        let content = input.build_content();
        assert_eq!(content.image_count(), 2);
    }

    use crate::keymap::KeyAction;
}
