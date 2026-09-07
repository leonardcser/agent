use super::*;

impl TestApp {
    /// Side-channel: invoke the transactional `/reload` pipeline. It loads a
    /// fresh candidate, commits it only after validation, retires the previous
    /// generation, and fires `on_ready` with `ctx.kind = "reload"`. Named
    /// resources keep stable ids while anonymous resources from the retired
    /// generation are reaped.
    pub fn reload_lua(&mut self) {
        self.app.reload_lua();
    }

    pub fn schedule_lua_reload(&mut self) -> bool {
        self.app.schedule_lua_reload()
    }

    pub fn drain_idle_work(&mut self) -> bool {
        self.app.drain_idle_work()
    }

    /// Pump a bounded batch of Lua wakeups so spawned callbacks reach their next wait point.
    pub fn settle_lua(&mut self) {
        for _ in 0..4 {
            self.feed_one(SourceEvent::LuaWakeup);
        }
    }

    /// Wait for background enrichment of a named native document's current viewport.
    /// Tests call this only when asserting final styles, never to time usable content.
    pub fn wait_for_document(&mut self, name: &str, timeout: Duration) {
        self.render_silent();
        let win = self
            .ui_probe()
            .named_win(name)
            .expect("native document window");
        let source = self
            .ui_probe()
            .win(win)
            .unwrap()
            .row_source()
            .expect("native source")
            .clone();
        let deadline = std::time::Instant::now() + timeout;
        while source.is_pending() {
            assert!(
                std::time::Instant::now() < deadline,
                "{name}: background document work stalled"
            );
            std::thread::sleep(Duration::from_millis(1));
            self.feed_one(SourceEvent::LuaWakeup);
            self.render_silent();
        }
        self.render_silent();
    }

    pub fn pending_lua_reload(&self) -> bool {
        self.app.lua_reload_pending()
    }

    pub(crate) fn reload_lua_config(&mut self) {
        self.app.reload_lua_config();
    }

    pub(crate) fn apply_lua_command(&mut self, command: &str) {
        self.app.apply_lua_command(command);
    }

    pub(crate) fn drive_lua_tasks(&mut self) {
        self.app.drive_lua_tasks();
    }

    pub(crate) fn drive_lua_tasks_once(&mut self) -> usize {
        let now = self.app.core.clock.instant_now();
        let lua = self.app.lua.execution();
        crate::lua::scope_app(&mut self.app, || lua.drive_tasks(now)).len()
    }

    #[cfg(test)]
    pub(crate) fn try_receive_lua_wakeup(&mut self) -> bool {
        self.app.try_receive_lua_wakeup()
    }

    pub(crate) fn shutdown_lua(&mut self) -> (Vec<String>, Option<String>) {
        self.app.shutdown_lua()
    }

    pub(crate) fn drain_launch_ready_hooks(&mut self) {
        self.app.drain_launch_ready_hooks_for_harness();
    }

    #[cfg(test)]
    pub(crate) fn lua_runtime_reconcile_pending(&self) -> bool {
        self.app.lua_runtime_reconcile_pending()
    }

    pub(crate) fn reconcile_committed_lua_runtime(&mut self) -> Result<(), String> {
        self.app.reconcile_committed_lua_runtime()
    }

    pub(crate) fn complete_lua_tool(
        &mut self,
        invocation: smelt_core::lua::ToolInvocationContext,
        call_id: String,
        content: String,
        is_error: bool,
        metadata: Option<serde_json::Value>,
    ) {
        self.app.complete_lua_tool(
            invocation,
            call_id,
            crate::app::agent::LuaToolCompletion {
                content,
                is_error,
                metadata,
                display_content: Vec::new(),
                attachment: None,
            },
        );
    }

    /// Run an arbitrary Lua snippet while lending it the frontend host. Callers
    /// that intentionally probe invalid arguments can use this boolean form after
    /// wrapping those calls in Lua `pcall`.
    pub fn run_lua(&mut self, snippet: &str) -> bool {
        self.run_lua_result(snippet).is_ok()
    }

