//! `smelt.diff.*`, `smelt.syntax.*`, `smelt.bash.*`, `smelt.notebook.*`
//! renderer primitives. Any plugin can render syntax-highlit content
//! into a `ui::Buffer` it owns — same pipeline the built-in confirm
//! dialog uses, no longer confirm-private.
//!
//! Each primitive resolves a `BufId` (minted via `smelt.buf.create`),
//! grabs the term width + current theme snapshot, and runs the
//! `LayoutSink` projection through `crate::content::to_buffer::render_into_buffer`.

use mlua::prelude::*;

use crate::content::highlight::{print_inline_diff, print_syntax_file};
use crate::content::to_buffer::render_into_buffer;
use crate::theme;
use ui::BufId;

/// Wire `smelt.diff.*`, `smelt.syntax.*` (and friends as they land).
pub fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    register_diff(lua, smelt)?;
    register_syntax(lua, smelt)?;
    Ok(())
}

fn register_diff(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let diff = lua.create_table()?;
    diff.set(
        "render",
        lua.create_function(|_, (buf_id, opts): (u64, mlua::Table)| {
            let old: String = opts.get::<Option<String>>("old")?.unwrap_or_default();
            let new: String = opts.get::<Option<String>>("new")?.unwrap_or_default();
            let path: String = opts.get::<Option<String>>("path")?.unwrap_or_default();
            crate::lua::with_app(|app| {
                let theme_snap = theme::snapshot();
                let width = crate::content::term_width() as u16;
                if let Some(buf) = app.ui.buf_mut(BufId(buf_id)) {
                    render_into_buffer(buf, width, &theme_snap, |sink| {
                        print_inline_diff(sink, &old, &new, &path, &old, 0, u16::MAX);
                    });
                }
            });
            Ok(())
        })?,
    )?;
    smelt.set("diff", diff)?;
    Ok(())
}

fn register_syntax(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let syntax = lua.create_table()?;
    syntax.set(
        "render",
        lua.create_function(|_, (buf_id, opts): (u64, mlua::Table)| {
            let content: String = opts.get::<Option<String>>("content")?.unwrap_or_default();
            let path: String = opts.get::<Option<String>>("path")?.unwrap_or_default();
            crate::lua::with_app(|app| {
                let theme_snap = theme::snapshot();
                let width = crate::content::term_width() as u16;
                if let Some(buf) = app.ui.buf_mut(BufId(buf_id)) {
                    render_into_buffer(buf, width, &theme_snap, |sink| {
                        print_syntax_file(sink, &content, &path, 0, u16::MAX);
                    });
                }
            });
            Ok(())
        })?,
    )?;
    smelt.set("syntax", syntax)?;
    Ok(())
}
