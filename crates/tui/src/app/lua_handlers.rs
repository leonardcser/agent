//! `pub(crate)` operations called by Lua bindings through `with_app`.
//! Each method wraps a multi-step App mutation that the Lua layer
//! invokes as a single semantic action — `/<command>` dispatch,
//! settings toggle, transcript yank, and so on.

use super::transcript_model::ConfirmChoice;
use super::App;

impl App {
    /// Run a slash command. Mirrors the user typing `:<line>` into
    /// the cmdline — handles `Quit`, `CancelAndClear`, `Compact`,
    /// `Exec`, `Continue` action returns from `crate::api::cmd::run`.
    pub(crate) fn apply_lua_command(&mut self, line: &str) {
        match crate::api::cmd::run(self, line) {
            crate::app::CommandAction::Quit => {
                self.pending_quit = true;
            }
            crate::app::CommandAction::CancelAndClear => {
                self.reset_session();
                self.agent = None;
            }
            crate::app::CommandAction::Compact { instructions } => {
                self.compact_or_notify(instructions);
            }
            crate::app::CommandAction::Exec(rx, kill) => {
                self.exec_rx = Some(rx);
                self.exec_kill = Some(kill);
            }
            crate::app::CommandAction::Continue => {}
        }
    }

    /// Toggle one of the named boolean settings (`vim`, `auto_compact`,
    /// `show_tps`, …). Notifies an error toast on unknown keys.
    pub(crate) fn toggle_named_setting(&mut self, key: &str) {
        let mut s = self.settings_state();
        match key {
            "vim" => s.vim ^= true,
            "auto_compact" => s.auto_compact ^= true,
            "show_tps" => s.show_tps ^= true,
            "show_tokens" => s.show_tokens ^= true,
            "show_cost" => s.show_cost ^= true,
            "show_prediction" => s.show_prediction ^= true,
            "show_slug" => s.show_slug ^= true,
            "show_thinking" => s.show_thinking ^= true,
            "restrict_to_workspace" => s.restrict_to_workspace ^= true,
            "redact_secrets" => s.redact_secrets ^= true,
            _ => {
                self.notify_error(format!("unknown setting: {key}"));
                return;
            }
        }
        self.set_settings(s);
    }

    /// Compact the transcript or notify "nothing to compact" when
    /// `history` is empty.
    pub(crate) fn compact_or_notify(&mut self, instructions: Option<String>) {
        if self.history.is_empty() {
            self.notify_error("nothing to compact".into());
        } else {
            self.compact_history(instructions);
        }
    }

    /// Rewind to a transcript block (Rewind dialog) or, when
    /// `block_idx` is `None`, optionally restore Vim Insert mode.
    pub(crate) fn rewind_to_block(&mut self, block_idx: Option<usize>, restore_vim_insert: bool) {
        if let Some(bidx) = block_idx {
            if self.agent.is_some() {
                self.cancel_agent();
                self.agent = None;
            }
            if let Some((text, images)) = self.rewind_to(bidx) {
                self.input.restore_from_rewind(text, images);
            }
            while self.engine.try_recv().is_ok() {}
            self.save_session();
        } else if restore_vim_insert {
            self.input.set_vim_mode(crate::vim::ViMode::Insert);
        }
    }

    /// Load a saved session by id. Refreshes screen + token tally
    /// and scrolls to bottom on success; silent no-op on missing id.
    pub(crate) fn load_session_by_id(&mut self, id: &str) {
        if let Some(loaded) = crate::session::load(id) {
            self.load_session(loaded);
            self.restore_screen();
            if let Some(tokens) = self.session.context_tokens {
                self.context_tokens = Some(tokens);
            }
            self.finish_transcript_turn();
            self.transcript_window.scroll_to_bottom();
        }
    }

    /// Copy the transcript block under the cursor to the clipboard
    /// (`/yank-block`). Notifies success / failure.
    pub(crate) fn yank_current_block(&mut self) {
        let abs_row = self.transcript_window.cursor_abs_row();
        if let Some(text) = self.block_text_at_row(abs_row, self.settings.show_thinking) {
            if super::commands::copy_to_clipboard(&text).is_ok() {
                self.transcript_window
                    .kill_ring
                    .record_clipboard_write(text);
            }
            self.notify("block copied".into());
        } else {
            self.notify_error("no block at cursor".into());
        }
    }

    /// Resolve an open Confirm dialog with the user's choice. Heavy
    /// cancel (flush events, kill blocking subagents, drop the active
    /// turn) when the resolution asks the turn to cancel.
    pub(crate) fn handle_confirm_resolve(
        &mut self,
        choice: ConfirmChoice,
        message: Option<String>,
        request_id: u64,
        call_id: &str,
        tool_name: &str,
    ) {
        let should_cancel = self.resolve_confirm((choice, message), call_id, request_id, tool_name);
        if should_cancel {
            self.finish_turn(true);
            self.agent = None;
        }
    }
}
