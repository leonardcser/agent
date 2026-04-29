//! `smelt.diff.*`, `smelt.syntax.*`, `smelt.bash.*`, `smelt.notebook.*`
//! renderer primitives. Any plugin can render syntax-highlit content
//! into a `ui::Buffer` it owns — same pipeline the built-in confirm
//! dialog uses, no longer confirm-private.
//!
//! Each primitive resolves a `BufId` (minted via `smelt.buf.create`),
//! grabs the term width + current theme snapshot, and runs the
//! span-collector projection through `crate::content::to_buffer::render_into_buffer`.

use mlua::prelude::*;
use std::collections::HashMap;

use crate::app::dialogs::confirm_preview::ConfirmPreview;
use crate::content::highlight::{print_inline_diff, print_syntax_file, BashHighlighter};

use crate::content::to_buffer::render_into_buffer;
use crate::theme;
use ui::BufId;

/// Wire `smelt.{diff,syntax,bash,notebook}.*`.
pub fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    register_diff(lua, smelt)?;
    register_syntax(lua, smelt)?;
    register_bash(lua, smelt)?;
    register_notebook(lua, smelt)?;
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

fn register_bash(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let bash = lua.create_table()?;
    bash.set(
        "render",
        lua.create_function(|_, (buf_id, command): (u64, String)| {
            crate::lua::with_app(|app| {
                let theme_snap = theme::snapshot();
                let width = crate::content::term_width() as u16;
                if let Some(buf) = app.ui.buf_mut(BufId(buf_id)) {
                    render_into_buffer(buf, width, &theme_snap, |sink| {
                        let mut bh = BashHighlighter::new();
                        for line in command.lines() {
                            sink.print(" ");
                            bh.print_line(sink, line);
                            sink.newline();
                        }
                    });
                }
            });
            Ok(())
        })?,
    )?;
    smelt.set("bash", bash)?;
    Ok(())
}

fn register_notebook(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let notebook = lua.create_table()?;
    notebook.set(
        "render",
        lua.create_function(|_, (buf_id, args): (u64, mlua::Table)| {
            let args = lua_table_to_json_map(&args)
                .map_err(|e| LuaError::RuntimeError(format!("notebook.render: {e}")))?;
            crate::lua::with_app(|app| {
                let theme_snap = theme::snapshot();
                let width = crate::content::term_width() as u16;
                let preview = ConfirmPreview::from_tool("edit_notebook", "", &args);
                if !preview.is_some() {
                    return;
                }
                if let Some(buf) = app.ui.buf_mut(BufId(buf_id)) {
                    preview.render_into_buffer(buf, width, &theme_snap);
                }
            });
            Ok(())
        })?,
    )?;
    smelt.set("notebook", notebook)?;
    Ok(())
}

/// Shallow Lua → JSON map conversion for tool-arg tables. Strings,
/// numbers, booleans, nil pass through; nested tables become arrays
/// or objects depending on whether they look sequence-shaped.
/// Used by `smelt.notebook.render` so the existing Rust-side
/// `ConfirmPreview::from_tool` can consume Lua-supplied args.
fn lua_table_to_json_map(t: &mlua::Table) -> mlua::Result<HashMap<String, serde_json::Value>> {
    let mut out = HashMap::new();
    for pair in t.clone().pairs::<String, mlua::Value>() {
        let (k, v) = pair?;
        out.insert(k, lua_value_to_json(&v)?);
    }
    Ok(out)
}

fn lua_value_to_json(v: &mlua::Value) -> mlua::Result<serde_json::Value> {
    use serde_json::Value as J;
    Ok(match v {
        mlua::Value::Nil => J::Null,
        mlua::Value::Boolean(b) => J::Bool(*b),
        mlua::Value::Integer(i) => J::Number((*i).into()),
        mlua::Value::Number(n) => serde_json::Number::from_f64(*n)
            .map(J::Number)
            .unwrap_or(J::Null),
        mlua::Value::String(s) => J::String(s.to_str()?.to_string()),
        mlua::Value::Table(t) => {
            let len = t.raw_len();
            if len > 0 {
                let mut arr = Vec::with_capacity(len);
                for i in 1..=len {
                    arr.push(lua_value_to_json(&t.raw_get::<mlua::Value>(i)?)?);
                }
                J::Array(arr)
            } else {
                let mut obj = serde_json::Map::new();
                for pair in t.clone().pairs::<String, mlua::Value>() {
                    let (k, v) = pair?;
                    obj.insert(k, lua_value_to_json(&v)?);
                }
                J::Object(obj)
            }
        }
        _ => J::Null,
    })
}
