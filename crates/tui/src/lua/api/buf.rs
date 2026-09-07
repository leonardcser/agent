//! `smelt.buf` - Buf handle. UiHost-only.
//!
//! `smelt.buf.new(opts?)` returns a `Buf` userdata with chainable
//! methods. `opts.name` opts the buffer into hot-reload survival:
//! a repeat call with the same name returns the existing buf with
//! its mutable opts re-applied.

use crate::lua::LuaShared;
use lua_doc_derive::{LuaAlias, LuaOpts};
use mlua::prelude::*;
use smelt_core::lua::doc::{record_class, Tier};
use smelt_core::lua::lua_type::{LuaClassDecl, LuaClassField, LuaType};
use smelt_core::lua::module::LuaMod;
use std::sync::Arc;

/// Where a virtual-text chunk is rendered relative to the line.
#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.buf.VirtTextPos")]
pub enum LuaVirtTextPos {
    /// Inserted inline at `(row, col)`, shifting real text after it.
    Inline,
    /// Painted on top of existing text at `(row, col)`.
    Overlay,
    /// Right-aligned at the end of the screen line.
    RightAlign,
    /// Appended after the last column (default).
    Eol,
}

impl From<LuaVirtTextPos> for smelt_core::buffer::VirtTextPos {
    fn from(p: LuaVirtTextPos) -> Self {
        use smelt_core::buffer::VirtTextPos;
        match p {
            LuaVirtTextPos::Inline => VirtTextPos::Inline,
            LuaVirtTextPos::Overlay => VirtTextPos::Overlay,
            LuaVirtTextPos::RightAlign => VirtTextPos::RightAlign,
            LuaVirtTextPos::Eol => VirtTextPos::Eol,
        }
    }
}

/// Color value for the `fg`/`bg` highlight fields. Accepts:
/// - `string` - theme-group name resolved through the active theme.
/// - `{ r, g, b }` - direct RGB triple (integer array).
/// - `{ rgb = { r, g, b } }` - same RGB triple in the `StyleDecl` shape
///   that `smelt.theme.get(group)` returns, so a caller can pipe a
///   theme-derived color straight back into another highlight.
/// - `{ ansi = N }` - direct 256-color slot in the same shape.
#[derive(Debug, Clone)]
pub enum LuaColor {
    Group(String),
    Direct(smelt_core::style::Color),
}

impl LuaType for LuaColor {
    fn lua_type() -> String {
        "string | integer[] | { ansi: integer } | { rgb: integer[] }".into()
    }
}

impl FromLua for LuaColor {
    fn from_lua(value: mlua::Value, _: &Lua) -> LuaResult<Self> {
        match value {
            mlua::Value::String(s) => Ok(LuaColor::Group(s.to_str()?.to_string())),
            mlua::Value::Table(t) => {
                if let Ok(n) = t.get::<u8>("ansi") {
                    return Ok(LuaColor::Direct(smelt_core::style::Color::AnsiValue(n)));
                }
                if let Ok(rgb) = t.get::<[u8; 3]>("rgb") {
                    return Ok(LuaColor::Direct(smelt_core::style::Color::Rgb {
                        r: rgb[0],
                        g: rgb[1],
                        b: rgb[2],
                    }));
                }
                let r: u8 = t.get(1)?;
                let g: u8 = t.get(2)?;
                let b: u8 = t.get(3)?;
                Ok(LuaColor::Direct(smelt_core::style::Color::Rgb { r, g, b }))
            }
            other => Err(mlua::Error::FromLuaConversionError {
                from: other.type_name(),
                to: "smelt color (theme group name, {r,g,b}, {ansi=N}, or {rgb={...}})".into(),
                message: None,
            }),
        }
    }
}

impl LuaColor {
    fn resolve_fg(&self) -> Option<smelt_core::style::Color> {
        match self {
            LuaColor::Group(name) => {
                crate::lua::with_runtime_host(|host| host.theme_group(name).fg)
            }
            LuaColor::Direct(c) => Some(*c),
        }
    }

