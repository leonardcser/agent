//! Lua → ui helpers: overlay open, picker construction, list/input window
//! recipes.
//!
//! Overlay sizing is driven entirely by `opts.layout` - a `LayoutTree` built
//! from `smelt.ui.layout.leaf` / `.vbox` / `.hbox`. The overlay's outer size
//! comes from `LayoutTree::natural_size_with` evaluated against the current
//! terminal extent every frame; resize tracks automatically. To pin a width or
//! height, wrap the inner tree in a one-slot vbox/hbox with a `Length(N)` /
//! `Percentage(N)` constraint. Shared pointer and window resize operations
//! write `Overlay::size_override`.

use crate::app::TuiApp;
use crate::smelt_edit::layout::{Align, Anchor, Corner, PaintId};
use crate::smelt_edit::{
    add_signed_row, BodyDrag, Callback, CallbackResult, Constraint, Decoration, DragConfig,
    KeyBind, LayoutTree, Overlay, Payload, ResizeConfig, RowIndex, WinEvent, WinId,
};
use crossterm::event::{KeyCode, KeyModifiers};

/// Resolve a Lua-built layout description against host-owned windows, paints,
/// and dialog stages. Window leaves are returned for first-frame pre-rendering.
pub(crate) fn build_layout_tree(
    app: &mut TuiApp,
    node: &crate::lua::api::overlay_layout::LayoutNode,
    window_leaves: &mut Vec<WinId>,
) -> Result<(Constraint, LayoutTree), String> {
    use crate::lua::api::overlay_layout::{ContainerKind, LayoutNode};

    let (constraint, mut tree, chrome) = match node {
        LayoutNode::DialogStage { id } => {
            let modal = app
                .ui
                .docked_surface(*id)
                .map(crate::smelt_edit::DockedSurface::modal)
                .ok_or_else(|| format!("layout references missing docked dialog {}", id.0))?;
            let tree = app
                .docked_dialog_stage_layout(*id)
                .ok_or_else(|| format!("layout references missing docked dialog {}", id.0))?;
            if let Some(leaves) = app.ui.modal_leaves(modal) {
                window_leaves.extend_from_slice(leaves);
            }
            return Ok((Constraint::Fill, tree));
        }
        LayoutNode::Leaf {
            raw_id,
            chrome,
            collapse_when_empty,
            natural,
        } => {
            let leaf = app.resolve_leaf_id(*raw_id).ok_or_else(|| {
                format!("layout leaf references missing window/paint id {raw_id}")
            })?;
            let mut tree = match leaf {
                crate::lua::paint::LeafKind::Window(window) => {
                    window_leaves.push(window);
                    LayoutTree::leaf(window)
                }
                crate::lua::paint::LeafKind::Paint(paint) => LayoutTree::leaf(paint),
            };
            if let Some(natural) = natural.clone() {
                tree = tree.with_natural(natural);
            }
            let constraint = if let crate::lua::paint::LeafKind::Window(window) = leaf {
                if *collapse_when_empty && window_buffer_empty_pub(app, window) {
                    Constraint::Length(0)
                } else {
                    Constraint::Fill
                }
            } else {
                Constraint::Fill
            };
            (constraint, tree, chrome)
        }
        LayoutNode::Frame { child, chrome } => {
            let (_, child) = build_layout_tree(app, child, window_leaves)?;
            (Constraint::Fill, LayoutTree::frame(child), chrome)
        }
        LayoutNode::Split {
            split,
            children,
            chrome,
        } => {
            let (_, first) = build_layout_tree(app, &children[0], window_leaves)?;
            let (_, second) = build_layout_tree(app, &children[1], window_leaves)?;
            (
                Constraint::Fill,
                LayoutTree::split(split.clone(), first, second),
                chrome,
            )
        }
        LayoutNode::Container {
            kind,
            items,
            chrome,
            gap,
        } => {
            let mut tree_items = Vec::with_capacity(items.len());
            for item in items {
                let (_, child_tree) = build_layout_tree(app, &item.node, window_leaves)?;
                tree_items.push((item.constraint, child_tree));
            }
            let tree = match kind {
                ContainerKind::Vbox => LayoutTree::vbox(tree_items),
                ContainerKind::Hbox => LayoutTree::hbox(tree_items),
            };
            (Constraint::Fill, tree.with_gap(*gap), chrome)
        }
    };
    let target = tree.chrome_mut();
    target.border = chrome.border;
    target.title = chrome.title.clone();
    target.padding = chrome.padding;
    target.justify = chrome.justify;
    Ok((constraint, tree))
}

/// Validate the lowered tree, including opaque dialog stages, against the other
/// registered presentation trees. Callers exclude the tree they are replacing.
pub(crate) fn validate_split_mounts<'a>(
    tree: &LayoutTree,
    existing: impl Iterator<Item = &'a LayoutTree>,
) -> Result<(), String> {
    let mut ids = std::collections::HashSet::new();
    let duplicate = tree.splits().any(|split| !ids.insert(split.id()));
    if duplicate
        || (!ids.is_empty()
            && existing
                .flat_map(LayoutTree::splits)
                .any(|split| ids.contains(&split.id())))
    {
        return Err("a split handle can be mounted only once at a time".into());
    }
    Ok(())
}

