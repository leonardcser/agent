//! `smelt.session` bindings - current session metadata, turn list,
//! messages snapshot, rewind, list / load / delete persisted sessions.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

/// Convert full `protocol::Message`-shaped Lua rows via serde. Rows that fail to
/// deserialize (unknown role, malformed shape) are silently dropped so a
/// single bad entry doesn't poison the whole replacement list.
pub(crate) fn lua_messages_to_protocol(lua: &Lua, table: &mlua::Table) -> Vec<protocol::Message> {
    let mut out = Vec::new();
    for value in table.clone().sequence_values::<mlua::Value>().flatten() {
        if let Some(msg) = smelt_core::lua::lua_to_serde::<protocol::Message>(lua, &value) {
            out.push(msg);
        }
    }
    out
}

const DEFAULT_LUA_SESSION_LIMIT: usize = 200;
const DEFAULT_LUA_SESSION_MAX_BYTES: usize = 1024 * 1024;

fn session_entries_to_lua(
    lua: &Lua,
    entries: Vec<smelt_core::session::SessionListEntry>,
    current_id: &str,
) -> LuaResult<mlua::Table> {
    let out = lua.create_table()?;
    let mut index = 1;
    for entry in entries {
        if entry.id == current_id {
            continue;
        }
        let row = lua.create_table()?;
        row.set("id", entry.id)?;
        match entry.status {
            smelt_core::session::SessionListStatus::Available(meta) => {
                let meta = *meta;
                row.set("available", true)?;
                row.set("title", meta.title.unwrap_or_default())?;
                row.set("subtitle", meta.first_user_message.unwrap_or_default())?;
                row.set("cwd", meta.cwd.unwrap_or_default())?;
                row.set("parent_id", meta.parent_id.unwrap_or_default())?;
                row.set("updated_at_ms", meta.updated_at_ms)?;
                row.set("created_at_ms", meta.created_at_ms)?;
                if let Some(size) = meta.text_bytes {
                    row.set("size_bytes", size)?;
                }
            }
            smelt_core::session::SessionListStatus::Unavailable(error) => {
                row.set("available", false)?;
                row.set("error_kind", error.code())?;
                row.set("error", error.to_string())?;
            }
        }
        out.set(index, row)?;
        index += 1;
    }
    Ok(out)
}

fn opt_field<T: mlua::FromLua>(table: &Option<mlua::Table>, key: &str) -> LuaResult<Option<T>> {
    match table {
        Some(table) => table.get::<Option<T>>(key),
        None => Ok(None),
    }
}

pub(crate) fn messages_to_lua(lua: &Lua, msgs: &[protocol::Message]) -> LuaResult<mlua::Table> {
    let tbl = lua.create_table()?;
    for (i, msg) in msgs.iter().enumerate() {
        let entry = lua.create_table()?;
        let role = match msg.role {
            protocol::Role::System => "system",
            protocol::Role::User => "user",
            protocol::Role::Assistant => "assistant",
            protocol::Role::Tool => "tool",
        };
        entry.set("role", role)?;
        if let Some(ref c) = msg.content {
            entry.set("content", c.text_content())?;
        }
        if let Some(ref rc) = msg.reasoning_content {
            entry.set("reasoning_content", rc.as_str())?;
        }
        if let Some(ref tc) = msg.tool_calls {
            let calls = lua.create_table()?;
            for (j, call) in tc.iter().enumerate() {
                let ct = lua.create_table()?;
                ct.set("id", call.id.as_str())?;
                ct.set("type", "function")?;
                let func = lua.create_table()?;
                func.set("name", call.function.name.as_str())?;
                func.set("arguments", call.function.arguments.as_str())?;
                ct.set("function", func)?;
                calls.set(j + 1, ct)?;
            }
            entry.set("tool_calls", calls)?;
        }
        if let Some(ref id) = msg.tool_call_id {
            entry.set("tool_call_id", id.as_str())?;
        }
        if msg.is_error {
            entry.set("is_error", true)?;
        }
        tbl.set(i + 1, entry)?;
    }
    Ok(tbl)
}

fn usage_to_lua(lua: &Lua, usage: &protocol::TokenUsage) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    let input = u64::from(usage.prompt_tokens.unwrap_or(0));
    let output = u64::from(usage.completion_tokens.unwrap_or(0));
    let cache_read = u64::from(usage.cache_read_tokens.unwrap_or(0));
    let cache_write = u64::from(usage.cache_write_tokens.unwrap_or(0));
    let reasoning = u64::from(usage.reasoning_tokens.unwrap_or(0));
    let cached_input = cache_read + cache_write;
    let input_total = input + cached_input;
    let standard_total = input + output;
    t.set("input", input)?;
    t.set("output", output)?;
    t.set("cache_read", cache_read)?;
    t.set("cache_write", cache_write)?;
    t.set("cached_input", cached_input)?;
    t.set("input_total", input_total)?;
    t.set("reasoning", reasoning)?;
    t.set("standard_total", standard_total)?;
    // The denominator is input + cache_read: input is the count of tokens the
    // provider had to read fresh, cache_read is the count served from cache.
    // Together they cover the prefix this turn consumed.
    let denom = input + cache_read;
    if denom > 0 {
        t.set("cache_hit_ratio", cache_read as f64 / denom as f64)?;
    }
    Ok(t)
}

fn note_kind_to_lua(kind: protocol::HistoryNoteKind) -> &'static str {
    match kind {
        protocol::HistoryNoteKind::ModeChange => "mode_change",
        protocol::HistoryNoteKind::Context => "context",
        protocol::HistoryNoteKind::ProcessStatus => "process_status",
    }
}

