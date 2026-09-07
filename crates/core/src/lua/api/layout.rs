//! `smelt.layout` - declarative, width-independent content layout returned from Lua display callbacks.

use crate::content::block_layout::{
    BlockLayout, CapKeep, CapMarker, CapSpec, CodeSpec, Constraint, ContentDiffSpec,
    ContentRenderSpec, ContentSpec, DiffSpec, FileViewSpec, GutterSpec, HboxItem, LineSpec,
    LuaLeaf, MarkdownSpec, PanelSpec, RefreshSpec, RowPrefixSpec, RunsSpec, SeparatorSpec,
    StyleSpec, TextSpec,
};
use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use mlua::prelude::*;

pub struct LuaBlockLayout(pub BlockLayout);

impl mlua::UserData for LuaBlockLayout {}

fn layout_from_value(value: mlua::Value, name: &str) -> LuaResult<BlockLayout> {
    match value {
        mlua::Value::UserData(ud) => Ok(ud.borrow::<LuaBlockLayout>()?.0.clone()),
        other => Err(mlua::Error::external(format!(
            "smelt.layout.{name}: expected layout userdata, got {}",
            other.type_name()
        ))),
    }
}

fn collect_vbox_items(items: mlua::Table) -> LuaResult<Vec<BlockLayout>> {
    let mut out = Vec::new();
    for entry in items.sequence_values::<mlua::AnyUserData>() {
        let ud = entry?;
        let layout = ud.borrow::<LuaBlockLayout>()?;
        out.push(layout.0.clone());
    }
    Ok(out)
}

fn collect_hbox_items(items: mlua::Table) -> LuaResult<Vec<HboxItem>> {
    let mut out = Vec::new();
    for entry in items.sequence_values::<mlua::Value>() {
        let value = entry?;
        let item = match value {
            mlua::Value::UserData(ud) => {
                let layout = ud.borrow::<LuaBlockLayout>()?;
                HboxItem {
                    constraint: Constraint::Fill(1),
                    layout: layout.0.clone(),
                    copy_owner: false,
                }
            }
            mlua::Value::Table(t) => {
                let layout_ud: mlua::AnyUserData = t.get(1)?;
                let layout = layout_ud.borrow::<LuaBlockLayout>()?.0.clone();
                let cols: Option<u16> = t.get("cols").ok();
                let fit: bool = t.get("fit").unwrap_or(false);
                let weight: Option<u16> = t.get("weight").ok();
                let copy_owner = t.get::<Option<bool>>("copy_owner")?.unwrap_or(false);
                let constraint = if let Some(n) = cols {
                    Constraint::Length(n)
                } else if fit {
                    Constraint::Fit
                } else {
                    Constraint::Fill(weight.unwrap_or(1))
                };
                HboxItem {
                    constraint,
                    layout,
                    copy_owner,
                }
            }
            other => {
                return Err(mlua::Error::external(format!(
                    "smelt.layout.hbox: expected layout userdata or {{ layout, weight=N | cols=N | fit=true, copy_owner=bool }} table, got {}",
                    other.type_name()
                )));
            }
        };
        out.push(item);
    }
    let owner_count = out.iter().filter(|item| item.copy_owner).count();
    if owner_count > 1 {
        return Err(mlua::Error::external(
            "smelt.layout.hbox: only one item may set copy_owner=true",
        ));
    }
    if owner_count == 0 {
        if let Some(first) = out.first_mut() {
            first.copy_owner = true;
        }
    }
    Ok(out)
}

fn style_spec_from_opts(opts: Option<&mlua::Table>) -> LuaResult<StyleSpec> {
    let Some(opts) = opts else {
        return Ok(StyleSpec::default());
    };
    Ok(StyleSpec {
        hl_group: opts
            .get::<Option<String>>("hl_group")?
            .or(opts.get::<Option<String>>("hl")?),
        fg: opts.get::<Option<String>>("fg")?,
        bg: opts.get::<Option<String>>("bg")?,
        dim: opts.get::<Option<bool>>("dim")?.unwrap_or(false),
        bold: opts.get::<Option<bool>>("bold")?.unwrap_or(false),
        italic: opts.get::<Option<bool>>("italic")?.unwrap_or(false),
    })
}