pub(crate) fn open_overlay(app: &mut TuiApp, opts: mlua::Table) -> Result<u64, String> {
    // Parse the full opts up front so a named re-open refreshes the whole
    // layout (not just title/border) - structural edits to a plugin's
    // overlay tree hot-reload in place. `size_override` on the existing
    // overlay is left intact so user-resized panels stay resized.
    let name: Option<String> = opts.get::<Option<String>>("name").ok().flatten();
    let title = crate::lua::parse::title(opts.get::<mlua::Value>("title").ok())
        .map_err(|e| format!("overlay title: {e}"))?;
    let layout_ud: mlua::AnyUserData = opts
        .get("layout")
        .map_err(|_| "overlay.open: missing `layout = <smelt.ui.layout.* userdata>`".to_string())?;
    let layout_node = {
        let borrowed = layout_ud
            .borrow::<crate::lua::api::overlay_layout::LuaUiLayout>()
            .map_err(|e| format!("overlay.open: `layout` must be a smelt.ui.layout node: {e}"))?;
        borrowed.0.clone()
    };
    let (term_w, term_h) = app.ui.terminal_size();
    let anchor = parse_overlay_anchor(&opts, term_w, term_h)?;
    let border = crate::lua::parse::border(&opts).map_err(|e| format!("overlay border: {e}"))?;
    let blocks_agent: bool = opts.get("blocks_agent").unwrap_or(false);
    let modal: bool = opts.get("modal").unwrap_or(true);
    let z: u16 = opts.get("z").unwrap_or(50);
    let draggable = parse_drag_config(opts.get::<mlua::Value>("draggable").ok())?;
    let resizable = parse_resize_config(opts.get::<mlua::Value>("resizable").ok())?;
    // `width`/`height` resolve the overlay rect per-frame against the
    // terminal extent. Reuse the same constraint parser as `layout.vbox`
    // items so `"70%"`, `"max:60"`, `"fit"`, integer cells, etc. all work.
    // Default `Fit` matches the legacy "size from layout.natural" path.
    let width = parse_overlay_constraint(&opts, "width", "overlay.width")?;
    let height = parse_overlay_constraint(&opts, "height", "overlay.height")?;
    // `max_width`/`max_height` are optional upper bounds applied after the
    // primary axis resolves. Lets `width = "fit"` + `max_width = "50%"`
    // express "shrink to content, cap at 50% of terminal".
    let max_width = parse_overlay_constraint_opt(&opts, "max_width", "overlay.max_width")?;
    let max_height = parse_overlay_constraint_opt(&opts, "max_height", "overlay.max_height")?;
    // `min_width`/`min_height` are the symmetric floors. Pairs with
    // `height = "fit"` + `min_height = 2` to express "shrink to content,
    // but never smaller than 2 cells".
    let min_width = parse_overlay_constraint_opt(&opts, "min_width", "overlay.min_width")?;
    let min_height = parse_overlay_constraint_opt(&opts, "min_height", "overlay.min_height")?;

    let mut window_leaves: Vec<WinId> = Vec::new();
    let (_root_constraint, inner) = build_layout_tree(app, &layout_node, &mut window_leaves)?;
    let mut layout = inner;
    if let Some(b) = border {
        layout = layout.with_border(b);
    }
    if let Some(t) = title {
        layout = layout.with_title(t);
    }

    let replaced = name
        .as_deref()
        .and_then(|name| app.ui.named_overlay(name))
        .and_then(|id| app.ui.overlay(id));
    validate_split_mounts(
        &layout,
        app.ui
            .layout_trees()
            .filter(|tree| !replaced.is_some_and(|overlay| std::ptr::eq(*tree, &overlay.layout))),
    )?;

    if let Some(ref n) = name {
        if let Some((id, ov)) = app.ui.lookup_named_overlay_mut(n) {
            ov.layout = layout;
            ov.anchor = anchor;
            ov.width = width;
            ov.height = height;
            ov.max_width = max_width;
            ov.max_height = max_height;
            ov.min_width = min_width;
            ov.min_height = min_height;
            ov.z = z;
            ov.modal = modal;
            ov.blocks_agent = blocks_agent;
            ov.draggable = draggable;
            ov.resizable = resizable;
            app.ui.sync_overlay_modal(id);
            return Ok(id.0 as u64);
        }
    }

    // First-frame prime: render every window leaf's buffer at the terminal
    // width so wrap-driven gutters and intra-frame paints have something to
    // read. Subsequent frames hit the buffer's wrap cache.
    for &win_id in &window_leaves {
        let content_w = app.ui.win_content_width(win_id).unwrap_or(term_w);
        if let Some(buf_id) = app.ui.win(win_id).map(|w| w.buf) {
            if let Some(buf) = app.ui.buf_mut(buf_id) {
                buf.ensure_rendered_at(content_w);
            }
        }
    }

    let overlay = Overlay::new(layout, anchor)
        .with_z(z)
        .modal(modal)
        .blocks_agent(blocks_agent)
        .drag_config(draggable)
        .resize_config(resizable)
        .with_width(width)
        .with_height(height)
        .with_max_width(max_width)
        .with_max_height(max_height)
        .with_min_width(min_width)
        .with_min_height(min_height);
    let id = app.ui.overlay_open(overlay);
    if let Some(n) = name {
        app.ui.name_overlay(n, id);
    }
    app.ui.prime_overlay_viewports();
    Ok(id.0 as u64)
}

