//! Per-tick glue between `App` and the Lua runtime. Pushes engine /
//! buffer / window state into the runtime's snapshot before handlers
//! run, drains ops and task-runtime outputs after.

use super::*;

impl App {
    pub(super) fn snapshot_lua_context(&mut self) -> (Option<String>, String) {
        let transcript_text = self
            .full_transcript_display_text(self.settings.show_thinking)
            .join("\n");
        let prompt_text = self.input.win.edit_buf.buf.clone();
        let focused_window = match self.app_focus {
            crate::app::AppFocus::Content => "transcript",
            crate::app::AppFocus::Prompt => "prompt",
        };
        let vim_mode = match self.app_focus {
            crate::app::AppFocus::Content => self
                .transcript_window
                .vim
                .as_ref()
                .map(|v| format!("{:?}", v.mode())),
            crate::app::AppFocus::Prompt => self.input.vim_mode().map(|m| format!("{m:?}")),
        };
        self.lua.set_context(
            Some(transcript_text),
            Some(prompt_text),
            Some(focused_window.to_string()),
            vim_mode.clone(),
        );
        self.snapshot_engine_context(self.agent.is_some());
        (vim_mode, focused_window.to_string())
    }

    /// Populate engine-related Lua snapshot fields.
    pub(super) fn snapshot_engine_context(&self, is_busy: bool) {
        let session_dir = crate::session::dir_for(&self.session);
        self.lua.set_engine_context(crate::lua::EngineSnapshot {
            model: self.model.clone(),
            mode: self.mode.as_str().to_string(),
            reasoning_effort: self.reasoning_effort.label().to_string(),
            is_busy,
            session_cost: self.session_cost_usd,
            context_tokens: self.context_tokens,
            context_window: self.context_window,
            session_dir: session_dir.display().to_string(),
            session_id: self.session.id.clone(),
            session_title: self.session.title.clone(),
            session_cwd: self.cwd.clone(),
            session_created_at_ms: self.session.created_at_ms,
            session_turns: self.user_turns(),
            vim_enabled: self.input.vim_enabled(),
            permission_session_entries: self
                .session_permission_entries()
                .into_iter()
                .map(|e| (e.tool, e.pattern))
                .collect(),
        });
        self.lua.set_history(self.history.clone());
    }

    /// Push the current user-facing boolean settings into the Lua
    /// snapshot. Called at startup and whenever settings mutate —
    /// not every tick. Keeps `smelt.settings.snapshot()` cheap.
    pub(super) fn push_settings_snapshot_to_lua(&self) {
        let s = self.settings_state();
        self.lua.set_settings_snapshot(vec![
            ("vim".into(), s.vim),
            ("auto_compact".into(), s.auto_compact),
            ("show_tps".into(), s.show_tps),
            ("show_tokens".into(), s.show_tokens),
            ("show_cost".into(), s.show_cost),
            ("show_prediction".into(), s.show_prediction),
            ("show_slug".into(), s.show_slug),
            ("show_thinking".into(), s.show_thinking),
            ("restrict_to_workspace".into(), s.restrict_to_workspace),
            ("redact_secrets".into(), s.redact_secrets),
        ]);
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
        // Sync the Lua snapshot so `smelt.prompt.text()` inside the
        // handler sees the new value (the snapshot is otherwise only
        // refreshed during keymap dispatch).
        self.lua.set_prompt_text_snapshot(current_text.clone());
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
        self.apply_lua_ops();
    }

    /// Drain and apply all pending Lua ops (notifications, errors,
    /// commands, engine mutations). Also pumps the task-runtime inbox
    /// (dialog resolutions etc.) so resumption side-effects become ops.
    /// Call after any Lua handler dispatch.
    pub(super) fn apply_lua_ops(&mut self) {
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
        self.apply_lua_ops();
        let outs = self.lua.drive_tasks();
        // Drain the ops pushed by the coroutine *before* it yielded —
        // a task that calls `buf.create` + `buf.set_lines` right
        // before `dialog.open` needs those ops applied now so the
        // reducer sees the buffers when `OpenLuaDialog` fires.
        self.apply_lua_ops();
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
        self.apply_lua_ops();
    }
}
