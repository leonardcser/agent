//! `smelt.*` binding setup. `LuaRuntime::register_api` is the big mlua
//! table builder that wires every Rust-side primitive onto the `smelt`
//! global. Theme / JSON / color helpers it uses live here too; payload
//! + panel helpers for dialog callbacks live in `tasks.rs`.

use super::{
    lua_commands_snapshot, messages_to_lua, parse_keybind, parse_win_event, AutocmdEvent,
    LuaHandle, LuaRuntime, LuaShared, TaskCompletion, TaskEvent,
};
use mlua::prelude::*;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

impl LuaRuntime {
    pub(super) fn register_api(lua: &Lua, shared: &Arc<LuaShared>) -> LuaResult<()> {
        let smelt = lua.create_table()?;
        let smelt_ui = lua.create_table()?;
        let smelt_keymap = lua.create_table()?;

        smelt.set("version", crate::api::VERSION)?;

        // Helper: register a 0-arg getter that reads live state from `App`
        // via `try_with_app`. Replaces the old snapshot-mirror pattern —
        // every read goes through the TLS pointer installed at the top of
        // each tick / Lua-entry boundary.
        //
        // Reads use `try_with_app` (not `with_app`) so callers from a
        // context without `install_app_ptr` get the type's `Default`
        // instead of a panic. In production every Lua-entry path installs
        // the pointer, so the fallback is dead; tests that exercise
        // bindings without an App get empty/zeroed values rather than
        // panics, which keeps autoload-registration tests trivial.
        macro_rules! app_read {
            ($lua:expr, |$app:ident| $body:expr) => {{
                $lua.create_function(|_, ()| {
                    Ok(crate::lua::try_with_app(|$app| $body).unwrap_or_default())
                })?
            }};
        }

        // smelt.transcript.text()
        let transcript_tbl = lua.create_table()?;
        transcript_tbl.set(
            "text",
            app_read!(lua, |app| app
                .full_transcript_display_text(app.settings.show_thinking)
                .join("\n")),
        )?;
        transcript_tbl.set(
            "yank_block",
            lua.create_function(|_, ()| {
                crate::lua::with_app(|app| app.yank_current_block());
                Ok(())
            })?,
        )?;
        smelt.set("transcript", transcript_tbl)?;

        // smelt.cmd
        let cmd_tbl = lua.create_table()?;
        {
            let s = shared.clone();
            cmd_tbl.set(
                "register",
                lua.create_function(
                    move |lua,
                          (name, handler, opts): (
                        String,
                        mlua::Function,
                        Option<mlua::Table>,
                    )| {
                        let desc: Option<String> = opts
                            .as_ref()
                            .and_then(|t| t.get::<Option<String>>("desc").ok().flatten());
                        // `args` may be either a Lua array of strings
                        // (static) or omitted. Drives the secondary
                        // CommandArg picker that opens after `/name `.
                        let args: Vec<String> = opts
                            .as_ref()
                            .and_then(|t| t.get::<Option<mlua::Table>>("args").ok().flatten())
                            .map(|t| {
                                let mut v = Vec::new();
                                for pair in t.pairs::<mlua::Value, String>().flatten() {
                                    v.push(pair.1);
                                }
                                v
                            })
                            .unwrap_or_default();
                        let key = lua.create_registry_value(handler)?;
                        if let Ok(mut map) = s.commands.lock() {
                            map.insert(name.clone(), LuaHandle { key });
                        }
                        if let Ok(mut snap) = lua_commands_snapshot().lock() {
                            snap.insert(
                                name,
                                crate::lua::CommandMeta {
                                    description: desc,
                                    args,
                                },
                            );
                        }
                        Ok(())
                    },
                )?,
            )?;
        }
        cmd_tbl.set(
            "run",
            lua.create_function(|_, line: String| {
                crate::lua::with_app(|app| app.apply_lua_command(&line));
                Ok(())
            })?,
        )?;
        {
            let s = shared.clone();
            cmd_tbl.set(
                "list",
                lua.create_function(move |lua, ()| {
                    let names: Vec<String> = s
                        .commands
                        .lock()
                        .map(|m| m.keys().cloned().collect())
                        .unwrap_or_default();
                    let table = lua.create_table()?;
                    for (i, name) in names.iter().enumerate() {
                        table.set(i + 1, name.as_str())?;
                    }
                    Ok(table)
                })?,
            )?;
        }
        smelt.set("cmd", cmd_tbl)?;

        // smelt.engine.*
        let engine_tbl = lua.create_table()?;

        engine_tbl.set("model", app_read!(lua, |app| app.model.clone()))?;
        engine_tbl.set("mode", app_read!(lua, |app| app.mode.as_str().to_string()))?;
        engine_tbl.set(
            "reasoning_effort",
            app_read!(lua, |app| app.reasoning_effort.label().to_string()),
        )?;
        engine_tbl.set("is_busy", app_read!(lua, |app| app.agent.is_some()))?;
        engine_tbl.set("cost", app_read!(lua, |app| app.session_cost_usd))?;
        engine_tbl.set("context_tokens", app_read!(lua, |app| app.context_tokens))?;
        engine_tbl.set("context_window", app_read!(lua, |app| app.context_window))?;
        // smelt.session.*
        let session_tbl = lua.create_table()?;
        session_tbl.set("title", app_read!(lua, |app| app.session.title.clone()))?;
        session_tbl.set("cwd", app_read!(lua, |app| app.cwd.clone()))?;
        session_tbl.set(
            "created_at_ms",
            app_read!(lua, |app| app.session.created_at_ms),
        )?;
        session_tbl.set("id", app_read!(lua, |app| app.session.id.clone()))?;
        session_tbl.set(
            "dir",
            app_read!(lua, |app| crate::session::dir_for(&app.session)
                .display()
                .to_string()),
        )?;
        session_tbl.set(
            "turns",
            lua.create_function(|lua, ()| {
                let turns = crate::lua::try_with_app(|app| app.user_turns()).unwrap_or_default();
                let out = lua.create_table()?;
                for (i, (block_idx, text)) in turns.into_iter().enumerate() {
                    let row = lua.create_table()?;
                    row.set("block_idx", block_idx)?;
                    let label = text.lines().next().unwrap_or("").to_string();
                    row.set("label", label)?;
                    out.set(i + 1, row)?;
                }
                Ok(out)
            })?,
        )?;
        session_tbl.set(
            "rewind_to",
            lua.create_function(
                |_, (block_idx, opts): (Option<usize>, Option<mlua::Table>)| {
                    let restore_vim_insert = opts
                        .and_then(|t| t.get::<bool>("restore_vim_insert").ok())
                        .unwrap_or(false);
                    crate::lua::with_app(|app| app.rewind_to_block(block_idx, restore_vim_insert));
                    Ok(())
                },
            )?,
        )?;
        session_tbl.set(
            "list",
            lua.create_function(|lua, ()| {
                let current_id =
                    crate::lua::try_with_app(|app| app.session.id.clone()).unwrap_or_default();
                let sessions = crate::session::list_sessions();
                let out = lua.create_table()?;
                let mut idx = 1;
                for meta in sessions {
                    if meta.id == current_id {
                        continue;
                    }
                    let row = lua.create_table()?;
                    row.set("id", meta.id)?;
                    row.set("title", meta.title.unwrap_or_default())?;
                    row.set("subtitle", meta.first_user_message.unwrap_or_default())?;
                    row.set("cwd", meta.cwd.unwrap_or_default())?;
                    row.set("parent_id", meta.parent_id.unwrap_or_default())?;
                    row.set("updated_at_ms", meta.updated_at_ms)?;
                    row.set("created_at_ms", meta.created_at_ms)?;
                    if let Some(size) = meta.text_bytes {
                        row.set("size_bytes", size)?;
                    }
                    out.set(idx, row)?;
                    idx += 1;
                }
                Ok(out)
            })?,
        )?;
        session_tbl.set(
            "load",
            lua.create_function(|_, id: String| {
                crate::lua::with_app(|app| app.load_session_by_id(&id));
                Ok(())
            })?,
        )?;
        session_tbl.set(
            "delete",
            lua.create_function(|_, id: String| {
                crate::lua::with_app(|app| {
                    if id != app.session.id {
                        crate::session::delete(&id);
                    }
                });
                Ok(())
            })?,
        )?;
        smelt.set("session", session_tbl)?;

        engine_tbl.set(
            "set_model",
            lua.create_function(|_, v: String| {
                crate::lua::with_app(|app| app.apply_model(&v));
                Ok(())
            })?,
        )?;
        // smelt.engine.models() → array of `{key, name, provider}`
        // for the prompt-docked `/model` picker.
        engine_tbl.set(
            "models",
            lua.create_function(|lua, ()| {
                let out = lua.create_table()?;
                if let Some(res) = crate::lua::try_with_app(|app| -> LuaResult<()> {
                    for (i, m) in app.available_models.iter().enumerate() {
                        let entry = lua.create_table()?;
                        entry.set("key", m.key.clone())?;
                        entry.set("name", m.model_name.clone())?;
                        entry.set("provider", m.provider_name.clone())?;
                        out.set(i + 1, entry)?;
                    }
                    Ok(())
                }) {
                    res?;
                }
                Ok(out)
            })?,
        )?;
        engine_tbl.set(
            "set_mode",
            lua.create_function(|_, v: String| {
                crate::lua::with_app(|app| match protocol::Mode::parse(&v) {
                    Some(mode) => app.set_mode(mode),
                    None => app.notify_error(format!("unknown mode: {v}")),
                });
                Ok(())
            })?,
        )?;
        engine_tbl.set(
            "set_reasoning_effort",
            lua.create_function(|_, v: String| {
                crate::lua::with_app(|app| match protocol::ReasoningEffort::parse(&v) {
                    Some(effort) => app.set_reasoning_effort(effort),
                    None => app.notify_error(format!("unknown reasoning effort: {v}")),
                });
                Ok(())
            })?,
        )?;
        engine_tbl.set(
            "submit",
            lua.create_function(|_, v: String| {
                crate::lua::with_app(|app| app.queued_messages.push(v));
                Ok(())
            })?,
        )?;
        engine_tbl.set(
            "cancel",
            lua.create_function(|_, ()| {
                crate::lua::with_app(|app| app.engine.send(protocol::UiCommand::Cancel));
                Ok(())
            })?,
        )?;
        engine_tbl.set(
            "compact",
            lua.create_function(|_, instructions: Option<String>| {
                crate::lua::with_app(|app| app.compact_or_notify(instructions));
                Ok(())
            })?,
        )?;

        // smelt.engine.ask({ system, messages?, question?, task?, on_response })
        {
            let s = shared.clone();
            engine_tbl.set(
                "ask",
                lua.create_function(move |lua, spec: mlua::Table| {
                    let system: String = spec.get("system")?;
                    let task_str: Option<String> = spec.get("task")?;
                    let task = match task_str.as_deref() {
                        Some("title") => protocol::AuxiliaryTask::Title,
                        Some("prediction") => protocol::AuxiliaryTask::Prediction,
                        Some("compaction") => protocol::AuxiliaryTask::Compaction,
                        Some("btw") | None => protocol::AuxiliaryTask::Btw,
                        Some(other) => {
                            return Err(mlua::Error::external(format!(
                                "engine.ask: unknown task {other:?}; expected one of title / prediction / compaction / btw"
                            )));
                        }
                    };
                    let on_response: Option<mlua::Function> = spec.get("on_response")?;

                    let mut messages = Vec::new();
                    if let Ok(msgs) = spec.get::<mlua::Table>("messages") {
                        for pair in msgs.sequence_values::<mlua::Table>().flatten() {
                            let role: String = pair.get("role")?;
                            let content: String = pair.get("content")?;
                            let msg = match role.as_str() {
                                "user" => {
                                    protocol::Message::user(protocol::Content::text(&content))
                                }
                                "assistant" => protocol::Message::assistant(
                                    Some(protocol::Content::text(&content)),
                                    None,
                                    None,
                                ),
                                _ => continue,
                            };
                            messages.push(msg);
                        }
                    }
                    if let Ok(question) = spec.get::<String>("question") {
                        messages.push(protocol::Message::user(protocol::Content::text(&question)));
                    }

                    let id = s.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                    if let Some(func) = on_response {
                        let key = lua.create_registry_value(func)?;
                        if let Ok(mut cbs) = s.callbacks.lock() {
                            cbs.insert(id, LuaHandle { key });
                        }
                    }

                    crate::lua::with_app(|app| {
                        app.engine.send(protocol::UiCommand::EngineAsk {
                            id,
                            system,
                            messages,
                            task,
                        })
                    });
                    Ok(id)
                })?,
            )?;
        }

        // smelt.engine.history() → [{role, content, tool_calls?, tool_call_id?}]
        engine_tbl.set(
            "history",
            lua.create_function(|lua, ()| {
                let history =
                    crate::lua::try_with_app(|app| app.history.clone()).unwrap_or_default();
                messages_to_lua(lua, &history)
            })?,
        )?;

        smelt.set("engine", engine_tbl)?;

        // smelt.process.*
        let process_tbl = lua.create_table()?;
        {
            let s = shared.clone();
            process_tbl.set(
                "list",
                lua.create_function(move |lua, ()| {
                    let Ok(guard) = s.processes.lock() else {
                        return lua.create_table();
                    };
                    let Some(registry) = guard.as_ref() else {
                        return lua.create_table();
                    };
                    let procs = registry.list();
                    drop(guard);
                    let out = lua.create_table()?;
                    for (i, p) in procs.into_iter().enumerate() {
                        let row = lua.create_table()?;
                        row.set("id", p.id)?;
                        row.set("command", p.command)?;
                        row.set("elapsed_secs", p.started_at.elapsed().as_secs())?;
                        out.set(i + 1, row)?;
                    }
                    Ok(out)
                })?,
            )?;
        }
        process_tbl.set(
            "kill",
            lua.create_function(|_, id: String| {
                crate::lua::with_app(|app| {
                    let registry = app.engine.processes.clone();
                    tokio::spawn(async move {
                        let _ = registry.stop(&id).await;
                    });
                });
                Ok(())
            })?,
        )?;
        {
            let s = shared.clone();
            process_tbl.set(
                "read_output",
                lua.create_function(move |lua, id: String| {
                    let Ok(guard) = s.processes.lock() else {
                        return lua.create_table();
                    };
                    let Some(registry) = guard.as_ref() else {
                        return lua.create_table();
                    };
                    match registry.read(&id) {
                        Ok((text, running, exit_code)) => {
                            let t = lua.create_table()?;
                            t.set("text", text)?;
                            t.set("running", running)?;
                            if let Some(code) = exit_code {
                                t.set("exit_code", code)?;
                            }
                            Ok(t)
                        }
                        Err(_) => lua.create_table(),
                    }
                })?,
            )?;
        }
        smelt.set("process", process_tbl)?;

        // smelt.agent.*
        let agent_tbl = lua.create_table()?;
        agent_tbl.set(
            "list",
            lua.create_function(|lua, ()| {
                let my_pid = std::process::id();
                let entries = engine::registry::children_of(my_pid);
                let out = lua.create_table()?;
                for (i, e) in entries.into_iter().enumerate() {
                    let row = lua.create_table()?;
                    row.set("pid", e.pid)?;
                    row.set("agent_id", e.agent_id)?;
                    row.set("session_id", e.session_id)?;
                    row.set("cwd", e.cwd)?;
                    row.set(
                        "status",
                        match e.status {
                            engine::registry::AgentStatus::Working => "working",
                            engine::registry::AgentStatus::Idle => "idle",
                        },
                    )?;
                    row.set("task_slug", e.task_slug.unwrap_or_default())?;
                    row.set("git_root", e.git_root.unwrap_or_default())?;
                    row.set("git_branch", e.git_branch.unwrap_or_default())?;
                    row.set("depth", e.depth)?;
                    row.set("started_at", e.started_at)?;
                    out.set(i + 1, row)?;
                }
                Ok(out)
            })?,
        )?;
        agent_tbl.set(
            "kill",
            lua.create_function(|_, pid: u32| {
                engine::registry::kill_agent(pid);
                Ok(())
            })?,
        )?;
        {
            let s = shared.clone();
            agent_tbl.set(
                "snapshots",
                lua.create_function(move |lua, ()| {
                    let snaps = {
                        let Ok(guard) = s.agent_snapshots.lock() else {
                            return lua.create_table();
                        };
                        let Some(ref shared_snaps) = *guard else {
                            return lua.create_table();
                        };
                        shared_snaps.lock().map(|v| v.clone()).unwrap_or_default()
                    };
                    let out = lua.create_table()?;
                    for (i, snap) in snaps.into_iter().enumerate() {
                        let row = lua.create_table()?;
                        row.set("agent_id", snap.agent_id)?;
                        row.set("prompt", snap.prompt.as_str())?;
                        row.set("cost_usd", snap.cost_usd)?;
                        if let Some(t) = snap.context_tokens {
                            row.set("context_tokens", t)?;
                        }
                        let calls = lua.create_table()?;
                        for (j, call) in snap.tool_calls.into_iter().enumerate() {
                            let c = lua.create_table()?;
                            c.set("call_id", call.call_id)?;
                            c.set("tool_name", call.tool_name)?;
                            c.set("summary", call.summary)?;
                            c.set(
                                "status",
                                match call.status {
                                    crate::app::transcript_model::ToolStatus::Pending => "pending",
                                    crate::app::transcript_model::ToolStatus::Confirm => "confirm",
                                    crate::app::transcript_model::ToolStatus::Ok => "ok",
                                    crate::app::transcript_model::ToolStatus::Err => "err",
                                    crate::app::transcript_model::ToolStatus::Denied => "denied",
                                },
                            )?;
                            if let Some(d) = call.elapsed {
                                c.set("elapsed_ms", d.as_millis() as u64)?;
                            }
                            calls.set(j + 1, c)?;
                        }
                        row.set("tool_calls", calls)?;
                        out.set(i + 1, row)?;
                    }
                    Ok(out)
                })?,
            )?;
        }

        agent_tbl.set(
            "peek",
            lua.create_function(|lua, (pid, max_lines): (u32, Option<usize>)| {
                let my_pid = std::process::id();
                let entries = engine::registry::children_of(my_pid);
                let Some(entry) = entries.iter().find(|e| e.pid == pid) else {
                    return lua.create_table();
                };
                let session = match crate::session::load(&entry.session_id) {
                    Some(s) => s,
                    None => return lua.create_table(),
                };
                let dir = crate::session::dir_for(&session);
                let lines = engine::registry::read_agent_logs(&dir, pid, max_lines.unwrap_or(200));
                let out = lua.create_table()?;
                for (i, line) in lines.into_iter().enumerate() {
                    out.set(i + 1, line)?;
                }
                Ok(out)
            })?,
        )?;
        smelt.set("agent", agent_tbl)?;

        // smelt.permissions.*
        let permissions_tbl = lua.create_table()?;
        permissions_tbl.set(
            "list",
            lua.create_function(|lua, ()| {
                let (session_entries, cwd) = crate::lua::try_with_app(|app| {
                    let entries = app
                        .session_permission_entries()
                        .into_iter()
                        .map(|e| (e.tool, e.pattern))
                        .collect::<Vec<_>>();
                    (entries, app.cwd.clone())
                })
                .unwrap_or_default();
                let out = lua.create_table()?;
                let session_arr = lua.create_table()?;
                for (i, (tool, pattern)) in session_entries.into_iter().enumerate() {
                    let row = lua.create_table()?;
                    row.set("tool", tool)?;
                    row.set("pattern", pattern)?;
                    session_arr.set(i + 1, row)?;
                }
                out.set("session", session_arr)?;
                let workspace_arr = lua.create_table()?;
                for (i, rule) in crate::workspace_permissions::load(&cwd)
                    .into_iter()
                    .enumerate()
                {
                    let row = lua.create_table()?;
                    row.set("tool", rule.tool)?;
                    let pats = lua.create_table()?;
                    for (j, p) in rule.patterns.into_iter().enumerate() {
                        pats.set(j + 1, p)?;
                    }
                    row.set("patterns", pats)?;
                    workspace_arr.set(i + 1, row)?;
                }
                out.set("workspace", workspace_arr)?;
                Ok(out)
            })?,
        )?;
        permissions_tbl.set(
            "sync",
            lua.create_function(|_, spec: mlua::Table| {
                let mut session_entries: Vec<crate::app::transcript_model::PermissionEntry> =
                    Vec::new();
                if let Ok(arr) = spec.get::<mlua::Table>("session") {
                    for row in arr.sequence_values::<mlua::Table>().flatten() {
                        let tool: String = row.get("tool").unwrap_or_default();
                        let pattern: String = row.get("pattern").unwrap_or_default();
                        session_entries
                            .push(crate::app::transcript_model::PermissionEntry { tool, pattern });
                    }
                }
                let mut workspace_rules: Vec<crate::workspace_permissions::Rule> = Vec::new();
                if let Ok(arr) = spec.get::<mlua::Table>("workspace") {
                    for row in arr.sequence_values::<mlua::Table>().flatten() {
                        let tool: String = row.get("tool").unwrap_or_default();
                        let mut patterns: Vec<String> = Vec::new();
                        if let Ok(pats) = row.get::<mlua::Table>("patterns") {
                            for p in pats.sequence_values::<String>().flatten() {
                                patterns.push(p);
                            }
                        }
                        workspace_rules.push(crate::workspace_permissions::Rule { tool, patterns });
                    }
                }
                crate::lua::with_app(|app| app.sync_permissions(session_entries, workspace_rules));
                Ok(())
            })?,
        )?;
        smelt.set("permissions", permissions_tbl)?;

        // smelt.keymap.help_sections()
        let keymap_tbl = lua.create_table()?;
        keymap_tbl.set(
            "help_sections",
            lua.create_function(|lua, ()| {
                let vim_enabled =
                    crate::lua::try_with_app(|app| app.input.vim_enabled()).unwrap_or(false);
                let sections = crate::keymap::hints::help_sections(vim_enabled);
                let out = lua.create_table()?;
                for (i, (title, entries)) in sections.into_iter().enumerate() {
                    let row = lua.create_table()?;
                    row.set("title", title)?;
                    let entries_tbl = lua.create_table()?;
                    for (j, (label, detail)) in entries.into_iter().enumerate() {
                        let entry = lua.create_table()?;
                        entry.set("label", label)?;
                        entry.set("detail", detail)?;
                        entries_tbl.set(j + 1, entry)?;
                    }
                    row.set("entries", entries_tbl)?;
                    out.set(i + 1, row)?;
                }
                Ok(out)
            })?,
        )?;
        smelt_keymap.set("help", keymap_tbl.get::<mlua::Function>("help_sections")?)?;

        // smelt.ui.ghost_text.{set, clear}
        let ghost_text_tbl = lua.create_table()?;
        ghost_text_tbl.set(
            "set",
            lua.create_function(|_, text: String| {
                crate::lua::with_app(|app| app.input_prediction = Some(text));
                Ok(())
            })?,
        )?;
        ghost_text_tbl.set(
            "clear",
            lua.create_function(|_, ()| {
                crate::lua::with_app(|app| app.input_prediction = None);
                Ok(())
            })?,
        )?;
        smelt_ui.set("ghost_text", ghost_text_tbl)?;

        // smelt.ui.spinner — same glyph set and cadence the status bar
        // uses for its "working" pill, exposed as primitives so Lua
        // plugins (e.g. /btw's "thinking" placeholder) can animate in
        // lockstep with the rest of the UI. Lua drives the animation
        // via `smelt.defer(period_ms, tick)`; `glyph()` returns the
        // current frame without any server-side state.
        let spinner_tbl = lua.create_table()?;
        spinner_tbl.set(
            "glyph",
            lua.create_function(|_, ()| Ok(crate::render::spinner_glyph()))?,
        )?;
        spinner_tbl.set(
            "period_ms",
            lua.create_function(|_, ()| Ok(crate::render::SPINNER_FRAME_MS))?,
        )?;
        smelt_ui.set("spinner", spinner_tbl)?;

        // smelt.notify / smelt.notify_error (top-level convenience).
        smelt.set(
            "notify",
            lua.create_function(|_, msg: String| {
                crate::lua::with_app(|app| app.notify(msg));
                Ok(())
            })?,
        )?;
        smelt.set(
            "notify_error",
            lua.create_function(|_, msg: String| {
                crate::lua::with_app(|app| app.notify_error(msg));
                Ok(())
            })?,
        )?;

        // smelt.theme
        let theme_tbl = lua.create_table()?;
        theme_tbl.set(
            "accent",
            lua.create_function(|lua, ()| color_to_lua(lua, crate::theme::accent()))?,
        )?;
        theme_tbl.set(
            "get",
            lua.create_function(|lua, role: String| {
                let color = theme_role_get(&role)
                    .ok_or_else(|| LuaError::RuntimeError(format!("unknown theme role: {role}")))?;
                color_to_lua(lua, color)
            })?,
        )?;
        theme_tbl.set(
            "set",
            lua.create_function(|_, (role, value): (String, mlua::Table)| {
                let ansi = color_ansi_from_lua(&value)?;
                theme_role_set(&role, ansi)
            })?,
        )?;
        theme_tbl.set(
            "snapshot",
            lua.create_function(|lua, ()| {
                let t = lua.create_table()?;
                for (name, color) in theme_snapshot_pairs() {
                    t.set(name, color_to_lua(lua, color)?)?;
                }
                Ok(t)
            })?,
        )?;
        theme_tbl.set(
            "is_light",
            lua.create_function(|_, ()| Ok(crate::theme::is_light()))?,
        )?;
        // Built-in color presets (name, description, ANSI-256 value).
        // Exposed so Lua-side pickers (`/theme`, `/color`) can use
        // them instead of hard-coding the list.
        theme_tbl.set(
            "presets",
            lua.create_function(|lua, ()| {
                let list = lua.create_table()?;
                for (i, (name, detail, ansi)) in crate::theme::PRESETS.iter().enumerate() {
                    let entry = lua.create_table()?;
                    entry.set("name", *name)?;
                    entry.set("detail", *detail)?;
                    entry.set("ansi", *ansi)?;
                    list.set(i + 1, entry)?;
                }
                Ok(list)
            })?,
        )?;
        smelt.set("theme", theme_tbl)?;

        // smelt.buf
        let buf_tbl = lua.create_table()?;
        buf_tbl.set(
            "text",
            app_read!(lua, |app| app.input.win.edit_buf.buf.clone()),
        )?;
        {
            let s = shared.clone();
            buf_tbl.set(
                "create",
                lua.create_function(move |_, opts: Option<mlua::Table>| {
                    let format = match opts.as_ref() {
                        Some(t) => match t.get::<Option<String>>("mode")? {
                            Some(mode) => {
                                Some(crate::format::BufFormat::from_lua_spec(&mode, t).map_err(
                                    |e| LuaError::RuntimeError(format!("buf.create: {e}")),
                                )?)
                            }
                            None => None,
                        },
                        None => None,
                    };
                    let id = s
                        .next_buf_id
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    crate::lua::with_app(|app| {
                        match app.ui.buf_create_with_id(
                            ui::BufId(id),
                            ui::buffer::BufCreateOpts {
                                buftype: ui::buffer::BufType::Scratch,
                                ..Default::default()
                            },
                        ) {
                            Ok(bid) => {
                                if let Some(fmt) = format {
                                    if let Some(buf) = app.ui.buf_mut(bid) {
                                        buf.set_formatter(fmt.into_formatter());
                                    }
                                }
                            }
                            Err(clash) => {
                                app.notify_error(format!(
                                    "buf.create: id {} already in use",
                                    clash.0
                                ));
                            }
                        }
                    });
                    Ok(id)
                })?,
            )?;
        }
        buf_tbl.set(
            "set_lines",
            lua.create_function(|_, (id, lines): (u64, mlua::Table)| {
                let lines: Vec<String> = lines
                    .sequence_values::<String>()
                    .filter_map(|v| v.ok())
                    .collect();
                crate::lua::with_app(|app| {
                    if let Some(buf) = app.ui.buf_mut(ui::BufId(id)) {
                        buf.set_all_lines(lines);
                    }
                });
                Ok(())
            })?,
        )?;
        buf_tbl.set(
            "set_source",
            lua.create_function(|_, (id, source): (u64, String)| {
                crate::lua::with_app(|app| {
                    if let Some(buf) = app.ui.buf_mut(ui::BufId(id)) {
                        buf.set_source(source);
                    }
                });
                Ok(())
            })?,
        )?;
        buf_tbl.set(
            "add_highlight",
            lua.create_function(
                |_,
                 (id, line, col_start, col_end, style): (
                    u64,
                    u64,
                    u64,
                    u64,
                    Option<mlua::Table>,
                )| {
                    let Some(line0) = line.checked_sub(1) else {
                        return Ok(());
                    };
                    if col_end <= col_start {
                        return Ok(());
                    }
                    let (fg, bold, italic, dim) = match style {
                        Some(t) => {
                            let fg = match t.get::<Option<String>>("fg").ok().flatten() {
                                Some(role) => Some(theme_role_get(&role).ok_or_else(|| {
                                    LuaError::RuntimeError(format!("unknown theme role: {role}"))
                                })?),
                                None => None,
                            };
                            (
                                fg,
                                t.get::<bool>("bold").unwrap_or(false),
                                t.get::<bool>("italic").unwrap_or(false),
                                t.get::<bool>("dim").unwrap_or(false),
                            )
                        }
                        None => (None, false, false, false),
                    };
                    crate::lua::with_app(|app| {
                        if let Some(buf) = app.ui.buf_mut(ui::BufId(id)) {
                            if (line0 as usize) < buf.line_count() {
                                buf.add_highlight(
                                    line0 as usize,
                                    col_start.min(u16::MAX as u64) as u16,
                                    col_end.min(u16::MAX as u64) as u16,
                                    ui::buffer::SpanStyle {
                                        fg,
                                        bg: None,
                                        bold,
                                        dim,
                                        italic,
                                    },
                                );
                            }
                        }
                    });
                    Ok(())
                },
            )?,
        )?;
        buf_tbl.set(
            "add_dim",
            lua.create_function(|_, (id, line, col_start, col_end): (u64, u64, u64, u64)| {
                let Some(line0) = line.checked_sub(1) else {
                    return Ok(());
                };
                if col_end <= col_start {
                    return Ok(());
                }
                crate::lua::with_app(|app| {
                    if let Some(buf) = app.ui.buf_mut(ui::BufId(id)) {
                        if (line0 as usize) < buf.line_count() {
                            buf.add_highlight(
                                line0 as usize,
                                col_start.min(u16::MAX as u64) as u16,
                                col_end.min(u16::MAX as u64) as u16,
                                ui::buffer::SpanStyle {
                                    fg: None,
                                    bg: None,
                                    bold: false,
                                    dim: true,
                                    italic: false,
                                },
                            );
                        }
                    }
                });
                Ok(())
            })?,
        )?;
        smelt.set("buf", buf_tbl)?;

        // smelt.win
        let win_tbl = lua.create_table()?;
        win_tbl.set(
            "focus",
            app_read!(lua, |app| match app.app_focus {
                crate::app::AppFocus::Content => "transcript".to_string(),
                crate::app::AppFocus::Prompt => "prompt".to_string(),
            }),
        )?;
        win_tbl.set(
            "mode",
            app_read!(lua, |app| match app.app_focus {
                crate::app::AppFocus::Content => app
                    .transcript_window
                    .vim
                    .as_ref()
                    .map(|v| format!("{:?}", v.mode()))
                    .unwrap_or_default(),
                crate::app::AppFocus::Prompt => app
                    .input
                    .vim_mode()
                    .map(|m| format!("{m:?}"))
                    .unwrap_or_default(),
            }),
        )?;
        win_tbl.set(
            "close",
            lua.create_function(|_, id: u64| {
                crate::lua::with_app(|app| {
                    app.close_float(ui::WinId(id));
                });
                Ok(())
            })?,
        )?;
        {
            let s = shared.clone();
            win_tbl.set(
                "set_keymap",
                lua.create_function(
                    move |lua, (win_id, key_str, func): (u64, String, mlua::Function)| {
                        let Some(key) = parse_keybind(&key_str) else {
                            return Err(mlua::Error::RuntimeError(format!(
                                "win.set_keymap: unknown key `{key_str}`"
                            )));
                        };
                        let id = crate::lua::register_callback_handle(&s, lua, func)?;
                        crate::lua::with_app(|app| {
                            let prev = app.ui.win_set_keymap(
                                ui::WinId(win_id),
                                key,
                                ui::Callback::Lua(ui::LuaHandle(id)),
                            );
                            crate::lua::drop_displaced_lua_handle(app, prev);
                        });
                        Ok(())
                    },
                )?,
            )?;
        }
        {
            let s = shared.clone();
            win_tbl.set(
                "on_event",
                lua.create_function(
                    move |lua, (win_id, ev_str, func): (u64, String, mlua::Function)| {
                        let Some(event) = parse_win_event(&ev_str) else {
                            return Err(mlua::Error::RuntimeError(format!(
                                "win.on_event: unknown event `{ev_str}`"
                            )));
                        };
                        let id = crate::lua::register_callback_handle(&s, lua, func)?;
                        crate::lua::with_app(|app| {
                            app.ui.win_on_event(
                                ui::WinId(win_id),
                                event,
                                ui::Callback::Lua(ui::LuaHandle(id)),
                            );
                        });
                        Ok(id)
                    },
                )?,
            )?;
        }
        win_tbl.set(
            "clear_keymap",
            lua.create_function(|_, (win_id, key_str): (u64, String)| {
                let Some(key) = parse_keybind(&key_str) else {
                    return Err(mlua::Error::RuntimeError(format!(
                        "win.clear_keymap: unknown key `{key_str}`"
                    )));
                };
                crate::lua::with_app(|app| {
                    let prev = app.ui.win_clear_keymap(ui::WinId(win_id), key);
                    crate::lua::drop_displaced_lua_handle(app, prev);
                });
                Ok(())
            })?,
        )?;
        win_tbl.set(
            "clear_event",
            lua.create_function(|_, (win_id, ev_str, callback_id): (u64, String, u64)| {
                let Some(event) = parse_win_event(&ev_str) else {
                    return Err(mlua::Error::RuntimeError(format!(
                        "win.clear_event: unknown event `{ev_str}`"
                    )));
                };
                crate::lua::with_app(|app| {
                    let prev = app
                        .ui
                        .win_clear_event_by_id(ui::WinId(win_id), event, callback_id);
                    crate::lua::drop_displaced_lua_handle(app, prev);
                });
                Ok(())
            })?,
        )?;
        smelt.set("win", win_tbl)?;

        // smelt.settings — user preference booleans (vim, auto-compact,
        // etc.). `snapshot()` returns the current state as a table;
        // `toggle(key)` flips one by name. Used by `/settings` to build
        // its picker entirely in Lua.
        {
            let settings_tbl = lua.create_table()?;
            settings_tbl.set(
                "snapshot",
                lua.create_function(|lua, ()| {
                    let t = lua.create_table()?;
                    if let Some(res) = crate::lua::try_with_app(|app| -> LuaResult<()> {
                        let s = app.settings_state();
                        t.set("vim", s.vim)?;
                        t.set("auto_compact", s.auto_compact)?;
                        t.set("show_tps", s.show_tps)?;
                        t.set("show_tokens", s.show_tokens)?;
                        t.set("show_cost", s.show_cost)?;
                        t.set("show_prediction", s.show_prediction)?;
                        t.set("show_slug", s.show_slug)?;
                        t.set("show_thinking", s.show_thinking)?;
                        t.set("restrict_to_workspace", s.restrict_to_workspace)?;
                        t.set("redact_secrets", s.redact_secrets)?;
                        Ok(())
                    }) {
                        res?;
                    }
                    Ok(t)
                })?,
            )?;
            settings_tbl.set(
                "toggle",
                lua.create_function(|_, v: String| {
                    crate::lua::with_app(|app| app.toggle_named_setting(&v));
                    Ok(())
                })?,
            )?;
            smelt.set("settings", settings_tbl)?;
        }

        // smelt.task
        {
            let task_tbl = lua.create_table()?;
            {
                let s = shared.clone();
                task_tbl.set(
                    "alloc",
                    lua.create_function(move |_, ()| {
                        Ok(s.next_external_id.fetch_add(1, Ordering::Relaxed))
                    })?,
                )?;
            }
            {
                let s = shared.clone();
                task_tbl.set(
                    "resume",
                    lua.create_function(move |lua, (id, value): (u64, mlua::Value)| {
                        let key = lua.create_registry_value(value)?;
                        if let Ok(mut inbox) = s.task_inbox.lock() {
                            inbox.push(TaskEvent::ExternalResolved {
                                external_id: id,
                                value: key,
                            });
                        }
                        Ok(())
                    })?,
                )?;
            }
            smelt.set("task", task_tbl)?;
        }

        // smelt.prompt — the main editable input surface.
        //
        // `win_id()` returns the stable `WinId` so plugins can reuse
        // `smelt.win.on_event(prompt, "text_changed", …)` and
        // `smelt.win.set_keymap(prompt, …)`. `text()` snapshots the
        // current buffer; `set_text(s)` replaces it.
        let prompt_tbl = lua.create_table()?;
        prompt_tbl.set("win_id", lua.create_function(|_, ()| Ok(ui::PROMPT_WIN.0))?)?;
        prompt_tbl.set(
            "text",
            app_read!(lua, |app| app.input.win.edit_buf.buf.clone()),
        )?;
        prompt_tbl.set(
            "set_text",
            lua.create_function(|_, text: String| {
                crate::lua::with_app(|app| {
                    crate::api::buf::replace(&mut app.input, text, None);
                });
                Ok(())
            })?,
        )?;
        prompt_tbl.set(
            "set_section",
            lua.create_function(|_, (name, content): (String, String)| {
                crate::lua::with_app(|app| app.prompt_sections.set(&name, content));
                Ok(())
            })?,
        )?;
        prompt_tbl.set(
            "remove_section",
            lua.create_function(|_, name: String| {
                crate::lua::with_app(|app| app.prompt_sections.remove(&name));
                Ok(())
            })?,
        )?;
        smelt.set("prompt", prompt_tbl)?;

        // smelt.tools.register(def) / unregister(name) / resolve(...)
        let tools_tbl = lua.create_table()?;
        let s = shared.clone();
        let tools_register = lua.create_function(move |lua, def: mlua::Table| {
            let name: String = def.get("name")?;
            let handler: mlua::Function = def.get("execute")?;
            let key = lua.create_registry_value(handler)?;

            let meta = lua.create_table()?;
            let desc: String = def.get("description").unwrap_or_default();
            meta.set("description", desc)?;
            if let Ok(params) = def.get::<mlua::Table>("parameters") {
                if let Ok(json_str) = serde_json::to_string(&lua_table_to_json(lua, &params)) {
                    meta.set("parameters_json", json_str)?;
                }
            }
            if let Ok(modes) = def.get::<mlua::Table>("modes") {
                meta.set("modes", modes)?;
            }
            if let Ok(mode_str) = def.get::<String>("execution_mode") {
                meta.set("execution_mode", mode_str)?;
            }
            lua.set_named_registry_value(&format!("__pt_meta_{name}"), meta)?;

            if let Ok(mut map) = s.plugin_tools.lock() {
                map.insert(name, LuaHandle { key });
            }
            Ok(())
        })?;
        tools_tbl.set("register", tools_register)?;
        {
            let s = shared.clone();
            tools_tbl.set(
                "unregister",
                lua.create_function(move |_, name: String| {
                    if let Ok(mut map) = s.plugin_tools.lock() {
                        map.remove(&name);
                    }
                    Ok(())
                })?,
            )?;
        }
        tools_tbl.set(
            "resolve",
            lua.create_function(
                |_, (request_id, call_id, result): (u64, String, mlua::Table)| {
                    let content: String = result.get("content").unwrap_or_default();
                    let is_error: bool = result.get("is_error").unwrap_or(false);
                    crate::lua::with_app(|app| {
                        app.engine.send(protocol::UiCommand::PluginToolResult {
                            request_id,
                            call_id,
                            content,
                            is_error,
                        })
                    });
                    Ok(())
                },
            )?,
        )?;
        smelt.set("tools", tools_tbl)?;

        // smelt.fuzzy.score
        let fuzzy_tbl = lua.create_table()?;
        fuzzy_tbl.set(
            "score",
            lua.create_function(
                |_, (text, query): (String, String)| match crate::fuzzy::fuzzy_score(&text, &query)
                {
                    Some(s) => Ok(Some(s)),
                    None => Ok(None),
                },
            )?,
        )?;
        smelt.set("fuzzy", fuzzy_tbl)?;

        // smelt.history — past submitted prompts.
        //   entries()      → array of strings (oldest first)
        //   search(query)  → [{index, score}] ranked by the
        //                    history-specific scorer (word-match boosts,
        //                    recency bonus). 1-based index into entries().
        {
            let history_tbl = lua.create_table()?;
            history_tbl.set(
                "entries",
                lua.create_function(|lua, ()| {
                    let entries =
                        crate::lua::try_with_app(|app| app.input_history.entries().to_vec())
                            .unwrap_or_default();
                    let out = lua.create_table()?;
                    for (i, entry) in entries.into_iter().enumerate() {
                        out.set(i + 1, entry)?;
                    }
                    Ok(out)
                })?,
            )?;
            history_tbl.set(
                "search",
                lua.create_function(|lua, query: String| {
                    let entries =
                        crate::lua::try_with_app(|app| app.input_history.entries().to_vec())
                            .unwrap_or_default();
                    // Oldest first in the vec; the scorer wants
                    // newest-first so "recent" ranks highest. Iterate
                    // reversed and dedupe to match the old
                    // `Completer::history` construction.
                    let mut seen = std::collections::HashSet::new();
                    let mut scored: Vec<(u32, usize, usize)> = Vec::new();
                    for (rank, (orig_idx, entry)) in entries.iter().enumerate().rev().enumerate() {
                        if !seen.insert(entry.as_str()) {
                            continue;
                        }
                        let label = entry
                            .trim_start()
                            .lines()
                            .map(str::trim)
                            .find(|l| !l.is_empty())
                            .unwrap_or("");
                        if let Some(s) =
                            crate::completer::history::history_score(label, &query, rank)
                        {
                            scored.push((s, rank, orig_idx));
                        }
                    }
                    scored.sort_by_key(|(s, rank, _)| (*s, *rank));
                    let out = lua.create_table()?;
                    for (i, (score, _rank, orig_idx)) in scored.into_iter().enumerate() {
                        let entry = lua.create_table()?;
                        entry.set("index", orig_idx + 1)?;
                        entry.set("score", score)?;
                        out.set(i + 1, entry)?;
                    }
                    Ok(out)
                })?,
            )?;
            smelt.set("history", history_tbl)?;
        }

        // smelt.ui.picker
        {
            let picker_tbl = lua.create_table()?;
            picker_tbl.set(
                "set_selected",
                lua.create_function(|_, (win_id, idx): (u64, i64)| {
                    let index = if idx < 0 { 0 } else { idx as usize };
                    crate::lua::with_app(|app| {
                        if let Some(p) = app.ui.picker_mut(ui::WinId(win_id)) {
                            p.set_selected(index);
                        }
                    });
                    Ok(())
                })?,
            )?;
            picker_tbl.set(
                "_open",
                lua.create_function(|_, opts: mlua::Table| -> LuaResult<u64> {
                    let win_id =
                        crate::lua::with_app(|app| crate::lua::ui_ops::open_picker(app, opts))
                            .map_err(|e| LuaError::RuntimeError(format!("picker.open: {e}")))?;
                    Ok(win_id.0)
                })?,
            )?;
            picker_tbl.set(
                "set_items",
                lua.create_function(|_, (win_id, items_tbl): (u64, mlua::Table)| {
                    let mut items = Vec::new();
                    for pair in items_tbl.sequence_values::<mlua::Value>() {
                        let v = pair?;
                        let it = crate::lua::ui_ops::parse_picker_item(&v)
                            .map_err(LuaError::RuntimeError)?;
                        items.push(it);
                    }
                    crate::lua::with_app(|app| {
                        if let Some(p) = app.ui.picker_mut(ui::WinId(win_id)) {
                            p.set_items(items);
                        }
                    });
                    Ok(())
                })?,
            )?;
            smelt_ui.set("picker", picker_tbl)?;
        }

        // smelt.ui.dialog
        {
            let dialog_tbl = lua.create_table()?;
            dialog_tbl.set(
                "_open",
                lua.create_function(|_, opts: mlua::Table| -> LuaResult<u64> {
                    let win_id =
                        crate::lua::with_app(|app| crate::lua::ui_ops::open_dialog(app, opts))
                            .map_err(|e| LuaError::RuntimeError(format!("dialog.open: {e}")))?;
                    Ok(win_id.0)
                })?,
            )?;
            smelt_ui.set("dialog", dialog_tbl)?;
        }

        smelt.set("ui", smelt_ui)?;

        // smelt.confirm.* primitives consumed by confirm.lua.
        crate::lua::confirm_ops::register(lua, &smelt)?;

        smelt.set(
            "clipboard",
            lua.create_function(|_, text: String| {
                crate::app::commands::copy_to_clipboard(&text).map_err(LuaError::RuntimeError)?;
                Ok(())
            })?,
        )?;

        {
            let s = shared.clone();
            smelt_keymap.set(
                "set",
                lua.create_function(
                    move |lua, (mode, chord, handler): (String, String, mlua::Function)| {
                        // Canonicalize at registration so `"normal"` / `"n"`
                        // / `""` and `"c-r"` / `"<C-r>"` / `"<c-r>"` all
                        // land as the same lookup key as the dispatcher
                        // produces from a crossterm KeyEvent. Unknown
                        // mode or chord → raise immediately, not silent
                        // miss at dispatch.
                        let canonical_mode = crate::lua::normalize_mode(&mode).ok_or_else(
                            || {
                                LuaError::RuntimeError(format!(
                                    "keymap.set: unknown mode `{mode}` (expected \"n\"|\"i\"|\"v\"|\"\" or \"normal\"|\"insert\"|\"visual\")"
                                ))
                            },
                        )?;
                        let canonical_chord = crate::lua::canonicalize_chord(&chord)
                            .ok_or_else(|| {
                                LuaError::RuntimeError(format!(
                                    "keymap.set: unknown chord `{chord}`"
                                ))
                            })?;
                        let key = lua.create_registry_value(handler)?;
                        if let Ok(mut map) = s.keymaps.lock() {
                            map.insert(
                                (canonical_mode, canonical_chord),
                                LuaHandle { key },
                            );
                        }
                        Ok(())
                    },
                )?,
            )?;
        }
        smelt.set("keymap", smelt_keymap)?;

        {
            let s = shared.clone();
            smelt.set(
                "on",
                lua.create_function(move |lua, (event, handler): (String, mlua::Function)| {
                    let Some(kind) = AutocmdEvent::from_lua_name(&event) else {
                        return Err(LuaError::RuntimeError(format!("unknown event: {event}")));
                    };
                    let key = lua.create_registry_value(handler)?;
                    if let Ok(mut map) = s.autocmds.lock() {
                        map.entry(kind).or_default().push(LuaHandle { key });
                    }
                    Ok(())
                })?,
            )?;
        }

        {
            let s = shared.clone();
            smelt.set(
                "defer",
                lua.create_function(move |lua, (ms, handler): (u64, mlua::Function)| {
                    let key = lua.create_registry_value(handler)?;
                    if let Ok(mut q) = s.timers.lock() {
                        q.push((
                            Instant::now() + Duration::from_millis(ms),
                            LuaHandle { key },
                        ));
                    }
                    Ok(())
                })?,
            )?;
        }

        // smelt.statusline.{register, unregister}
        {
            let statusline_tbl = lua.create_table()?;
            {
                let s = shared.clone();
                statusline_tbl.set(
                    "register",
                    lua.create_function(
                        move |lua,
                              (name, handler, opts): (
                            String,
                            mlua::Function,
                            Option<mlua::Table>,
                        )| {
                            let default_align_right = opts
                                .as_ref()
                                .and_then(|t| t.get::<Option<String>>("align").ok().flatten())
                                .map(|s| s == "right")
                                .unwrap_or(false);
                            let key = lua.create_registry_value(handler)?;
                            let source = crate::lua::StatusSource {
                                handle: LuaHandle { key },
                                default_align_right,
                            };
                            if let Ok(mut sources) = s.statusline_sources.lock() {
                                if let Some(existing) = sources.iter_mut().find(|(n, _)| n == &name)
                                {
                                    existing.1 = source;
                                } else {
                                    sources.push((name, source));
                                }
                            }
                            Ok(())
                        },
                    )?,
                )?;
            }
            {
                let s = shared.clone();
                statusline_tbl.set(
                    "unregister",
                    lua.create_function(move |_, name: String| {
                        if let Ok(mut sources) = s.statusline_sources.lock() {
                            sources.retain(|(n, _)| n != &name);
                        }
                        Ok(())
                    })?,
                )?;
            }
            smelt.set("statusline", statusline_tbl)?;
        }

        {
            let s = shared.clone();
            smelt.set(
                "spawn",
                lua.create_function(move |lua, handler: mlua::Function| {
                    if let Ok(mut rt) = s.tasks.lock() {
                        rt.spawn(lua, handler, LuaValue::Nil, TaskCompletion::FireAndForget)?;
                    }
                    Ok(())
                })?,
            )?;
        }

        lua.globals().set("smelt", smelt)?;

        super::load_bootstrap_chunks(lua)?;

        Ok(())
    }
}