pub(crate) fn open_decoration(
    app: &mut TuiApp,
    owner: WinId,
    opts: mlua::Table,
) -> Result<crate::smelt_edit::DecorationId, String> {
    let layout_ud: mlua::AnyUserData = opts
        .get("layout")
        .map_err(|_| "win:decorate: missing `layout = <smelt.ui.layout.* userdata>`".to_string())?;
    let layout_node = {
        let borrowed = layout_ud
            .borrow::<crate::lua::api::overlay_layout::LuaUiLayout>()
            .map_err(|e| format!("win:decorate: `layout` must be a smelt.ui.layout node: {e}"))?;
        borrowed.0.clone()
    };
    let align = crate::lua::parse::align(
        opts.get::<String>("align").ok().as_deref(),
        crate::smelt_edit::Align::Center,
    )?;
    let row_offset: i32 = opts.get("row_offset").unwrap_or(0);
    let col_offset: i32 = opts.get("col_offset").unwrap_or(0);
    let z: u16 = opts.get("z").unwrap_or(0);
    let width = parse_overlay_constraint(&opts, "width", "decoration.width")?;
    let height = parse_overlay_constraint(&opts, "height", "decoration.height")?;
    let max_width = parse_overlay_constraint_opt(&opts, "max_width", "decoration.max_width")?;
    let max_height = parse_overlay_constraint_opt(&opts, "max_height", "decoration.max_height")?;
    let min_width = parse_overlay_constraint_opt(&opts, "min_width", "decoration.min_width")?;
    let min_height = parse_overlay_constraint_opt(&opts, "min_height", "decoration.min_height")?;

    let mut window_leaves = Vec::new();
    let (_root_constraint, layout) = build_layout_tree(app, &layout_node, &mut window_leaves)?;
    validate_split_mounts(&layout, app.ui.layout_trees())?;
    let (term_w, _) = app.ui.terminal_size();
    for &win_id in &window_leaves {
        let content_w = app.ui.win_content_width(win_id).unwrap_or(term_w);
        if let Some(buf_id) = app.ui.win(win_id).map(|w| w.buf) {
            if let Some(buf) = app.ui.buf_mut(buf_id) {
                buf.ensure_rendered_at(content_w);
            }
        }
    }

    let decoration = Decoration::new(owner, layout)
        .with_align(align)
        .with_offset(row_offset, col_offset)
        .with_z(z)
        .with_width(width)
        .with_height(height)
        .with_max_width(max_width)
        .with_max_height(max_height)
        .with_min_width(min_width)
        .with_min_height(min_height);
    let id = app.ui.decoration_open(decoration);
    app.ui.prime_decoration_viewports();
    Ok(id)
}

fn parse_drag_config(raw: Option<mlua::Value>) -> Result<DragConfig, String> {
    match raw.unwrap_or(mlua::Value::Nil) {
        mlua::Value::Nil => Ok(DragConfig::none()),
        mlua::Value::Boolean(true) => Ok(DragConfig::floating()),
        mlua::Value::Boolean(false) => Ok(DragConfig::none()),
        mlua::Value::Table(t) => {
            let title = t.get("title").unwrap_or(false);
            let body = match t.get::<mlua::Value>("body").unwrap_or(mlua::Value::Nil) {
                mlua::Value::Nil | mlua::Value::Boolean(false) => BodyDrag::Never,
                mlua::Value::Boolean(true) => BodyDrag::Always,
                mlua::Value::String(s) if s.to_str().ok().as_deref() == Some("inert") => {
                    BodyDrag::Inert
                }
                other => {
                    return Err(format!(
                        "overlay.draggable.body must be boolean or 'inert', got {}",
                        other.type_name()
                    ));
                }
            };
            Ok(DragConfig { title, body })
        }
        other => Err(format!(
            "overlay.draggable must be boolean or table, got {}",
            other.type_name()
        )),
    }
}

fn parse_resize_config(raw: Option<mlua::Value>) -> Result<ResizeConfig, String> {
    match raw.unwrap_or(mlua::Value::Nil) {
        mlua::Value::Nil => Ok(ResizeConfig::none()),
        mlua::Value::Boolean(true) => Ok(ResizeConfig::floating()),
        mlua::Value::Boolean(false) => Ok(ResizeConfig::none()),
        mlua::Value::Table(t) => Ok(ResizeConfig {
            top: t.get("top").unwrap_or(false),
            right: t.get("right").unwrap_or(false),
            bottom: t.get("bottom").unwrap_or(false),
            left: t.get("left").unwrap_or(false),
            corners: t.get("corners").unwrap_or(false),
        }),
        other => Err(format!(
            "overlay.resizable must be boolean or table, got {}",
            other.type_name()
        )),
    }
}

/// Read `opts[key]` as a `Constraint` for the overlay's width or height.
/// Missing/`nil` defaults to `Fit` (read the layout's natural size on that
/// axis - back-compat with the original `natural_size_with` path). Any
/// shape accepted by `crate::lua::parse::constraint` is supported here.
fn parse_overlay_constraint(
    opts: &mlua::Table,
    key: &str,
    ctx: &str,
) -> Result<crate::smelt_edit::layout::Constraint, String> {
    let v: Option<mlua::Value> = opts.get(key).ok();
    if matches!(v, None | Some(mlua::Value::Nil)) {
        return Ok(crate::smelt_edit::layout::Constraint::Fit);
    }
    crate::lua::parse::constraint(v, ctx)
}

