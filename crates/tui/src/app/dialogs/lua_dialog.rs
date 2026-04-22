//! Dialog built from a Lua `opts` table and driven by a parked
//! `LuaTask`. Opened when a plugin tool / `smelt.task` coroutine yields
//! `smelt.api.dialog.open({...})`. When the user resolves the dialog
//! (selects an option, dismisses), the task is resumed with a result
//! table and continues executing.
//!
//! Supported panel kinds (from `opts.panels[]`):
//! - `{ kind = "content",  text = "..." }`      plain lines
//! - `{ kind = "markdown", text = "..." }`      rendered via `render_markdown_inner`
//! - `{ kind = "options",  items = [{label, action?, shortcut?, on_select?}] }`
//! - `{ kind = "input",    name = "x", placeholder? = "..." }`

use super::super::App;
use crate::app::ops::AppOp;
use crate::keymap::hints;
use crate::lua::TaskEvent;
use crossterm::event::{KeyCode, KeyModifiers};
use mlua::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use ui::buffer::BufCreateOpts;
use ui::text_input::TextInput;
use ui::{
    BufId, Callback, CallbackResult, KeyBind, OptionItem, OptionList, PanelHeight, PanelSpec,
    Payload, WinEvent, WinId,
};

/// Per-option data consumed when the task resumes. Drained out of
/// `DialogState` into the [`TaskEvent::DialogResolved`] payload when
/// the user selects an option, so the RegistryKey (which isn't Clone)
/// moves cleanly from the dialog's state to the Lua task-runtime
/// inbox.
struct OptionEntry {
    /// Action string reported back to the task (`result.action`).
    /// Defaults to `"select"` when the plugin didn't specify one.
    action: String,
    /// Optional `on_select` callback fired *before* the task resumes.
    on_select: Option<mlua::RegistryKey>,
}

/// Per-input panel, to gather final text on resolution.
struct InputEntry {
    name: String,
    panel_index: usize,
    /// Optional `on_change(ctx)` callback id. When present, each
    /// keystroke edit fires `TaskEvent::InputChanged`; when absent,
    /// edits are consumed silently.
    on_change: Option<u64>,
}

/// In-closure state for a Lua dialog. Held by `Rc<RefCell>` so the
/// Submit, Dismiss, and keymap callbacks share the same options /
/// inputs / on_press callback ids.
struct DialogState {
    dialog_id: u64,
    options: Vec<OptionEntry>,
    inputs: Vec<InputEntry>,
    /// `shared.callbacks` ids for each registered `on_press`. Removed
    /// on dialog close so the registry doesn't leak.
    keymap_callback_ids: Vec<u64>,
    /// `shared.callbacks` ids for each input panel's `on_change`.
    /// Removed alongside the keymap ids on dialog close.
    input_change_callback_ids: Vec<u64>,
    /// Optional dialog-level `on_tick` callback id.
    on_tick_id: Option<u64>,
}