// ── theme + color helpers (called only from register_api) ─────────────

/// Encode a `crossterm::style::Color` as a Lua table.
///
/// Shapes: `{ ansi = u8 }` for palette colors, `{ rgb = { r, g, b } }`
/// for truecolor, `{ named = "red" }` for the 16 legacy names.
fn color_to_lua(lua: &Lua, color: crossterm::style::Color) -> LuaResult<mlua::Table> {
    use crossterm::style::Color;
    let t = lua.create_table()?;
    match color {
        Color::AnsiValue(v) => t.set("ansi", v)?,
        Color::Rgb { r, g, b } => {
            let rgb = lua.create_table()?;
            rgb.set("r", r)?;
            rgb.set("g", g)?;
            rgb.set("b", b)?;
            t.set("rgb", rgb)?;
        }
        Color::Reset => t.set("named", "reset")?,
        Color::Black => t.set("named", "black")?,
        Color::DarkGrey => t.set("named", "dark_grey")?,
        Color::Red => t.set("named", "red")?,
        Color::DarkRed => t.set("named", "dark_red")?,
        Color::Green => t.set("named", "green")?,
        Color::DarkGreen => t.set("named", "dark_green")?,
        Color::Yellow => t.set("named", "yellow")?,
        Color::DarkYellow => t.set("named", "dark_yellow")?,
        Color::Blue => t.set("named", "blue")?,
        Color::DarkBlue => t.set("named", "dark_blue")?,
        Color::Magenta => t.set("named", "magenta")?,
        Color::DarkMagenta => t.set("named", "dark_magenta")?,
        Color::Cyan => t.set("named", "cyan")?,
        Color::DarkCyan => t.set("named", "dark_cyan")?,
        Color::White => t.set("named", "white")?,
        Color::Grey => t.set("named", "grey")?,
    }
    Ok(t)
}