/// Twin of [`parse_overlay_constraint`] that returns `None` for absent /
/// `nil` values instead of defaulting to `Fit`. Used by `max_width` and
/// `max_height` where "unset" means "no cap" rather than "fit".
fn parse_overlay_constraint_opt(
    opts: &mlua::Table,
    key: &str,
    ctx: &str,
) -> Result<Option<crate::smelt_edit::layout::Constraint>, String> {
    let v: Option<mlua::Value> = opts.get(key).ok();
    if matches!(v, None | Some(mlua::Value::Nil)) {
        return Ok(None);
    }
    crate::lua::parse::constraint(v, ctx).map(Some)
}

/// Accept `target = <integer>` or `target = <Win userdata>`. Used by the
/// `anchor = "win"` / `"win_center"` paths so callers can hand the window
/// handle straight through without an explicit `.id` accessor.
fn parse_win_target(opts: &mlua::Table) -> Result<u64, String> {
    let raw: mlua::Value = opts
        .get("target")
        .map_err(|e| format!("anchor = 'win' requires target = <win or win_id>: {e}"))?;
    match raw {
        mlua::Value::Integer(n) if n >= 0 => Ok(n as u64),
        mlua::Value::Number(n) if n >= 0.0 => Ok(n as u64),
        mlua::Value::UserData(ud) => ud
            .borrow::<crate::lua::api::win::LuaWin>()
            .map(|w| w.id.0)
            .map_err(|e| format!("anchor = 'win': target userdata is not a Win: {e}")),
        other => Err(format!(
            "anchor = 'win': target must be a Win handle or integer id, got {}",
            other.type_name()
        )),
    }
}

/// Parse `opts.anchor` (plus anchor-specific extras) directly into a
/// `term::Anchor`. The four dock variants resolve to literal screen
/// edges; the host has no `statusline` concept anymore, so plugins that
/// want their overlay to clear the Lua-allocated statusline pass the
/// statusline's row count via `opts.above_rows` (typically computed
/// from `require("smelt.statusline").win:rect().height`):
///
/// ```lua
/// local sl = require("smelt.statusline").win
/// smelt.overlay.new({
///   anchor = "dock_bottom",
///   above_rows = (sl and sl:rect() or {}).height or 0,
///   ...
/// })
/// ```
fn parse_overlay_anchor(opts: &mlua::Table, term_w: u16, term_h: u16) -> Result<Anchor, String> {
    match opts.get::<String>("anchor").ok().as_deref() {
        Some("dock_bottom") | None => {
            let above_rows: u16 = opts.get("above_rows").unwrap_or(0);
            Ok(Anchor::ScreenBottom { above_rows })
        }
        Some("dock_top") => Ok(Anchor::ScreenAt {
            row: 0,
            col: 0,
            corner: Corner::NW,
        }),
        Some("dock_left") => Ok(Anchor::ScreenAt {
            row: 0,
            col: 0,
            corner: Corner::NW,
        }),
        Some("dock_right") => Ok(Anchor::ScreenAt {
            row: 0,
            col: term_w.saturating_sub(1) as i32,
            corner: Corner::NE,
        }),
        Some("center") => Ok(Anchor::ScreenCenter),
        Some("screen_at") => {
            let corner =
                crate::lua::parse::corner(opts.get::<String>("corner").ok().as_deref(), Corner::NW);
            let row: u16 = opts.get("row").unwrap_or(0);
            let col: u16 = opts.get("col").unwrap_or(0);
            let abs_row = match corner {
                Corner::NW | Corner::NE => row as i32,
                Corner::SW | Corner::SE => term_h.saturating_sub(1) as i32 - row as i32,
            };
            let abs_col = match corner {
                Corner::NW | Corner::SW => col as i32,
                Corner::NE | Corner::SE => term_w.saturating_sub(1) as i32 - col as i32,
            };
            Ok(Anchor::ScreenAt {
                row: abs_row,
                col: abs_col,
                corner,
            })
        }
        Some("win") => {
            let target_id: u64 = parse_win_target(opts)?;
            let target = PaintId::from(WinId(target_id));
            // `attach` selects one of nine alignment points inside the
            // target rect (`nw|n|ne|w|center|e|sw|s|se`); the same point on
            // the overlay rect is mapped to it. Defaults to `nw`.
            let attach =
                crate::lua::parse::align(opts.get::<String>("attach").ok().as_deref(), Align::NW)?;
            let row_offset: i32 = opts.get("row_offset").unwrap_or(0);
            let col_offset: i32 = opts.get("col_offset").unwrap_or(0);
            Ok(Anchor::Win {
                target,
                attach,
                row_offset,
                col_offset,
            })
        }
        Some(other) => Err(format!(
            "overlay anchor: unknown '{other}' (expected dock_bottom|dock_top|dock_left|dock_right|center|screen_at|win)"
        )),
    }
}

