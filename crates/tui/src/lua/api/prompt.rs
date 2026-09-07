//! `smelt.prompt` bindings - the main editable input surface.
//!
//! `win()` returns a `Win` userdata for the prompt input so plugins can
//! bind keys / events via the chainable handle API. `text()` snapshots
//! the current buffer; `set_text(s)` replaces it.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "prompt",
        "The main editable input surface: win handle, text get/set, and cursor control. UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "win",
        "Return a `Win` handle for the prompt input. Use `win:key(...)` and `win:on(...)` to attach plugin behaviour.",
        &[],
        |_, ()| Ok(super::win::LuaWin { id: crate::app::PROMPT_WIN }),
    )?;
    m.fn_(
        "text",
        "Return the prompt input buffer's current text. Internal attachment markers are stripped - plugins see only the user-visible characters.",
        &[],
        |_, ()| {
            Ok(crate::lua::try_with_conversation_host(|host| host.prompt_text()).unwrap_or_default())
        },
    )?;
    m.live_only_fn(
        "set_text",
        "Replace the prompt buffer with `text`. The cursor lands at the end and undo state is reset.",
        &["text"],
        |_, text: String| -> LuaResult<()> {
            crate::lua::with_conversation_host(|host| host.replace_prompt_text(text));
            Ok(())
        },
    )?;
    m.live_only_fn(
        "cursor",
        "Read or write the prompt cursor as a byte offset into `text()`. Without an argument returns the current offset; with one snaps it to a char boundary and clamps to source length. Returns the resulting offset.",
        &["pos"],
        |_, pos: Option<i64>| -> LuaResult<i64> {
            Ok(crate::lua::with_conversation_host(|host| host.prompt_cursor(pos)))
        },
    )?;
    m.live_only_fn(
        "replace_range",
        "UTF-8-safe replace of the byte range `[start, end)` in the prompt with `text`. Endpoints are snapped to char boundaries and clamped to source length. The cursor lands at `start + #text`. Returns the new cursor offset.",
        &["start", "end", "text"],
        |_, (start, end, text): (i64, i64, String)| -> LuaResult<i64> {
            Ok(crate::lua::with_conversation_host(|host| {
                host.replace_prompt_range(start, end, &text)
            }))
        },
    )?;
    m.fn_(
        "queued",
        "Return the queued prompt text rows, including while execution is paused. Queue changes invalidate the retained top bar so waiting messages remain visible above the input.",
        &[],
        |_, ()| -> LuaResult<Vec<String>> {
            Ok(crate::lua::try_with_conversation_host(|host| host.queued_prompt_texts())
                .unwrap_or_default())
        },
    )?;
    m.fn_(
        "queued_rows",
        "Return queued prompt rows as `{ text, kind }` tables. `kind` is `request` for rows added to the current turn's next request, or `turn` for rows waiting for the next turn.",
        &[],
        |lua, ()| -> LuaResult<Vec<mlua::Table>> {
            let rows =
                crate::lua::try_with_conversation_host(|host| host.queued_prompt_rows()).unwrap_or_default();
            rows.into_iter()
                .map(|row| {
                    let t = lua.create_table()?;
                    t.set("text", row.0)?;
                    t.set("kind", row.1)?;
                    Ok(t)
                })
                .collect()
        },
    )?;
    m.fn_(
        "has_stash",
        "Return whether the prompt currently holds a stashed input snapshot (Ctrl+S). The top-bar renderer uses this to surface a `◌ Stashed (ctrl+s to unstash)` row.",
        &[],
        |_, ()| -> LuaResult<bool> {
            Ok(crate::lua::try_with_conversation_host(|host| host.prompt_has_stash()).unwrap_or(false))
        },
    )?;
    Ok(())
}
