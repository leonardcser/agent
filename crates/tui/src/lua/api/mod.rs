//! `smelt.*` binding setup. `LuaRuntime::register_api` orchestrates four
//! domain modules — each one builds a coherent subtree of the `smelt`
//! global via a `register_*` helper:
//!
//! - [`state`] — live engine reads (transcript, engine, session, process,
//!   shell, agent, permissions, history, fuzzy).
//! - [`widgets`] — UI primitives (theme, buf, win, ui.*, prompt, settings).
//! - [`dispatch`] — registration surfaces (cmd, keymap, task, tools,
//!   statusline, on, defer, spawn).
//!
//! Top-level convenience bindings (notify, clipboard, confirm) and shared
//! helpers (color/theme/json conversion) live here.

mod dispatch;
mod state;
mod widgets;

use super::{LuaRuntime, LuaShared};
use mlua::prelude::*;
use std::sync::Arc;

/// Register a 0-arg getter that reads live state from `App` via
/// `try_with_app`. Replaces the old snapshot-mirror pattern — every
/// read goes through the TLS pointer installed at the top of each
/// tick / Lua-entry boundary.
///
/// Reads use `try_with_app` (not `with_app`) so callers from a context
/// without `install_app_ptr` get the type's `Default` instead of a
/// panic. In production every Lua-entry path installs the pointer, so
/// the fallback is dead; tests that exercise bindings without an App
/// get empty/zeroed values rather than panics.
macro_rules! app_read {
    ($lua:expr, |$app:ident| $body:expr) => {{
        $lua.create_function(
            |_, ()| Ok(crate::lua::try_with_app(|$app| $body).unwrap_or_default()),
        )?
    }};
}
pub(crate) use app_read;

impl LuaRuntime {
    pub(super) fn register_api(lua: &Lua, shared: &Arc<LuaShared>) -> LuaResult<()> {
        let smelt = lua.create_table()?;
        let smelt_ui = lua.create_table()?;
        let smelt_keymap = lua.create_table()?;

        smelt.set("version", crate::api::VERSION)?;

        state::register(lua, &smelt, shared)?;
        widgets::register(lua, &smelt, &smelt_ui, shared)?;
        dispatch::register(lua, &smelt, &smelt_keymap, shared)?;

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
        smelt.set(
            "clipboard",
            lua.create_function(|_, text: String| {
                crate::app::commands::copy_to_clipboard(&text).map_err(LuaError::RuntimeError)?;
                Ok(())
            })?,
        )?;

        smelt.set("ui", smelt_ui)?;
        smelt.set("keymap", smelt_keymap)?;

        // smelt.confirm.* primitives consumed by confirm.lua.
        crate::lua::confirm_ops::register(lua, &smelt)?;
        // smelt.{diff,syntax,…} renderer primitives shared by every
        // plugin that wants to draw highlit content into a buffer.
        crate::lua::render_ops::register(lua, &smelt)?;

        lua.globals().set("smelt", smelt)?;

        super::load_bootstrap_chunks(lua)?;

        Ok(())
    }
}

// ── theme + color helpers ──────────────────────────────────────────────

/// Encode a `crossterm::style::Color` as a Lua table.
///
/// Shapes: `{ ansi = u8 }` for palette colors, `{ rgb = { r, g, b } }`
/// for truecolor, `{ named = "red" }` for the 16 legacy names.
pub(super) fn color_to_lua(lua: &Lua, color: crossterm::style::Color) -> LuaResult<mlua::Table> {
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
pub(super) fn color_ansi_from_lua(table: &mlua::Table) -> LuaResult<u8> {
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
pub(super) fn theme_role_get(role: &str) -> Option<crossterm::style::Color> {
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
pub(super) fn theme_role_set(role: &str, ansi: u8) -> LuaResult<()> {
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
pub(super) fn theme_snapshot_pairs() -> Vec<(&'static str, crossterm::style::Color)> {
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

/// Treat a Lua table as a `{ string => json }` arg map, the shape every
/// tool call accepts. Skips non-string keys.
pub(super) fn lua_table_to_args(
    lua: &Lua,
    table: &mlua::Table,
) -> std::collections::HashMap<String, serde_json::Value> {
    let mut out = std::collections::HashMap::new();
    for pair in table.pairs::<mlua::Value, mlua::Value>().flatten() {
        let (k, v) = pair;
        let key = match k {
            mlua::Value::String(s) => s.to_string_lossy().to_string(),
            _ => continue,
        };
        out.insert(key, lua_value_to_json(lua, &v));
    }
    out
}