/// Decode a Lua color table to an ANSI palette index. Accepts
/// `{ ansi = u8 }`, `{ preset = "name" }`, or `{ rgb = { r, g, b } }`
/// (rgb is down-sampled via the nearest-palette approximation).
fn color_ansi_from_lua(table: &mlua::Table) -> LuaResult<u8> {
    if let Ok(v) = table.get::<u8>("ansi") {
        return Ok(v);
    }
    if let Ok(name) = table.get::<String>("preset") {
        return crate::theme::preset_by_name(&name)
            .ok_or_else(|| LuaError::RuntimeError(format!("unknown preset: {name}")));
    }
    if let Ok(rgb) = table.get::<mlua::Table>("rgb") {
        let r: u8 = rgb.get("r")?;
        let g: u8 = rgb.get("g")?;
        let b: u8 = rgb.get("b")?;
        return Ok(rgb_to_ansi_256(r, g, b));
    }
    Err(LuaError::RuntimeError(
        "color table must have one of: ansi, preset, rgb".into(),
    ))
}

/// Nearest 6×6×6 palette index for an sRGB triple.
fn rgb_to_ansi_256(r: u8, g: u8, b: u8) -> u8 {
    fn band(c: u8) -> u8 {
        let levels = [0u8, 95, 135, 175, 215, 255];
        levels
            .iter()
            .enumerate()
            .min_by_key(|(_, v)| (c as i32 - **v as i32).abs())
            .map(|(i, _)| i as u8)
            .unwrap_or(0)
    }
    16 + 36 * band(r) + 6 * band(g) + band(b)
}

