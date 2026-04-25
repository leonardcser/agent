//! `smelt.confirm._*` primitives consumed by
//! `runtime/lua/smelt/confirm.lua`.
//!
//! The Lua side owns dialog orchestration (open the float, attach
//! keymaps, route Submit / Dismiss). Rust owns the heavy rendering
//! and the canonical request state — buffers go through
//! `crate::app::dialogs::confirm::build_*_buf`, choices through
//! `build_options`, and resolution through `App::handle_confirm_*`
//! methods on `lua_handlers`. Each primitive looks up the live
//! request by `handle_id` against `App::confirm_requests`.

use mlua::prelude::*;

use crate::app::dialogs::confirm;
use crate::app::transcript_model::ConfirmChoice;

const PANEL_PREVIEW: usize = 2;
const PANEL_REASON: usize = 4;

/// Wire `smelt.confirm.*` primitives onto the supplied table.
pub fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let confirm_tbl = lua.create_table()?;

    // smelt.confirm._info(handle_id) → { tool_name, desc, summary }.
    // Lua reads this for the dialog title / hints; absent handle =>
    // nil so callers can guard.
    confirm_tbl.set(
        "_info",
        lua.create_function(|lua, handle_id: u64| {
            let info = crate::lua::with_app(|app| {
                let entry = app.confirm_requests.get(&handle_id)?;
                Some((
                    entry.req.tool_name.clone(),
                    entry.req.desc.clone(),
                    entry.req.summary.clone(),
                ))
            });
            match info {
                Some((tool, desc, summary)) => {
                    let t = lua.create_table()?;
                    t.set("tool_name", tool)?;
                    t.set("desc", desc)?;
                    t.set("summary", summary.unwrap_or_default())?;
                    Ok(mlua::Value::Table(t))
                }
                None => Ok(mlua::Value::Nil),
            }
        })?,
    )?;

    // smelt.confirm._build_title_buf(handle_id) → buf_id.
    confirm_tbl.set(
        "_build_title_buf",
        lua.create_function(|_, handle_id: u64| {
            let buf_id = crate::lua::with_app(|app| {
                let req = app.confirm_requests.get(&handle_id)?.req.clone();
                Some(confirm::build_title_buf(app, &req).0)
            });
            Ok(buf_id)
        })?,
    )?;

    // smelt.confirm._build_summary_buf(handle_id) → buf_id.
    confirm_tbl.set(
        "_build_summary_buf",
        lua.create_function(|_, handle_id: u64| {
            let buf_id = crate::lua::with_app(|app| {
                let req = app.confirm_requests.get(&handle_id)?.req.clone();
                Some(confirm::build_summary_buf(app, &req).0)
            });
            Ok(buf_id)
        })?,
    )?;

    // smelt.confirm._build_preview_buf(handle_id) → buf_id.
    confirm_tbl.set(
        "_build_preview_buf",
        lua.create_function(|_, handle_id: u64| {
            let buf_id = crate::lua::with_app(|app| {
                let req = app.confirm_requests.get(&handle_id)?.req.clone();
                Some(confirm::build_preview_buf(app, &req).0)
            });
            Ok(buf_id)
        })?,
    )?;

    // smelt.confirm._option_labels(handle_id) → { "yes", "no", … }.
    // Choices are registered when `agent.rs` inserts the request, so
    // this just rebuilds the parallel labels array.
    confirm_tbl.set(
        "_option_labels",
        lua.create_function(|lua, handle_id: u64| {
            let labels = crate::lua::with_app(|app| {
                let req = app.confirm_requests.get(&handle_id)?.req.clone();
                let (labels, _) = confirm::build_options(&req);
                Some(labels)
            });
            match labels {
                Some(labels) => {
                    let out = lua.create_table()?;
                    for (i, l) in labels.into_iter().enumerate() {
                        out.set(i + 1, l)?;
                    }
                    Ok(mlua::Value::Table(out))
                }
                None => Ok(mlua::Value::Nil),
            }
        })?,
    )?;

    // smelt.confirm._scroll_preview(win_id, dir).
    // dir: -1 = page up, 1 = page down. Half-page steps to mirror
    // the previous Rust behaviour.
    confirm_tbl.set(
        "_scroll_preview",
        lua.create_function(|_, (win_id, dir): (u64, i64)| {
            crate::lua::with_app(|app| {
                if let Some(dialog) = app.ui.dialog_mut(ui::WinId(win_id)) {
                    let page = (dialog.panel_rect_height(PANEL_PREVIEW).max(1) as isize) / 2;
                    dialog.panel_scroll_by(PANEL_PREVIEW, dir as isize * page);
                }
            });
            Ok(())
        })?,
    )?;

    // smelt.confirm._focus_reason(win_id).
    confirm_tbl.set(
        "_focus_reason",
        lua.create_function(|_, win_id: u64| {
            crate::lua::with_app(|app| {
                if let Some(dialog) = app.ui.dialog_mut(ui::WinId(win_id)) {
                    dialog.focus_panel(PANEL_REASON);
                }
            });
            Ok(())
        })?,
    )?;

    // smelt.confirm._back_tab(handle_id) → bool. Toggles the app
    // mode; returns true when the new mode auto-allows this request
    // (caller closes the dialog) and false otherwise (dialog stays
    // open so the user can pick manually).
    confirm_tbl.set(
        "_back_tab",
        lua.create_function(|_, handle_id: u64| {
            let auto_allowed = crate::lua::with_app(|app| {
                let entry = match app.confirm_requests.get(&handle_id) {
                    Some(e) => e,
                    None => return false,
                };
                let request_id = entry.req.request_id;
                let call_id = entry.req.call_id.clone();
                let tool_name = entry.req.tool_name.clone();
                let args = entry.req.args.clone();
                app.toggle_mode();
                if app.permissions.decide(app.mode, &tool_name, &args, false)
                    == engine::permissions::Decision::Allow
                {
                    app.set_active_status(
                        &call_id,
                        crate::app::transcript_model::ToolStatus::Pending,
                    );
                    app.send_permission_decision(request_id, true, None);
                    app.confirm_requests.remove(&handle_id);
                    true
                } else {
                    false
                }
            });
            Ok(auto_allowed)
        })?,
    )?;

    // smelt.confirm._resolve(handle_id, choice_idx, message?).
    // `choice_idx` is 1-based to match Lua. Removes the registry
    // entry; the caller is expected to close the dialog.
    confirm_tbl.set(
        "_resolve",
        lua.create_function(
            |_, (handle_id, choice_idx, message): (u64, usize, Option<String>)| {
                crate::lua::with_app(|app| {
                    let entry = match app.confirm_requests.remove(&handle_id) {
                        Some(e) => e,
                        None => return,
                    };
                    let choice = entry
                        .choices
                        .get(choice_idx.saturating_sub(1))
                        .cloned()
                        .unwrap_or(ConfirmChoice::No);
                    let request_id = entry.req.request_id;
                    let call_id = entry.req.call_id.clone();
                    let tool_name = entry.req.tool_name.clone();
                    app.handle_confirm_resolve(choice, message, request_id, &call_id, &tool_name);
                });
                Ok(())
            },
        )?,
    )?;

    smelt.set("confirm", confirm_tbl)?;
    Ok(())
}