    fn resolve_bg(&self) -> Option<smelt_core::style::Color> {
        match self {
            LuaColor::Group(name) => {
                crate::lua::with_runtime_host(|host| host.theme_group(name).bg)
            }
            LuaColor::Direct(c) => Some(*c),
        }
    }
}

/// Options accepted by `buf:mark(ns, row, col, opts)`. Mirrors a useful
/// subset of `nvim_buf_set_extmark`'s keyset; pick highlight or
/// virt-text fields, not both.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.buf.MarkOpts")]
pub struct LuaMarkOpts {
    /// Retarget an existing mark by id instead of allocating a new one.
    pub id: Option<u32>,
    /// 1-based end row (inclusive). `nil` keeps the mark single-line.
    pub end_row: Option<u64>,
    /// End byte offset for highlight ranges (exclusive). Same unit as
    /// `col` - bytes into the line, matching `#s` and `string.find`.
    pub end_col: Option<u64>,
    /// Higher-priority marks paint over lower-priority ones.
    #[lua(default)]
    pub priority: u32,
    /// If true, the mark sticks with text inserted to its right.
    pub right_gravity: Option<bool>,
    /// Right-gravity flag for the end-of-range cursor.
    pub end_right_gravity: Option<bool>,

    /// Theme group whose style is applied as the highlight base.
    pub hl_group: Option<String>,
    /// Foreground override. Either a theme group name (string) or a
    /// direct RGB triple `{ r, g, b }`. Takes precedence over `hl_group`.
    pub fg: Option<LuaColor>,
    /// Background override. Either a theme group name (string) or a
    /// direct RGB triple `{ r, g, b }`. Takes precedence over `hl_group`.
    pub bg: Option<LuaColor>,
    /// Force-bold the highlight.
    pub bold: Option<bool>,
    /// Force-dim the highlight.
    pub dim: Option<bool>,
    /// Force-italic the highlight.
    pub italic: Option<bool>,
    /// Force reverse-video on the highlight.
    pub reverse: Option<bool>,
    /// Extend the highlight past the last column to fill the EOL.
    pub hl_eol: Option<bool>,
    /// Paint only on the window's cursor row. Decorates the
    /// selected list item without re-rendering on every move.
    pub on_cursor_row: Option<bool>,

    /// Virtual-text chunk to render alongside the line.
    pub virt_text: Option<String>,
    /// Theme group applied to the virt-text chunk. `fg`, `bg`, and text
    /// attributes below can further override it.
    pub virt_text_hl: Option<String>,
    /// Where the virt-text appears relative to the line.
    pub virt_text_pos: Option<LuaVirtTextPos>,

    /// If false, the range is skipped by mouse selection.
    pub selectable: Option<bool>,
    /// Override the yanked string when the user copies this range.
    pub yank_as: Option<String>,
}

/// Options accepted by `smelt.buf.new(opts?)`.
struct LuaBufNewOpts(mlua::Table);

impl LuaType for LuaBufNewOpts {
    fn lua_type() -> String {
        record_class(LuaClassDecl {
            name: "smelt.buf.NewOpts",
            classification: smelt_core::lua::doc::classification_for_type("smelt.buf.NewOpts"),
            doc: "Options for `smelt.buf.new(opts?)`. Named buffers survive `/reload`; anonymous buffers are reaped.",
            fields: vec![
                LuaClassField {
                    name: "name",
                    ty: "string".into(),
                    optional: true,
                                        doc: "Stable name used to reuse this buffer across `/reload`.",
                },
                LuaClassField {
                    name: "readonly",
                    ty: "boolean".into(),
                    optional: true,
                                        doc: "When true, UI editing operations cannot mutate the buffer.",
                },
                LuaClassField {
                    name: "editable",
                    ty: "boolean".into(),
                    optional: true,
                                        doc: "Enable undo history for plugin-managed editable buffers.",
                },
                LuaClassField {
                    name: "undo",
                    ty: "integer".into(),
                    optional: true,
                                        doc: "Undo history entry limit when `editable = true` (defaults to 100).",
                },
                LuaClassField {
                    name: "mode",
                    ty: "\"plain\"|\"markdown\"|\"md\"|\"code\"".into(),
                    optional: true,
                                        doc: "Attach a parser-backed renderer to the buffer.",
                },
                LuaClassField {
                    name: "lang",
                    ty: "string".into(),
                    optional: true,
                                        doc: "Syntax language token required by `mode = \"code\"`.",
                },
                LuaClassField {
                    name: "diff_base",
                    ty: "string".into(),
                    optional: true,
                                        doc: "When `mode = \"code\"`, render the buffer source as an inline diff against this base text.",
                },
            ],
        });
        "smelt.buf.NewOpts".into()
    }
}

