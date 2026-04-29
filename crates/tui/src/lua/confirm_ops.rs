//! `smelt.confirm._*` primitives consumed by
//! `runtime/lua/smelt/confirm.lua`.
//!
//! The Lua side owns dialog orchestration (open the float, attach
//! keymaps, route Submit / Dismiss) and now composes the summary +
//! preview buffers itself via `smelt.{diff,syntax,bash,notebook}.render`.
//! Rust exposes:
//!
//! - `_get(handle_id)` — full request snapshot (tool_name, desc,
//!   summary, args, outside_dir, approval_patterns, cwd_label, options).
//!   `options` is the pre-built label array; index into it on resolve.
//! - `_render_title(buf_id, handle_id)` — fills the title buffer.
//!   Stays Rust-side because the title's inline bash-highlight on the
//!   desc needs span-level composition we don't expose to Lua yet.
//! - `_back_tab` — toggles app mode + auto-allows when the new mode
//!   covers this request.
//! - `_resolve` — final pick, removes the registry entry.
//!
//! Per-panel control (`scroll_by`, `focus`, …) goes through the
//! generic `smelt.ui.dialog._panel_*` primitives surfaced by the
//! typed panel handles in `runtime/lua/smelt/dialog.lua`.

use mlua::prelude::*;

use crate::app::dialogs::confirm;
use crate::app::transcript_model::ConfirmChoice;

/// Wire `smelt.confirm.*` primitives onto the supplied table.
pub fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let confirm_tbl = lua.create_table()?;

    // smelt.confirm._get(handle_id) → full request snapshot or nil.
    confirm_tbl.set(
        "_get",
        lua.create_function(|lua, handle_id: u64| {
            let snapshot = crate::lua::with_app(|app| {
                let entry = app.confirm_requests.get(&handle_id)?;
                let req = &entry.req;
                let (labels, _) = confirm::build_options(req);
                Some(RequestSnapshot {
                    tool_name: req.tool_name.clone(),
                    desc: req.desc.clone(),
                    summary: req.summary.clone(),
                    outside_dir: req.outside_dir.as_ref().map(|p| p.to_string_lossy().into()),
                    approval_patterns: req.approval_patterns.clone(),
                    args: req.args.clone(),
                    cwd_label: cwd_label(),
                    options: labels,
                })
            });
            match snapshot {
                Some(s) => Ok(mlua::Value::Table(s.into_lua_table(lua)?)),
                None => Ok(mlua::Value::Nil),
            }
        })?,
    )?;

    // smelt.confirm._render_title(buf_id, handle_id) — fill an
    // existing buffer with the title line.
    confirm_tbl.set(
        "_render_title",
        lua.create_function(|_, (buf_id, handle_id): (u64, u64)| {
            crate::lua::with_app(|app| {
                let req = match app.confirm_requests.get(&handle_id) {
                    Some(e) => e.req.clone(),
                    None => return,
                };
                confirm::render_title_into_buf(app, ui::BufId(buf_id), &req);
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

struct RequestSnapshot {
    tool_name: String,
    desc: String,
    summary: Option<String>,
    outside_dir: Option<String>,
    approval_patterns: Vec<String>,
    args: std::collections::HashMap<String, serde_json::Value>,
    cwd_label: String,
    options: Vec<String>,
}

impl RequestSnapshot {
    fn into_lua_table(self, lua: &Lua) -> LuaResult<mlua::Table> {
        let t = lua.create_table()?;
        t.set("tool_name", self.tool_name)?;
        t.set("desc", self.desc)?;
        t.set("summary", self.summary.unwrap_or_default())?;
        match self.outside_dir {
            Some(s) => t.set("outside_dir", s)?,
            None => t.set("outside_dir", mlua::Value::Nil)?,
        }
        let patterns = lua.create_table()?;
        for (i, p) in self.approval_patterns.into_iter().enumerate() {
            patterns.set(i + 1, p)?;
        }
        t.set("approval_patterns", patterns)?;
        let args = lua.create_table()?;
        for (k, v) in &self.args {
            args.set(k.as_str(), crate::lua::json_to_lua(lua, v)?)?;
        }
        t.set("args", args)?;
        t.set("cwd_label", self.cwd_label)?;
        let opts = lua.create_table()?;
        for (i, label) in self.options.into_iter().enumerate() {
            opts.set(i + 1, label)?;
        }
        t.set("options", opts)?;
        Ok(t)
    }
}

/// Same `~/path` rewrite the Rust-side `build_options` uses, hoisted
/// here so the Lua snapshot can compose extra labels without hopping
/// back into Rust.
fn cwd_label() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| {
            let home = engine::home_dir();
            if let Ok(rel) = p.strip_prefix(&home) {
                return Some(format!("~/{}", rel.display()));
            }
            p.to_str().map(String::from)
        })
        .unwrap_or_default()
}