    pub(crate) fn run_bundled_lua(&mut self, snippet: &str) -> bool {
        let lua = self.app.lua.lua().clone();
        let result = crate::lua::scope_app(&mut self.app, || {
            let environment = smelt_core::lua::module::bundled_chunk_environment(&lua)
                .map_err(|error| error.to_string())?;
            lua.load(snippet)
                .set_environment(environment)
                .exec()
                .map_err(|error| error.to_string())
        });
        self.app.pump_lua();
        self.app.try_perform_scheduled_runtime_reconcile();
        self.app.drain_deferred_layout();
        result.is_ok()
    }

    /// Run a Lua snippet and preserve the Lua error for focused harnesses that
    /// require a valid call path to succeed.
    pub fn run_lua_result(&mut self, snippet: &str) -> Result<(), String> {
        let result = self.exec_lua_entry(snippet);
        self.app.pump_lua();
        self.app.try_perform_scheduled_runtime_reconcile();
        self.app.drain_deferred_layout();
        result
    }

    /// Execute one Lua entry without pumping deferred callbacks or reconciliation.
    pub fn exec_lua_entry(&mut self, snippet: &str) -> Result<(), String> {
        let lua = self.app.lua.lua().clone();
        crate::lua::scope_app(&mut self.app, || {
            lua.load(snippet).exec().map_err(|error| error.to_string())
        })
    }

    /// Evaluate a Lua expression while lending it the frontend host.
    pub fn eval_lua<T: mlua::FromLuaMulti>(&mut self, snippet: &str) -> Result<T, String> {
        let lua = self.app.lua.lua().clone();
        crate::lua::scope_app(&mut self.app, || {
            lua.load(snippet).eval().map_err(|error| error.to_string())
        })
    }

    pub fn lua_int_global(&self, name: &str) -> Option<i64> {
        self.app.lua.lua.globals().get(name).ok()
    }

    pub(crate) fn set_lua_string_global(
        &mut self,
        name: &str,
        value: impl Into<String>,
    ) -> Result<(), mlua::Error> {
        self.app.lua.lua.globals().set(name, value.into())
    }