/// Wire the built-in list keymap: j/k/arrows/Home/End/PgUp/PgDn navigate,
/// Enter fires `WinEvent::Submit` with the absolute selected row.
pub(crate) fn configure_list_leaf(app: &mut TuiApp, leaf: WinId, initial_cursor: RowIndex) {
    let buf_id = match app.ui.win(leaf) {
        Some(w) => w.buf,
        None => return,
    };
    let line_count = app.ui.buf(buf_id).map(|b| b.line_count()).unwrap_or(0);
    let viewport = app
        .ui
        .paint_rect(PaintId::from(leaf))
        .map(|r| r.height)
        .unwrap_or(0);
    let max = line_count.saturating_sub(1) as RowIndex;
    let target = initial_cursor.min(max);
    let list_focusable = app.ui.win(leaf).is_some_and(|w| w.accepts_focus());
    let (win, buf) = app.ui.win_and_buf_mut(leaf, buf_id);
    if let (Some(win), Some(buf)) = (win, buf) {
        win.set_list_selection_highlight(true);
        win.set_surface(crate::smelt_edit::WindowSurface::list(list_focusable));
        win.jump_to_row(buf, target, viewport);
        if target > 0 {
            win.pending_scroll_to_cursor = true;
        }
    }

    fn move_cursor(ctx: &mut crate::smelt_edit::CallbackCtx<'_>, delta: isize) -> CallbackResult {
        let buf_id = match ctx.ui.win(ctx.win) {
            Some(w) => w.buf,
            None => return CallbackResult::Consumed,
        };
        let line_count = ctx.ui.buf(buf_id).map(|b| b.line_count()).unwrap_or(0);
        if line_count == 0 {
            return CallbackResult::Consumed;
        }
        let viewport = ctx.ui.paint_rect(PaintId::from(ctx.win)).map(|r| r.height);
        let mut new_abs: Option<usize> = None;
        let (win, buf) = ctx.ui.win_and_buf_mut(ctx.win, buf_id);
        if let (Some(win), Some(buf)) = (win, buf) {
            let abs = win.cursor_abs_row();
            let max = line_count.saturating_sub(1) as RowIndex;
            let target = add_signed_row(abs, delta).min(max);
            if target == abs {
                return CallbackResult::Consumed;
            }
            let scroll_top = scroll_to_show(win.scroll_top(), target, viewport);
            win.pin_scroll(scroll_top);
            win.jump_to_row(buf, target, viewport.unwrap_or(0));
            new_abs = Some(target as usize);
        }
        match new_abs {
            Some(abs) => CallbackResult::Event(
                WinEvent::SelectionChanged,
                Payload::Selection { index: abs },
            ),
            None => CallbackResult::Consumed,
        }
    }

    let bindings: &[(KeyBind, isize)] = &[
        (KeyBind::new(KeyCode::Char('j'), KeyModifiers::NONE), 1),
        (KeyBind::new(KeyCode::Down, KeyModifiers::NONE), 1),
        (KeyBind::new(KeyCode::Char('k'), KeyModifiers::NONE), -1),
        (KeyBind::new(KeyCode::Up, KeyModifiers::NONE), -1),
        (KeyBind::new(KeyCode::Char('j'), KeyModifiers::CONTROL), 1),
        (KeyBind::new(KeyCode::Char('k'), KeyModifiers::CONTROL), -1),
        (KeyBind::new(KeyCode::Char('n'), KeyModifiers::CONTROL), 1),
        (KeyBind::new(KeyCode::Char('p'), KeyModifiers::CONTROL), -1),
        (KeyBind::new(KeyCode::PageDown, KeyModifiers::NONE), 10),
        (KeyBind::new(KeyCode::PageUp, KeyModifiers::NONE), -10),
        (KeyBind::new(KeyCode::Char('d'), KeyModifiers::CONTROL), 5),
        (KeyBind::new(KeyCode::Char('u'), KeyModifiers::CONTROL), -5),
        (
            KeyBind::new(KeyCode::Home, KeyModifiers::NONE),
            isize::MIN / 2,
        ),
        (
            KeyBind::new(KeyCode::End, KeyModifiers::NONE),
            isize::MAX / 2,
        ),
    ];
    for (key, delta) in bindings {
        let d = *delta;
        let cb: Callback = Callback::Rust(Box::new(move |ctx| move_cursor(ctx, d)));
        let _ = app.ui.win_set_keymap(leaf, *key, cb);
    }

    // Enter fires Submit with the absolute selected line index.
    let submit_cb: Callback = Callback::Rust(Box::new(|ctx| {
        let abs = ctx
            .ui
            .win(ctx.win)
            .map(|w| w.cursor_abs_row() as usize)
            .unwrap_or(0);
        CallbackResult::Event(WinEvent::Submit, Payload::Selection { index: abs })
    }));
    let _ = app.ui.win_set_keymap(
        leaf,
        KeyBind::new(KeyCode::Enter, KeyModifiers::NONE),
        submit_cb,
    );
}

/// Adjust `scroll_top` so `target` falls within `[scroll, scroll + height)`. `height`
/// is the leaf's painted viewport height (rows). Falls back to top-edge-only when
/// height is unknown.
fn scroll_to_show(scroll: RowIndex, target: RowIndex, height: Option<u16>) -> RowIndex {
    match height {
        Some(h) if h >= 1 => {
            let h = h as RowIndex;
            if target < scroll {
                target
            } else if target >= scroll.saturating_add(h) {
                target + 1 - h
            } else {
                scroll
            }
        }
        _ => {
            if target < scroll {
                target
            } else {
                scroll
            }
        }
    }
}