impl FromLua for LuaBufNewOpts {
    fn from_lua(value: mlua::Value, lua: &Lua) -> LuaResult<Self> {
        Ok(Self(mlua::Table::from_lua(value, lua)?))
    }
}

/// Lua-side handle for a `BufId`. Methods on this userdata are the
/// only public mutators; the constructor `smelt.buf.new(opts?)` returns
/// one of these.
#[derive(Clone, Copy, Debug)]
pub struct LuaBuf {
    pub(crate) id: crate::smelt_edit::BufId,
}

impl LuaType for LuaBuf {
    fn lua_type() -> String {
        "smelt.buf.Buf".into()
    }
}

impl FromLua for LuaBuf {
    fn from_lua(value: mlua::Value, _: &Lua) -> LuaResult<Self> {
        match value {
            mlua::Value::UserData(ud) => Ok(*ud.borrow::<LuaBuf>()?),
            other => Err(mlua::Error::FromLuaConversionError {
                from: other.type_name(),
                to: "smelt.buf.Buf".into(),
                message: Some("expected a Buf userdata (built via `smelt.buf.new(...)`)".into()),
            }),
        }
    }
}

impl mlua::UserData for LuaBuf {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, ()| {
            Ok(format!("Buf#{}", this.id.0))
        });

        // ── source (full text): get / set ──────────────────────────
        methods.add_function(
            "source",
            |lua, (this_ud, s): (mlua::AnyUserData, Option<String>)| -> LuaResult<mlua::Value> {
                let this = *this_ud.borrow::<LuaBuf>()?;
                match s {
                    Some(text) => {
                        if this.id == crate::app::PROMPT_EDIT_BUF {
                            crate::lua::with_conversation_host(|host| {
                                host.replace_prompt_text(text);
                            });
                        } else {
                            crate::lua::with_ui_host(|host| {
                                host.with_ui(|ui| {
                                    if let Some(buf) = ui.buf_mut(this.id) {
                                        buf.set_source(text);
                                    }
                                });
                            });
                        }
                        Ok(mlua::Value::UserData(this_ud))
                    }
                    None => {
                        let out = crate::lua::try_with_ui_host(|host| {
                            host.with_ui(|ui| ui.buf(this.id).map(|buf| buf.source().to_string()))
                        })
                        .flatten();
                        Ok(match out {
                            Some(s) => mlua::Value::String(lua.create_string(&s)?),
                            None => mlua::Value::Nil,
                        })
                    }
                }
            },
        );

        // ── lines (vec<string>): get / set ─────────────────────────
        methods.add_function(
            "lines",
            |lua,
             (this_ud, arr): (mlua::AnyUserData, Option<Vec<String>>)|
             -> LuaResult<mlua::Value> {
                let this = *this_ud.borrow::<LuaBuf>()?;
                match arr {
                    Some(lines) => {
                        if this.id == crate::app::PROMPT_EDIT_BUF {
                            crate::lua::with_conversation_host(|host| {
                                host.replace_prompt_text(lines.join("\n"));
                            });
                        } else {
                            crate::lua::with_ui_host(|host| {
                                host.with_ui(|ui| {
                                    if let Some(buf) = ui.buf_mut(this.id) {
                                        buf.set_all_lines(lines);
                                    }
                                });
                            });
                        }
                        Ok(mlua::Value::UserData(this_ud))
                    }
                    None => {
                        let out: Option<Vec<String>> = crate::lua::try_with_ui_host(|host| {
                            host.with_ui(|ui| {
                                ui.buf(this.id).map(|buf| {
                                    buf.lines().iter().map(|line| line.to_string()).collect()
                                })
                            })
                        })
                        .flatten();
                        match out {
                            Some(v) => {
                                let t = lua.create_table()?;
                                for (i, s) in v.into_iter().enumerate() {
                                    t.set(i + 1, s)?;
                                }
                                Ok(mlua::Value::Table(t))
                            }
                            None => Ok(mlua::Value::Nil),
                        }
                    }
                }
            },
        );

        // ── line(idx) - single line read, 1-based ──────────────────
        methods.add_method("line", |_, this, idx: u64| -> LuaResult<Option<String>> {
            let line0 = match idx.checked_sub(1) {
                Some(n) => n as usize,
                None => return Ok(None),
            };
            Ok(crate::lua::try_with_ui_host(|host| {
                host.with_ui(|ui| {
                    ui.buf(this.id)
                        .and_then(|buf| buf.get_line(line0).map(|line| line.to_string()))
                })
            })
            .flatten())
        });

        // ── styled(spans) - set styled lines (chainable) ───────────
        methods.add_function(
            "styled",
            |_,
             (this_ud, lines): (mlua::AnyUserData, mlua::Table)|
             -> LuaResult<mlua::AnyUserData> {
                let this = *this_ud.borrow::<LuaBuf>()?;
                set_styled_lines(this.id, lines)?;
                Ok(this_ud)
            },
        );

        // ── readonly: get / set ────────────────────────────────────
        methods.add_function(
            "readonly",
            |_, (this_ud, val): (mlua::AnyUserData, Option<bool>)| -> LuaResult<mlua::Value> {
                let this = *this_ud.borrow::<LuaBuf>()?;
                match val {
                    Some(ro) => {
                        crate::lua::with_ui_host(|host| {
                            host.with_ui(|ui| {
                                if let Some(buf) = ui.buf_mut(this.id) {
                                    buf.readonly = ro;
                                }
                            });
                        });
                        Ok(mlua::Value::UserData(this_ud))
                    }
                    None => {
                        let out = crate::lua::try_with_ui_host(|host| {
                            host.with_ui(|ui| ui.buf(this.id).map(|buf| buf.readonly))
                        })
                        .flatten()
                        .unwrap_or(false);
                        Ok(mlua::Value::Boolean(out))
                    }
                }
            },
        );

        // ── mark(ns, row, col, opts?) → extmark id ─────────────────
        methods.add_method(
            "mark",
            |_,
             this,
             (ns, row, col, opts): (u32, u64, u64, Option<LuaMarkOpts>)|
             -> LuaResult<u64> { Ok(set_extmark(this.id, ns, row, col, opts)) },
        );

        // ── clear_ns(ns, start?, end?) - chainable ─────────────────
        methods.add_function(
            "clear_ns",
            |_,
             (this_ud, ns, start, end_): (mlua::AnyUserData, u32, Option<i64>, Option<i64>)|
             -> LuaResult<mlua::AnyUserData> {
                let this = *this_ud.borrow::<LuaBuf>()?;
                use smelt_core::buffer::NsId;
                let start_line = match start {
                    Some(n) if n > 0 => (n as usize).saturating_sub(1),
                    _ => 0,
                };
                let end_line = match end_ {
                    Some(n) if n > 0 => n as usize,
                    _ => usize::MAX,
                };
                crate::lua::with_ui_host(|host| {
                    host.with_ui(|ui| {
                        if let Some(buf) = ui.buf_mut(this.id) {
                            buf.clear_namespace(NsId(ns), start_line, end_line);
                        }
                    });
                });
                Ok(this_ud)
            },
        );
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::advanced(
        lua,
        smelt,
        "buf",
        "Buffer handle constructor. `smelt.buf.new(opts?)` returns a `Buf` userdata. \
`opts.name` opts the buffer into hot-reload survival - repeat calls with the same name \
return the same handle with mutable opts re-applied. Anonymous buffers are reaped on `/reload`. \
UiHost-only - buffers are terminal-screen backing stores that windows render into.",
        Tier::UiHost,
    )?;

    record_class(LuaClassDecl {
        name: "smelt.buf.Buf",
        classification: smelt_core::lua::doc::classification_for_type("smelt.buf.Buf"),
        doc: "Buffer handle returned by `smelt.buf.new(opts?)`. Setter methods return the same handle for chaining.",
        fields: smelt_core::class_methods! {
            "source" => fn(s: Option<String>) -> mlua::Value, "Read or write the buffer's full source. Without arg returns the source string (or `nil` if the buffer is gone). With arg replaces the source and returns the handle for chaining. On the built-in prompt buffer, this routes through `smelt.prompt.set_text` semantics so cursor, undo, attachments, and completer state stay coherent.",
            "lines" => fn(arr: Option<Vec<String>>) -> mlua::Value, "Read or write the buffer as a string array. Without arg returns the lines; with arg replaces all lines and returns the handle for chaining. On the built-in prompt buffer, writes are joined with `\\n` and installed through `smelt.prompt.set_text` semantics.",
            "line" => fn(idx: u64) -> Option<String>, "Read a single line by 1-based index. `nil` if out of range or the buffer is gone.",
            "styled" => fn(lines: mlua::Table) -> LuaBuf, "Replace the buffer with a list of styled lines (`{ { text, style?, syntax? }, ... }`). Returns the handle for chaining.",
            "readonly" => fn(val: Option<bool>) -> mlua::Value, "Read or write the readonly flag. With arg, returns the handle for chaining.",
            "mark" => fn(ns: u32, row: u64, col: u64, opts: Option<LuaMarkOpts>) -> u64, "Place a highlight or virt-text extmark at `(row, col)`. Row is 1-based; `col` and `opts.end_col` are byte offsets into the line, the same unit as `#s`, `string.find`, and `string.sub`. Off-boundary bytes snap to the nearest UTF-8 char boundary; out-of-range bytes clamp to the line end. Returns the new extmark id. Allocate `ns` via `smelt.ns(name)`.",
            "clear_ns" => fn(ns: u32, start: Option<i64>, end_: Option<i64>) -> LuaBuf, "Drop every extmark owned by `ns` between `[start, end)` (1-based, exclusive end). Defaults clear the whole buffer. Returns the handle for chaining.",
        },
    });

    // ── smelt.buf.new(opts?) ───────────────────────────────────────
    {
        let s = shared.clone();
        m.fn_(
            "new",
            "Create a buffer and return a `Buf` userdata. `opts.name` opts the buffer into hot-reload survival. Repeat calls with the same name return the same handle with mutable opts re-applied. When omitted from a module body, a stable per-(plugin, declaration-index) name is auto-assigned so the buffer survives `/reload` without explicit naming.",
            &["opts"],
            move |lua, opts: Option<LuaBufNewOpts>| -> LuaResult<LuaBuf> {
                // Auto-name from active plugin scope when caller didn't.
                if let Some(ref opts) = opts {
                    let tbl = &opts.0;
                    let has_name = tbl
                        .get::<Option<String>>("name")
                        .ok()
                        .flatten()
                        .is_some();
                    if !has_name {
                        if let Some(auto) = crate::lua::auto_name_for_scope(lua, "buf") {
                            tbl.set("name", auto)?;
                        }
                    }
                }
                let opts = match opts {
                    Some(t) => Some(t),
                    None => {
                        // No table at all: still want to auto-name if scoped.
                        if let Some(auto) = crate::lua::auto_name_for_scope(lua, "buf") {
                            let t = lua.create_table()?;
                            t.set("name", auto)?;
                            Some(LuaBufNewOpts(t))
                        } else {
                            None
                        }
                    }
                };
                let id = create_or_open(&s, opts.as_ref().map(|opts| &opts.0))?;
                Ok(LuaBuf { id })
            },
        )?;
    }

    Ok(())
}