/// Open the dialog described by the Lua `opts_key` table. On success,
/// registers Submit/Dismiss callbacks that resume the parked task.
/// Returns `Err` when the table is malformed — the caller should
/// resolve the parked task with an error string.
pub fn open(app: &mut App, dialog_id: u64, opts_key: mlua::RegistryKey) -> Result<(), String> {
    let opts: mlua::Table = app
        .lua
        .lua()
        .registry_value(&opts_key)
        .map_err(|e| format!("dialog opts: {e}"))?;
    let title: Option<String> = opts.get("title").ok();
    let panels_tbl: mlua::Table = opts
        .get("panels")
        .map_err(|e| format!("dialog panels: {e}"))?;

    let mut panel_specs: Vec<PanelSpec> = Vec::new();
    let mut options: Vec<OptionEntry> = Vec::new();
    let mut inputs: Vec<InputEntry> = Vec::new();

    for pair in panels_tbl.sequence_values::<mlua::Table>() {
        let panel = pair.map_err(|e| format!("dialog panel entry: {e}"))?;
        let kind: String = panel.get("kind").map_err(|e| format!("panel.kind: {e}"))?;
        let panel_index = panel_specs.len();
        let height = parse_height(&panel)?;
        match kind.as_str() {
            "content" => {
                let text: String = panel.get("text").unwrap_or_default();
                let buf = make_text_buffer(app, &text);
                panel_specs.push(PanelSpec::content(buf, height.unwrap_or(PanelHeight::Fit)));
            }
            "markdown" => {
                let text: String = panel.get("text").unwrap_or_default();
                let buf = make_markdown_buffer(app, &text);
                panel_specs.push(PanelSpec::content(buf, height.unwrap_or(PanelHeight::Fit)));
            }
            "options" => {
                let items_tbl: mlua::Table = panel
                    .get("items")
                    .map_err(|e| format!("options.items: {e}"))?;
                let mut list_items = Vec::new();
                for it_pair in items_tbl.sequence_values::<mlua::Table>() {
                    let it = it_pair.map_err(|e| format!("option item: {e}"))?;
                    let label: String = it.get("label").unwrap_or_default();
                    let shortcut: Option<char> = it
                        .get::<String>("shortcut")
                        .ok()
                        .and_then(|s| s.chars().next());
                    let mut item = OptionItem::new(label);
                    if let Some(c) = shortcut {
                        item = item.with_shortcut(c);
                    }
                    let action: String = it.get("action").unwrap_or_else(|_| "select".into());
                    let on_select = it
                        .get::<mlua::Function>("on_select")
                        .ok()
                        .and_then(|f| app.lua.lua().create_registry_value(f).ok());
                    list_items.push(item);
                    options.push(OptionEntry { action, on_select });
                }
                let multi: bool = panel.get("multi").unwrap_or(false);
                let widget = Box::new(
                    OptionList::new(list_items)
                        .multi(multi)
                        .with_cursor_style(accent_style())
                        .with_shortcut_style(accent_style()),
                );
                panel_specs.push(PanelSpec::widget(widget, PanelHeight::Fit));
            }
            "input" => {
                let name: String = panel.get("name").unwrap_or_default();
                let placeholder: Option<String> = panel.get("placeholder").ok();
                let mut ti = TextInput::new();
                if let Some(p) = placeholder {
                    ti = ti.with_placeholder(p);
                }
                let widget = Box::new(ti);
                panel_specs.push(PanelSpec::widget(widget, PanelHeight::Fit));
                let on_change = panel
                    .get::<mlua::Function>("on_change")
                    .ok()
                    .and_then(|f| app.lua.register_callback(f).ok());
                inputs.push(InputEntry {
                    name,
                    panel_index,
                    on_change,
                });
            }
            "list" => {
                // Buffer-backed selectable list. Plugin pre-creates a
                // buf via `smelt.api.buf.create` + `set_lines`, then
                // mutates it from `on_change` / `on_tick` callbacks.
                // Enter on a list panel submits with the row index.
                let buf_id: u64 = panel.get("buf").map_err(|e| format!("list.buf: {e}"))?;
                panel_specs.push(PanelSpec::list(
                    BufId(buf_id),
                    height.unwrap_or(PanelHeight::Fill),
                ));
            }
            other => return Err(format!("unknown panel kind: {other}")),
        }
    }

    if panel_specs.is_empty() {
        return Err("dialog must have at least one panel".into());
    }

    // Parse plugin keymaps up front so we can fold their hints into the
    // footer alongside the default Confirm / Cancel labels.
    // Each keymap entry is `{ key, on_press = function(ctx) ... end,
    // hint? }`. The `on_press` callback receives a `ctx` table with
    // `selected_index`, `inputs`, and `close()` — it decides whether
    // to mutate state or resolve the dialog.
    let mut keymaps: Vec<(KeyBind, u64)> = Vec::new();
    let mut keymap_callback_ids: Vec<u64> = Vec::new();
    let mut extra_hints: Vec<String> = Vec::new();
    if let Ok(km_tbl) = opts.get::<mlua::Table>("keymaps") {
        for entry_res in km_tbl.sequence_values::<mlua::Table>() {
            let entry = entry_res.map_err(|e| format!("keymap entry: {e}"))?;
            let key_str: String = entry.get("key").map_err(|e| format!("keymap.key: {e}"))?;
            let on_press: mlua::Function = entry
                .get("on_press")
                .map_err(|e| format!("keymap.on_press: {e}"))?;
            if let Ok(hint) = entry.get::<String>("hint") {
                if !hint.is_empty() {
                    extra_hints.push(hint);
                }
            }
            let callback_id = app
                .lua
                .register_callback(on_press)
                .map_err(|e| format!("keymap register: {e}"))?;
            keymaps.push((parse_key(&key_str)?, callback_id));
            keymap_callback_ids.push(callback_id);
        }
    }

    let mut hint_parts: Vec<&str> = vec![hints::CONFIRM, hints::CANCEL];
    for h in &extra_hints {
        hint_parts.push(h.as_str());
    }
    let dialog_config = app.builtin_dialog_config(Some(hints::join(&hint_parts)), vec![]);
    // Lua dialogs block the agent event drain until the task resumes.
    let win_id = app
        .ui
        .dialog_open(
            ui::FloatConfig {
                title,
                border: ui::Border::None,
                placement: ui::Placement::dock_bottom_full_width(ui::Constraint::Pct(60)),
                blocks_agent: true,
                ..Default::default()
            },
            dialog_config,
            panel_specs,
        )
        .ok_or_else(|| "failed to open dialog window".to_string())?;

    // Harvest on_change callback ids for cleanup on close.
    let input_change_callback_ids: Vec<u64> = inputs.iter().filter_map(|i| i.on_change).collect();

    // Optional top-level `on_tick = fn(ctx)` — fired every engine tick
    // while the dialog is open. Lets plugins refresh from live
    // external state (registry, process list) without reopening.
    let on_tick_id: Option<u64> = opts
        .get::<mlua::Function>("on_tick")
        .ok()
        .and_then(|f| app.lua.register_callback(f).ok());

    let state = Rc::new(RefCell::new(DialogState {
        dialog_id,
        options,
        inputs,
        keymap_callback_ids,
        input_change_callback_ids,
        on_tick_id,
    }));

    let ops = app.lua.ops_handle();

    let state_submit = state.clone();
    let ops_submit = ops.clone();
    app.ui.win_on_event(
        win_id,
        WinEvent::Submit,
        Callback::Rust(Box::new(move |ctx| {
            let idx = match ctx.payload {
                Payload::Selection { index } => index,
                _ => 0,
            };
            let mut s = state_submit.borrow_mut();
            let (action, on_select) = match s.options.get_mut(idx) {
                Some(entry) => (entry.action.clone(), entry.on_select.take()),
                None => ("select".to_string(), None),
            };
            let inputs = collect_inputs(ctx.ui, ctx.win, &s.inputs);
            for id in &s.keymap_callback_ids {
                ops_submit.remove_callback(*id);
            }
            for id in &s.input_change_callback_ids {
                ops_submit.remove_callback(*id);
            }
            if let Some(id) = s.on_tick_id {
                ops_submit.remove_callback(id);
            }
            ops_submit.push_task_event(TaskEvent::DialogResolved {
                dialog_id: s.dialog_id,
                action,
                option_index: Some(idx + 1),
                inputs,
                on_select,
            });
            ops_submit.push(AppOp::CloseFloat(ctx.win));
            CallbackResult::Consumed
        })),
    );

    // Custom keymaps: each fires `TaskEvent::KeymapFired`, which the
    // Lua runtime's `pump_task_events` routes to the registered
    // `on_press(ctx)` callback. The dialog stays open; the callback
    // decides what to do via `ctx.close()`, other API calls, etc.
    for (kb, callback_id) in keymaps {
        let state_k = state.clone();
        let ops_k = ops.clone();
        app.ui.win_set_keymap(
            win_id,
            kb,
            Callback::Rust(Box::new(move |ctx| {
                let idx = ctx.ui.dialog_mut(ctx.win).and_then(|d| d.selected_index());
                let s = state_k.borrow();
                let inputs = collect_inputs(ctx.ui, ctx.win, &s.inputs);
                ops_k.push_task_event(TaskEvent::KeymapFired {
                    callback_id,
                    dialog_id: s.dialog_id,
                    win_id: ctx.win,
                    selected_index: idx,
                    inputs,
                });
                CallbackResult::Consumed
            })),
        );
    }

    // Input panel text-change events. If any input panel registered an
    // `on_change` callback, install a single `WinEvent::TextChanged`
    // handler that fans out to per-input callbacks (there's usually
    // only one input panel per dialog, but the dispatch handles
    // multiple).
    if state.borrow().inputs.iter().any(|i| i.on_change.is_some()) {
        let state_c = state.clone();
        let ops_c = ops.clone();
        app.ui.win_on_event(
            win_id,
            WinEvent::TextChanged,
            Callback::Rust(Box::new(move |ctx| {
                let idx = ctx.ui.dialog_mut(ctx.win).and_then(|d| d.selected_index());
                let s = state_c.borrow();
                let inputs = collect_inputs(ctx.ui, ctx.win, &s.inputs);
                for input in &s.inputs {
                    if let Some(cb_id) = input.on_change {
                        ops_c.push_task_event(TaskEvent::InputChanged {
                            callback_id: cb_id,
                            dialog_id: s.dialog_id,
                            win_id: ctx.win,
                            selected_index: idx,
                            inputs: inputs.clone(),
                        });
                    }
                }
                CallbackResult::Consumed
            })),
        );
    }

    // Dialog-level tick callback. Fires on every engine tick while the
    // dialog is open, lets the plugin refresh panel buffers from live
    // external state (subagent registry, process list, session list
    // cache). No payload — callback re-queries whatever it needs.
    if let Some(tick_cb_id) = state.borrow().on_tick_id {
        let state_t = state.clone();
        let ops_t = ops.clone();
        app.ui.win_on_event(
            win_id,
            WinEvent::Tick,
            Callback::Rust(Box::new(move |ctx| {
                let idx = ctx.ui.dialog_mut(ctx.win).and_then(|d| d.selected_index());
                let s = state_t.borrow();
                let inputs = collect_inputs(ctx.ui, ctx.win, &s.inputs);
                ops_t.push_task_event(TaskEvent::TickFired {
                    callback_id: tick_cb_id,
                    dialog_id: s.dialog_id,
                    win_id: ctx.win,
                    selected_index: idx,
                    inputs,
                });
                CallbackResult::Consumed
            })),
        );
    }

    let state_dismiss = state;
    app.ui.win_on_event(
        win_id,
        WinEvent::Dismiss,
        Callback::Rust(Box::new(move |ctx| {
            let s = state_dismiss.borrow();
            for id in &s.keymap_callback_ids {
                ops.remove_callback(*id);
            }
            for id in &s.input_change_callback_ids {
                ops.remove_callback(*id);
            }
            if let Some(id) = s.on_tick_id {
                ops.remove_callback(id);
            }
            ops.push_task_event(TaskEvent::DialogResolved {
                dialog_id: s.dialog_id,
                action: "dismiss".into(),
                option_index: None,
                inputs: Vec::new(),
                on_select: None,
            });
            ops.push(AppOp::CloseFloat(ctx.win));
            CallbackResult::Consumed
        })),
    );

    Ok(())
}

