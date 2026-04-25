//! Typed mutation surface for app code to drive window + buffer
//! state. Each operation is a named function with clear pre/post-
//! conditions, so call sites read as intent rather than tag dispatch.
//!
//! This mirrors neovim's `vim.api.{nvim_buf_*, nvim_win_*}` split:
//! `buf::*` operates on buffer text; `win::*` on cursor + viewport +
//! selection. The same surface internal code uses is what plugins will
//! eventually use — keep it small, named, and intent-shaped.
//!
//! # Stability
//!
//! Breaking changes to any `pub fn` in this module bump [`VERSION`].
//! User scripts (Lua, Rust plugins) can branch on it to target a
//! specific API generation.

/// Semantic-version tag for the public API surface. Increments on any
/// signature change, removal, or behaviour-altering rename. Additive
/// changes (new functions) do not bump the version.
pub const VERSION: &str = "1";

/// Buffer-level operations — text, attachments, whole-buffer replace.
pub mod buf {
    use crate::input::PromptState;
    use ui::EditBuffer;

    /// Read the text of a buffer.
    pub fn get_text(buffer: &EditBuffer) -> &str {
        &buffer.buf
    }

    /// Replace the prompt buffer's text wholesale. Snapshots undo,
    /// clears attachments + shift-selection anchor, resets paste
    /// state, drops the completer so it re-derives, and places the
    /// cursor at `cursor` (or end-of-text if `None`).
    ///
    /// This is the canonical path for commands that stuff new text
    /// into the prompt (unqueue, resume restore, ghost accept). Direct
    /// `input.buf = …` writes skip these invariants and have been a
    /// recurring source of undo / completer / paste-state bugs.
    pub fn replace(input: &mut PromptState, text: String, cursor: Option<usize>) {
        input.replace_text(text, cursor);
    }
}

/// Per-block mutations on the transcript — view state (collapsed /
/// trimmed / expanded) and lifecycle status (streaming / done). This
/// is the surface "click to expand", "collapse long tool output",
/// "agent re-runs a block" and similar features will grow against.
///
/// Only view state + status land in this slice; `append_text`,
/// `rewrite`, `invoke`, and per-block keymaps arrive in Stage 3.5's
/// subsequent slices as the `active_*` → live-block collapse progresses.
pub mod block {
    use crate::app::transcript_model::{Block, BlockId, Status, ViewState};
    use crate::app::App;

    /// Current view state of a block.
    pub fn view_state(app: &App, id: BlockId) -> ViewState {
        app.block_view_state(id)
    }

    /// Set a block's view state. Invalidates that block's layout cache
    /// so the next frame re-lays-out against the new state.
    pub fn set_view_state(app: &mut App, id: BlockId, state: ViewState) {
        app.set_block_view_state(id, state);
    }

    /// Current lifecycle status of a block.
    pub fn status(app: &App, id: BlockId) -> Status {
        app.block_status(id)
    }

    /// Set a block's lifecycle status.
    pub fn set_status(app: &mut App, id: BlockId, status: Status) {
        app.set_block_status(id, status);
    }

    /// Push a new `Streaming` block onto the transcript and return its
    /// `BlockId`. The id is stable across subsequent `rewrite` calls —
    /// the canonical handle for live-updating a block as a stream
    /// arrives.
    pub fn push_streaming(app: &mut App, block: Block) -> BlockId {
        app.push_streaming(block)
    }

    /// Replace the content of an existing block in place. Preserves
    /// `BlockId`, `Status`, and `ViewState`; the layout cache
    /// auto-invalidates via the content-hash component of `LayoutKey`.
    pub fn rewrite(app: &mut App, id: BlockId, block: Block) {
        app.rewrite_block(id, block);
    }

    /// `BlockId`s of blocks currently in `Status::Streaming`, in
    /// transcript order.
    pub fn streaming_ids(app: &App) -> Vec<BlockId> {
        app.streaming_block_ids()
    }
}

/// Command dispatch — the single entry point for `/cmd` and `:cmd`
/// style actions. Internal handlers and (future) plugin handlers both
/// register here; keybindings resolve to names that route through
/// `run`. Modelled on nvim's `nvim_command` / `user_command` split.
pub mod cmd {
    use crate::app::App;

    /// Result of running a command — tells the caller whether the app
    /// should continue, quit, clear, open a dialog, etc. Thin wrapper
    /// around the internal `CommandAction` so the outer API can evolve
    /// without exposing every enum variant.
    pub use crate::app::commands::CommandOutcome as Outcome;

    /// Run a command line. Accepts `/name args...` or `:name args...`
    /// or a bare `name`. Parses the name, looks it up in the registry,
    /// and dispatches.
    ///
    /// The same code path runs whether the user typed the command,
    /// pressed a keybind that resolved to `Action::Cmd(name)`, or a
    /// plugin invoked it programmatically.
    pub fn run(app: &mut App, line: &str) -> Outcome {
        crate::app::commands::run_command(app, line)
    }
}