/// Place `leaf`'s cursor at `target` (clamped to its document or buffer), keep the
/// row on-screen by nudging `scroll_top`, and emit `SelectionChanged` if the position
/// actually moved. Used by both the absolute (`set_cursor_row`) and relative
/// (`move_cursor`) entry points.
fn apply_cursor(app: &mut TuiApp, leaf: WinId, target: RowIndex) {
    let buf_id = match app.ui.win(leaf) {
        Some(w) => w.buf,
        None => return,
    };
    let line_count = app
        .ui
        .win(leaf)
        .and_then(|win| win.row_source())
        .map(|source| source.snapshot().total_rows)
        .unwrap_or_else(|| {
            app.ui
                .buf(buf_id)
                .map_or(0, |buf| buf.line_count() as RowIndex)
        });
    if line_count == 0 {
        return;
    }
    let max = line_count.saturating_sub(1);
    let target = target.min(max);
    let viewport = app.ui.paint_rect(PaintId::from(leaf)).map(|r| r.height);
    let (win, buf) = app.ui.win_and_buf_mut(leaf, buf_id);
    let (Some(win), Some(buf)) = (win, buf) else {
        return;
    };
    let abs = win.cursor_abs_row();
    let scroll_top = scroll_to_show(win.scroll_top(), target, viewport);
    win.pin_scroll(scroll_top);
    if abs == target {
        return;
    }
    win.jump_to_row(buf, target, viewport.unwrap_or(0));
    let lua = &app.lua;
    let mut lua_invoke =
        |handle: crate::smelt_edit::LuaHandle, win: WinId, payload: &crate::smelt_edit::Payload| {
            lua.queue_invocation(handle, win, payload);
        };
    app.ui.fire_win_event(
        leaf,
        crate::smelt_edit::WinEvent::SelectionChanged,
        crate::smelt_edit::Payload::Selection {
            index: target as usize,
        },
        &mut lua_invoke,
    );
}

/// Move `leaf`'s cursor by `delta` rows (clamped to its document or buffer) and emit
/// `SelectionChanged`. Used by external panels (e.g. an input docked next to a list)
/// to drive selection without holding focus on the list itself.
pub(crate) fn move_cursor(app: &mut TuiApp, leaf: WinId, delta: isize) {
    let abs = match app.ui.win(leaf) {
        Some(w) => w.cursor_abs_row(),
        None => return,
    };
    let target = add_signed_row(abs, delta);
    apply_cursor(app, leaf, target);
}

/// Place `leaf`'s cursor at an absolute row.
pub(crate) fn set_cursor_row(app: &mut TuiApp, leaf: WinId, row: RowIndex) {
    apply_cursor(app, leaf, row);
}

/// Read the current cursor row of `leaf` (0-based), or `None` if the leaf doesn't exist.
pub(crate) fn cursor_row(app: &TuiApp, leaf: WinId) -> Option<RowIndex> {
    if leaf == crate::app::TRANSCRIPT_WIN {
        if let Some(row) = app.conversation.transcript().pending_cursor_row() {
            return Some(row);
        }
    }
    app.ui.win(leaf).map(|w| w.cursor_abs_row())
}