/// Parse the per-panel `height` option. `"fit"` → Fit (auto-shrink),
/// `"fill"` → Fill (stretch + scroll), an integer → Fixed rows. Absent
/// → Ok(None) so the caller can pick a kind-appropriate default.
fn parse_height(panel: &mlua::Table) -> Result<Option<PanelHeight>, String> {
    match panel.get::<mlua::Value>("height").ok() {
        None | Some(mlua::Value::Nil) => Ok(None),
        Some(mlua::Value::String(s)) => match s.to_str().map_err(|e| e.to_string())?.as_ref() {
            "fit" => Ok(Some(PanelHeight::Fit)),
            "fill" => Ok(Some(PanelHeight::Fill)),
            other => Err(format!("panel.height: unknown value '{other}'")),
        },
        Some(mlua::Value::Integer(n)) if n > 0 => Ok(Some(PanelHeight::Fixed(n as u16))),
        Some(other) => Err(format!(
            "panel.height: expected 'fit' | 'fill' | int, got {other:?}"
        )),
    }
}

/// Parse a plugin-facing key string like `"bs"`, `"tab"`, `"ctrl-x"`,
/// `"shift-tab"` into a [`KeyBind`]. Case-insensitive for named keys;
/// single characters are taken verbatim.
fn parse_key(spec: &str) -> Result<KeyBind, String> {
    let raw = spec.trim();
    if raw.is_empty() {
        return Err("keymap.key: empty string".into());
    }
    let (mods, name) = match raw.rsplit_once('-') {
        Some((prefix, name)) => {
            let mut mods = KeyModifiers::NONE;
            for part in prefix.split('-') {
                match part.to_ascii_lowercase().as_str() {
                    "ctrl" | "c" => mods |= KeyModifiers::CONTROL,
                    "alt" | "a" | "meta" | "m" => mods |= KeyModifiers::ALT,
                    "shift" | "s" => mods |= KeyModifiers::SHIFT,
                    other => return Err(format!("keymap: unknown modifier '{other}'")),
                }
            }
            (mods, name)
        }
        None => (KeyModifiers::NONE, raw),
    };
    let code = match name.to_ascii_lowercase().as_str() {
        "bs" | "backspace" => KeyCode::Backspace,
        "tab" => {
            if mods.contains(KeyModifiers::SHIFT) {
                return Ok(KeyBind::new(KeyCode::BackTab, mods - KeyModifiers::SHIFT));
            }
            KeyCode::Tab
        }
        "del" | "delete" => KeyCode::Delete,
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        s if s.chars().count() == 1 => KeyCode::Char(name.chars().next().unwrap()),
        other => return Err(format!("keymap: unknown key '{other}'")),
    };
    Ok(KeyBind::new(code, mods))
}