/// Implementation of `smelt.buf.new(opts?)`. If `opts.name` resolves to an
/// existing buffer, returns its id with mutable opts re-applied (the
/// hot-reload survival path); otherwise allocates a fresh one.
pub(super) fn create_or_open(
    shared: &Arc<LuaShared>,
    opts: Option<&mlua::Table>,
) -> LuaResult<crate::smelt_edit::BufId> {
    let format = match opts {
        Some(t) => match t.get::<Option<String>>("mode")? {
            Some(mode) => Some(
                crate::format::BufFormat::from_lua_spec(&mode, t)
                    .map_err(|e| LuaError::RuntimeError(format!("buf: {e}")))?,
            ),
            None => None,
        },
        None => None,
    };
    let readonly: bool = opts
        .and_then(|t| t.get::<bool>("readonly").ok())
        .unwrap_or(false);
    let editable: bool = opts
        .and_then(|t| t.get::<bool>("editable").ok())
        .unwrap_or(false);
    let undo_limit: Option<usize> = opts
        .and_then(|t| t.get::<Option<u64>>("undo").ok())
        .flatten()
        .map(|n| n as usize);
    let name: Option<String> = opts
        .and_then(|t| t.get::<Option<String>>("name").ok())
        .flatten();

    // Optional access lets bootstrap chunks call `smelt.buf.new` before the
    // frontend host is in scope. The buffer is created on the launch reload.
    let (buffer_id, clash) = crate::lua::try_with_ui_host(|host| {
        host.with_ui(|ui| {
            // Named buffer that already exists - refresh mutable opts.
            if let Some(ref name) = name {
                if let Some((buffer_id, buffer)) = ui.lookup_named_buf_mut(name) {
                    buffer.readonly = readonly;
                    if let Some(format) = format {
                        buffer.set_parser(format.into_parser());
                    }
                    return (buffer_id, None);
                }
            }
            let id = shared
                .next_buf_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match ui.buf_create_with_id(
                crate::smelt_edit::BufId(id),
                crate::smelt_edit::BufCreateOpts::default(),
            ) {
                Ok(buffer_id) => {
                    if let Some(buffer) = ui.buf_mut(buffer_id) {
                        buffer.readonly = readonly;
                        if let Some(format) = format {
                            buffer.set_parser(format.into_parser());
                        }
                        if editable {
                            let limit = undo_limit.or(Some(100));
                            buffer.history = crate::smelt_edit::UndoHistory::new(limit);
                        }
                    }
                    if let Some(ref name) = name {
                        ui.name_buf(name.clone(), buffer_id);
                    }
                    (buffer_id, None)
                }
                Err(clash) => (crate::smelt_edit::BufId(0), Some(clash)),
            }
        })
    })
    .unwrap_or((crate::smelt_edit::BufId(0), None));
    if let Some(clash) = clash {
        crate::lua::with_runtime_host(|host| {
            host.notify_error(format!("buf: id {} already in use", clash.0));
        });
    }

    Ok(buffer_id)
}

