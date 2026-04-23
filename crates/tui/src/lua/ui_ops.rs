//! Lua → ui translators for `smelt.api.dialog.open` and
//! `smelt.api.picker.open`. These parse the plugin-supplied `opts`
//! tables into the typed `PanelSpec` / `PickerItem` shapes that the
//! `ui` crate consumes, create the compositor float, and hand the new
//! `WinId` back to the parked Lua task. Everything else — keymap
//! callbacks, submit / dismiss routing, selection tracking —
//! lives in `runtime/lua/smelt/{dialog,picker}.lua`.

use crate::app::App;
use ui::buffer::BufCreateOpts;
use ui::text_input::TextInput;
use ui::{
    BufId, Constraint, FitMax, FloatConfig, OptionItem, OptionList, PanelHeight, PanelSpec,
    Placement, WinId,
};

// ── Dialog ───────────────────────────────────────────────────────────
//
// Supported panel kinds from `opts.panels[]`:
// - `{ kind = "content",  text = "..." | buf = <id> }`
// - `{ kind = "markdown", text = "..." }`  (rendered via `render_markdown_inner`)
// - `{ kind = "options",  items = [{label, shortcut?}] }`
// - `{ kind = "input",    placeholder? = "..." }`
// - `{ kind = "list",     buf = <id> }`

pub fn open_dialog(app: &mut App, opts_key: mlua::RegistryKey) -> Result<WinId, String> {
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
    for pair in panels_tbl.sequence_values::<mlua::Table>() {
        let panel = pair.map_err(|e| format!("dialog panel entry: {e}"))?;
        let kind: String = panel.get("kind").map_err(|e| format!("panel.kind: {e}"))?;
        let height = parse_panel_height(&panel)?;
        let initial_focus: bool = panel.get("focus").unwrap_or(false);
        match kind.as_str() {
            "content" => {
                let buf = if let Ok(id) = panel.get::<u64>("buf") {
                    BufId(id)
                } else {
                    let text: String = panel.get("text").unwrap_or_default();
                    make_text_buffer(app, &text)
                };
                let focusable: bool = panel.get("focusable").unwrap_or(false);
                let pad_left: u16 = panel.get("pad_left").unwrap_or(0);
                let mut spec = PanelSpec::content(buf, height.unwrap_or(PanelHeight::Fit));
                if pad_left > 0 {
                    spec = spec.with_pad_left(pad_left);
                }
                spec = spec.focusable(focusable).with_initial_focus(initial_focus);
                panel_specs.push(spec);
            }
            "markdown" => {
                let text: String = panel.get("text").unwrap_or_default();
                let buf = make_markdown_buffer(app, &text);
                panel_specs.push(
                    PanelSpec::content(buf, height.unwrap_or(PanelHeight::Fit))
                        .with_initial_focus(initial_focus),
                );
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
                    list_items.push(item);
                }
                let multi: bool = panel.get("multi").unwrap_or(false);
                let widget = Box::new(
                    OptionList::new(list_items)
                        .multi(multi)
                        .with_cursor_style(accent_style())
                        .with_shortcut_style(accent_style()),
                );
                panel_specs.push(
                    PanelSpec::widget(widget, PanelHeight::Fit).with_initial_focus(initial_focus),
                );
            }
            "input" => {
                let placeholder: Option<String> = panel.get("placeholder").ok();
                let mut ti = TextInput::new();
                if let Some(p) = placeholder {
                    ti = ti.with_placeholder(p);
                }
                let widget = Box::new(ti);
                panel_specs.push(
                    PanelSpec::widget(widget, PanelHeight::Fit).with_initial_focus(initial_focus),
                );
            }
            "list" => {
                let buf_id: u64 = panel.get("buf").map_err(|e| format!("list.buf: {e}"))?;
                panel_specs.push(
                    PanelSpec::list(BufId(buf_id), height.unwrap_or(PanelHeight::Fill))
                        .with_initial_focus(initial_focus),
                );
            }
            other => return Err(format!("unknown panel kind: {other}")),
        }
    }

    if panel_specs.is_empty() {
        return Err("dialog must have at least one panel".into());
    }

    // Collect footer hints from top-level `opts.keymaps[].hint`. The Lua
    // side handles the actual keymap registration; Rust only needs the
    // hint strings to build the footer.
    let mut extra_hints: Vec<String> = Vec::new();
    if let Ok(km_tbl) = opts.get::<mlua::Table>("keymaps") {
        for entry_res in km_tbl.sequence_values::<mlua::Table>() {
            let entry = entry_res.map_err(|e| format!("keymap entry: {e}"))?;
            if let Ok(hint) = entry.get::<String>("hint") {
                if !hint.is_empty() {
                    extra_hints.push(hint);
                }
            }
        }
    }

    let mut hint_parts: Vec<&str> =
        vec![crate::keymap::hints::CONFIRM, crate::keymap::hints::CANCEL];
    for h in &extra_hints {
        hint_parts.push(h.as_str());
    }
    let dialog_config =
        app.builtin_dialog_config(Some(crate::keymap::hints::join(&hint_parts)), vec![]);

    app.ui
        .dialog_open(
            FloatConfig {
                title,
                border: ui::Border::None,
                placement: Placement::fit_content(FitMax::HalfScreen),
                blocks_agent: true,
                ..Default::default()
            },
            dialog_config,
            panel_specs,
        )
        .ok_or_else(|| "failed to open dialog window".to_string())
}

// ── Picker ───────────────────────────────────────────────────────────

pub fn open_picker(app: &mut App, opts_key: mlua::RegistryKey) -> Result<WinId, String> {
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
        items.push(parse_picker_item(&v)?);
    }
    if items.is_empty() {
        return Err("picker.open: items must be non-empty".into());
    }

    let placement = parse_picker_placement(&opts);
    let title: Option<String> = opts.get("title").ok();

    let float_config = FloatConfig {
        title,
        border: ui::Border::Rounded,
        placement,
        focusable: true,
        blocks_agent: false,
        ..Default::default()
    };

    // Lua picker floats default to non-reversed (top-down); the plugin
    // controls placement, and they don't necessarily dock above a prompt.
    app.ui
        .picker_open(float_config, items, 0, Default::default(), false)
        .ok_or_else(|| "picker.open: failed to create float".to_string())
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Parse a per-panel `height` option. `"fit"` → Fit (auto-shrink),
/// `"fill"` → Fill (stretch + scroll), integer → Fixed rows. Absent →
/// Ok(None) so the caller can pick a kind-appropriate default.
fn parse_panel_height(panel: &mlua::Table) -> Result<Option<PanelHeight>, String> {
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

fn parse_picker_item(v: &mlua::Value) -> Result<ui::picker::PickerItem, String> {
    match v {
        mlua::Value::String(s) => Ok(ui::picker::PickerItem::new(s.to_string_lossy().to_string())),
        mlua::Value::Table(t) => {
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

fn parse_picker_placement(opts: &mlua::Table) -> Placement {
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
