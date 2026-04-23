//! `smelt.api.*` binding setup. `LuaRuntime::register_api` is the big
//! mlua table builder that wires every Rust-side primitive onto the
//! `smelt` global. Theme / JSON / color helpers it uses live here too;
//! payload + panel helpers for dialog callbacks live in `tasks.rs`.

use super::{
    lua_commands_snapshot, messages_to_lua, parse_keybind, parse_win_event, AutocmdEvent,
    LuaHandle, LuaRuntime, LuaShared, TaskCompletion, TaskEvent,
};
use crate::app::ops::{DomainOp, UiOp};
use mlua::prelude::*;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

impl LuaRuntime {
    pub(super) fn register_api(lua: &Lua, shared: &Arc<LuaShared>) -> LuaResult<()> {
        let smelt = lua.create_table()?;

        let api = lua.create_table()?;
        api.set("version", crate::api::VERSION)?;

        // Helper macro: lock shared.ops and read a snapshot field.
        macro_rules! snap_read {
            ($lua:expr, $s:expr, |$o:ident| $body:expr) => {{
                let s = $s.clone();
                $lua.create_function(move |_, ()| {
                    let $o = s
                        .ops
                        .lock()
                        .map_err(|e| LuaError::RuntimeError(e.to_string()))?;
                    Ok($body)
                })?
            }};
        }

        macro_rules! push_op {
            ($lua:expr, $s:expr, |$val:ident : $ty:ty| $op:expr) => {{
                let s = $s.clone();
                $lua.create_function(move |_, $val: $ty| {
                    if let Ok(mut o) = s.ops.lock() {
                        o.push($op);
                    }
                    Ok(())
                })?
            }};
            ($lua:expr, $s:expr, || $op:expr) => {{
                let s = $s.clone();
                $lua.create_function(move |_, ()| {
                    if let Ok(mut o) = s.ops.lock() {
                        o.push($op);
                    }
                    Ok(())
                })?
            }};
        }

        // smelt.api.transcript.text()
        let transcript_tbl = lua.create_table()?;
        transcript_tbl.set(
            "text",
            snap_read!(lua, shared, |o| o
                .transcript_text
                .clone()
                .unwrap_or_default()),
        )?;
        transcript_tbl.set(
            "yank_block",
            push_op!(lua, shared, || DomainOp::YankBlockAtCursor),
        )?;
        api.set("transcript", transcript_tbl)?;

        // smelt.api.cmd
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
                        let key = lua.create_registry_value(handler)?;
                        if let Ok(mut map) = s.commands.lock() {
                            map.insert(name.clone(), LuaHandle { key });
                        }
                        if let Ok(mut snap) = lua_commands_snapshot().lock() {
                            snap.insert(name, desc);
                        }
                        Ok(())
                    },
                )?,
            )?;
        }
        cmd_tbl.set(
            "run",
            push_op!(lua, shared, |line: String| DomainOp::RunCommand(line)),
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
        api.set("cmd", cmd_tbl)?;

        // smelt.api.engine.*
        let engine_tbl = lua.create_table()?;

        engine_tbl.set("model", snap_read!(lua, shared, |o| o.engine.model.clone()))?;
        engine_tbl.set("mode", snap_read!(lua, shared, |o| o.engine.mode.clone()))?;
        engine_tbl.set(
            "reasoning_effort",
            snap_read!(lua, shared, |o| o.engine.reasoning_effort.clone()),
        )?;
        engine_tbl.set("is_busy", snap_read!(lua, shared, |o| o.engine.is_busy))?;
        engine_tbl.set("cost", snap_read!(lua, shared, |o| o.engine.session_cost))?;
        engine_tbl.set(
            "context_tokens",
            snap_read!(lua, shared, |o| o.engine.context_tokens),
        )?;
        engine_tbl.set(
            "context_window",
            snap_read!(lua, shared, |o| o.engine.context_window),
        )?;
        engine_tbl.set(
            "session_dir",
            snap_read!(lua, shared, |o| o.engine.session_dir.clone()),
        )?;
        engine_tbl.set(
            "session_id",
            snap_read!(lua, shared, |o| o.engine.session_id.clone()),
        )?;

        // smelt.api.session.*
        let session_tbl = lua.create_table()?;
        session_tbl.set(
            "title",
            snap_read!(lua, shared, |o| o.engine.session_title.clone()),
        )?;
        session_tbl.set(
            "cwd",
            snap_read!(lua, shared, |o| o.engine.session_cwd.clone()),
        )?;
        session_tbl.set(
            "created_at_ms",
            snap_read!(lua, shared, |o| o.engine.session_created_at_ms),
        )?;
        session_tbl.set(
            "id",
            snap_read!(lua, shared, |o| o.engine.session_id.clone()),
        )?;
        session_tbl.set(
            "dir",
            snap_read!(lua, shared, |o| o.engine.session_dir.clone()),
        )?;
        {
            let s = shared.clone();
            session_tbl.set(
                "turns",
                lua.create_function(move |lua, ()| {
                    let turns = s
                        .ops
                        .lock()
                        .map(|o| o.engine.session_turns.clone())
                        .unwrap_or_default();
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
        }
        {
            let s = shared.clone();
            session_tbl.set(
                "rewind_to",
                lua.create_function(
                    move |_, (block_idx, opts): (Option<usize>, Option<mlua::Table>)| {
                        let restore_vim_insert = opts
                            .and_then(|t| t.get::<bool>("restore_vim_insert").ok())
                            .unwrap_or(false);
                        if let Ok(mut o) = s.ops.lock() {
                            o.push(DomainOp::RewindToBlock {
                                block_idx,
                                restore_vim_insert,
                            });
                        }
                        Ok(())
                    },
                )?,
            )?;
        }
        {
            let s = shared.clone();
            session_tbl.set(
                "list",
                lua.create_function(move |lua, ()| {
                    let current_id = s
                        .ops
                        .lock()
                        .map(|o| o.engine.session_id.clone())
                        .unwrap_or_default();
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
        }
        session_tbl.set(
            "load",
            push_op!(lua, shared, |id: String| DomainOp::LoadSession(id)),
        )?;
        session_tbl.set(
            "delete",
            push_op!(lua, shared, |id: String| DomainOp::DeleteSession(id)),
        )?;
        api.set("session", session_tbl)?;

        engine_tbl.set(
            "set_model",
            push_op!(lua, shared, |v: String| DomainOp::SetModel(v)),
        )?;
        engine_tbl.set(
            "set_mode",
            push_op!(lua, shared, |v: String| DomainOp::SetMode(v)),
        )?;
        engine_tbl.set(
            "set_reasoning_effort",
            push_op!(lua, shared, |v: String| DomainOp::SetReasoningEffort(v)),
        )?;
        engine_tbl.set(
            "submit",
            push_op!(lua, shared, |v: String| DomainOp::Submit(v)),
        )?;
        engine_tbl.set("cancel", push_op!(lua, shared, || DomainOp::Cancel))?;
        {
            let s = shared.clone();
            engine_tbl.set(
                "compact",
                lua.create_function(move |_, instructions: Option<String>| {
                    if let Ok(mut o) = s.ops.lock() {
                        o.push(DomainOp::Compact(instructions));
                    }
                    Ok(())
                })?,
            )?;
        }

        // smelt.api.engine.ask({ system, messages?, question?, task?, on_response })
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

                    if let Ok(mut o) = s.ops.lock() {
                        o.push(DomainOp::EngineAsk {
                            id,
                            system,
                            messages,
                            task,
                        });
                    }
                    Ok(id)
                })?,
            )?;
        }

        // smelt.api.engine.history() → [{role, content, tool_calls?, tool_call_id?}]
        {
            let s = shared.clone();
            engine_tbl.set(
                "history",
                lua.create_function(move |lua, ()| {
                    let Ok(guard) = s.history.lock() else {
                        return lua.create_table();
                    };
                    let history = Arc::clone(&*guard);
                    drop(guard);
                    messages_to_lua(lua, &history)
                })?,
            )?;
        }

        api.set("engine", engine_tbl)?;

        // smelt.api.process.*
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
        {
            let s = shared.clone();
            process_tbl.set(
                "kill",
                lua.create_function(move |_, id: String| {
                    if let Ok(mut o) = s.ops.lock() {
                        o.push(DomainOp::KillProcess(id));
                    }
                    Ok(())
                })?,
            )?;
        }
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
        api.set("process", process_tbl)?;

        // smelt.api.agent.*
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
            push_op!(lua, shared, |pid: u32| DomainOp::KillAgent(pid)),
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
                                    crate::render::ToolStatus::Pending => "pending",
                                    crate::render::ToolStatus::Confirm => "confirm",
                                    crate::render::ToolStatus::Ok => "ok",
                                    crate::render::ToolStatus::Err => "err",
                                    crate::render::ToolStatus::Denied => "denied",
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
        api.set("agent", agent_tbl)?;

        // smelt.api.permissions.*
        let permissions_tbl = lua.create_table()?;
        {
            let s = shared.clone();
            permissions_tbl.set(
                "list",
                lua.create_function(move |lua, ()| {
                    let (session_entries, cwd) = {
                        let Ok(o) = s.ops.lock() else {
                            return lua.create_table();
                        };
                        (
                            o.engine.permission_session_entries.clone(),
                            o.engine.session_cwd.clone(),
                        )
                    };
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
        }
        {
            let s = shared.clone();
            permissions_tbl.set(
                "sync",
                lua.create_function(move |_, spec: mlua::Table| {
                    let mut session_entries: Vec<crate::render::PermissionEntry> = Vec::new();
                    if let Ok(arr) = spec.get::<mlua::Table>("session") {
                        for row in arr.sequence_values::<mlua::Table>().flatten() {
                            let tool: String = row.get("tool").unwrap_or_default();
                            let pattern: String = row.get("pattern").unwrap_or_default();
                            session_entries.push(crate::render::PermissionEntry { tool, pattern });
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
                            workspace_rules
                                .push(crate::workspace_permissions::Rule { tool, patterns });
                        }
                    }
                    if let Ok(mut o) = s.ops.lock() {
                        o.push(DomainOp::SyncPermissions {
                            session_entries,
                            workspace_rules,
                        });
                    }
                    Ok(())
                })?,
            )?;
        }
        api.set("permissions", permissions_tbl)?;

        // smelt.api.keymap.help_sections()
        let keymap_tbl = lua.create_table()?;
        {
            let s = shared.clone();
            keymap_tbl.set(
                "help_sections",
                lua.create_function(move |lua, ()| {
                    let vim_enabled = s.ops.lock().map(|o| o.engine.vim_enabled).unwrap_or(false);
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
        }
        api.set("keymap", keymap_tbl)?;

        // smelt.api.ui
        let ui_tbl = lua.create_table()?;
        ui_tbl.set(
            "set_ghost_text",
            push_op!(lua, shared, |text: String| UiOp::SetGhostText(text)),
        )?;
        ui_tbl.set(
            "clear_ghost_text",
            push_op!(lua, shared, || UiOp::ClearGhostText),
        )?;
        ui_tbl.set(
            "notify",
            push_op!(lua, shared, |msg: String| UiOp::Notify(msg)),
        )?;
        ui_tbl.set(
            "notify_error",
            push_op!(lua, shared, |msg: String| UiOp::NotifyError(msg)),
        )?;
        api.set("ui", ui_tbl)?;

        // smelt.api.theme
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
        api.set("theme", theme_tbl)?;

        // smelt.api.buf
        let buf_tbl = lua.create_table()?;
        buf_tbl.set(
            "text",
            snap_read!(lua, shared, |o| o.prompt_text.clone().unwrap_or_default()),
        )?;
        {
            let s = shared.clone();
            buf_tbl.set(
                "create",
                lua.create_function(move |_, ()| {
                    let id = s
                        .next_buf_id
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if let Ok(mut o) = s.ops.lock() {
                        o.push(UiOp::BufCreate { id });
                    }
                    Ok(id)
                })?,
            )?;
        }
        {
            let s = shared.clone();
            buf_tbl.set(
                "set_lines",
                lua.create_function(move |_, (id, lines): (u64, mlua::Table)| {
                    let lines: Vec<String> = lines
                        .sequence_values::<String>()
                        .filter_map(|v| v.ok())
                        .collect();
                    if let Ok(mut o) = s.ops.lock() {
                        o.push(UiOp::BufSetLines { id, lines });
                    }
                    Ok(())
                })?,
            )?;
        }
        {
            let s = shared.clone();
            buf_tbl.set(
                "add_highlight",
                lua.create_function(
                    move |_,
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
                                        LuaError::RuntimeError(format!(
                                            "unknown theme role: {role}"
                                        ))
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
                        if let Ok(mut o) = s.ops.lock() {
                            o.push(UiOp::BufAddHighlight {
                                id,
                                line: line0 as usize,
                                col_start: col_start.min(u16::MAX as u64) as u16,
                                col_end: col_end.min(u16::MAX as u64) as u16,
                                fg,
                                bold,
                                italic,
                                dim,
                            });
                        }
                        Ok(())
                    },
                )?,
            )?;
        }
        {
            let s = shared.clone();
            buf_tbl.set(
                "add_dim",
                lua.create_function(
                    move |_, (id, line, col_start, col_end): (u64, u64, u64, u64)| {
                        let Some(line0) = line.checked_sub(1) else {
                            return Ok(());
                        };
                        if col_end <= col_start {
                            return Ok(());
                        }
                        if let Ok(mut o) = s.ops.lock() {
                            o.push(UiOp::BufAddHighlight {
                                id,
                                line: line0 as usize,
                                col_start: col_start.min(u16::MAX as u64) as u16,
                                col_end: col_end.min(u16::MAX as u64) as u16,
                                fg: None,
                                bold: false,
                                italic: false,
                                dim: true,
                            });
                        }
                        Ok(())
                    },
                )?,
            )?;
        }
        api.set("buf", buf_tbl)?;

        // smelt.api.win
        let win_tbl = lua.create_table()?;
        win_tbl.set(
            "focus",
            snap_read!(lua, shared, |o| o
                .focused_window
                .clone()
                .unwrap_or_default()),
        )?;
        win_tbl.set(
            "mode",
            snap_read!(lua, shared, |o| o.vim_mode.clone().unwrap_or_default()),
        )?;
        {
            let s = shared.clone();
            win_tbl.set(
                "open_float",
                lua.create_function(move |lua, (buf_id, opts): (u64, mlua::Table)| {
                    let title: String = opts.get("title").unwrap_or_default();
                    let accent: Option<crossterm::style::Color> =
                        opts.get::<mlua::Table>("accent").ok().and_then(|t| {
                            t.get::<u8>("ansi")
                                .ok()
                                .map(crossterm::style::Color::AnsiValue)
                        });

                    let mut footer_items = Vec::new();
                    let mut on_select_handle = None;
                    let mut on_dismiss_handle = None;
                    if let Ok(footer) = opts.get::<mlua::Table>("footer") {
                        if let Ok(items) = footer.get::<mlua::Table>("items") {
                            footer_items = items
                                .sequence_values::<String>()
                                .filter_map(|v| v.ok())
                                .collect();
                        }
                        if let Ok(func) = footer.get::<mlua::Function>("on_select") {
                            let key = lua.create_registry_value(func)?;
                            on_select_handle = Some(key);
                        }
                    }
                    if let Ok(func) = opts.get::<mlua::Function>("on_dismiss") {
                        let key = lua.create_registry_value(func)?;
                        on_dismiss_handle = Some(key);
                    }

                    if let Some(key) = on_select_handle {
                        if let Ok(mut cbs) = s.callbacks.lock() {
                            cbs.insert(buf_id, LuaHandle { key });
                        }
                    }
                    if let Some(key) = on_dismiss_handle {
                        let dismiss_id = buf_id | (1 << 63);
                        if let Ok(mut cbs) = s.callbacks.lock() {
                            cbs.insert(dismiss_id, LuaHandle { key });
                        }
                    }

                    if let Ok(mut o) = s.ops.lock() {
                        o.push(UiOp::WinOpenFloat {
                            buf_id,
                            title,
                            footer_items,
                            accent,
                        });
                    }
                    Ok(buf_id)
                })?,
            )?;
        }
        {
            let s = shared.clone();
            win_tbl.set(
                "close",
                lua.create_function(move |_, id: u64| {
                    if let Ok(mut o) = s.ops.lock() {
                        o.push(UiOp::CloseFloat(ui::WinId(id)));
                    }
                    Ok(())
                })?,
            )?;
        }
        {
            let s = shared.clone();
            win_tbl.set(
                "set_title",
                lua.create_function(move |_, (id, title): (u64, String)| {
                    if let Ok(mut o) = s.ops.lock() {
                        o.push(UiOp::WinUpdate {
                            id,
                            title: Some(title),
                        });
                    }
                    Ok(())
                })?,
            )?;
        }
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
                        let registry_key = lua.create_registry_value(func)?;
                        let id = s.next_id.fetch_add(1, Ordering::Relaxed);
                        if let Ok(mut cbs) = s.callbacks.lock() {
                            cbs.insert(id, LuaHandle { key: registry_key });
                        }
                        if let Ok(mut o) = s.ops.lock() {
                            o.push(UiOp::WinBindLuaKeymap {
                                win: ui::WinId(win_id),
                                key,
                                callback_id: id,
                            });
                        }
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
                        let registry_key = lua.create_registry_value(func)?;
                        let id = s.next_id.fetch_add(1, Ordering::Relaxed);
                        if let Ok(mut cbs) = s.callbacks.lock() {
                            cbs.insert(id, LuaHandle { key: registry_key });
                        }
                        if let Ok(mut o) = s.ops.lock() {
                            o.push(UiOp::WinBindLuaEvent {
                                win: ui::WinId(win_id),
                                event,
                                callback_id: id,
                            });
                        }
                        Ok(())
                    },
                )?,
            )?;
        }
        api.set("win", win_tbl)?;

        // smelt.api.task
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
            api.set("task", task_tbl)?;
        }

        // smelt.api.prompt.set_section(name, content) / remove_section(name)
        let prompt_tbl = lua.create_table()?;
        {
            let s = shared.clone();
            prompt_tbl.set(
                "set_section",
                lua.create_function(move |_, (name, content): (String, String)| {
                    if let Ok(mut o) = s.ops.lock() {
                        o.push(DomainOp::SetPromptSection(name, content));
                    }
                    Ok(())
                })?,
            )?;
        }
        {
            let s = shared.clone();
            prompt_tbl.set(
                "remove_section",
                lua.create_function(move |_, name: String| {
                    if let Ok(mut o) = s.ops.lock() {
                        o.push(DomainOp::RemovePromptSection(name));
                    }
                    Ok(())
                })?,
            )?;
        }
        api.set("prompt", prompt_tbl)?;

        // smelt.api.tools.register(def) / unregister(name) / resolve(...)
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
        {
            let s = shared.clone();
            tools_tbl.set(
                "resolve",
                lua.create_function(
                    move |_, (request_id, call_id, result): (u64, String, mlua::Table)| {
                        let content: String = result.get("content").unwrap_or_default();
                        let is_error: bool = result.get("is_error").unwrap_or(false);
                        if let Ok(mut o) = s.ops.lock() {
                            o.push(DomainOp::ResolveToolResult {
                                request_id,
                                call_id,
                                content,
                                is_error,
                            });
                        }
                        Ok(())
                    },
                )?,
            )?;
        }
        api.set("tools", tools_tbl)?;

        // smelt.api.fuzzy.score
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
        api.set("fuzzy", fuzzy_tbl)?;

        // smelt.api.picker
        {
            let picker_tbl = lua.create_table()?;
            {
                let s = shared.clone();
                picker_tbl.set(
                    "set_selected",
                    lua.create_function(move |_, (win_id, idx): (u64, i64)| {
                        let index = if idx < 0 { 0 } else { idx as usize };
                        if let Ok(mut o) = s.ops.lock() {
                            o.push(UiOp::PickerSetSelected {
                                win: ui::WinId(win_id),
                                index,
                            });
                        }
                        Ok(())
                    })?,
                )?;
            }
            {
                let s = shared.clone();
                picker_tbl.set(
                    "_request_open",
                    lua.create_function(move |lua, (task_id, opts): (u64, mlua::Table)| {
                        let key = lua.create_registry_value(opts)?;
                        if let Ok(mut o) = s.ops.lock() {
                            o.push(UiOp::OpenLuaPicker { task_id, opts: key });
                        }
                        Ok(())
                    })?,
                )?;
            }
            api.set("picker", picker_tbl)?;
        }

        // smelt.api.dialog
        {
            let dialog_tbl = lua.create_table()?;
            {
                let s = shared.clone();
                dialog_tbl.set(
                    "_request_open",
                    lua.create_function(move |lua, (task_id, opts): (u64, mlua::Table)| {
                        let key = lua.create_registry_value(opts)?;
                        if let Ok(mut o) = s.ops.lock() {
                            o.push(UiOp::OpenLuaDialog { task_id, opts: key });
                        }
                        Ok(())
                    })?,
                )?;
            }
            api.set("dialog", dialog_tbl)?;
        }

        smelt.set("api", api)?;

        smelt.set(
            "notify",
            push_op!(lua, shared, |msg: String| UiOp::Notify(msg)),
        )?;

        smelt.set(
            "clipboard",
            lua.create_function(|_, text: String| {
                crate::app::commands::copy_to_clipboard(&text).map_err(LuaError::RuntimeError)?;
                Ok(())
            })?,
        )?;

        {
            let s = shared.clone();
            smelt.set(
                "keymap",
                lua.create_function(
                    move |lua, (mode, chord, handler): (String, String, mlua::Function)| {
                        let key = lua.create_registry_value(handler)?;
                        if let Ok(mut map) = s.keymaps.lock() {
                            map.insert((mode, chord), LuaHandle { key });
                        }
                        Ok(())
                    },
                )?,
            )?;
        }

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

        {
            let s = shared.clone();
            smelt.set(
                "statusline",
                lua.create_function(move |lua, handler: mlua::Function| {
                    let key = lua.create_registry_value(handler)?;
                    if let Ok(mut slot) = s.statusline.lock() {
                        *slot = Some(LuaHandle { key });
                    }
                    Ok(())
                })?,
            )?;
        }

        {
            let s = shared.clone();
            smelt.set(
                "task",
                lua.create_function(move |lua, handler: mlua::Function| {
                    if let Ok(mut rt) = s.tasks.lock() {
                        rt.spawn(lua, handler, LuaValue::Nil, TaskCompletion::FireAndForget)?;
                    }
                    Ok(())
                })?,
            )?;
        }

        lua.globals().set("smelt", smelt)?;

        // Install the yielding primitives as Lua wrappers around
        // `coroutine.yield`. Each checks `coroutine.isyieldable()` so
        // calls from a non-task context raise a clear error instead of
        // yielding into the void.
        lua.load(TASK_YIELD_PRIMITIVES)
            .set_name("smelt/task_primitives")
            .exec()?;

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

/// Lua source injected at bootstrap to install the task-yielding
/// primitives. Each checks `coroutine.isyieldable()` so calls from
/// outside a task raise a clear error rather than failing later.
const TASK_YIELD_PRIMITIVES: &str = r#"
smelt.api = smelt.api or {}
smelt.api.dialog = smelt.api.dialog or {}
smelt.api.picker = smelt.api.picker or {}

function smelt.api.sleep(ms)
  if not coroutine.isyieldable() then
    error("smelt.api.sleep: call from inside smelt.task(fn) or tool.execute", 2)
  end
  return coroutine.yield({__yield = "sleep", ms = ms})
end

-- `smelt.api.dialog.open` is installed by `runtime/lua/smelt/dialog.lua`.
-- It allocs a task id, calls `smelt.api.dialog._request_open(task_id,
-- opts)` (which queues a `UiOp::OpenLuaDialog` so the reducer opens
-- the float + panels and resolves the task with `{win_id = …}`), parks
-- on an External yield, then wires Lua-side keymaps/events and parks
-- again for the final result.

-- `smelt.api.picker.open` is installed by `runtime/lua/smelt/picker.lua`
-- with the same `_request_open` → External pattern.
"#;