/// Read a named theme role. Returns `None` for unknown names.
fn theme_role_get(role: &str) -> Option<crossterm::style::Color> {
    use crate::theme;
    Some(match role {
        "accent" => theme::accent(),
        "slug" => theme::slug_color(),
        "user_bg" => theme::user_bg(),
        "code_block_bg" => theme::code_block_bg(),
        "bar" => theme::bar(),
        "tool_pending" => theme::tool_pending(),
        "reason_off" => theme::reason_off(),
        "muted" => theme::muted(),
        "agent" => theme::AGENT,
        _ => return None,
    })
}

/// Set a writable theme role. Only `accent` and `slug` are mutable.
fn theme_role_set(role: &str, ansi: u8) -> LuaResult<()> {
    use crate::theme;
    match role {
        "accent" => {
            theme::set_accent(ansi);
            Ok(())
        }
        "slug" => {
            theme::set_slug_color(ansi);
            Ok(())
        }
        other => Err(LuaError::RuntimeError(format!(
            "theme role is read-only: {other}"
        ))),
    }
}

/// List of (role_name, current_color) pairs for `theme.snapshot()`.
fn theme_snapshot_pairs() -> Vec<(&'static str, crossterm::style::Color)> {
    use crate::theme;
    vec![
        ("accent", theme::accent()),
        ("slug", theme::slug_color()),
        ("user_bg", theme::user_bg()),
        ("code_block_bg", theme::code_block_bg()),
        ("bar", theme::bar()),
        ("tool_pending", theme::tool_pending()),
        ("reason_off", theme::reason_off()),
        ("muted", theme::muted()),
        ("agent", theme::AGENT),
    ]
}

