//! Focusable `ui::Picker` float opened by a parked `LuaTask` that
//! yielded `smelt.api.picker.open({...})`. On Enter/Escape, resumes
//! the task with `{ index, item }` or `nil`.

use super::super::App;
use crate::app::ops::AppOp;
use crate::lua::TaskEvent;
use crossterm::event::{KeyCode, KeyModifiers};
use mlua::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use ui::{Callback, CallbackResult, Constraint, FloatConfig, KeyBind, Placement, WinId};

/// Stash for the picker's opts registry key. Taken once on resolve so
/// the pump can look up the picked item.
struct PickerState {
    picker_id: u64,
    opts: Option<mlua::RegistryKey>,
}

/// Open a focusable picker float driven by a parked `LuaTask`. On
/// success, binds Up/Down/Enter/Escape to callbacks that push
/// `TaskEvent::PickerResolved` + `AppOp::CloseFloat`. Returns `Err`
/// when the opts table is malformed — the caller should resolve the
/// parked task with `nil` in that case.
pub fn open(app: &mut App, picker_id: u64, opts_key: mlua::RegistryKey) -> Result<(), String> {
    let lua = app.lua.lua();
    let opts: mlua::Table = lua
        .registry_value(&opts_key)
        .map_err(|e| format!("picker opts: {e}"))?;

    let items_tbl: mlua::Table = opts
        .get("items")
        .map_err(|e| format!("picker items: {e}"))?;
    let mut items: Vec<ui::picker::PickerItem> = Vec::new();
    for pair in items_tbl.sequence_values::<mlua::Value>() {
        let v = pair.map_err(|e| format!("picker item: {e}"))?;
        items.push(parse_item(&v)?);
    }
    if items.is_empty() {
        return Err("picker.open: items must be non-empty".into());
    }

    let placement = parse_placement(&opts);
    let title: Option<String> = opts.get("title").ok();

    let float_config = FloatConfig {
        title,
        border: ui::Border::Rounded,
        placement,
        focusable: true,
        blocks_agent: false,
        ..Default::default()
    };

    let win_id = app
        .ui
        .picker_open(float_config, items, 0, Default::default())
        .ok_or_else(|| "picker.open: failed to create float".to_string())?;

    let state = Rc::new(RefCell::new(PickerState {
        picker_id,
        opts: Some(opts_key),
    }));

    // Navigation keymaps (don't resolve).
    bind_move(app, win_id, KeyCode::Up, KeyModifiers::NONE, -1);
    bind_move(app, win_id, KeyCode::Down, KeyModifiers::NONE, 1);
    bind_move(app, win_id, KeyCode::Char('k'), KeyModifiers::CONTROL, -1);
    bind_move(app, win_id, KeyCode::Char('j'), KeyModifiers::CONTROL, 1);
    bind_move(app, win_id, KeyCode::Char('p'), KeyModifiers::CONTROL, -1);
    bind_move(app, win_id, KeyCode::Char('n'), KeyModifiers::CONTROL, 1);

    // Submit — Enter with current selection.
    let ops = app.lua.ops_handle();
    let state_submit = state.clone();
    let ops_submit = ops.clone();
    app.ui.win_set_keymap(
        win_id,
        KeyBind::plain(KeyCode::Enter),
        Callback::Rust(Box::new(move |ctx| {
            let selected = ctx
                .ui
                .picker_mut(ctx.win)
                .map(|p| p.selected())
                .unwrap_or(0);
            let mut s = state_submit.borrow_mut();
            let opts = s.opts.take();
            if let Some(opts) = opts {
                ops_submit.push_task_event(TaskEvent::PickerResolved {
                    picker_id: s.picker_id,
                    selected_index: Some(selected),
                    opts,
                });
            }
            ops_submit.push(AppOp::CloseFloat(ctx.win));
            CallbackResult::Consumed
        })),
    );

    // Dismiss — Escape.
    let state_dismiss = state;
    let ops_dismiss = ops;
    app.ui.win_set_keymap(
        win_id,
        KeyBind::plain(KeyCode::Esc),
        Callback::Rust(Box::new(move |ctx| {
            let mut s = state_dismiss.borrow_mut();
            if let Some(opts) = s.opts.take() {
                ops_dismiss.push_task_event(TaskEvent::PickerResolved {
                    picker_id: s.picker_id,
                    selected_index: None,
                    opts,
                });
            }
            ops_dismiss.push(AppOp::CloseFloat(ctx.win));
            CallbackResult::Consumed
        })),
    );

    Ok(())
}

fn bind_move(app: &mut App, win: WinId, code: KeyCode, mods: KeyModifiers, delta: isize) {
    app.ui.win_set_keymap(
        win,
        KeyBind::new(code, mods),
        Callback::Rust(Box::new(move |ctx| {
            if let Some(picker) = ctx.ui.picker_mut(ctx.win) {
                let n = picker.items().len();
                if n == 0 {
                    return CallbackResult::Consumed;
                }
                let cur = picker.selected() as isize;
                let next = (cur + delta).rem_euclid(n as isize) as usize;
                picker.set_selected(next);
            }
            CallbackResult::Consumed
        })),
    );
}

fn parse_item(v: &LuaValue) -> Result<ui::picker::PickerItem, String> {
    match v {
        LuaValue::String(s) => Ok(ui::picker::PickerItem::new(s.to_string_lossy().to_string())),
        LuaValue::Table(t) => {
            let label: String = t
                .get("label")
                .map_err(|e| format!("picker item.label: {e}"))?;
            let mut item = ui::picker::PickerItem::new(label);
            if let Ok(desc) = t.get::<String>("description") {
                item = item.with_description(desc);
            }
            if let Ok(prefix) = t.get::<String>("prefix") {
                item = item.with_prefix(prefix);
            }
            Ok(item)
        }
        other => Err(format!(
            "picker item: expected string or table, got {}",
            other.type_name()
        )),
    }
}

fn parse_placement(opts: &mlua::Table) -> Placement {
    let mode: String = opts
        .get("placement")
        .ok()
        .unwrap_or_else(|| "center".to_string());
    match mode.as_str() {
        "bottom" => Placement::dock_bottom_full_width(Constraint::Pct(40)),
        "cursor" => Placement::AnchorCursor {
            row_offset: 1,
            col_offset: 0,
            width: Constraint::Fixed(48),
            height: Constraint::Pct(40),
        },
        _ => Placement::centered(Constraint::Pct(60), Constraint::Pct(50)),
    }
}