/// Wire the built-in input recipe: printable chars insert at cursor, Backspace deletes,
/// Left/Right/Home/End move the cursor, Enter fires `WinEvent::Submit`.
/// Every edit also fires `WinEvent::TextChanged`.
///
/// Placeholder: when `placeholder` is non-empty, it is rendered as virtual text
/// while the buffer remains empty. The first printable keystroke inserts real
/// input at column 0; Backspace and horizontal movement are no-ops while only
/// the placeholder is visible.
pub(crate) fn configure_input_leaf(app: &mut TuiApp, leaf: WinId, placeholder: String) {
    if !placeholder.is_empty() {
        app.set_placeholder(leaf, placeholder.clone());
    }
    if let Some(win) = app.ui.win_mut(leaf) {
        win.reset_cursor();
    }

    fn current_line(ctx: &crate::smelt_edit::CallbackCtx<'_>) -> String {
        let buf_id = match ctx.ui.win(ctx.win) {
            Some(w) => w.buf,
            None => return String::new(),
        };
        ctx.ui
            .buf(buf_id)
            .and_then(|b| b.get_line(0).map(|s| s.to_string()))
            .unwrap_or_default()
    }

    fn is_placeholder(ctx: &crate::smelt_edit::CallbackCtx<'_>, placeholder: &str) -> bool {
        !placeholder.is_empty() && current_line(ctx).is_empty()
    }

    fn set_placeholder_mark(ctx: &mut crate::smelt_edit::CallbackCtx<'_>, placeholder: &str) {
        let buf_id = match ctx.ui.win(ctx.win) {
            Some(w) => w.buf,
            None => return,
        };
        if let Some(buf) = ctx.ui.buf_mut(buf_id) {
            crate::content::prompt_buf::set_placeholder_extmark(buf, Some(placeholder.to_string()));
        }
    }

    fn clear_placeholder_mark(ctx: &mut crate::smelt_edit::CallbackCtx<'_>) {
        set_placeholder_mark(ctx, "");
    }

    fn restore_placeholder(ctx: &mut crate::smelt_edit::CallbackCtx<'_>, placeholder: &str) {
        let buf_id = match ctx.ui.win(ctx.win) {
            Some(w) => w.buf,
            None => return,
        };
        if let Some(buf) = ctx.ui.buf_mut(buf_id) {
            buf.set_lines(0, 1, vec![String::new()]);
        }
        set_placeholder_mark(ctx, placeholder);
        if let Some(win) = ctx.ui.win_mut(ctx.win) {
            win.set_cursor_byte_single_line("", 0);
            win.clear_selection_anchor();
        }
    }

    fn replace_line(ctx: &mut crate::smelt_edit::CallbackCtx<'_>, new: String, cursor: usize) {
        let buf_id = match ctx.ui.win(ctx.win) {
            Some(w) => w.buf,
            None => return,
        };
        if let Some(buf) = ctx.ui.buf_mut(buf_id) {
            buf.set_lines(0, 1, vec![new.clone()]);
        }
        if let Some(win) = ctx.ui.win_mut(ctx.win) {
            win.set_cursor_byte_single_line(&new, cursor);
        }
    }

    fn line_edit(
        ctx: &crate::smelt_edit::CallbackCtx<'_>,
        placeholder: &str,
    ) -> crate::line_input::LineEdit {
        if is_placeholder(ctx, placeholder) {
            return crate::line_input::LineEdit::new(String::new(), 0);
        }
        let text = current_line(ctx);
        let (cursor, anchor) = ctx
            .ui
            .win(ctx.win)
            .map(|w| (w.cpos(), w.selection_anchor()))
            .unwrap_or((0, None));
        crate::line_input::LineEdit::with_selection(text, cursor, anchor)
    }

    fn apply_edit(
        ctx: &mut crate::smelt_edit::CallbackCtx<'_>,
        placeholder: &str,
        command: crate::line_input::EditCommand,
    ) -> CallbackResult {
        let placeholder_mode = is_placeholder(ctx, placeholder);
        let mut edit = line_edit(ctx, placeholder);
        let old = edit.text().to_string();
        let state_changed = edit.apply(command);
        let text_changed = edit.text() != old;
        if placeholder_mode && state_changed {
            clear_placeholder_mark(ctx);
        }
        let new = edit.text().to_string();
        if new.is_empty() && !placeholder.is_empty() {
            restore_placeholder(ctx, placeholder);
        } else {
            let cursor = edit.cursor();
            let anchor = edit.selection_anchor();
            replace_line(ctx, new.clone(), cursor);
            if let Some(win) = ctx.ui.win_mut(ctx.win) {
                win.set_selection_anchor(anchor);
            }
        }
        if text_changed {
            CallbackResult::Event(WinEvent::TextChanged, Payload::Text { content: new })
        } else {
            CallbackResult::Consumed
        }
    }

    fn bind_edit_key(
        app: &mut TuiApp,
        leaf: WinId,
        bind: KeyBind,
        placeholder: String,
        command: crate::line_input::EditCommand,
    ) {
        let _ = app.ui.win_set_keymap(
            leaf,
            bind,
            Callback::Rust(Box::new(move |ctx| {
                apply_edit(ctx, &placeholder, command.clone())
            })),
        );
    }

    for entry in crate::line_input::default_key_bindings() {
        bind_edit_key(app, leaf, entry.bind, placeholder.clone(), entry.command);
    }

    let placeholder_for_submit = placeholder.clone();
    let submit: Callback = Callback::Rust(Box::new(move |ctx| {
        let content = if is_placeholder(ctx, &placeholder_for_submit) {
            String::new()
        } else {
            current_line(ctx)
        };
        CallbackResult::Event(WinEvent::Submit, Payload::Text { content })
    }));
    let _ = app.ui.win_set_keymap(
        leaf,
        KeyBind::new(KeyCode::Enter, KeyModifiers::NONE),
        submit,
    );

    let placeholder_for_fallback = placeholder.clone();
    let fallback: Callback = Callback::Rust(Box::new(move |ctx| match &ctx.payload {
        Payload::Key {
            code: KeyCode::Esc, ..
        }
        | Payload::Key {
            code: KeyCode::Char('c'),
            mods: KeyModifiers::CONTROL,
        } => {
            if ctx.ui.has_win_event(ctx.win, WinEvent::Dismiss) {
                let content = if is_placeholder(ctx, &placeholder_for_fallback) {
                    String::new()
                } else {
                    current_line(ctx)
                };
                CallbackResult::Event(WinEvent::Dismiss, Payload::Text { content })
            } else {
                CallbackResult::Pass
            }
        }
        Payload::Key { code, mods } => {
            let key = crossterm::event::KeyEvent::new(*code, *mods);
            match crate::line_input::command_for_key(key) {
                Some(command) => apply_edit(ctx, &placeholder_for_fallback, command),
                None => CallbackResult::Pass,
            }
        }
        Payload::Paste { content } => apply_edit(
            ctx,
            &placeholder_for_fallback,
            crate::line_input::EditCommand::InsertText(content.clone()),
        ),
        _ => CallbackResult::Consumed,
    }));
    let _ = app.ui.win_set_key_fallback(leaf, fallback);
}