fn history_items_to_lua(lua: &Lua, items: &[protocol::HistoryItem]) -> LuaResult<mlua::Table> {
    let tbl = lua.create_table()?;
    for (i, item) in items.iter().enumerate() {
        let entry = lua.create_table()?;
        match item {
            protocol::HistoryItem::System { content } => {
                entry.set("kind", "system")?;
                entry.set("content", content.text_content())?;
            }
            protocol::HistoryItem::User {
                content,
                display,
                command,
                sent_at_ms,
            } => {
                entry.set("kind", "user")?;
                entry.set("content", content.text_content())?;
                entry.set("sent_at_ms", *sent_at_ms)?;
                if let Some(display) = display {
                    entry.set("display", display.as_str())?;
                }
                if *command {
                    entry.set("command", true)?;
                }
            }
            protocol::HistoryItem::Assistant(step) => {
                entry.set("kind", "assistant")?;
                if let Some(content) = &step.content {
                    entry.set("content", content.text_content())?;
                }
                if let Some(reasoning) = &step.reasoning {
                    entry.set("reasoning_content", reasoning.as_str())?;
                }
                if !step.invocations.is_empty() {
                    let invocations = lua.create_table()?;
                    for (j, inv) in step.invocations.iter().enumerate() {
                        let row = lua.create_table()?;
                        row.set("call_id", inv.call_id.as_str())?;
                        row.set("name", inv.name.as_str())?;
                        row.set("arguments", inv.arguments.as_str())?;
                        if let Some(elapsed_ms) = inv.elapsed_ms {
                            row.set("elapsed_ms", elapsed_ms)?;
                        }
                        let result = lua.create_table()?;
                        result.set("content", inv.result.content.as_str())?;
                        result.set("is_error", inv.result.is_error)?;
                        if let Some(metadata) = &inv.result.metadata {
                            result.set("metadata", smelt_core::lua::json_to_lua(lua, metadata)?)?;
                        }
                        row.set("result", result)?;
                        invocations.set(j + 1, row)?;
                    }
                    entry.set("invocations", invocations)?;
                }
            }
            protocol::HistoryItem::Note(note) => {
                entry.set("kind", "note")?;
                entry.set("note_kind", note_kind_to_lua(note.kind()))?;
                entry.set("text", note.text())?;
                if let Some(mode) = note.mode() {
                    entry.set("mode", mode)?;
                }
                if let Some(context_name) = note.context_name() {
                    entry.set("context_name", context_name)?;
                }
            }
        }
        tbl.set(i + 1, entry)?;
    }
    Ok(tbl)
}

fn push_conversation_history_item(
    lua: &Lua,
    out: &mlua::Table,
    idx: &mut usize,
    item: &protocol::HistoryItem,
) -> LuaResult<bool> {
    match item {
        protocol::HistoryItem::User { content, .. } => {
            let row = lua.create_table()?;
            row.set("role", "user")?;
            row.set("content", content.text_content())?;
            out.set(*idx, row)?;
            *idx += 1;
            Ok(true)
        }
        protocol::HistoryItem::Assistant(step) => {
            let Some(content) = &step.content else {
                return Ok(false);
            };
            let row = lua.create_table()?;
            row.set("role", "assistant")?;
            row.set("content", content.text_content())?;
            out.set(*idx, row)?;
            *idx += 1;
            Ok(true)
        }
        protocol::HistoryItem::System { .. } | protocol::HistoryItem::Note(_) => Ok(false),
    }
}

fn conversation_to_lua(
    lua: &Lua,
    history: &[protocol::HistoryItem],
    limit: Option<usize>,
) -> LuaResult<mlua::Table> {
    let _perf = smelt_perf::perf::begin("lua:session:conversation");
    let out = lua.create_table()?;
    let mut idx = 1;
    let Some(limit) = limit.filter(|limit| *limit > 0) else {
        for item in history {
            push_conversation_history_item(lua, &out, &mut idx, item)?;
        }
        smelt_perf::perf::record_value(
            "lua:session:conversation_rows_scanned",
            history.len() as u64,
        );
        smelt_perf::perf::record_value(
            "lua:session:conversation_rows_returned",
            idx.saturating_sub(1) as u64,
        );
        return Ok(out);
    };

    let mut start = history.len();
    let mut kept = 0usize;
    for (item_idx, item) in history.iter().enumerate().rev() {
        if matches!(
            item,
            protocol::HistoryItem::User { .. }
                | protocol::HistoryItem::Assistant(protocol::AssistantStep {
                    content: Some(_),
                    ..
                })
        ) {
            kept += 1;
            start = item_idx;
            if kept >= limit {
                break;
            }
        }
    }
    smelt_perf::perf::record_value(
        "lua:session:conversation_rows_scanned",
        history.len().saturating_sub(start) as u64,
    );
    for item in &history[start..] {
        push_conversation_history_item(lua, &out, &mut idx, item)?;
    }
    smelt_perf::perf::record_value(
        "lua:session:conversation_rows_returned",
        idx.saturating_sub(1) as u64,
    );
    Ok(out)
}