/// `buf:styled(spans)` - set a styled line list. Same semantics as the
/// old `set_styled_lines`; lifted out so `Buf` methods stay tidy.
fn set_styled_lines(id: crate::smelt_edit::BufId, lines: mlua::Table) -> LuaResult<()> {
    use crate::content::to_buffer::render_into_buffer;
    use smelt_core::content::highlight::InlineSyntax;
    use smelt_core::style::Style;
    use smelt_core::theme::intern;

    struct SpanStyle {
        hl: Option<String>,
        dim: bool,
        bold: bool,
        italic: bool,
        reverse: bool,
        fg: Option<LuaColor>,
        bg: Option<LuaColor>,
    }

    struct Span {
        text: String,
        style: SpanStyle,
        syntax: Option<String>,
    }

    fn decode_style(tbl: Option<mlua::Table>) -> LuaResult<SpanStyle> {
        let Some(tbl) = tbl else {
            return Ok(SpanStyle {
                hl: None,
                dim: false,
                bold: false,
                italic: false,
                reverse: false,
                fg: None,
                bg: None,
            });
        };
        Ok(SpanStyle {
            hl: tbl.get::<Option<String>>("hl")?,
            dim: tbl.get::<Option<bool>>("dim")?.unwrap_or(false),
            bold: tbl.get::<Option<bool>>("bold")?.unwrap_or(false),
            italic: tbl.get::<Option<bool>>("italic")?.unwrap_or(false),
            reverse: tbl.get::<Option<bool>>("reverse")?.unwrap_or(false),
            fg: tbl.get::<Option<LuaColor>>("fg")?,
            bg: tbl.get::<Option<LuaColor>>("bg")?,
        })
    }

    let mut decoded: Vec<Vec<Span>> = Vec::new();
    for value in lines.sequence_values::<mlua::Value>() {
        let value = value?;
        let line_tbl = match value {
            mlua::Value::Table(t) => t,
            mlua::Value::Nil => {
                decoded.push(Vec::new());
                continue;
            }
            other => {
                return Err(mlua::Error::external(format!(
                    "buf:styled: expected line to be a table of spans, got {}",
                    other.type_name()
                )));
            }
        };
        let mut spans: Vec<Span> = Vec::new();
        for span_val in line_tbl.sequence_values::<mlua::Table>() {
            let span_tbl = span_val?;
            let style = decode_style(span_tbl.get::<Option<mlua::Table>>("style")?)?;
            spans.push(Span {
                text: span_tbl.get::<Option<String>>("text")?.unwrap_or_default(),
                style,
                syntax: span_tbl.get::<Option<String>>("syntax")?,
            });
        }
        decoded.push(spans);
    }

    crate::lua::with_ui_host(|host| {
        host.with_ui(|ui| {
            let theme_snap = ui.theme().clone();
            let width = crate::content::term_width() as u16;
            let Some(buf) = ui.buf_mut(id) else {
                return;
            };
            buf.set_all_lines(Vec::new());
            render_into_buffer(buf, width, &theme_snap, |sink| {
                for spans in &decoded {
                    for span in spans {
                        let group = span.style.hl.as_deref().map(intern);
                        let mut style = Style::new();
                        style.dim = span.style.dim;
                        style.bold = span.style.bold;
                        style.italic = span.style.italic;
                        style.reverse = span.style.reverse;
                        if let Some(c) = &span.style.fg {
                            style.fg = match c {
                                LuaColor::Group(name) => sink.theme().get(name).fg,
                                LuaColor::Direct(color) => Some(*color),
                            };
                        }
                        if let Some(c) = &span.style.bg {
                            style.bg = match c {
                                LuaColor::Group(name) => sink.theme().get(name).bg,
                                LuaColor::Direct(color) => Some(*color),
                            };
                        }
                        sink.push(group, style);
                        if let Some(lang) = &span.syntax {
                            let mut hi = InlineSyntax::new(lang, sink.theme());
                            hi.print_line(sink, &span.text);
                        } else {
                            sink.print(&span.text);
                        }
                        sink.pop_style();
                    }
                    sink.newline();
                }
            });
        })
    });
    Ok(())
}