/// Convert a Lua table to a `serde_json::Value`. Tables with contiguous
/// 1..N integer keys become JSON arrays; anything else becomes an object.
pub(super) fn lua_table_to_json(lua: &Lua, table: &mlua::Table) -> serde_json::Value {
    let mut pairs: Vec<(mlua::Value, mlua::Value)> = Vec::new();
    for pair in table.pairs::<mlua::Value, mlua::Value>() {
        let Ok(kv) = pair else { continue };
        pairs.push(kv);
    }

    let is_array = !pairs.is_empty()
        && pairs
            .iter()
            .all(|(k, _)| matches!(k, mlua::Value::Integer(_)))
        && {
            let mut ints: Vec<i64> = pairs
                .iter()
                .filter_map(|(k, _)| match k {
                    mlua::Value::Integer(i) => Some(*i),
                    _ => None,
                })
                .collect();
            ints.sort_unstable();
            ints.first().copied() == Some(1) && ints.windows(2).all(|w| w[1] == w[0] + 1)
        };

    if is_array || pairs.is_empty() {
        let len = table.raw_len();
        let mut arr = Vec::with_capacity(len);
        for i in 1..=len {
            let val: mlua::Value = table.raw_get(i).unwrap_or(mlua::Value::Nil);
            arr.push(lua_value_to_json(lua, &val));
        }
        serde_json::Value::Array(arr)
    } else {
        let mut map = serde_json::Map::new();
        for (key, val) in pairs {
            let key_str = match &key {
                mlua::Value::String(s) => s.to_string_lossy().to_string(),
                mlua::Value::Integer(i) => i.to_string(),
                _ => continue,
            };
            map.insert(key_str, lua_value_to_json(lua, &val));
        }
        serde_json::Value::Object(map)
    }
}

fn lua_value_to_json(lua: &Lua, val: &mlua::Value) -> serde_json::Value {
    match val {
        mlua::Value::Nil => serde_json::Value::Null,
        mlua::Value::Boolean(b) => serde_json::Value::Bool(*b),
        mlua::Value::Integer(i) => serde_json::json!(*i),
        mlua::Value::Number(n) => serde_json::json!(*n),
        mlua::Value::String(s) => serde_json::Value::String(s.to_string_lossy().to_string()),
        mlua::Value::Table(t) => lua_table_to_json(lua, t),
        _ => serde_json::Value::Null,
    }
}