// ── Picker ───────────────────────────────────────────────────────────

pub(crate) fn open_picker(app: &mut TuiApp, opts: mlua::Table) -> Result<WinId, String> {
    let items_tbl: mlua::Table = opts
        .get("items")
        .map_err(|e| format!("picker items: {e}"))?;
    let mut items: Vec<crate::picker::PickerItem> = Vec::new();
    for pair in items_tbl.sequence_values::<mlua::Value>() {
        let v = pair.map_err(|e| format!("picker item: {e}"))?;
        items.push(parse_picker_item(app, &v)?);
    }
    if items.is_empty() {
        return Err("picker.open: items must be non-empty".into());
    }

    let placement_str: String = opts
        .get("placement")
        .ok()
        .unwrap_or_else(|| "center".to_string());
    let placement = match placement_str.as_str() {
        "bottom" => crate::picker::PickerPlacement::ScreenBottom,
        "cursor" => crate::picker::PickerPlacement::Cursor,
        "prompt_docked" => crate::picker::PickerPlacement::PromptDocked { max_rows: 8 },
        _ => crate::picker::PickerPlacement::ScreenCenter,
    };
    // prompt_docked is non-focusable (keys flow to the prompt); other placements own dispatch.
    // All pickers use the dialog z-plane (50) so transient notifications
    // (z=40) are covered instead of shifting the picker upward.
    let focusable = !matches!(
        placement,
        crate::picker::PickerPlacement::PromptDocked { .. }
    );
    let z = 50;

    crate::picker::open(app, items, 0, placement, focusable, false, z)
        .ok_or_else(|| "picker.open: failed to create overlay".to_string())
}

// ── Helpers ──────────────────────────────────────────────────────────

pub(crate) fn parse_picker_item(
    app: &TuiApp,
    v: &mlua::Value,
) -> Result<crate::picker::PickerItem, String> {
    match v {
        mlua::Value::String(s) => Ok(crate::picker::PickerItem::new(
            s.to_string_lossy().to_string(),
        )),
        mlua::Value::Table(t) => {
            let label: String = t
                .get("label")
                .map_err(|e| format!("picker item.label: {e}"))?;
            let mut item = crate::picker::PickerItem::new(label);
            if let Ok(desc) = t.get::<String>("description") {
                item = item.with_description(desc);
            }
            let explicit_prefix = t.get::<Option<String>>("prefix").ok().flatten();
            if let Some(prefix) = explicit_prefix {
                item = item.with_prefix(prefix);
            } else {
                item = apply_picker_icon(app, item, t);
            }
            if let Ok(Some(ansi)) = t.get::<Option<u64>>("ansi_color") {
                item = item.with_prefix_style(
                    smelt_core::style::Style::new()
                        .fg(smelt_core::style::Color::AnsiValue(ansi as u8)),
                );
            }
            if let Ok(Some(ansi)) = t.get::<Option<u64>>("label_color") {
                item = item.with_label_style(
                    smelt_core::style::Style::new()
                        .fg(smelt_core::style::Color::AnsiValue(ansi as u8)),
                );
            }
            Ok(item)
        }
        other => Err(format!(
            "picker item: expected string or table, got {}",
            other.type_name()
        )),
    }
}

fn apply_picker_icon(
    app: &TuiApp,
    item: crate::picker::PickerItem,
    t: &mlua::Table,
) -> crate::picker::PickerItem {
    let Some(icon) = t.get::<Option<mlua::Table>>("icon").ok().flatten() else {
        return item;
    };
    let Some(kind) = icon.get::<Option<String>>("kind").ok().flatten() else {
        return item;
    };
    let Some(path) = icon.get::<Option<String>>("path").ok().flatten() else {
        return item;
    };
    apply_file_icon(app, item, &kind, &path)
}

fn apply_file_icon(
    app: &TuiApp,
    item: crate::picker::PickerItem,
    kind: &str,
    path: &str,
) -> crate::picker::PickerItem {
    let settings = &app.core.config.settings;
    if !settings.file_icons {
        return item;
    }

    let colors = settings.file_icon_colors;
    if kind == "dir" {
        let style = if colors {
            app.ui.theme().get("Directory")
        } else {
            Default::default()
        };
        return item.with_prefix(" ").with_prefix_style(style);
    }

    let options = smelt_core::content::file_icons::FileIconOptions::new(
        settings.file_icons,
        colors,
        app.ui.theme().is_light(),
        None,
    );
    let Some(icon) =
        smelt_core::content::file_icons::lookup_path(std::path::Path::new(path), &options)
    else {
        return item;
    };
    let style = icon
        .group
        .map(|group| app.ui.theme().resolve(group))
        .unwrap_or_default();
    item.with_prefix(format!("{} ", icon.icon))
        .with_prefix_style(style)
}

pub(crate) fn window_buffer_empty_pub(app: &TuiApp, win: WinId) -> bool {
    window_buffer_empty(app, win)
}

fn window_buffer_empty(app: &TuiApp, win: WinId) -> bool {
    let Some(buf_id) = app.ui.win(win).map(|w| w.buf) else {
        return false;
    };
    app.ui
        .buf(buf_id)
        .map(|b| {
            let n = b.line_count();
            n == 0 || (n == 1 && b.lines()[0].is_empty())
        })
        .unwrap_or(false)
}