/// `buf:mark(ns, row, col, opts?) → extmark id`. Row is 1-based;
/// `col`/`end_col` are byte offsets into the line.
fn set_extmark(
    id: crate::smelt_edit::BufId,
    ns: u32,
    row: u64,
    col: u64,
    opts: Option<LuaMarkOpts>,
) -> u64 {
    use smelt_core::buffer::{ExtmarkId, ExtmarkOpts, NsId};

    let Some(row0) = row.checked_sub(1) else {
        return 0;
    };
    let row0 = row0 as usize;
    let byte_col = col as usize;

    let opts = opts.unwrap_or_default();

    let end_row: Option<usize> = opts
        .end_row
        .and_then(|n| n.checked_sub(1).map(|x| x as usize));
    let end_byte_col: Option<usize> = opts.end_col.map(|n| n as usize);
    let mark_id: Option<ExtmarkId> = opts.id.map(ExtmarkId);

    // Underlying extmarks store display-cell columns. Convert byte
    // offsets → cells using the current line content. Snaps off-boundary
    // bytes and clamps overshoot to the end of the line.
    let (col0, end_col) = crate::lua::with_ui_host(|host| {
        host.with_ui(|ui| {
            let line = ui
                .buf(id)
                .and_then(|b| b.get_line(row0).map(String::from))
                .unwrap_or_default();
            let s = smelt_buffer::text::byte_to_cell(&line, byte_col);
            let e = end_byte_col.map(|b| smelt_buffer::text::byte_to_cell(&line, b));
            (s, e)
        })
    });

    let mut payload_opts = if let Some(text) = opts.virt_text.clone() {
        let mut o = ExtmarkOpts::virt_text(text, build_virt_text_hl(&opts));
        if let Some(pos) = opts.virt_text_pos {
            o = o.with_virt_pos(pos.into());
        }
        o
    } else {
        let style = build_highlight_style(&opts);
        let meta = smelt_core::buffer::SpanMeta {
            selectable: opts.selectable.unwrap_or(true),
            copy_as: opts.yank_as.clone(),
            action: None,
        };
        let mut o = ExtmarkOpts::highlight(end_col.unwrap_or(col0), style, meta);
        if opts.hl_eol == Some(true) {
            o = o.with_hl_eol(true);
        }
        if opts.on_cursor_row == Some(true) {
            o = o.with_on_cursor_row(true);
        }
        o
    };

    payload_opts.end_row = end_row;
    if !matches!(
        payload_opts.payload,
        smelt_core::buffer::ExtmarkPayload::Highlight { .. }
    ) {
        payload_opts.end_col = end_col;
    }
    payload_opts.priority = opts.priority;
    payload_opts.right_gravity = opts.right_gravity.unwrap_or(true);
    payload_opts.end_right_gravity = opts.end_right_gravity.unwrap_or(false);
    payload_opts.id = mark_id;

    crate::lua::with_ui_host(|host| {
        host.with_ui(|ui| {
            ui.buf_mut(id)
                .map(|buf| buf.set_extmark(NsId(ns), row0, col0, payload_opts))
        })
    })
    .map(|eid: ExtmarkId| eid.0 as u64)
    .unwrap_or(0)
}

