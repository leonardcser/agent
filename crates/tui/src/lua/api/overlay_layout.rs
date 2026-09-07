//! Shared layout-tree types used by both `smelt.ui.layout` (main TUI
//! layout composer) and `smelt.overlay.new` (which consumes the same
//! layout userdata via `opts.layout`).
//!
//! The constructors (`leaf`, boxes, splits, and `measure`) are registered
//! exclusively under `smelt.ui.layout` - `smelt.overlay.new` accepts the
//! resulting userdata but doesn't host its own copy of the namespace.
//!
//! Constraint vocabulary on item slots matches `Constraint`:
//! integer (cells), `"fit"`, `"fill"`, `"N%"` (shorthand for `"pct:N"`),
//! `"min:N"`, `"max:N"`, `"pct:N"`, `"ratio:N/M"`, or the long table form
//! `{ kind = "...", n = N }`.

use crate::smelt_edit::layout::{
    Axis, Border, DividerStyles, Justify, Split, SplitOptions, SplitPane, SplitResizeMode,
    SplitSize,
};
use crate::smelt_edit::{Constraint, Natural, NaturalRef, StaticNatural};
use mlua::prelude::*;
use smelt_core::lua::lua_type::{LuaClassDecl, LuaClassField, LuaType};
use smelt_core::lua::module::LuaMod;
use smelt_term::Line;
use std::sync::{Arc, Mutex};

/// Mutable cell shared between Lua (`measure_handle:set(w, h)`) and the
/// term layout resolver (`Natural::size`). Lock contention is negligible -
/// the cell is only read during layout passes and written from Lua key
/// handlers / state changes.
#[derive(Clone)]
pub struct LuaMeasure {
    pub(crate) inner: Arc<Mutex<(u16, u16)>>,
}

impl LuaMeasure {
    fn new(w: u16, h: u16) -> Self {
        Self {
            inner: Arc::new(Mutex::new((w, h))),
        }
    }
}

impl mlua::UserData for LuaMeasure {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("set", |_, this, (w, h): (u16, u16)| {
            if let Ok(mut cell) = this.inner.lock() {
                *cell = (w, h);
            }
            Ok(())
        });
        methods.add_method("get", |_, this, ()| {
            let (w, h) = this.inner.lock().map(|c| *c).unwrap_or((0, 0));
            Ok((w, h))
        });
    }
}

impl LuaType for LuaMeasure {
    fn lua_type() -> String {
        smelt_core::lua::doc::record_class(LuaClassDecl {
            name: "smelt.ui.layout.Measure",
            classification: smelt_core::lua::doc::classification_for_type(
                "smelt.ui.layout.Measure",
            ),
            doc: "Shareable natural-size handle returned by `smelt.ui.layout.measure`.",
            fields: vec![
                LuaClassField {
                    name: "set",
                    ty: "fun(w: integer, h: integer): nil".into(),
                    optional: false,
                    doc: "Update the measured natural size.",
                },
                LuaClassField {
                    name: "get",
                    ty: "fun(): integer, integer".into(),
                    optional: false,
                    doc: "Return the current measured width and height.",
                },
            ],
        });
        "smelt.ui.layout.Measure".into()
    }
}

/// `Natural` impl that reads from the shared `LuaMeasure` cell each frame.
struct LuaMeasureNatural(Arc<Mutex<(u16, u16)>>);

impl Natural for LuaMeasureNatural {
    fn size(&self, _cap: (u16, u16)) -> (u16, u16) {
        self.0.lock().map(|c| *c).unwrap_or((0, 0))
    }
}

/// A layout node built in Lua and resolved by the host for root layouts,
/// dialogs, overlays, and decorations.
#[derive(Clone)]
pub(crate) enum LayoutNode {
    /// Opaque host-owned transcript-dialog stage placed by the main layout composer.
    DialogStage { id: crate::smelt_edit::ContainerId },
    /// A window/paint id leaf. Resolution to `WinId` vs `PaintId` happens at
    /// `overlay.open` time via `resolve_leaf_id`.
    Leaf {
        raw_id: u64,
        chrome: NodeChrome,
        collapse_when_empty: bool,
        natural: Option<NaturalRef>,
    },
    /// Vertical or horizontal container. Children are laid out along the
    /// primary axis with their `constraint`; the cross axis fills the
    /// container's extent.
    Container {
        kind: ContainerKind,
        items: Vec<LayoutItem>,
        chrome: NodeChrome,
        gap: u16,
    },
    Frame {
        child: Box<LayoutNode>,
        chrome: NodeChrome,
    },
    Split {
        split: Split,
        children: Box<[LayoutNode; 2]>,
        chrome: NodeChrome,
    },
}

