//! Per-tick glue between `App` and the Lua runtime. Drains pending
//! Lua callback invocations + the task-runtime inbox so dispatched
//! handlers see a consistent state.

use super::*;

impl App {
    /// Vim-mode label for the currently focused buffer Window. Asks
    /// `Ui` for a focused dialog buffer panel first (so a dialog with
    /// an interactive preview pane reports its own mode); falls back
    /// to the active split (transcript / prompt). Returns `None` when
    /// the focused surface is a widget panel or non-interactive
    /// chrome — same as nvim's "widget windows have no mode."
    pub(super) fn current_vim_mode_label(&self) -> Option<String> {
        if let Some(win) = self.ui.focused_dialog_buffer_window() {
            return win.vim.as_ref().map(|v| format!("{:?}", v.mode()));
        }
        match self.app_focus {
            crate::app::AppFocus::Content => self
                .transcript_window
                .vim
                .as_ref()
                .map(|v| format!("{:?}", v.mode())),
            crate::app::AppFocus::Prompt => self.input.vim_mode().map(|m| format!("{m:?}")),
        }
    }

    /// Fire `WinEvent::TextChanged` on `PROMPT_WIN` when the prompt
    /// buffer has changed since the last dispatch. Any Lua subscriber
    /// (`smelt.win.on_event(smelt.prompt.win_id(), "text_changed", fn)`)
    /// runs in the callback's own invocation pass; ops pushed by the
    /// handler are drained before returning so downstream code sees
    /// a consistent state.
    pub(super) fn emit_prompt_text_changed_if_dirty(&mut self) {
        let current_text = self.input.win.edit_buf.buf.clone();
        if self.last_prompt_text == current_text {
            return;
        }
        self.last_prompt_text = current_text.clone();
        let lua = &self.lua;
        let mut lua_invoke = |handle: ui::LuaHandle,
                              win: ui::WinId,
                              payload: &ui::Payload,
                              panels: &[ui::PanelSnapshot]| {
            lua.queue_invocation(handle, win, payload, panels);
        };
        self.ui.dispatch_event(
            ui::PROMPT_WIN,
            ui::WinEvent::TextChanged,
            ui::Payload::Text {
                content: current_text,
            },
            &mut lua_invoke,
        );
        self.flush_lua_callbacks();
    }

    /// Drain pending Lua callback invocations + the task-runtime
    /// inbox. Call after any Lua handler dispatch.
    pub(super) fn flush_lua_callbacks(&mut self) {
        self.drain_lua_invocations();
        self.lua.pump_task_events();
    }

    /// Drain the pending-invocation queue built up during
    /// `ui.handle_key` / `ui.dispatch_event`. Each Lua callback fires
    /// under an `install_app_ptr` scope so its body can reach `&mut App`
    /// through `crate::lua::with_app`. Until a binding uses it, this is
    /// behaviour-neutral: callbacks just fire after the ui borrow
    /// releases instead of during it.
    ///
    /// Two-phase to keep `&mut App` aliasing clean: phase 1 uses the
    /// `&mut self` borrow to prepare mlua Function + payload handles
    /// (these own internal refs to the Lua state, independent of Rust
    /// borrows on self); phase 2 installs the TLS pointer and calls
    /// each function with no Rust-level borrow on self alive — so a
    /// Lua body that reaches back via `with_app` gets the sole `&mut
    /// App` reborrow.
    pub(super) fn drain_lua_invocations(&mut self) {
        loop {
            let pending = self.lua.drain_invocations();
            if pending.is_empty() {
                return;
            }
            // Phase 1: collect (func, payload_table, handle_id) tuples.
            // Uses the `&mut self` borrow on self.lua.
            let prepared: Vec<(mlua::Function, mlua::Table, u64)> = pending
                .into_iter()
                .filter_map(|inv| {
                    let (func, payload) = self.lua.prepare_invocation(
                        inv.handle,
                        inv.win,
                        &inv.payload,
                        &inv.panels,
                    )?;
                    Some((func, payload, inv.handle.0))
                })
                .collect();
            // Phase 2: install TLS app pointer and call each. After
            // install_app_ptr returns, no &mut self use remains on any
            // control-flow path, so NLL kills the borrow — Lua bodies
            // that reach back via `with_app` get the sole reborrow.
            let _guard = crate::lua::install_app_ptr(self);
            for (func, payload, handle_id) in prepared {
                if let Err(e) = func.call::<()>(payload) {
                    crate::lua::try_with_app(|app| {
                        app.lua.record_callback_error(handle_id, e);
                    });
                }
            }
            // Loop: a callback body may itself queue further invocations
            // (e.g. a text_changed handler dispatches a submit inside
            // the same frame). Drain them in the same tick.
        }
    }

    /// Drive the `LuaTask` runtime and act on its outputs. Errors are
    /// queued via `NotifyError` internally; the only remaining output
    /// is `ToolComplete` (tool-as-task results). Dialog/picker opens
    /// now ride on `UiOp::OpenLuaDialog` / `OpenLuaPicker` and are
    /// resolved inside `apply_ui_op`.
    pub(super) fn drive_lua_tasks(&mut self) {
        self.flush_lua_callbacks();
        let outs = self.lua.drive_tasks();
        // Drain the ops pushed by the coroutine *before* it yielded —
        // a task that calls `buf.create` + `buf.set_lines` right
        // before `dialog.open` needs those ops applied now so the
        // reducer sees the buffers when `OpenLuaDialog` fires.
        self.flush_lua_callbacks();
        for out in outs {
            match out {
                crate::lua::TaskDriveOutput::ToolComplete {
                    request_id,
                    call_id,
                    content,
                    is_error,
                } => {
                    self.engine.send(protocol::UiCommand::PluginToolResult {
                        request_id,
                        call_id,
                        content,
                        is_error,
                    });
                }
                crate::lua::TaskDriveOutput::Error(msg) => {
                    self.notify_error(msg);
                }
            }
        }
        self.flush_lua_callbacks();
    }
}