fn collect_inputs(ui: &mut ui::Ui, win: WinId, entries: &[InputEntry]) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let text = ui
            .dialog_mut(win)
            .and_then(|d| d.panel_widget_mut::<TextInput>(entry.panel_index))
            .map(|w| w.text().to_string())
            .unwrap_or_default();
        out.push((entry.name.clone(), text));
    }
    out
}

/// Build the result table the Lua task resumes with. Called by
/// `LuaRuntime::pump_task_events` when handling
/// [`crate::lua::TaskEvent::DialogResolved`].
pub(crate) fn build_result(
    lua: &Lua,
    action: &str,
    option_index: Option<usize>,
    inputs: Vec<(String, String)>,
) -> LuaResult<mlua::Value> {
    let t = lua.create_table()?;
    t.set("action", action)?;
    if let Some(i) = option_index {
        t.set("option_index", i)?;
    }
    let inputs_tbl = lua.create_table()?;
    for (k, v) in inputs {
        inputs_tbl.set(k, v)?;
    }
    t.set("inputs", inputs_tbl)?;
    Ok(mlua::Value::Table(t))
}

/// Build the `ctx` table passed to an `on_press` callback. Carries the
/// current `selected_index`, input values, and a `close()` function
/// bound to this dialog's `dialog_id` / `win_id`. Called by
/// `LuaRuntime::pump_task_events` when handling
/// [`crate::lua::TaskEvent::KeymapFired`].
pub(crate) fn build_keymap_ctx(
    lua: &Lua,
    shared: std::sync::Arc<crate::lua::LuaShared>,
    dialog_id: u64,
    win_id: ui::WinId,
    selected_index: Option<usize>,
    inputs: Vec<(String, String)>,
) -> LuaResult<mlua::Value> {
    let t = lua.create_table()?;
    if let Some(i) = selected_index {
        t.set("selected_index", i + 1)?;
    }
    let inputs_tbl = lua.create_table()?;
    for (k, v) in inputs {
        inputs_tbl.set(k, v)?;
    }
    t.set("inputs", inputs_tbl)?;

    // ctx.close() — resolve the dialog as "dismiss" and close the win.
    let shared_close = shared.clone();
    t.set(
        "close",
        lua.create_function(move |_, ()| {
            if let Ok(mut inbox) = shared_close.task_inbox.lock() {
                inbox.push(crate::lua::TaskEvent::DialogResolved {
                    dialog_id,
                    action: "dismiss".into(),
                    option_index: None,
                    inputs: Vec::new(),
                    on_select: None,
                });
            }
            if let Ok(mut ops) = shared_close.ops.lock() {
                ops.ops.push(AppOp::CloseFloat(win_id));
            }
            Ok(())
        })?,
    )?;
    Ok(mlua::Value::Table(t))
}

fn accent_style() -> ui::grid::Style {
    ui::grid::Style {
        fg: Some(crate::theme::accent()),
        ..Default::default()
    }
}

fn make_text_buffer(app: &mut App, text: &str) -> BufId {
    let id = app.ui.buf_create(BufCreateOpts::default());
    if let Some(buf) = app.ui.buf_mut(id) {
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.lines().map(|s| s.to_string()).collect()
        };
        buf.set_all_lines(lines);
    }
    id
}

fn make_markdown_buffer(app: &mut App, text: &str) -> BufId {
    let id = app.ui.buf_create(BufCreateOpts::default());
    let theme = crate::theme::snapshot();
    // Width is unknown at buffer-build time (the float hasn't been
    // placed yet). Use a conservative default; markdown wrapping will
    // reflow when the dialog picks its rect on first draw.
    let width: u16 = 80;
    if let Some(buf) = app.ui.buf_mut(id) {
        crate::render::to_buffer::render_into_buffer(buf, width, &theme, |sink| {
            crate::render::blocks::render_markdown_inner(
                sink,
                text,
                width as usize,
                " ",
                false,
                None,
            );
        });
    }
    id
}