pub(super) fn register(
    lua: &Lua,
    smelt: &mlua::Table,
    shared: &std::sync::Arc<crate::lua::LuaShared>,
) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "session",
        "Current session metadata, turn list, message snapshots, rewind, and persisted session management. UiHost-only.",
        Tier::UiHost,
    )?;
    let title = m.sub(
        "title",
        "Session title. Use `smelt.session.title.get()` to read the current title and `smelt.session.title.set(title, slug?)` to write it. Writes update the task label and save the session.",
    )?;
    title.fn_(
        "get",
        "Return the current session title, or `nil` when it has not been set.",
        &[],
        |_, ()| -> LuaResult<Option<String>> {
            Ok(crate::lua::try_with_session_host(|host| host.session_title()).unwrap_or_default())
        },
    )?;
    title.live_only_fn(
        "set",
        "Set the session title. When `slug` is omitted, one is derived from the title.",
        &["title", "slug"],
        |_, (title, slug): (String, Option<String>)| -> LuaResult<()> {
            crate::lua::with_session_host(|host| {
                let slug = slug.unwrap_or_else(|| engine::provider::slugify(&title));
                host.set_session_title(title, slug, None);
            });
            Ok(())
        },
    )?;
    m.advanced_live_only_fn(
        "set_title_for_history",
        "Set the session title and slug for a specific history length. Intended for title/session metadata plugins that compute metadata for an already-submitted turn.",
        &["title", "slug", "history_len"],
        |_, (title, slug, history_len): (String, String, usize)| -> LuaResult<()> {
            crate::lua::with_session_host(|host| {
                host.set_session_title(title, slug, Some(history_len));
            });
            Ok(())
        },
    )?;
    let slug = m.sub(
        "slug",
        "Session slug. Use `smelt.session.slug.get()` to read it. Writing flows through `smelt.session.title.set(title, slug)`.",
    )?;
    slug.fn_(
        "get",
        "Return the current session slug, or `nil` when it has not been set.",
        &[],
        |_, ()| -> LuaResult<Option<String>> {
            Ok(crate::lua::try_with_session_host(|host| host.session_slug()).unwrap_or_default())
        },
    )?;
    let cwd_context = shared.core_arc();
    m.fn_(
        "cwd",
        "Current working directory. Updated when smelt enters a managed worktree.",
        &[],
        move |_, ()| {
            if cwd_context.external_effects_active() {
                Ok(
                    crate::lua::try_with_session_host(|host| host.workspace_cwd())
                        .unwrap_or_default(),
                )
            } else {
                Ok(cwd_context.evaluation_cwd().to_string_lossy().into_owned())
            }
        },
    )?;
    m.live_only_fn(
        "context_note",
        "Set or clear a named hidden model-visible context note. `context_note(name, text)` creates or replaces that note for future turns; `context_note(name, nil)` clears it. Named notes do not replace each other, so plugins can maintain independent steering state. UiHost-only.",
        &["name", "text", "opts"],
        |_, (name, text, _opts): (String, Option<String>, Option<mlua::Table>)| -> LuaResult<()> {
            let name = smelt_buffer::text::trim_whitespace(&name).to_owned();
            if name.is_empty() {
                return Err(LuaError::RuntimeError(
                    "smelt.session.context_note: name must be non-empty".into(),
                ));
            }
            crate::lua::try_with_session_host(|host| host.set_context_note(name, text));
            Ok(())
        },
    )?;
    m.live_only_fn(
        "enter_worktree",
        "Create or open a managed git worktree and request a coherent project-context transition. `opts.name` is required and is normalized to a safe lowercase folder/branch name. New worktrees are created under `smelt.settings.worktree_root`: relative roots are resolved inside the git root, absolute roots use a per-repository bucket. The transition updates Lua project config, process and engine cwd, session metadata, prompt inputs, permissions, and watcher roots together. The returned `pending` field is true inside the Lua callback. Sequential model tool callbacks commit at tool completion before their result is released; concurrent model tool callbacks are rejected, and other callers commit when the event loop reaches an idle safe point. Returns `{ name, branch, path, base, created, pending }`.",
        &["opts"],
        |lua, opts: Option<mlua::Table>| -> LuaResult<mlua::Table> {
            let name: String = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("name").ok().flatten())
                .map(|s| smelt_buffer::text::trim_whitespace(&s).to_owned())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| mlua::Error::external("name is required"))?;
            let base: Option<String> = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("base").ok().flatten())
                .map(|s| smelt_buffer::text::trim_whitespace(&s).to_owned())
                .filter(|s| !s.is_empty());
            let (info, pending) = crate::lua::with_session_host(|host| {
                host.enter_worktree(name.as_str(), base.as_deref())
            })
            .map_err(mlua::Error::external)?;
            let out = lua.create_table()?;
            out.set("name", info.name)?;
            out.set("branch", info.branch)?;
            out.set("path", info.path.display().to_string())?;
            out.set("base", info.base)?;
            out.set("created", info.created)?;
            out.set("pending", pending)?;
            Ok(out)
        },
    )?;
    m.fn_(
        "worktrees",
        "List smelt-managed git worktrees for the current repository. Rows are `{ name, branch, path, base, current }` and are sorted by name.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let out = lua.create_table()?;
            if let Some(worktrees) = crate::lua::try_with_session_host(|host| host.managed_worktrees()) {
                for (i, worktree) in worktrees
                    .map_err(mlua::Error::external)?
                    .iter()
                    .enumerate()
                {
                    let row = lua.create_table()?;
                    row.set("name", worktree.name.as_str())?;
                    row.set("branch", worktree.branch.as_str())?;
                    row.set("path", worktree.path.display().to_string())?;
                    row.set("base", worktree.base.as_str())?;
                    row.set("current", worktree.current)?;
                    out.set(i + 1, row)?;
                }
            }
            Ok(out)
        },
    )?;
    m.live_only_fn(
        "switch_cwd",
        "Request a coherent project-context transition. Relative paths resolve from the current cwd, and a leading `~` resolves from the runtime home. Lua project config, process and engine cwd, session metadata, prompt inputs, permissions, and watcher roots commit together. The returned `pending` field is true inside the Lua callback. Sequential model tool callbacks commit at tool completion before their result is released; concurrent model tool callbacks are rejected, and other callers commit when the event loop reaches an idle safe point. Returns `{ cwd, pending }`.",
        &["path"],
        |lua, path: String| -> LuaResult<mlua::Table> {
            let path = std::path::PathBuf::from(smelt_buffer::text::trim_whitespace(&path));
            let (cwd, pending) = crate::lua::with_session_host(|host| host.change_cwd(path))
                .map_err(mlua::Error::external)?;
            let out = lua.create_table()?;
            out.set("cwd", cwd)?;
            out.set("pending", pending)?;
            Ok(out)
        },
    )?;
    m.fn_(
        "system",
        "Currently-assembled system prompt sent on the next turn. Reflects the configured base prompt, skills, and instructions. Useful for auxiliary LLM calls that want to share the main turn's prompt-cache slot.",
        &[],
        |_, ()| -> LuaResult<String> {
            Ok(crate::lua::try_with_session_host(|host| host.system_prompt()).unwrap_or_default())
        },
    )?;
    m.fn_(
        "cost",
        "Cumulative session cost in USD across every model call this session has made.",
        &[],
        |_, ()| -> LuaResult<f64> {
            Ok(crate::lua::try_with_session_host(|host| host.session_cost()).unwrap_or_default())
        },
    )?;
    m.fn_(
        "tokens",
        "Cumulative token usage across every turn this session has made. Returns a table with `input` (non-cached input), `output`, `cache_read`, `cache_write`, `cached_input`, `input_total`, `reasoning` (output detail), `standard_total` (input + output), and `cache_hit_ratio` (cache_read / (input + cache_read), `nil` if no input observed yet).",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let usage = crate::lua::try_with_session_host(|host| host.session_usage()).unwrap_or_default();
            usage_to_lua(lua, &usage)
        },
    )?;
    m.live_only_fn(
        "set_fast_mode",
        "Enable or disable accelerated inference for the current session.",
        &["enabled"],
        |_, enabled: bool| -> LuaResult<()> {
            crate::lua::with_session_host(|host| host.set_fast_mode(enabled));
            Ok(())
        },
    )?;
    m.fn_(
        "status",
        "Return compact live status for prompt/status bars: `{ model, provider, api_base, mode = { name, pending, marker }, reasoning = { effort, pending, marker }, fast = { supported, active }, context = { state, tokens, window, stale, marker }, cost }`. Context state is `ready` or `recalculating`; tokens are the latest display reading and can remain visible while recalculating. Markers are `*` for pending config and `?` for stale readings.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let out = lua.create_table()?;
            if let Some(status) = crate::lua::try_with_session_host(|host| host.session_status()) {
                let active = status.active_model.as_ref();
                out.set("model", active.map(|model| model.key.as_str()))?;
                out.set(
                    "provider",
                    active.map(|model| model.provider_type.as_str()),
                )?;
                out.set("api_base", active.map(|model| model.api_base.as_str()))?;
                out.set("cost", status.cost)?;

                let mode = lua.create_table()?;
                mode.set("name", status.mode)?;
                mode.set("pending", status.mode_pending)?;
                mode.set("marker", if status.mode_pending { "*" } else { "" })?;
                out.set("mode", mode)?;

                let reasoning = lua.create_table()?;
                reasoning.set("effort", status.reasoning)?;
                reasoning.set("pending", status.reasoning_pending)?;
                reasoning.set(
                    "marker",
                    if status.reasoning_pending { "*" } else { "" },
                )?;
                out.set("reasoning", reasoning)?;

                let fast = lua.create_table()?;
                fast.set("supported", status.fast_supported)?;
                fast.set("active", status.fast_active)?;
                out.set("fast", fast)?;

                let context = lua.create_table()?;
                context.set("state", status.context_state)?;
                context.set("tokens", status.context_tokens)?;
                context.set("window", status.context_window)?;
                context.set("stale", status.context_stale)?;
                context.set("marker", if status.context_stale { "?" } else { "" })?;
                out.set("context", context)?;
            }
            Ok(out)
        },
    )?;
    m.fn_(
        "context_tokens",
        "Latest non-background provider-reported active-context token count, or `nil` before the first usage report and while `status().context.state` is `recalculating`. During other in-flight requests this may be the previous turn's reading until the provider sends a fresh usage update. Use `status().context` for display state and stale markers; stale counts are display-only and are not used as authoritative request baselines.",
        &[],
        |_, ()| -> LuaResult<Option<u32>> {
            Ok(
                crate::lua::try_with_session_host(|host| host.session_context_tokens())
                    .unwrap_or_default(),
            )
        },
    )?;
    m.fn_(
        "context_window",
        "Configured context-window size in tokens for the active model. `nil` when the model entry has no declared limit.",
        &[],
        |_, ()| -> LuaResult<Option<u32>> {
            Ok(crate::lua::try_with_runtime_host(|host| host.context_window()).unwrap_or_default())
        },
    )?;
    m.fn_(
        "created_at_ms",
        "Unix-epoch timestamp (milliseconds) at which this session was started.",
        &[],
        |_, ()| -> LuaResult<u64> {
            Ok(
                crate::lua::try_with_session_host(|host| host.session_created_at_ms())
                    .unwrap_or_default(),
            )
        },
    )?;
    m.fn_(
        "info",
        "Return current session metadata as a table. Includes id, title, slug, timestamps, paths, ephemeral flag, parent id, model/mode, usage counts, and current worktree context.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let out = lua.create_table()?;
            if let Some(info) = crate::lua::try_with_session_host(|host| host.session_info()) {
                let info = info.map_err(mlua::Error::RuntimeError)?;
                out.set("id", info.id)?;
                out.set("artifact_dir", info.artifact_dir.display().to_string())?;
                out.set("ephemeral", info.ephemeral)?;
                out.set("title", info.title)?;
                out.set("slug", info.slug)?;
                out.set("first_user_message", info.first_user_message)?;
                out.set("parent_id", info.parent_id)?;
                out.set("created_at_ms", info.created_at_ms)?;
                out.set("updated_at_ms", info.updated_at_ms)?;
                out.set("cwd", info.cwd)?;
                out.set("session_cwd", info.session_cwd)?;
                let active = info.active_model.as_ref();
                out.set("model", active.map(|model| model.key.as_str()))?;
                out.set(
                    "provider",
                    active.map(|model| model.provider_type.as_str()),
                )?;
                out.set("api_base", active.map(|model| model.api_base.as_str()))?;
                out.set("mode", info.mode)?;
                out.set("reasoning", info.reasoning)?;
                out.set("context_tokens", info.context_tokens)?;
                out.set("context_tokens_stale", info.context_tokens_stale)?;
                out.set("context_window", info.context_window)?;
                out.set("cost", info.cost)?;
                out.set("history_count", info.history_count)?;
                out.set("message_count", info.message_count)?;
                out.set(
                    "message_count_approximate",
                    info.message_count_approximate,
                )?;
                out.set("turn_count", info.turn_count)?;
                out.set("tokens", usage_to_lua(lua, &info.usage)?)?;

                let worktree = lua.create_table()?;
                worktree.set("managed", info.managed_worktree)?;
                worktree.set("project", info.project)?;
                worktree.set("branch", info.branch)?;
                worktree.set("name", info.worktree)?;
                worktree.set("path", info.worktree_path)?;
                out.set("worktree", worktree)?;
            }
            Ok(out)
        },
    )?;
    m.fn_(
        "id",
        "Stable session id used by canonical storage and provider cache routing.",
        &[],
        |_, ()| Ok(crate::lua::try_with_session_host(|host| host.session_id()).unwrap_or_default()),
    )?;
    m.fn_(
        "artifact_dir",
        "Absolute path for artifacts owned by the current session, such as plans. Persistent artifacts are separate from canonical lineage storage. Ephemeral sessions return a temporary directory that is removed when smelt exits.",
        &[],
        |_, ()| -> LuaResult<String> {
            Ok(
                crate::lua::try_with_session_host(|host| host.current_artifact_dir().display().to_string())
                    .unwrap_or_default(),
            )
        },
    )?;
    m.advanced_live_only_fn(
        "checkpoint",
        "Install a model-context checkpoint without deleting transcript history. Takes `{ kind?, summary, first_live_message_index, tokens_before?, guard? }`; future model requests use the summary plus the original model-visible suffix starting at `first_live_message_index`. When `guard` from `smelt.work.guard()` is provided, the checkpoint is installed only if that lifecycle is still current; late callbacks after cancel or turn replacement return `nil`. Returns `true` when a checkpoint was installed, or `nil` when the boundary would be a no-op. Use `smelt.session.model_messages()` to read the model-visible messages after checkpointing.",
        &["spec"],
        |_, spec: mlua::Table| -> LuaResult<Option<bool>> {
            let kind = spec
                .get::<Option<String>>("kind")?
                .unwrap_or_else(|| "compaction".to_string());
            let summary = spec.get::<String>("summary")?;
            let first_live_message_index = spec.get::<usize>("first_live_message_index")?;
            let tokens_before = spec.get::<Option<u32>>("tokens_before")?;
            let guard = spec
                .get::<Option<mlua::Table>>("guard")?
                .map(|guard| {
                    (
                        guard.get::<Option<u64>>("turn_id").ok().flatten(),
                        guard.get::<u64>("cancel_generation").ok(),
                    )
                });
            let installed = crate::lua::with_session_host(|host| {
                host.install_context_checkpoint(
                    kind,
                    summary,
                    first_live_message_index,
                    tokens_before,
                    guard,
                )
            });
            Ok(installed.then_some(true))
        },
    )?;
    let msgs = m.sub(
        "messages",
        "Session messages. `smelt.session.messages.list(opts?)` returns transcript rows as `{ role, content?, tool_calls?, tool_call_id?, is_error? }`; by default this reads a bounded tail. Pass `opts.limit`, `opts.since_index`, or `opts.all = true` for an explicit full read. Use `smelt.session.model_messages()` for the model-visible history after checkpointing.",
    )?;
    msgs.fn_(
        "list",
        "Return transcript messages, optionally filtered by `{ roles?, include_tool?, since_index?, limit?, all? }`. Without `all = true`, reads at most a bounded tail.",
        &["opts"],
        |lua, arg: Option<mlua::Table>| -> LuaResult<mlua::Table> {
            let roles = opt_field::<Vec<String>>(&arg, "roles")?;
            let include_tool = opt_field::<bool>(&arg, "include_tool")?.unwrap_or(true);
            let since_index = opt_field::<usize>(&arg, "since_index")?;
            let limit = opt_field::<usize>(&arg, "limit")?;
            let all = opt_field::<bool>(&arg, "all")?.unwrap_or(false);
            let history = crate::lua::try_with_session_host(|host| {
                let len = host.session_history_len();
                if all {
                    return host.session_history_range(0..len);
                }
                let limit = limit.unwrap_or(DEFAULT_LUA_SESSION_LIMIT).max(1);
                if let Some(since_index) = since_index {
                    let start = since_index.saturating_sub(1).min(len);
                    let end = start.saturating_add(limit).min(len);
                    host.session_history_range(start..end)
                } else {
                    host.session_history_tail(limit, Some(DEFAULT_LUA_SESSION_MAX_BYTES))
                }
            })
            .transpose()
            .map_err(mlua::Error::RuntimeError)?
            .unwrap_or_default();
            let messages = protocol::history_to_messages(&history);
            let role_filter: Option<std::collections::HashSet<String>> =
                roles.map(|v| v.into_iter().collect());
            let filtered: Vec<protocol::Message> = messages
                .into_iter()
                .filter(|m| {
                    if !include_tool && matches!(m.role, protocol::Role::Tool) {
                        return false;
                    }
                    if let Some(ref rf) = role_filter {
                        let r = match m.role {
                            protocol::Role::System => "system",
                            protocol::Role::User => "user",
                            protocol::Role::Assistant => "assistant",
                            protocol::Role::Tool => "tool",
                        };
                        if !rf.contains(r) {
                            return false;
                        }
                    }
                    true
                })
                .collect();
            messages_to_lua(lua, &filtered)
        },
    )?;
    m.advanced_fn(
        "model_messages",
        "Return the model-visible message list for the next request as full `protocol::Message` rows, preserving multipart content, provider reasoning blocks, and tool metadata. If the session has a context checkpoint, this is the checkpoint summary plus retained live tail; otherwise it is the persisted transcript. Read-only.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let messages = crate::lua::try_with_session_host(|host| host.model_history_messages())
                .unwrap_or_default();
            mlua::Table::from_lua(
                smelt_core::lua::serde_to_lua_preserving_nulls(lua, &messages)?,
                lua,
            )
        },
    )?;
    m.fn_(
        "history_len",
        "Return the number of semantic items in the current session history without materializing them.",
        &[],
        |_, ()| -> LuaResult<usize> {
            Ok(
                crate::lua::try_with_session_host(|host| host.session_history_len())
                    .unwrap_or_default(),
            )
        },
    )?;
    m.advanced_fn(
        "history",
        "Return the semantic session history as compaction-safe items. Rows are `{ kind = 'system'|'user'|'assistant'|'note', ... }`; assistant rows include `invocations`, and note rows include `note_kind` plus `text`. By default this returns a bounded tail; pass `{ all = true }` for an explicit full read.",
        &["opts"],
        |lua, opts: Option<mlua::Table>| -> LuaResult<mlua::Table> {
            let all = opt_field::<bool>(&opts, "all")?.unwrap_or(false);
            let since_index = opt_field::<usize>(&opts, "since_index")?;
            let limit = opt_field::<usize>(&opts, "limit")?
                .unwrap_or(DEFAULT_LUA_SESSION_LIMIT)
                .max(1);
            let history = crate::lua::try_with_session_host(|host| {
                let len = host.session_history_len();
                if all {
                    return host.session_history_range(0..len);
                }
                if let Some(since_index) = since_index {
                    let start = since_index.saturating_sub(1).min(len);
                    let end = start.saturating_add(limit).min(len);
                    host.session_history_range(start..end)
                } else {
                    host.session_history_tail(limit, Some(DEFAULT_LUA_SESSION_MAX_BYTES))
                }
            })
            .transpose()
            .map_err(mlua::Error::RuntimeError)?
            .unwrap_or_default();
            history_items_to_lua(lua, &history)
        },
    )?;
    m.fn_(
        "conversation",
        "Return user and assistant text from semantic history, excluding system messages, internal notes, and tool results. Rows are `{ role = 'user'|'assistant', content }`. By default reads the latest bounded conversation tail; pass `{ limit = n }`, `{ since_index = n }`, or `{ all = true }`. Read-only; intended for lightweight auxiliary prompts such as input prediction.",
        &["opts"],
        |lua, opts: Option<mlua::Table>| -> LuaResult<mlua::Table> {
            let all = opt_field::<bool>(&opts, "all")?.unwrap_or(false);
            let since_index = opt_field::<usize>(&opts, "since_index")?;
            let limit = opt_field::<usize>(&opts, "limit")?;
            let history = crate::lua::try_with_session_host(|host| {
                let len = host.session_history_len();
                if all {
                    host.session_history_range(0..len)
                } else if let Some(since_index) = since_index {
                    let limit = limit.unwrap_or(DEFAULT_LUA_SESSION_LIMIT).max(1);
                    let start = since_index.saturating_sub(1).min(len);
                    let end = start.saturating_add(limit).min(len);
                    host.session_history_range(start..end)
                } else {
                    host.session_history_tail(
                        limit.unwrap_or(DEFAULT_LUA_SESSION_LIMIT).max(1),
                        Some(DEFAULT_LUA_SESSION_MAX_BYTES),
                    )
                }
            })
            .transpose()
            .map_err(mlua::Error::RuntimeError)?
            .unwrap_or_default();
            conversation_to_lua(lua, &history, limit)
        },
    )?;
    m.fn_(
        "turns",
        "Return rewindable user turns as `{ history_idx, block_idx, label }` rows where `history_idx` is the canonical rewind coordinate, deprecated `block_idx` is its compatibility alias, and `label` is the first line of the user message.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let turns = crate::lua::try_with_session_host(|host| host.rewind_turns())
                .transpose()
                .map_err(mlua::Error::RuntimeError)?
                .unwrap_or_default();
            let out = lua.create_table()?;
            for (i, turn) in turns.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("history_idx", turn.history_idx)?;
                // COMPAT(lua-session-turn-block-idx): remove after third-party rewind dialogs use history_idx.
                row.set("block_idx", turn.history_idx)?;
                row.set("label", turn.label)?;
                out.set(i + 1, row)?;
            }
            Ok(out)
        },
    )?;
    m.live_only_fn(
        "rewind_to",
        "Rewind the session to a prior user turn. `history_idx = nil` rewinds to before the first turn; `opts.restore_vim_insert = true` re-enters vim insert mode after the rewind.",
        &["history_idx", "opts"],
        |_, (history_idx, opts): (Option<usize>, Option<mlua::Table>)| -> LuaResult<()> {
            let restore_vim_insert = opts
                .and_then(|t| t.get::<bool>("restore_vim_insert").ok())
                .unwrap_or(false);
            crate::lua::with_session_host(|host| {
                host.rewind_to_history(history_idx, restore_vim_insert);
            });
            Ok(())
        },
    )?;
    // Used by the bundled esc-chord plugin to rewind only before output starts.
    m.private_live_only_fn(
        "_rewind_active_turn_if_clean",
        &["opts"],
        |_, opts: Option<mlua::Table>| -> LuaResult<bool> {
            let restore_vim_insert = opts
                .and_then(|t| t.get::<bool>("restore_vim_insert").ok())
                .unwrap_or(false);
            Ok(crate::lua::with_session_host(|host| {
                host.rewind_active_user_turn_if_no_output(restore_vim_insert)
            }))
        },
    )?;
    m.fn_(
        "list",
        "List canonical lineage sessions other than the current one from the read-only derived catalog. Without `opts`, returns all rows. With `opts = { limit, cursor, cwd, availability }`, returns `{ entries, next_cursor, catalog }`; `availability` is `available` or `unavailable`.",
        &["opts"],
        |lua, opts: Option<mlua::Table>| -> LuaResult<mlua::Table> {
            let current_id =
                crate::lua::try_with_session_host(|host| host.session_id()).unwrap_or_default();
            let Some(opts) = opts else {
                let entries = crate::lua::with_session_host(|host| host.list_session_entries())
                    .map_err(|error| mlua::Error::RuntimeError(error.to_string()))?;
                return session_entries_to_lua(lua, entries, &current_id);
            };
            let cursor = opts
                .get::<Option<mlua::Table>>("cursor")?
                .map(|cursor| -> LuaResult<smelt_core::session::SessionListCursor> {
                    Ok(smelt_core::session::SessionListCursor {
                        updated_at_ms: cursor.get("updated_at_ms")?,
                        id: cursor.get("id")?,
                    })
                })
                .transpose()?;
            let availability = match opts.get::<Option<String>>("availability")?.as_deref() {
                None => None,
                Some("available") => Some(smelt_core::session::SessionListAvailability::Available),
                Some("unavailable") => {
                    Some(smelt_core::session::SessionListAvailability::Unavailable)
                }
                Some(other) => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "invalid session availability {other:?}"
                    )))
                }
            };
            let query = smelt_core::session::SessionListQuery {
                limit: opts.get::<Option<u32>>("limit")?.unwrap_or(200),
                cursor,
                cwd: opts.get("cwd")?,
                availability,
            };
            let page = crate::lua::with_session_host(|host| host.list_session_page(query))
            .map_err(|error| mlua::Error::RuntimeError(error.to_string()))?;
            let result = lua.create_table()?;
            result.set(
                "entries",
                session_entries_to_lua(lua, page.entries, &current_id)?,
            )?;
            if let Some(cursor) = page.next_cursor {
                let next = lua.create_table()?;
                next.set("updated_at_ms", cursor.updated_at_ms)?;
                next.set("id", cursor.id)?;
                result.set("next_cursor", next)?;
            }
            let catalog = lua.create_table()?;
            catalog.set(
                "state",
                match page.catalog.state {
                    smelt_core::session::SessionCatalogState::Reconciling => "reconciling",
                    smelt_core::session::SessionCatalogState::Ready => "ready",
                    smelt_core::session::SessionCatalogState::Degraded => "degraded",
                },
            )?;
            catalog.set("completed_scan_id", page.catalog.completed_scan_id)?;
            catalog.set("reconciled_at_ms", page.catalog.reconciled_at_ms)?;
            catalog.set("error", page.catalog.last_error)?;
            result.set("catalog", catalog)?;
            Ok(result)
        },
    )?;
    m.advanced_live_only_fn(
        "retry_persistence",
        "Explicitly reconcile and retry blocked session persistence. Retained turns remain undispatched until their canonical operation is durable. Returns true when the retry request is accepted. No automatic retry timer is used.",
        &[],
        |_, ()| -> LuaResult<bool> {
            Ok(
                crate::lua::try_with_session_host(|host| host.retry_blocked_persistence())
                    .unwrap_or(false),
            )
        },
    )?;
    m.live_only_fn(
        "load",
        "Request a UI switch to canonical lineage session `id`. Reads and prepares persisted state in the background, then resets transient state when the destination is ready.",
        &["id"],
        |_, id: String| -> LuaResult<()> {
            crate::lua::with_session_host(|host| host.request_session_load(&id));
            Ok(())
        },
    )?;
    m.private_fn("__load_now", &["id"], |_, id: String| -> LuaResult<()> {
        crate::lua::with_session_host(|host| host.load_session_by_id_now(&id));
        Ok(())
    })?;
    m.advanced_fn(
        "text",
        "Return the searchable plain-text blob for session `id` (user + assistant text only; reasoning, tool output, and system messages excluded). Returns `nil` when the session is missing. Reads canonical SQLite without writing derived sidecars.",
        &["id"],
        |_, id: String| -> LuaResult<Option<String>> {
            Ok(crate::lua::with_session_host(|host| host.session_search_blob(&id)))
        },
    )?;
    m.advanced_fn(
        "texts",
        "Parallel batch read of `session.text(id)` for many ids. Returns a table keyed by id; missing sessions are omitted. Use this when a picker needs to search across all sessions. The heavy IO happens on a worker pool rather than serializing on the Lua thread.",
        &["ids"],
        |lua, ids: Vec<String>| -> LuaResult<mlua::Table> {
            let pairs = crate::lua::with_session_host(|host| host.session_search_blobs(ids));
            let out = lua.create_table()?;
            for (id, blob) in pairs {
                out.set(id, blob)?;
            }
            Ok(out)
        },
    )?;
    m.advanced_fn(
        "render_preview_into",
        "Bind persisted session `id` to `opts.buf` and `opts.win` as a virtualized transcript preview. `opts.width` controls wrapping; `opts.height` is the viewport height; a new binding opens at the tail while resize refreshes preserve its viewport; `opts.updated_at_ms` identifies cached session revisions. Once bound, wheel and scrollbar navigation use the same stateful viewport projection as the open transcript. Returns `{ status = 'ready', total_rows, scroll_top, row_base, materialized_rows }`, `{ status = 'pending' }` while the background preview service reads or hydrates persisted content, or `{ status = 'unavailable', reason }` when persisted content cannot be hydrated.",
        &["id", "opts"],
        |lua, (id, opts): (String, mlua::Table)| -> LuaResult<Option<mlua::Table>> {
            let _perf = smelt_perf::perf::begin("session:render_preview_into");
            let buf: super::buf::LuaBuf = opts.get("buf")?;
            let win = opts.get::<Option<super::win::LuaWin>>("win").ok().flatten();
            let width = opts
                .get::<u16>("width")
                .ok()
                .filter(|w| *w > 0)
                .unwrap_or(80);
            let height = opts
                .get::<u16>("height")
                .ok()
                .filter(|h| *h > 0)
                .unwrap_or(1);
            let cache_key_hint = opts
                .get::<u64>("updated_at_ms")
                .ok()
                .map(|ts| format!("{id}:{ts}"));

            let cache_key = cache_key_hint.unwrap_or_else(|| id.clone());
            let outcome = crate::lua::with_session_host(|host| {
                let _perf = smelt_perf::perf::begin("session:render_preview_into:app");
                host.render_session_preview(
                    crate::app::session_preview::SessionPreviewRender {
                        id,
                        cache_key,
                        width,
                        height,
                        buffer: buf.id,
                        window: win.map(|win| win.id),
                    },
                )
            });
            let out = lua.create_table()?;
            match outcome {
                crate::app::session_preview::SessionPreviewRenderOutcome::Ready(rows) => {
                    out.set("status", "ready")?;
                    out.set("total_rows", rows.total_rows)?;
                    out.set("scroll_top", rows.clamped_scroll)?;
                    out.set("row_base", rows.row_base)?;
                    out.set("materialized_rows", rows.materialized_rows)?;
                }
                crate::app::session_preview::SessionPreviewRenderOutcome::Pending => {
                    out.set("status", "pending")?;
                }
                crate::app::session_preview::SessionPreviewRenderOutcome::Unavailable(error) => {
                    match error {
                        crate::app::transcript::TranscriptProjectionHydrationError::MissingBlocks {
                            required_blocks,
                            missing_blocks,
                        } => {
                            smelt_perf::perf::record_value(
                                "session:render_preview_into:hydration_failure_required_blocks",
                                required_blocks as u64,
                            );
                            smelt_perf::perf::record_value(
                                "session:render_preview_into:hydration_failure_missing_blocks",
                                missing_blocks as u64,
                            );
                        }
                        crate::app::transcript::TranscriptProjectionHydrationError::Pending(_) => {
                            smelt_perf::perf::record_value(
                                "session:render_preview_into:hydration_pending_without_request",
                                1,
                            );
                        }
                    }
                    out.set("status", "unavailable")?;
                    out.set("reason", "persisted content could not be hydrated")?;
                }
            }
            Ok(Some(out))
        },
    )?;
    m.live_only_fn(
        "delete",
        "Delete the persisted session with `id`. Refuses to delete the currently active session.",
        &["id"],
        |_, id: String| -> LuaResult<()> {
            crate::lua::with_session_host(|host| host.delete_session(&id))
                .map_err(mlua::Error::RuntimeError)
        },
    )?;
    m.live_only_fn(
        "fork",
        "Fork the current session: clone its messages into a new session id and switch to it. Useful for branching off an experiment without losing the original timeline.",
        &[],
        |_, ()| -> LuaResult<()> {
            crate::lua::with_session_host(|host| host.fork_session());
            Ok(())
        },
    )?;
    m.live_only_fn(
        "reset",
        "Cancel any in-flight agent and clear the session to a blank slate. Logs an `agent_stop` event with reason `user_cancel_and_clear`.",
        &[],
        |_, ()| -> LuaResult<()> {
            engine::log::entry(
                engine::log::Level::Info,
                "agent_stop",
                &serde_json::json!({ "reason": "user_cancel_and_clear" }),
            );
            crate::lua::with_session_host(|host| host.reset_session());
            Ok(())
        },
    )?;
    Ok(())
}