    pub(crate) fn clear_lua_messages(&mut self) {
        self.app
            .lua
            .core_shared()
            .messages
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    #[cfg(test)]
    pub(crate) fn lua_message_count(&self, body: &str) -> usize {
        self.app
            .lua
            .core_shared()
            .messages
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entries()
            .iter()
            .filter(|entry| entry.full == body)
            .count()
    }

    #[cfg(test)]
    pub(crate) fn lua_messages_contain(&self, text: &str) -> bool {
        self.app
            .lua
            .core_shared()
            .messages
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entries()
            .iter()
            .any(|entry| entry.full.contains(text))
    }

    pub fn tool_summary(
        &mut self,
        tool_name: &str,
        args: &std::collections::HashMap<String, serde_json::Value>,
    ) -> protocol::StyledLines {
        let lua = self.app.lua.execution();
        crate::lua::scope_app(&mut self.app, || lua.tool_summary(tool_name, args))
    }

    pub fn mode_note(&mut self, mode: &str) -> String {
        let lua = self.app.lua.execution();
        crate::lua::scope_app(&mut self.app, || lua.mode_note(mode))
    }

    /// Re-publish the signal diff + fire queued subscribers. Production
    /// runs this every main-loop tick (`app.rs:1068`); the harness
    /// skips that loop and exposes it here so tests can assert against
    /// the reactive `work_*` / `vim_mode` / `now` signals without driving
    /// a synthetic event.
    pub fn tick_signals(&mut self) {
        self.app.publish_diff_signals();
        self.app.drain_signals_pending();
    }

    pub(crate) fn drain_signals_pending(&mut self) {
        self.app.drain_signals_pending();
    }

    pub(crate) fn pump_lua(&mut self) {
        self.app.pump_lua();
    }

    pub(crate) fn tick_timers(&mut self) {
        self.app.tick_timers();
    }

    pub(crate) fn clear_timers(&mut self) {
        self.app.core.timers.clear();
    }

    pub(crate) fn dispatch_ui_window_events(&mut self, include_tick: bool) {
        self.app.dispatch_ui_window_events(include_tick);
    }

    /// Counts of bound names across the four reload-managed registries:
    /// `(bufs, wins, overlays, paints)`. Failed-reload checks snapshot
    /// these before and after `reload_lua()` and assert equality. Successful
    /// reloads may add declared resources and retire names omitted by the
    /// candidate.
    pub fn named_resource_counts(&self) -> (usize, usize, usize, usize) {
        let (bufs, wins, overlays) = self.app.ui.named_counts();
        (bufs, wins, overlays, self.app.paint_registry.named_count())
    }

    /// Enumerate every Lua function recorded by `LuaMod::fn_` at
    /// registration time. Returned tuples are `(module, name)`, e.g.
    /// `("smelt.buf", "new")`. The same registry powers
    /// `cargo xtask gen-lua-docs`, so any function visible in the
    /// reference docs is also fuzzable - and a freshly-added
    /// `smelt.foo.bar` flows into the fuzz target automatically, with
    /// no manual update to a hand-written `LuaOp` table.
    pub fn lua_doc_snapshot(&self) -> Vec<(&'static str, &'static str)> {
        smelt_core::lua::doc::snapshot()
            .into_iter()
            .map(|m| (m.module, m.name))
            .collect()
    }

    /// Force a full Lua GC, then walk every registered `LuaHandle`
    /// across the shared registries and assert it still resolves in the
    /// mlua registry. A `Value::Nil` after `gc_collect` means a Rust
    /// path dropped the handle's `RegistryKey` (or the key was wrong
    /// from the start) - the Rust→Lua reference is dangling. Used by
    /// `lua_loop` between batches so leaks surface attached to the op
    /// that caused them rather than at scenario teardown.
    pub fn assert_lua_handles_alive(&self) {
        let lua = &self.app.lua.lua;
        lua.gc_collect().expect("lua gc_collect failed");

        let check = |label: &str, handle: &smelt_core::lua::LuaHandle| {
            let val: mlua::Value = lua
                .registry_value(&handle.key)
                .unwrap_or_else(|e| panic!("FFI-LEDGER: registry_value({label}) failed: {e}"));
            if matches!(val, mlua::Value::Nil) {
                panic!("FFI-LEDGER: dangling handle in {label} (registry value is Nil after gc_collect)");
            }
        };

        let shared = self.app.lua.shared();
        let core = &shared.core;
        if let Ok(cbs) = core.callbacks.lock() {
            for (id, h) in cbs.iter() {
                check(&format!("callbacks[{id}]"), h);
            }
        }
        if let Ok(asks) = core.ask_callbacks.lock() {
            for (id, callbacks) in asks.iter() {
                if let Some(h) = &callbacks.response {
                    check(&format!("ask_callbacks[{id}].response"), h);
                }
                if let Some(h) = &callbacks.delta {
                    check(&format!("ask_callbacks[{id}].delta"), h);
                }
            }
        }
        if let Ok(cmds) = core.commands.lock() {
            for (name, cmd) in cmds.iter() {
                check(&format!("commands[{name}]"), &cmd.handle);
            }
        }
        if let Ok(kms) = core.keymaps.lock() {
            for (k, entry) in kms.iter() {
                check(&format!("keymaps[{k:?}]"), &entry.handle);
            }
        }
        if let Ok(tools) = core.tools.lock() {
            for (name, t) in tools.iter() {
                check(&format!("tools[{name}].execute"), &t.execute);
                if let Some(h) = &t.approval_patterns {
                    check(&format!("tools[{name}].approval_patterns"), h);
                }
                if let Some(h) = &t.preflight {
                    check(&format!("tools[{name}].preflight"), h);
                }
                if let Some(h) = &t.paths_for_workspace {
                    check(&format!("tools[{name}].paths_for_workspace"), h);
                }
                if let Some(h) = &t.preview {
                    check(&format!("tools[{name}].preview"), h);
                }
                if let Some(h) = &t.preview_output {
                    check(&format!("tools[{name}].preview_output"), h);
                }
            }
        }

        let hooks = &core.hooks;
        let check_reg = |reg_label: &str, reg: &smelt_core::lua::HookRegistry| {
            reg.for_each_entry(|id, name, h| {
                check(&format!("{reg_label}[{id} name={name:?}]"), h);
            });
        };
        check_reg("hooks.provider_response", &hooks.provider_response);
        check_reg("hooks.context_limit", &hooks.context_limit);
        check_reg("hooks.lifecycle", &hooks.lifecycle);
    }

    /// Net live `LuaHandle` count, taken from this session's drop-counter
    /// ledger (`created - dropped`). Complements [`assert_lua_handles_alive`]:
    /// that function walks named registries and asserts each handle
    /// resolves; this one counts *every* handle that's ever crossed
    /// `LuaHandle::from_func` regardless of where it ended up stored,
    /// so it catches leaks the named walk can't see (anonymous closures
    /// stashed only in Lua tables, etc.).
    pub fn lua_handles_live(&self) -> u64 {
        self.app.lua.core_shared().lua_handles_live()
    }

    /// Reload the Lua context once, snapshot the live handle count,
    /// reload again, and assert the count didn't grow. Compares **two
    /// consecutive** reloads (not pre/post a single reload) because
    /// cold-start vs first-reload isn't apples-to-apples - lifecycle
    /// hooks fire with `ctx.kind = "reload"` only on the second-and-
    /// later bring-ups, so plugins legitimately do extra registration
    /// on the first reload. By reload N the system is in steady state;
    /// any drift between reload N and N+1 means a registry isn't
    /// being cleared.
    ///
    /// Intended for one-shot use at the END of a scenario, after the
    /// scenario's own reload ops have run - calling it inside the
    /// segment loop would inflate the reload count and obscure the
    /// scenario semantics.
    pub fn assert_no_handle_leak_across_reload(&mut self) {
        self.reload_lua();
        self.app.lua.lua.gc_collect().ok();
        self.app.lua.lua.gc_collect().ok();
        let baseline = self.app.lua.core_shared().lua_handles_live();
        self.reload_lua();
        self.app.lua.lua.gc_collect().ok();
        self.app.lua.lua.gc_collect().ok();
        let after = self.app.lua.core_shared().lua_handles_live();
        if after > baseline {
            panic!(
                "FFI-LEDGER: handle leak across reload - count went {baseline} -> {after} on second consecutive reload (steady state should be stable)"
            );
        }
    }

    /// Repeatedly load a candidate that allocates handles and then fails. The
    /// committed generation and named resources must remain unchanged, and
    /// discarding each candidate must return the session handle ledger to its
    /// baseline.
    pub fn assert_no_handle_leak_across_failed_reload(&mut self, init_path: &std::path::Path) {
        let original = std::fs::read(init_path).unwrap_or_default();
        let generation = self.app.core.lua_generation;
        let named_resources = self.named_resource_counts();
        self.app.lua.lua.gc_collect().ok();
        self.app.lua.lua.gc_collect().ok();
        let baseline = self.app.lua.core_shared().lua_handles_live();
        let failing_candidate = r#"
            local buffer = smelt.buf.new({ name = "fuzz.failed.buffer" })
            buffer:source("discarded")
            smelt.cmd.register("fuzz.failed", function() end)
            smelt.signal.new("fuzz.failed.signal", 1)
            smelt.signal.subscribe("fuzz.failed.signal", function() end)
            smelt.timer.set(60000, function() end)
            smelt.lifecycle.on_ready(function() end)
            error("intentional failed candidate")
        "#;

        for _ in 0..2 {
            std::fs::write(init_path, failing_candidate).expect("write failing Lua candidate");
            self.reload_lua();
            self.app.lua.lua.gc_collect().ok();
            self.app.lua.lua.gc_collect().ok();
            assert_eq!(self.app.core.lua_generation, generation);
            assert_eq!(self.named_resource_counts(), named_resources);
            let after = self.app.lua.core_shared().lua_handles_live();
            if after > baseline {
                panic!(
                    "FFI-LEDGER: handle leak across failed reload - count went {baseline} -> {after}"
                );
            }
            self.assert_lua_handles_alive();
            self.assert_invariants();
        }

        std::fs::write(init_path, original).expect("restore Lua init after failed candidate");
    }

    /// Consume the app and verify that Lua-owned registration userdata did not
    /// retain a strong reference to the VM during teardown.
    pub fn assert_lua_runtime_released(self) {
        let lua = self.app.lua.lua.weak();
        drop(self);
        assert!(
            lua.try_upgrade().is_none(),
            "Lua registration retained a strong reference to its own runtime"
        );
    }
}