fn build_virt_text_hl(opts: &LuaMarkOpts) -> Option<String> {
    if opts.fg.is_none()
        && opts.bg.is_none()
        && opts.bold.is_none()
        && opts.dim.is_none()
        && opts.italic.is_none()
        && opts.reverse.is_none()
    {
        return opts.virt_text_hl.clone();
    }

    let mut style = match opts.virt_text_hl.as_deref() {
        Some(name) => crate::lua::with_runtime_host(|host| host.theme_group(name)),
        None => smelt_core::style::Style::default(),
    };
    if let Some(c) = &opts.fg {
        style.fg = c.resolve_fg();
    }
    if let Some(c) = &opts.bg {
        style.bg = c.resolve_bg();
    }
    if let Some(b) = opts.bold {
        style.bold = b;
    }
    if let Some(b) = opts.dim {
        style.dim = b;
    }
    if let Some(b) = opts.italic {
        style.italic = b;
    }
    if let Some(b) = opts.reverse {
        style.reverse = b;
    }

    let group = smelt_core::theme::intern_anonymous_style(style);
    smelt_core::theme::name_of(group)
}

fn build_highlight_style(opts: &LuaMarkOpts) -> crate::smelt_edit::SpanStyle {
    use smelt_core::style::Style;

    let mut style = match opts.hl_group.as_deref() {
        Some(name) => crate::lua::with_runtime_host(|host| host.theme_group(name)),
        None => Style::default(),
    };
    if let Some(c) = &opts.fg {
        style.fg = c.resolve_fg();
    }
    if let Some(c) = &opts.bg {
        style.bg = c.resolve_bg();
    }
    if let Some(b) = opts.bold {
        style.bold = b;
    }
    if let Some(b) = opts.dim {
        style.dim = b;
    }
    if let Some(b) = opts.italic {
        style.italic = b;
    }
    if let Some(b) = opts.reverse {
        style.reverse = b;
    }
    style
}