impl LayoutNode {
    pub(crate) fn dialog_stage_counts(
        &self,
        active: Option<crate::smelt_edit::ContainerId>,
    ) -> (usize, usize) {
        match self {
            Self::DialogStage { id } => (usize::from(active == Some(*id)), 1),
            Self::Leaf { .. } => (0, 0),
            Self::Frame { child, .. } => child.dialog_stage_counts(active),
            Self::Split { children, .. } => {
                let (a, b) = children[0].dialog_stage_counts(active);
                let (c, d) = children[1].dialog_stage_counts(active);
                (a + c, b + d)
            }
            Self::Container { items, .. } => {
                items
                    .iter()
                    .fold((0, 0), |(active_count, total_count), item| {
                        let (item_active, item_total) = item.node.dialog_stage_counts(active);
                        (active_count + item_active, total_count + item_total)
                    })
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ContainerKind {
    Vbox,
    Hbox,
}

#[derive(Clone, Default)]
pub(crate) struct NodeChrome {
    pub border: Option<Border>,
    pub title: Option<Line<'static>>,
    pub padding: u16,
    pub justify: Justify,
}

/// One slot inside a `Container`. `constraint` sizes the slot along the
/// container's primary axis; the inner `node` fills the slot.
#[derive(Clone)]
pub(crate) struct LayoutItem {
    pub constraint: Constraint,
    pub node: LayoutNode,
}

/// Lua userdata wrapper for a built layout subtree.
#[derive(Clone)]
pub struct LuaUiLayout(pub(crate) LayoutNode);

impl mlua::UserData for LuaUiLayout {}

impl FromLua for LuaUiLayout {
    fn from_lua(value: mlua::Value, lua: &Lua) -> LuaResult<Self> {
        Ok(mlua::AnyUserData::from_lua(value, lua)?
            .borrow::<Self>()?
            .clone())
    }
}

fn parse_split_size(value: mlua::Value) -> LuaResult<SplitSize> {
    let size = match value {
        mlua::Value::Nil => Some(SplitSize::default()),
        mlua::Value::Integer(n) => u16::try_from(n).ok().map(SplitSize::Cells),
        mlua::Value::String(text) => {
            let text = text.to_str()?;
            if let Some(ratio) = text.strip_prefix("ratio:") {
                ratio
                    .split_once('/')
                    .and_then(|(n, d)| SplitSize::ratio(n.parse().ok()?, d.parse().ok()?))
            } else {
                text.strip_suffix('%')
                    .or_else(|| text.strip_prefix("pct:"))
                    .and_then(|p| SplitSize::ratio(p.parse().ok()?, 100))
            }
        }
        _ => None,
    };
    size.ok_or_else(|| mlua::Error::external(
        "split size must be integer cells in 0..65535, a percentage in 0..100%, or ratio:N/M with 0 <= N <= M and M > 0",
    ))
}

/// Retained sizing is independent of the layout children built for each frame.
#[derive(Clone)]
pub struct LuaSplit(Split);

impl LuaSplit {
    fn layout(
        &self,
        first: LuaUiLayout,
        second: LuaUiLayout,
        opts: Option<&mlua::Table>,
    ) -> LuaResult<LuaUiLayout> {
        Ok(LuaUiLayout(LayoutNode::Split {
            split: self.0.clone(),
            children: Box::new([first.0, second.0]),
            chrome: parse_node_chrome(opts, "split.layout").map_err(LuaError::external)?,
        }))
    }
}

fn split_changed(lua: &Lua, changed: bool) -> LuaResult<bool> {
    if changed {
        let shared = super::win::current_shared(lua)?;
        shared.request_layout_refresh();
        shared.invalidate_win_renderers();
    }
    Ok(changed)
}

impl mlua::UserData for LuaSplit {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("layout", |_, this, (first, second, opts): (LuaUiLayout, LuaUiLayout, Option<mlua::Table>)| {
            reject_options(opts.as_ref(), &["size", "resize", "min_first", "min_second", "divider", "gap", "justify"],
                "split:layout accepts only border, title and padding; configure sizing on the split handle")?;
            this.layout(first, second, opts.as_ref())
        });
        methods.add_method("size", |lua, this, ()| -> LuaResult<mlua::Value> {
            match this.0.preferred_size() {
                SplitSize::Cells(cells) => Ok(mlua::Value::Integer(i64::from(cells))),
                SplitSize::Ratio(ratio) => Ok(mlua::Value::String(lua.create_string(format!(
                    "ratio:{}/{}",
                    ratio.numerator(),
                    ratio.denominator()
                ))?)),
            }
        });
        methods.add_method("set_size", |lua, this, size: mlua::Value| {
            if size.is_nil() {
                return Err(LuaError::external("split size is required"));
            }
            split_changed(lua, this.0.set_preferred_size(parse_split_size(size)?))
        });
        methods.add_method("reset", |lua, this, ()| split_changed(lua, this.0.reset()));
        methods.add_method("resize", |lua, this, delta: i32| {
            let changed = crate::lua::with_ui_host(|host| {
                host.with_ui(|ui| {
                    ui.resolved_split(this.0.id())
                        .is_some_and(|split| split.resize(SplitPane::First, delta))
                })
            });
            split_changed(lua, changed)
        });
        methods.add_method("equalize", |lua, this, ()| {
            let changed = crate::lua::with_ui_host(|host| {
                host.with_ui(|ui| {
                    ui.resolved_split(this.0.id())
                        .is_some_and(|split| split.equalize())
                })
            });
            split_changed(lua, changed)
        });
    }
}

impl LuaType for LuaSplit {
    fn lua_type() -> String {
        smelt_core::lua::doc::record_class(LuaClassDecl {
            name: "smelt.ui.layout.Split",
            classification: smelt_core::lua::doc::classification_for_type("smelt.ui.layout.Split"),
            doc: "Retained split handle. Compose new children with layout() without resetting user sizing. Identity, axis, minima, resize policy, and divider styles are immutable. Mount a handle only once at a time; persist size(), not the handle.",
            fields: vec![
                LuaClassField { name: "layout", ty: "fun(self: smelt.ui.layout.Split, first: smelt.ui.layout, second: smelt.ui.layout, opts?: table): smelt.ui.layout".into(), optional: false, doc: "Compose children with this handle. opts accepts outer border, title, and padding, independently of retained sizing." },
                LuaClassField { name: "size", ty: "fun(self: smelt.ui.layout.Split): integer | string".into(), optional: false, doc: "Unclamped preferred size: integer cells or ratio:N/M. Can be persisted and passed to set_size or the constructor." },
                LuaClassField { name: "set_size", ty: "fun(self: smelt.ui.layout.Split, size: integer | string): boolean".into(), optional: false, doc: "Set or restore preferred sizing, without applying temporary screen bounds. Returns whether the preference changed." },
                LuaClassField { name: "reset", ty: "fun(self: smelt.ui.layout.Split): boolean".into(), optional: false, doc: "Restore the initial preference. Returns whether it changed." },
                LuaClassField { name: "resize", ty: "fun(self: smelt.ui.layout.Split, delta: integer): boolean".into(), optional: false, doc: "Grow the first pane by signed cells using current mounted geometry and resize policy. Returns whether the preference changed; false when unmounted or at its bounds." },
                LuaClassField { name: "equalize", ty: "fun(self: smelt.ui.layout.Split): boolean".into(), optional: false, doc: "Balance mounted panes using the configured resize policy. Returns whether the preference changed; false when unmounted." },
            ],
        });
        "smelt.ui.layout.Split".into()
    }
}

fn reject_options(opts: Option<&mlua::Table>, keys: &[&str], message: &str) -> LuaResult<()> {
    if let Some(opts) = opts {
        for key in keys {
            if !opts.get::<mlua::Value>(*key)?.is_nil() {
                return Err(LuaError::external(message));
            }
        }
    }
    Ok(())
}

fn split_minimum(opts: Option<&mlua::Table>, key: &str) -> LuaResult<u16> {
    match opts
        .map(|opts| opts.get(key))
        .transpose()?
        .unwrap_or(mlua::Value::Nil)
    {
        mlua::Value::Nil => Ok(1),
        mlua::Value::Integer(n) => u16::try_from(n)
            .map_err(|_| LuaError::external(format!("{key} must be integer cells in 0..65535"))),
        _ => Err(LuaError::external(format!(
            "{key} must be integer cells in 0..65535"
        ))),
    }
}

fn create_split(axis: Axis, opts: Option<&mlua::Table>) -> LuaResult<LuaSplit> {
    let size = parse_split_size(
        opts.map(|opts| opts.get("size"))
            .transpose()?
            .unwrap_or(mlua::Value::Nil),
    )?;
    let mode = opts
        .map(|opts| opts.get::<Option<String>>("resize"))
        .transpose()?
        .flatten();
    let resize_mode = match mode.as_deref() {
        None | Some("proportional") => SplitResizeMode::Proportional,
        Some("cells") => SplitResizeMode::Cells,
        _ => {
            return Err(LuaError::external(
                "split resize must be cells or proportional",
            ))
        }
    };
    let first = split_minimum(opts, "min_first")?;
    let second = split_minimum(opts, "min_second")?;
    let styles = opts
        .map(|opts| opts.get::<Option<mlua::Table>>("divider"))
        .transpose()?
        .flatten()
        .map(|styles| -> LuaResult<_> {
            let normal = crate::lua::parse::style(&styles.get::<mlua::Table>("normal")?)
                .map_err(LuaError::external)?;
            let active = styles
                .get::<Option<mlua::Table>>("active")?
                .map(|style| crate::lua::parse::style(&style).map_err(LuaError::external))
                .transpose()?
                .unwrap_or(normal);
            Ok(DividerStyles { normal, active })
        })
        .transpose()?;
    Ok(LuaSplit(Split::new(
        axis,
        SplitOptions {
            size,
            minimum: [first, second],
            resize_mode,
            styles,
        },
    )))
}

struct SplitOpts(mlua::Table);

impl FromLua for SplitOpts {
    fn from_lua(value: mlua::Value, lua: &Lua) -> LuaResult<Self> {
        Ok(Self(mlua::Table::from_lua(value, lua)?))
    }
}

impl LuaType for SplitOpts {
    fn lua_type() -> String {
        smelt_core::lua::doc::record_class(LuaClassDecl {
            name: "smelt.ui.layout.SplitOpts",
            classification: smelt_core::lua::doc::classification_for_type("smelt.ui.layout.SplitOpts"),
            doc: "Options for a two-pane resizable layout. Sizes include each child's border and padding. If the terminal cannot fit both minima, they shrink proportionally without discarding the preferred split.",
            fields: vec![
                LuaClassField { name: "size", ty: "integer | string".into(), optional: true, doc: "Initial first-pane size in cells, a percentage such as 30%, or ratio:N/M. Defaults to 50%. The resize option controls how user sizing is retained across terminal resizes." },
                LuaClassField { name: "resize", ty: "'cells' | 'proportional'".into(), optional: true, doc: "How user resizing is retained across terminal-size changes. Defaults to proportional; cells keeps a fixed first-pane width or height." },
                LuaClassField { name: "min_first", ty: "integer".into(), optional: true, doc: "Minimum first-pane size in cells; defaults to 1." },
                LuaClassField { name: "min_second", ty: "integer".into(), optional: true, doc: "Minimum second-pane size in cells; defaults to 1." },
                LuaClassField { name: "divider", ty: "{normal: table, active?: table}".into(), optional: true, doc: "Explicit divider styles, using fg, bg, bold, dim, italic, underline, crossedout, and reverse. normal is required; active defaults to normal. Omit to follow the renderer's theme." },
                LuaClassField { name: "border", ty: "string | table".into(), optional: true, doc: "Outer border for hsplit/vsplit. With a retained split, pass chrome to handle:layout instead." },
                LuaClassField { name: "title", ty: "string | table".into(), optional: true, doc: "Outer-border title for hsplit/vsplit; pass to handle:layout when using a retained split." },
                LuaClassField { name: "padding", ty: "integer".into(), optional: true, doc: "Outer padding for hsplit/vsplit; pass to handle:layout when using a retained split." },
            ],
        });
        "smelt.ui.layout.SplitOpts".into()
    }
}

impl LuaType for LuaUiLayout {
    fn lua_type() -> String {
        "smelt.ui.layout".into()
    }
}

/// Resolve a `smelt.ui.layout.leaf(target)` argument to the raw u64 id
/// stored in the layout node. Accepts a `Win` userdata, a `Paint`
/// handle from `smelt.paint.register`, a raw paint id integer, or a
/// raw win id integer.
fn resolve_leaf_target(target: &mlua::Value) -> mlua::Result<u64> {
    match target {
        mlua::Value::UserData(ud) => {
            if let Ok(w) = ud.borrow::<super::win::LuaWin>() {
                return Ok(w.id.0);
            }
            if let Ok(p) = ud.borrow::<super::paint::LuaPaintReg>() {
                return Ok(p.id.0);
            }
            Err(mlua::Error::external(
                "smelt.ui.layout.leaf: expected a Win or Paint handle (or raw id)",
            ))
        }
        mlua::Value::Integer(i) => Ok(*i as u64),
        mlua::Value::Number(n) => Ok(*n as u64),
        other => Err(mlua::Error::external(format!(
            "smelt.ui.layout.leaf: expected Win/Paint handle or integer, got {}",
            other.type_name()
        ))),
    }
}

/// Parse `opts.measure`. Accepts:
///   * `nil` - no override; the host's `LeafSizer` decides
///   * `{ w, h }` array - fixed natural size
///   * `smelt.ui.layout.measure(...)` userdata - shared mutable cell
fn parse_measure(opts: Option<&mlua::Table>, ctx: &str) -> mlua::Result<Option<NaturalRef>> {
    let Some(t) = opts else { return Ok(None) };
    let v: mlua::Value = match t.get("measure") {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    match v {
        mlua::Value::Nil => Ok(None),
        mlua::Value::Table(tbl) => {
            let w: u16 = tbl
                .get(1)
                .map_err(|e| mlua::Error::external(format!("{ctx}: missing width: {e}")))?;
            let h: u16 = tbl
                .get(2)
                .map_err(|e| mlua::Error::external(format!("{ctx}: missing height: {e}")))?;
            Ok(Some(Arc::new(StaticNatural(w, h)) as NaturalRef))
        }
        mlua::Value::UserData(ud) => {
            let m = ud.borrow::<LuaMeasure>().map_err(|e| {
                mlua::Error::external(format!(
                    "{ctx}: expected a measure handle or {{w, h}} table: {e}"
                ))
            })?;
            Ok(Some(
                Arc::new(LuaMeasureNatural(m.inner.clone())) as NaturalRef
            ))
        }
        other => Err(mlua::Error::external(format!(
            "{ctx}: expected nil, {{w, h}}, or measure handle; got {}",
            other.type_name()
        ))),
    }
}

/// Pull `border` / `title` / `padding` off any node-builder opts table.
fn parse_node_chrome(opts: Option<&mlua::Table>, ctx: &str) -> Result<NodeChrome, String> {
    let Some(t) = opts else {
        return Ok(NodeChrome::default());
    };
    let border = match t.get::<mlua::Value>("border").ok() {
        None | Some(mlua::Value::Nil) => None,
        _ => crate::lua::parse::border(t).map_err(|e| format!("{ctx}.border: {e}"))?,
    };
    let title = crate::lua::parse::title(t.get::<mlua::Value>("title").ok())
        .map_err(|e| format!("{ctx}.title: {e}"))?;
    let padding = t.get::<u16>("padding").unwrap_or(0);
    let justify = match t.get::<Option<String>>("justify").ok().flatten().as_deref() {
        None | Some("start") => Justify::Start,
        Some("space-between") | Some("space_between") => Justify::SpaceBetween,
        Some(other) => {
            return Err(format!(
                "{ctx}.justify: unknown value '{other}' (expected start|space-between)"
            ))
        }
    };
    Ok(NodeChrome {
        border,
        title,
        padding,
        justify,
    })
}

/// Read `items = { { node, width|height = ..., collapse_when_empty = ... }, ... }`
/// into a list of constrained slots. `axis_key` is `"width"` (hbox) or
/// `"height"` (vbox).
fn parse_items(t: &mlua::Table, axis_key: &str, ctx: &str) -> mlua::Result<Vec<LayoutItem>> {
    let mut out = Vec::new();
    for (i, pair) in t.sequence_values::<mlua::Table>().enumerate() {
        let item = pair?;
        // First positional element is the child node userdata.
        let node_ud: mlua::AnyUserData = item.get(1).map_err(|e| {
            mlua::Error::external(format!(
                "{ctx}.items[{}]: expected layout userdata at index 1: {e}",
                i + 1
            ))
        })?;
        let node = node_ud.borrow::<LuaUiLayout>()?.0.clone();
        let constraint = crate::lua::parse::constraint(
            item.get::<mlua::Value>(axis_key).ok(),
            &format!("{ctx}.items[{}].{axis_key}", i + 1),
        )
        .map_err(mlua::Error::external)?;
        out.push(LayoutItem { constraint, node });
    }
    Ok(out)
}

/// Register leaf, frame, box, split and measure constructors on the
/// `smelt.ui.layout` module. Error messages and userdata type names
/// reference `smelt.ui.layout` so a plugin author always sees the same
/// path back to the docs.
pub(crate) fn register_layout_constructors(m: &LuaMod) -> LuaResult<()> {
    const CTX: &str = "smelt.ui.layout";
    m.fn_(
        "leaf",
        "Wrap a Win handle or paint id into a leaf node. `opts` accepts `border`, `title`, `collapse_when_empty` (force the slot to zero size when the wrapped window's buffer is empty), `measure` (a `{w, h}` table for a static natural size or a `smelt.ui.layout.measure(...)` handle for one the plugin can live-update).",
        &["win_or_paint", "opts"],
        |_, (target, opts): (mlua::Value, Option<mlua::Table>)| -> LuaResult<LuaUiLayout> {
            let raw_id = resolve_leaf_target(&target)?;
            let chrome = parse_node_chrome(opts.as_ref(), CTX).map_err(mlua::Error::external)?;
            let collapse_when_empty = opts
                .as_ref()
                .and_then(|t| t.get::<bool>("collapse_when_empty").ok())
                .unwrap_or(false);
            let natural = parse_measure(opts.as_ref(), CTX)?;
            Ok(LuaUiLayout(LayoutNode::Leaf {
                raw_id,
                chrome,
                collapse_when_empty,
                natural,
            }))
        },
    )?;

    m.fn_(
        "frame",
        "Wrap a subtree in optional border, title and padding. Natural size is the child's demand plus chrome; the child fills the inset area when the parent grows. Unlike a box slot, no fit/fill constraint is needed. Child chrome and shared split positions remain intact.",
        &["node", "opts"],
        |_, (node, opts): (LuaUiLayout, Option<mlua::Table>)| -> LuaResult<LuaUiLayout> {
            let chrome = parse_node_chrome(opts.as_ref(), CTX).map_err(mlua::Error::external)?;
            Ok(LuaUiLayout(LayoutNode::Frame { child: Box::new(node.0), chrome }))
        },
    )?;

    m.fn_(
        "measure",
        "Construct a shareable natural-size handle for use with `smelt.ui.layout.leaf(opts.measure = ...)`. Initial size is `(w, h)` (default `(0, 0)`); update at any time via `handle:set(w, h)` to drive a live resize on the next frame. Read current size via `handle:get()`.",
        &["w", "h"],
        |_, (w, h): (Option<u16>, Option<u16>)| -> LuaResult<LuaMeasure> {
            Ok(LuaMeasure::new(w.unwrap_or(0), h.unwrap_or(0)))
        },
    )?;

    m.fn_(
        "vbox",
        "Vertical container. `items` is an array of `{ child_layout, height = <constraint>, collapse_when_empty = bool? }`. `opts` accepts `border`, `title`, `gap` (minimum cells between children), `justify = \"space-between\"` (put surplus cells into gaps), `padding` (uniform inner inset on all sides, inside any border).",
        &["items", "opts"],
        |_, (items_tbl, opts): (mlua::Table, Option<mlua::Table>)| -> LuaResult<LuaUiLayout> {
            let items = parse_items(&items_tbl, "height", CTX)?;
            let chrome = parse_node_chrome(opts.as_ref(), CTX).map_err(mlua::Error::external)?;
            let gap = opts
                .as_ref()
                .and_then(|t| t.get::<u16>("gap").ok())
                .unwrap_or(0);
            Ok(LuaUiLayout(LayoutNode::Container {
                kind: ContainerKind::Vbox,
                items,
                chrome,
                gap,
            }))
        },
    )?;

    m.fn_(
        "hbox",
        "Horizontal container. `items` is an array of `{ child_layout, width = <constraint>, collapse_when_empty = bool? }`. `opts` accepts `border`, `title`, `gap`, `justify = \"space-between\"`, `padding` (uniform inner inset on all sides, inside any border).",
        &["items", "opts"],
        |_, (items_tbl, opts): (mlua::Table, Option<mlua::Table>)| -> LuaResult<LuaUiLayout> {
            let items = parse_items(&items_tbl, "width", CTX)?;
            let chrome = parse_node_chrome(opts.as_ref(), CTX).map_err(mlua::Error::external)?;
            let gap = opts
                .as_ref()
                .and_then(|t| t.get::<u16>("gap").ok())
                .unwrap_or(0);
            Ok(LuaUiLayout(LayoutNode::Container {
                kind: ContainerKind::Hbox,
                items,
                chrome,
                gap,
            }))
        },
    )?;

    m.fn_("windows", "Return a layout's live window leaves in declaration order, excluding paints and repeated windows. Useful for installing shared callbacks and composing dialog bodies from arbitrary layouts.", &["node"], |_, (node,): (LuaUiLayout,)| -> LuaResult<Vec<super::win::LuaWin>> {
        crate::lua::with_ui_host(|host| host.layout_windows(&node.0))
            .map(|windows| windows.into_iter().map(|id| super::win::LuaWin { id }).collect())
            .map_err(mlua::Error::external)
    })?;

    m.fn_("split", "Create a retained split handle independent of its children. axis is horizontal (side by side) or vertical (stacked). Use handle:layout(first, second, chrome_opts) in composers; rebuilding children preserves sizing. Handle methods inspect, restore, reset, resize, or equalize this exact split.", &["axis", "opts"],
        |_, (axis, opts): (String, Option<SplitOpts>)| -> LuaResult<LuaSplit> {
            let axis = match axis.as_str() {
                "horizontal" => Axis::Horizontal,
                "vertical" => Axis::Vertical,
                _ => return Err(LuaError::external("split axis must be horizontal or vertical")),
            };
            let opts = opts.as_ref().map(|opts| &opts.0);
            reject_options(opts, &["border", "title", "padding", "gap", "justify"],
                "split constructor accepts sizing only; pass chrome to split:layout")?;
            create_split(axis, opts)
        })?;

    for (name, axis, doc) in [
        ("hsplit", Axis::Horizontal, "Place two subtrees side by side with a draggable divider. Retain this node across composer calls to preserve sizing. For independently rebuilt children or direct size control, use layout.split(\"horizontal\", opts) and handle:layout(first, second). win:resize(\"width\", delta) targets the nearest enclosing horizontal split."),
        ("vsplit", Axis::Vertical, "Stack two subtrees with a draggable divider. Retain this node across composer calls to preserve sizing. For independently rebuilt children or direct size control, use layout.split(\"vertical\", opts) and handle:layout(first, second). win:resize(\"height\", delta) targets the nearest enclosing vertical split."),
    ] {
        m.fn_(name, doc, &["first", "second", "opts"], move |_, (first, second, opts): (LuaUiLayout, LuaUiLayout, Option<SplitOpts>)| -> LuaResult<LuaUiLayout> {
            let opts = opts.as_ref().map(|opts| &opts.0);
            create_split(axis, opts)?.layout(first, second, opts)
        })?;
    }

    Ok(())
}
