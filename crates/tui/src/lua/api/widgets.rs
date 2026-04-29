//! UI primitives — theme colors, buffer / window manipulation, ui.*
//! float surfaces (ghost_text, spinner, picker, dialog), the prompt
//! input, and user-preference settings.

use super::{
    app_read, color_ansi_from_lua, color_to_lua, theme_role_get, theme_role_set,
    theme_snapshot_pairs,
};
use crate::lua::{parse_keybind, parse_win_event, LuaShared};
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(
    lua: &Lua,
    smelt: &mlua::Table,
    smelt_ui: &mlua::Table,
    shared: &Arc<LuaShared>,
) -> LuaResult<()> {
    register_theme(lua, smelt)?;
    register_buf(lua, smelt, shared)?;
    register_win(lua, smelt, shared)?;
    register_ghost_text(lua, smelt_ui)?;
    register_spinner(lua, smelt_ui)?;
    register_picker(lua, smelt_ui)?;
    register_dialog(lua, smelt_ui)?;
    register_prompt(lua, smelt)?;
    register_settings(lua, smelt)?;
    Ok(())
}

fn register_theme(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
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
    Ok(())
}

fn register_buf(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
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
                        Some(mode) => Some(
                            crate::format::BufFormat::from_lua_spec(&mode, t)
                                .map_err(|e| LuaError::RuntimeError(format!("buf.create: {e}")))?,
                        ),
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
                            app.notify_error(format!("buf.create: id {} already in use", clash.0));
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
    Ok(())
}

fn register_win(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
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
    Ok(())
}

fn register_ghost_text(lua: &Lua, smelt_ui: &mlua::Table) -> LuaResult<()> {
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
    Ok(())
}

fn register_spinner(lua: &Lua, smelt_ui: &mlua::Table) -> LuaResult<()> {
    // Same glyph set and cadence the status bar uses for its
    // "working" pill, exposed as primitives so Lua plugins (e.g.
    // /btw's "thinking" placeholder) can animate in lockstep with
    // the rest of the UI. Lua drives the animation via
    // `smelt.defer(period_ms, tick)`; `glyph()` returns the current
    // frame without any server-side state.
    let spinner_tbl = lua.create_table()?;
    spinner_tbl.set(
        "glyph",
        lua.create_function(|_, ()| Ok(crate::content::spinner_glyph()))?,
    )?;
    spinner_tbl.set(
        "period_ms",
        lua.create_function(|_, ()| Ok(crate::content::SPINNER_FRAME_MS))?,
    )?;
    smelt_ui.set("spinner", spinner_tbl)?;
    Ok(())
}

fn register_picker(lua: &Lua, smelt_ui: &mlua::Table) -> LuaResult<()> {
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
            let win_id = crate::lua::with_app(|app| crate::lua::ui_ops::open_picker(app, opts))
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
                let it =
                    crate::lua::ui_ops::parse_picker_item(&v).map_err(LuaError::RuntimeError)?;
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
    Ok(())
}

fn register_dialog(lua: &Lua, smelt_ui: &mlua::Table) -> LuaResult<()> {
    let dialog_tbl = lua.create_table()?;

    // smelt.ui.dialog._open(opts) → win_id. The richer
    // `{ win, panels = {…} }` handle is built Lua-side in
    // `runtime/lua/smelt/dialog.lua` (`make_handle`) so the Rust
    // surface stays small and we can iterate panel methods in Lua.
    dialog_tbl.set(
        "_open",
        lua.create_function(|_, opts: mlua::Table| -> LuaResult<u64> {
            let win_id = crate::lua::with_app(|app| crate::lua::ui_ops::open_dialog(app, opts))
                .map_err(|e| LuaError::RuntimeError(format!("dialog.open: {e}")))?;
            Ok(win_id.0)
        })?,
    )?;

    // Generic panel-focus primitive — panel handles built in
    // `dialog.lua` close over `(win_id, panel_idx)` and call this
    // when their `:focus()` method runs. Replaces confirm-specific
    // shims like `_focus_reason` that used to hardcode panel indices.
    // Scrolling deliberately isn't here: interactive content panels
    // are full `Window`s now, so they scroll themselves via mouse
    // wheel / vim motions when focused.
    dialog_tbl.set(
        "_panel_focus",
        lua.create_function(|_, (win_id, panel_idx): (u64, usize)| {
            crate::lua::with_app(|app| {
                if let Some(dialog) = app.ui.dialog_mut(ui::WinId(win_id)) {
                    dialog.focus_panel(panel_idx);
                }
            });
            Ok(())
        })?,
    )?;

    smelt_ui.set("dialog", dialog_tbl)?;
    Ok(())
}

fn register_prompt(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
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
    Ok(())
}

fn register_settings(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    // smelt.settings — user preference booleans (vim, auto-compact,
    // etc.). `snapshot()` returns the current state as a table;
    // `toggle(key)` flips one by name. Used by `/settings` to build
    // its picker entirely in Lua.
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
    Ok(())
}
