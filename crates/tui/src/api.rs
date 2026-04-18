//! Typed mutation surface for app code to drive window + buffer
//! state. Replaces the `Mutation` enum: each operation is a named
//! function with clear pre/post-conditions, so call sites read as
//! intent rather than tag dispatch.
//!
//! This mirrors neovim's `vim.api.{nvim_buf_set_text, nvim_win_set_cursor, ...}`
//! split — `buf::*` operates on buffer text, `win::*` on cursor +
//! viewport. For now only the handful of call sites that previously
//! routed through `Mutation::Replace` live here; more will migrate
//! as later stages of the architecture plan land.

/// Buffer-level operations.
pub mod buf {
    use crate::input::InputState;

    /// Replace the prompt buffer's text wholesale. Snapshots undo,
    /// clears attachments + shift-selection anchor, resets paste
    /// state, drops the completer so it re-derives, and places the
    /// cursor at `cursor` (or end-of-text if `None`).
    ///
    /// This is the canonical path for commands that stuff new text
    /// into the prompt (unqueue, resume restore, ghost accept). Direct
    /// `input.buf = …` writes skip these invariants and have been a
    /// recurring source of undo / completer / paste-state bugs.
    pub fn replace(input: &mut InputState, text: String, cursor: Option<usize>) {
        input.replace_text(text, cursor);
    }
}
