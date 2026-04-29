//! Live engine + session reads. Each table here is a thin getter
//! surface over `App` — calls go through `try_with_app` for reads and
//! `with_app` for writes, never through a snapshot mirror.

use super::app_read;
use crate::lua::{messages_to_lua, LuaHandle, LuaShared};
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    register_transcript(lua, smelt)?;
    register_engine_and_session(lua, smelt, shared)?;
    register_process(lua, smelt)?;
    register_shell(lua, smelt)?;
    register_agent(lua, smelt)?;
    register_permissions(lua, smelt)?;
    register_fuzzy(lua, smelt)?;
    register_history(lua, smelt)?;
    Ok(())
}

fn register_transcript(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
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
    Ok(())
}

fn register_engine_and_session(
    lua: &Lua,
    smelt: &mlua::Table,
    shared: &Arc<LuaShared>,
) -> LuaResult<()> {
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
                            "user" => protocol::Message::user(protocol::Content::text(&content)),
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
            let history = crate::lua::try_with_app(|app| app.history.clone()).unwrap_or_default();
            messages_to_lua(lua, &history)
        })?,
    )?;

    smelt.set("engine", engine_tbl)?;
    Ok(())
}

fn register_process(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let process_tbl = lua.create_table()?;
    process_tbl.set(
        "list",
        lua.create_function(|lua, ()| {
            let procs =
                crate::lua::try_with_app(|app| app.engine.processes.list()).unwrap_or_default();
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
    process_tbl.set(
        "read_output",
        lua.create_function(|lua, id: String| {
            let read = crate::lua::try_with_app(|app| app.engine.processes.read(&id));
            match read {
                Some(Ok((text, running, exit_code))) => {
                    let t = lua.create_table()?;
                    t.set("text", text)?;
                    t.set("running", running)?;
                    if let Some(code) = exit_code {
                        t.set("exit_code", code)?;
                    }
                    Ok(t)
                }
                _ => lua.create_table(),
            }
        })?,
    )?;
    // smelt.process.spawn_bg(command) → string id, or raises on
    // spawn error. Adds the child to the same `ProcessRegistry`
    // that the engine uses, so `smelt.process.list/read_output/kill`
    // (and the core `read_process_output` / `stop_process` tools)
    // observe it the same way as `bash run_in_background=true`.
    process_tbl.set(
        "spawn_bg",
        lua.create_function(|_, command: String| -> LuaResult<String> {
            let registry = crate::lua::try_with_app(|app| app.engine.processes.clone())
                .ok_or_else(|| mlua::Error::external("process.spawn_bg: app unavailable"))?;
            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-c")
                .arg(&command)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            #[cfg(unix)]
            cmd.process_group(0);
            let child = cmd
                .spawn()
                .map_err(|e| mlua::Error::external(e.to_string()))?;
            let id = registry.next_id();
            // Discard channel — plugin-spawned processes don't emit
            // `EngineEvent::ProcessCompleted` today.
            let (done_tx, _done_rx) = tokio::sync::mpsc::unbounded_channel();
            registry.spawn(id.clone(), &command, child, done_tx);
            Ok(id)
        })?,
    )?;
    smelt.set("process", process_tbl)?;
    Ok(())
}

fn register_shell(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    // Pure parsing helpers reused from the core bash tool. Plugins that
    // wrap `bash` (like background_commands) call these to validate
    // commands the same way before spawning.
    let shell_tbl = lua.create_table()?;
    shell_tbl.set(
        "split",
        lua.create_function(|_, command: String| {
            Ok(engine::permissions::split_shell_commands(&command))
        })?,
    )?;
    shell_tbl.set(
        "split_with_ops",
        lua.create_function(|lua, command: String| {
            let parts = engine::permissions::split_shell_commands_with_ops(&command);
            let out = lua.create_table()?;
            for (i, (cmd, op)) in parts.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("command", cmd)?;
                if let Some(op) = op {
                    row.set("op", op)?;
                }
                out.set(i + 1, row)?;
            }
            Ok(out)
        })?,
    )?;
    shell_tbl.set(
        "check_interactive",
        lua.create_function(|_, command: String| {
            Ok(engine::tools::check_interactive(&command).map(String::from))
        })?,
    )?;
    shell_tbl.set(
        "check_background_op",
        lua.create_function(|_, command: String| {
            Ok(engine::tools::check_shell_background_operator(&command))
        })?,
    )?;
    smelt.set("shell", shell_tbl)?;
    Ok(())
}

fn register_agent(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
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
    agent_tbl.set(
        "snapshots",
        lua.create_function(|lua, ()| {
            let snaps = crate::lua::try_with_app(|app| {
                app.agent_snapshots
                    .lock()
                    .map(|v| v.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
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
    Ok(())
}

fn register_permissions(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
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
    Ok(())
}

fn register_fuzzy(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let fuzzy_tbl = lua.create_table()?;
    fuzzy_tbl.set(
        "score",
        lua.create_function(
            |_, (text, query): (String, String)| match crate::fuzzy::fuzzy_score(&text, &query) {
                Some(s) => Ok(Some(s)),
                None => Ok(None),
            },
        )?,
    )?;
    smelt.set("fuzzy", fuzzy_tbl)?;
    Ok(())
}

fn register_history(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    // smelt.history — past submitted prompts.
    //   entries()      → array of strings (oldest first)
    //   search(query)  → [{index, score}] ranked by the
    //                    history-specific scorer (word-match boosts,
    //                    recency bonus). 1-based index into entries().
    let history_tbl = lua.create_table()?;
    history_tbl.set(
        "entries",
        lua.create_function(|lua, ()| {
            let entries = crate::lua::try_with_app(|app| app.input_history.entries().to_vec())
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
            let entries = crate::lua::try_with_app(|app| app.input_history.entries().to_vec())
                .unwrap_or_default();
            // Oldest first in the vec; the scorer wants newest-first
            // so "recent" ranks highest. Iterate reversed and dedupe
            // to match the old `Completer::history` construction.
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
                if let Some(s) = crate::completer::history::history_score(label, &query, rank) {
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
    Ok(())
}