fn cap_edge_marker(value: Option<&str>) -> LuaResult<Option<CapMarker>> {
    match value {
        None => Ok(None),
        Some("above") => Ok(Some(CapMarker::Above)),
        Some("below") => Ok(Some(CapMarker::Below)),
        Some(other) => Err(mlua::Error::external(format!(
            "smelt.layout.cap: invalid marker `{other}` for edge cap (expected `above`, `below`, or nil)"
        ))),
    }
}

fn cap_middle_marker(value: Option<&str>) -> LuaResult<bool> {
    match value {
        None => Ok(false),
        Some("middle") => Ok(true),
        Some(other) => Err(mlua::Error::external(format!(
            "smelt.layout.cap: invalid marker `{other}` for head_tail cap (expected `middle` or nil)"
        ))),
    }
}

pub(super) fn register(
    lua: &Lua,
    smelt: &mlua::Table,
    shared: &std::sync::Arc<crate::lua::LuaShared>,
) -> LuaResult<()> {
    let m = LuaMod::advanced(
        lua,
        smelt,
        "layout",
        "Declarative, width-independent content layout primitives for transcript/tool display.",
        Tier::Host,
    )?;
    m.private_fn(
        "__is_node",
        &["value"],
        |_, value: mlua::Value| -> LuaResult<bool> {
            Ok(matches!(value, mlua::Value::UserData(ud) if ud.is::<LuaBlockLayout>()))
        },
    )?;
    m.fn_(
        "text",
        "Plain text layout leaf. `opts.hl_group` / `opts.hl` may name a theme group; without it, text renders dimmed. `opts.ansi = true` enables ANSI parsing. Wrapping is computed by the transcript at the current width.",
        &["content", "opts"],
        |_, (content, opts): (String, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            let hl_group = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("hl_group").ok().flatten())
                .or_else(|| opts.as_ref().and_then(|t| t.get::<Option<String>>("hl").ok().flatten()));
            let ansi = opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("ansi").ok().flatten())
                .unwrap_or(false);
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::Text(TextSpec {
                content,
                hl_group,
                ansi,
            }))))
        },
    )?;
    m.fn_(
        "content",
        "Opaque transcript content leaf. `content_id` comes from renderer metadata and is resolved by Rust without exposing the complete payload to Lua. `opts.format` is `text` (default), `markdown`, `code`, or `file`; text accepts `hl_group` / `hl` and `ansi`, Markdown accepts `dim`, `italic`, and `inline`, code accepts `lang`, and file accepts `path` plus an optional `lang` override.",
        &["content_id", "opts"],
        |_, (content_id, opts): (u64, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            let format = opts
                .as_ref()
                .and_then(|table| table.get::<Option<String>>("format").ok().flatten())
                .unwrap_or_else(|| "text".to_string());
            let render = match format.as_str() {
                "text" => {
                    let hl_group = opts
                        .as_ref()
                        .and_then(|table| table.get::<Option<String>>("hl_group").ok().flatten())
                        .or_else(|| {
                            opts.as_ref()
                                .and_then(|table| table.get::<Option<String>>("hl").ok().flatten())
                        });
                    let ansi = opts
                        .as_ref()
                        .and_then(|table| table.get::<Option<bool>>("ansi").ok().flatten())
                        .unwrap_or(false);
                    ContentRenderSpec::Text { hl_group, ansi }
                }
                "markdown" => ContentRenderSpec::Markdown {
                    dim: opts
                        .as_ref()
                        .and_then(|table| table.get::<Option<bool>>("dim").ok().flatten())
                        .unwrap_or(false),
                    italic: opts
                        .as_ref()
                        .and_then(|table| table.get::<Option<bool>>("italic").ok().flatten())
                        .unwrap_or(false),
                    inline: opts
                        .as_ref()
                        .and_then(|table| table.get::<Option<bool>>("inline").ok().flatten())
                        .unwrap_or(false),
                },
                "code" => ContentRenderSpec::Code {
                    lang: opts
                        .as_ref()
                        .and_then(|table| table.get::<Option<String>>("lang").ok().flatten())
                        .unwrap_or_default(),
                    cache: Default::default(),
                },
                "file" => ContentRenderSpec::File {
                    path: opts
                        .as_ref()
                        .and_then(|table| table.get::<Option<String>>("path").ok().flatten())
                        .unwrap_or_default(),
                    lang: opts
                        .as_ref()
                        .and_then(|table| table.get::<Option<String>>("lang").ok().flatten()),
                    cache: Default::default(),
                },
                _ => {
                    return Err(mlua::Error::external(
                        "smelt.layout.content: opts.format must be text, markdown, code, or file",
                    ));
                }
            };
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::Content(
                ContentSpec {
                    id: crate::transcript_content::ContentId::new(content_id),
                    render,
                },
            ))))
        },
    )?;
    m.fn_(
        "content_diff",
        "Retained diff leaf. `old_content_id` and `new_content_id` come from transcript renderer metadata and are resolved by Rust without exposing source payloads to Lua. `opts.anchor_content_id` optionally identifies the edited source fragment, `opts.path` selects syntax, `opts.lang` overrides path-based syntax, and `opts.full_file` marks complete before/after files.",
        &["old_content_id", "new_content_id", "opts"],
        |_, (old_content_id, new_content_id, opts): (u64, u64, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            let anchor_id = opts
                .as_ref()
                .and_then(|table| table.get::<Option<u64>>("anchor_content_id").ok().flatten())
                .map(crate::transcript_content::ContentId::new);
            let path = opts
                .as_ref()
                .and_then(|table| table.get::<Option<String>>("path").ok().flatten())
                .unwrap_or_default();
            let lang = opts
                .as_ref()
                .and_then(|table| table.get::<Option<String>>("lang").ok().flatten());
            let full_file = opts
                .as_ref()
                .and_then(|table| table.get::<Option<bool>>("full_file").ok().flatten())
                .unwrap_or(false);
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::ContentDiff(
                ContentDiffSpec {
                    old_id: crate::transcript_content::ContentId::new(old_content_id),
                    new_id: crate::transcript_content::ContentId::new(new_content_id),
                    anchor_id,
                    path,
                    lang,
                    full_file,
                },
            ))))
        },
    )?;
    m.fn_(
        "group_children",
        "Opaque retained child-layout placeholder for transcript group renderers. Rust resolves each child independently, so updating one child does not serialize or recompile its siblings.",
        &[],
        |_, ()| -> LuaResult<LuaBlockLayout> {
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::GroupChildren)))
        },
    )?;
    m.fn_(
        "runs",
        "Styled inline text layout leaf. `lines` is a string or styled-lines table (`{ { { text=..., syntax?, hl?, fg?, bg?, dim?, bold?, italic?, selectable?, title_suffix? }, ... }, ... }`). `opts.hl_group` / `opts.hl` supplies a default theme group for spans without `hl`; `opts.continuation_indent` indents soft-wrapped continuation rows by display columns.",
        &["lines", "opts"],
        |_, (lines, opts): (mlua::Value, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            let hl_group = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("hl_group").ok().flatten())
                .or_else(|| opts.as_ref().and_then(|t| t.get::<Option<String>>("hl").ok().flatten()));
            let continuation_indent = opts
                .as_ref()
                .and_then(|t| t.get::<Option<u16>>("continuation_indent").ok().flatten())
                .unwrap_or(0);
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::Runs(RunsSpec {
                lines: crate::lua::styled_lines_from_lua(lines, "smelt.layout.runs")?,
                hl_group,
                continuation_indent,
                syntax_highlights: Default::default(),
            }))))
        },
    )?;
    m.fn_(
        "line",
        "Single styled line layout leaf. `spans` is a string or a one-dimensional span table; unlike `runs`, this does not wrap. A nonempty span table whose spans are all `selectable = false` is decorative: copying skips the row and its newline unless layout composition adds selectable cells or source text.",
        &["spans", "opts"],
        |_, (spans, opts): (mlua::Value, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            let hl_group = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("hl_group").ok().flatten())
                .or_else(|| opts.as_ref().and_then(|t| t.get::<Option<String>>("hl").ok().flatten()));
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::Line(LineSpec {
                spans: crate::lua::styled_line_from_lua(spans, "smelt.layout.line")?,
                hl_group,
                syntax_highlights: Default::default(),
            }))))
        },
    )?;
    m.fn_(
        "markdown",
        "Markdown layout leaf. `opts.dim` dims all spans; `opts.italic` italicizes inline-mode spans; `opts.inline = true` preserves line-by-line inline markdown without block parsing.",
        &["content", "opts"],
        |_, (content, opts): (String, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            let dim = opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("dim").ok().flatten())
                .unwrap_or(false);
            let italic = opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("italic").ok().flatten())
                .unwrap_or(false);
            let inline = opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("inline").ok().flatten())
                .unwrap_or(false);
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::Markdown(MarkdownSpec {
                content,
                dim,
                italic,
                inline,
            }))))
        },
    )?;
    m.fn_(
        "code",
        "Syntax-highlighted code layout leaf. `opts.lang` supplies the language name.",
        &["content", "opts"],
        |_, (content, opts): (String, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            let lang = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("lang").ok().flatten())
                .unwrap_or_default();
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::Code(CodeSpec {
                content,
                lang,
            }))))
        },
    )?;
    m.fn_(
        "refresh",
        "Return `child` unchanged while requesting that its containing top-level transcript node be rendered again after `opts.after_ms`. The positive delay is declarative cache metadata and does not affect measurement, rendering, or selection.",
        &["child", "opts"],
        |_, (child, opts): (mlua::Value, mlua::Table)| -> LuaResult<LuaBlockLayout> {
            let child = layout_from_value(child, "refresh")?;
            let after_ms = opts.get::<Option<u64>>("after_ms")?.ok_or_else(|| {
                mlua::Error::external("smelt.layout.refresh: opts.after_ms is required")
            })?;
            if after_ms == 0 {
                return Err(mlua::Error::external(
                    "smelt.layout.refresh: opts.after_ms must be positive",
                ));
            }
            Ok(LuaBlockLayout(BlockLayout::Refresh {
                child: Box::new(child),
                spec: RefreshSpec { after_ms },
            }))
        },
    )?;
    m.fn_(
        "separator",
        "Full-width horizontal separator. `opts.label` is centered in the row and accepts the same styled span shape as `smelt.layout.line`; generated line fill is chrome and is not searchable/selectable unless `opts.selectable` is true. `opts.dim` defaults to true.",
        &["opts"],
        |_, opts: Option<mlua::Table>| -> LuaResult<LuaBlockLayout> {
            let label = match opts.as_ref() {
                Some(opts) => opts.get::<mlua::Value>("label")?,
                None => mlua::Value::Nil,
            };
            let dim = opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("dim").ok().flatten())
                .unwrap_or(true);
            let selectable = opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("selectable").ok().flatten())
                .unwrap_or(false);
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::Separator(
                SeparatorSpec {
                    label: crate::lua::styled_line_from_lua(label, "smelt.layout.separator")?,
                    dim,
                    selectable,
                },
            ))))
        },
    )?;
    m.fn_(
        "panel",
        "Render `child` inside a full-width background panel. `opts.hl_group` / `opts.hl` names the panel highlight group; `opts.padding` defaults to 1 cell/row.",
        &["child", "opts"],
        |_, (child, opts): (mlua::Value, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            let child = layout_from_value(child, "panel")?;
            let hl_group = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("hl_group").ok().flatten())
                .or_else(|| opts.as_ref().and_then(|t| t.get::<Option<String>>("hl").ok().flatten()))
                .unwrap_or_else(|| "Normal".to_string());
            let padding = opts
                .as_ref()
                .and_then(|t| t.get::<Option<u16>>("padding").ok().flatten())
                .unwrap_or(1);
            Ok(LuaBlockLayout(BlockLayout::Panel {
                child: Box::new(child),
                spec: PanelSpec { hl_group, padding },
            }))
        },
    )?;
    m.fn_(
        "style",
        "Apply inherited style to a child layout. `opts.hl_group` / `opts.hl` names a theme group; `opts.fg` / `opts.bg` name theme colors; `opts.dim`, `opts.bold`, and `opts.italic` set text attributes. Child spans may override inherited fields.",
        &["child", "opts"],
        |_, (child, opts): (mlua::Value, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            let child = layout_from_value(child, "style")?;
            Ok(LuaBlockLayout(BlockLayout::Style {
                child: Box::new(child),
                spec: style_spec_from_opts(opts.as_ref())?,
            }))
        },
    )?;
    let diff_context = std::sync::Arc::clone(shared);
    m.fn_(
        "diff",
        "Inline-diff render directive. The worker renders the diff directly into the block buffer. `opts.old`, `opts.new` are the before/after strings; `opts.path` picks syntax via extension; `opts.anchor` (defaults to `opts.old`) is the diff-view anchor; `opts.lang` overrides path-based syntax; `opts.full_file` treats `opts.old` as the complete pre-edit file for stable previews after writes.",
        &["opts"],
        move |_, opts: mlua::Table| -> LuaResult<LuaBlockLayout> {
            let old: String = opts.get::<Option<String>>("old")?.unwrap_or_default();
            let new: String = opts.get::<Option<String>>("new")?.unwrap_or_default();
            let path: String = opts.get::<Option<String>>("path")?.unwrap_or_default();
            let anchor: String = opts
                .get::<Option<String>>("anchor")?
                .unwrap_or_else(|| old.clone());
            let lang: Option<String> = opts.get::<Option<String>>("lang")?;
            let full_file = opts.get::<Option<bool>>("full_file")?.unwrap_or(false);
            let base = if full_file {
                old.clone()
            } else {
                std::fs::read_to_string(diff_context.resolve_project_path(&path))
                    .unwrap_or_default()
            };
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::Diff(DiffSpec {
                old,
                new,
                path,
                anchor,
                lang,
                full_file,
                base,
            }))))
        },
    )?;
    m.fn_(
        "file_view",
        "Syntax-highlighted file-view render directive. Uses a single line-number column and no diff bg. `opts.content` is the source text; `opts.path` picks syntax via extension; `opts.lang` overrides path-based syntax.",
        &["opts"],
        |_, opts: mlua::Table| -> LuaResult<LuaBlockLayout> {
            let content: String = opts.get::<Option<String>>("content")?.unwrap_or_default();
            let path: String = opts.get::<Option<String>>("path")?.unwrap_or_default();
            let lang: Option<String> = opts.get::<Option<String>>("lang")?;
            Ok(LuaBlockLayout(BlockLayout::Leaf(LuaLeaf::FileView(
                FileViewSpec {
                    content,
                    path,
                    lang,
                },
            ))))
        },
    )?;
    m.fn_(
        "empty",
        "Explicit zero-row layout node. Use this instead of returning nil when a renderer intentionally hides content.",
        &[],
        |_, ()| -> LuaResult<LuaBlockLayout> { Ok(LuaBlockLayout(BlockLayout::Empty)) },
    )?;
    m.fn_(
        "gutter",
        "Render `child` with an explicit non-selectable gutter prefix on each emitted row. `opts.text` defaults to two spaces. The prefix consumes display width before wrapping/measuring the child; `opts.styled = true` lets row-level styles include the prefix.",
        &["child", "opts"],
        |_, (child, opts): (mlua::Value, Option<mlua::Table>)| -> LuaResult<LuaBlockLayout> {
            let child = layout_from_value(child, "gutter")?;
            let text = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("text").ok().flatten())
                .unwrap_or_else(|| "  ".to_string());
            let styled = opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("styled").ok().flatten())
                .unwrap_or(false);
            Ok(LuaBlockLayout(BlockLayout::Gutter {
                child: Box::new(child),
                spec: GutterSpec { text, styled },
            }))
        },
    )?;
    m.fn_(
        "row_prefix",
        "Apply row chrome to `child` after the child has produced rows. `opts.first` is a styled line or string for row 1; `opts.rest` is used for every later row and defaults to `opts.first`. Prefix spans keep their own `selectable` flags: set `selectable = false` for pure chrome, leave it true for copyable labels. The widest prefix consumes display width before wrapping/measuring the child, so prefixed rows stay within the layout width. Put this outside `layout.cap` when cap markers should inherit the same row chrome.",
        &["child", "opts"],
        |_, (child, opts): (mlua::Value, mlua::Table)| -> LuaResult<LuaBlockLayout> {
            let child = layout_from_value(child, "row_prefix")?;
            let first_value: mlua::Value = opts.get("first")?;
            let rest_value: mlua::Value = opts.get("rest")?;
            let first = crate::lua::styled_line_from_lua(
                first_value,
                "smelt.layout.row_prefix",
            )?;
            let rest = if matches!(rest_value, mlua::Value::Nil) {
                first.clone()
            } else {
                crate::lua::styled_line_from_lua(
                    rest_value,
                    "smelt.layout.row_prefix",
                )?
            };
            Ok(LuaBlockLayout(BlockLayout::RowPrefix {
                child: Box::new(child),
                spec: RowPrefixSpec { first, rest },
            }))
        },
    )?;
    m.fn_(
        "cap",
        "Cap a child by rendered rows. `opts.rows` is numeric; `opts.keep` is `head`, `tail`, or `head_tail`; edge caps accept `opts.marker = \"above\" | \"below\"`; `head_tail` uses `opts.head_rows` and accepts `opts.marker = \"middle\"`. `opts.total_rows` may provide the full source row count for clearer tail markers.",
        &["child", "opts"],
        |_, (child, opts): (mlua::Value, mlua::Table)| -> LuaResult<LuaBlockLayout> {
            let child = layout_from_value(child, "cap")?;
            let rows = opts.get::<Option<u16>>("rows")?.unwrap_or(20);
            let total_rows = opts.get::<Option<u64>>("total_rows")?;
            let keep_label = opts
                .get::<Option<String>>("keep")?
                .unwrap_or_else(|| "head".to_string());
            let marker_label = opts.get::<Option<String>>("marker")?;
            let keep = match keep_label.as_str() {
                "head" => CapKeep::Head {
                    marker: cap_edge_marker(marker_label.as_deref())?,
                },
                "tail" => CapKeep::Tail {
                    marker: cap_edge_marker(marker_label.as_deref())?,
                },
                "head_tail" => CapKeep::HeadTail {
                    head: opts.get::<Option<u16>>("head_rows")?.unwrap_or(1),
                    marker: cap_middle_marker(marker_label.as_deref())?,
                },
                other => {
                    return Err(mlua::Error::external(format!(
                        "smelt.layout.cap: invalid keep `{other}` (expected `head`, `tail`, or `head_tail`)"
                    )))
                }
            };
            Ok(LuaBlockLayout(BlockLayout::Cap {
                child: Box::new(child),
                spec: CapSpec {
                    rows,
                    keep,
                    total_rows,
                },
            }))
        },
    )?;
    m.fn_(
        "vbox",
        "Stack `items` vertically into a single block layout. Each item must be a layout userdata produced by a `smelt.layout` primitive.",
        &["items"],
        |_, items: mlua::Table| -> LuaResult<LuaBlockLayout> {
            Ok(LuaBlockLayout(BlockLayout::Vbox(collect_vbox_items(
                items,
            )?)))
        },
    )?;
    m.fn_(
        "hbox",
        "Lay `items` out horizontally. Each entry is either a layout userdata (defaults to fill weight 1) or `{ layout, cols=N }` / `{ layout, weight=N }` / `{ layout, fit=true }` for a fixed, weighted, or renderer-defined intrinsic-width slot. `fit=true` uses unwrapped content width when available, capped by the parent; fixed and fit slots are allocated before fill slots. The first item owns row-level copy metadata by default; set `copy_owner=true` on exactly one item when another column contains the primary copyable content.",
        &["items"],
        |_, items: mlua::Table| -> LuaResult<LuaBlockLayout> {
            Ok(LuaBlockLayout(BlockLayout::Hbox(collect_hbox_items(
                items,
            )?)))
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hbox_entry(lua: &Lua, copy_owner: Option<bool>) -> mlua::Table {
        let entry = lua.create_table().expect("hbox entry");
        entry
            .set(
                1,
                lua.create_userdata(LuaBlockLayout(BlockLayout::Empty))
                    .expect("layout userdata"),
            )
            .expect("entry layout");
        if let Some(copy_owner) = copy_owner {
            entry
                .set("copy_owner", copy_owner)
                .expect("copy_owner flag");
        }
        entry
    }

    #[test]
    fn hbox_defaults_copy_owner_to_first_item() {
        let lua = Lua::new();
        let items = lua.create_table().expect("items");
        items.set(1, hbox_entry(&lua, None)).expect("first item");
        items.set(2, hbox_entry(&lua, None)).expect("second item");

        let items = collect_hbox_items(items).expect("hbox items");

        assert!(items[0].copy_owner);
        assert!(!items[1].copy_owner);
    }

    #[test]
    fn hbox_accepts_one_explicit_copy_owner() {
        let lua = Lua::new();
        let items = lua.create_table().expect("items");
        items
            .set(1, hbox_entry(&lua, Some(false)))
            .expect("first item");
        items
            .set(2, hbox_entry(&lua, Some(true)))
            .expect("second item");

        let items = collect_hbox_items(items).expect("hbox items");

        assert!(!items[0].copy_owner);
        assert!(items[1].copy_owner);
    }

    #[test]
    fn hbox_rejects_multiple_copy_owners() {
        let lua = Lua::new();
        let items = lua.create_table().expect("items");
        items
            .set(1, hbox_entry(&lua, Some(true)))
            .expect("first item");
        items
            .set(2, hbox_entry(&lua, Some(true)))
            .expect("second item");

        let err = collect_hbox_items(items).expect_err("multiple owners must fail");

        assert!(err.to_string().contains("only one item"));
    }
}
