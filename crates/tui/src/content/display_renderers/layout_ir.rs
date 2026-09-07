use std::borrow::Cow;
use std::cell::RefCell;
use std::sync::Arc;

use super::markdown::{
    markdown_range_has_visible_text_with_options, measure_markdown_inner_with_options,
    measure_retained_markdown_inner_edge_with_options,
    measure_retained_markdown_inner_with_options, render_markdown_inner_range_with_options,
    render_retained_markdown_inner_edge_with_options,
    render_retained_markdown_inner_range_with_options,
};
use super::temp_rows::{
    apply_temp_decoration, emit_buffer_row_clipped, emit_buffer_row_clipped_with_scratch,
};
use crate::content::source_view::{render_source_view, SourceView, SourceViewTarget};
use smelt_core::buffer::{BufCreateOpts, BufId, Buffer, Span, SpanMeta};
use smelt_core::content::ansi::{emit_ansi_row, wrap_ansi};
use smelt_core::content::block_layout::{
    solve_hbox_widths_with_fit, BlockLayout, CodeSpec, ContentRenderSpec, GutterSpec, IrLeaf,
    LayoutIr, LineSpec, MarkdownSpec, PanelSpec, RetainedContentSpec, RetainedInlineSyntax,
    RowPrefixSpec, RunsSpec, SeparatorSpec, StyleSpec, TextSpec,
};
use smelt_core::content::builder::{
    display_width, wrapped_segments, LineBuilder, Outcome, WrappedSegmentKind,
};
use smelt_core::content::code_block::{measure_code_block, parse_code_block};
use smelt_core::content::highlight::{
    emit_inline_spans, parse_inline_spans_with_options, render_code_block, wrap_inline_spans,
    InlineOptions, InlineSpan, InlineSyntax, InlineSyntaxSpan,
};
use smelt_core::content::inline_line::{BreakPolicy, InlineLine, InlineRun, WrappedRun};
use smelt_core::theme::{intern, Theme};
use smelt_core::transcript_model::BlockHistory;

#[cfg(test)]
std::thread_local! {
    static TEST_RETAINED_TEXT_RENDER_COUNT: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

pub(crate) fn render_layout_ir_into(out: &mut LineBuilder, layout: &LayoutIr, width: u16) -> u16 {
    render_layout_ir_range_measured(
        out,
        layout,
        width,
        0,
        usize::MAX,
        None,
        None,
        &InlineOptions::default(),
        RenderMeasurement::Complete,
    )
}

pub(crate) fn render_layout_ir_into_with_history(
    out: &mut LineBuilder,
    layout: &LayoutIr,
    width: u16,
    history: &BlockHistory,
    inline_options: &InlineOptions,
) -> u16 {
    render_layout_ir_range_measured(
        out,
        layout,
        width,
        0,
        usize::MAX,
        None,
        Some(history),
        inline_options,
        RenderMeasurement::Complete,
    )
}

#[cfg(test)]
pub(crate) fn measure_layout_ir(layout: &LayoutIr, width: u16) -> usize {
    measure_layout_ir_with_options(layout, width, &InlineOptions::default())
}

#[cfg(test)]
pub(crate) fn measure_layout_ir_with_options(
    layout: &LayoutIr,
    width: u16,
    inline_options: &InlineOptions,
) -> usize {
    measure_layout_ir_full(layout, width, inline_options)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MeasuredLayout {
    rows: usize,
    kind: MeasuredLayoutKind,
}

#[derive(Debug, PartialEq, Eq)]
enum MeasuredLayoutKind {
    Terminal,
    Child(Box<MeasuredLayout>),
    Children(Vec<MeasuredLayout>),
    Hbox {
        widths: Vec<u16>,
        children: Vec<MeasuredLayout>,
    },
}

#[derive(Clone, Copy)]
enum RenderMeasurement<'a> {
    Complete,
    Measured(&'a MeasuredLayout),
}

impl RenderMeasurement<'_> {
    fn child(self) -> Self {
        match self {
            Self::Complete => Self::Complete,
            Self::Measured(measured) => Self::Measured(
                measured
                    .child()
                    .expect("measured layout must match its wrapper"),
            ),
        }
    }
}

impl MeasuredLayout {
    pub(crate) fn rows(&self) -> usize {
        self.rows
    }

    fn child(&self) -> Option<&Self> {
        match &self.kind {
            MeasuredLayoutKind::Child(child) => Some(child),
            _ => None,
        }
    }

    fn children(&self) -> Option<&[Self]> {
        match &self.kind {
            MeasuredLayoutKind::Children(children) | MeasuredLayoutKind::Hbox { children, .. } => {
                Some(children)
            }
            _ => None,
        }
    }

    fn hbox_widths(&self) -> Option<&[u16]> {
        match &self.kind {
            MeasuredLayoutKind::Hbox { widths, .. } => Some(widths),
            _ => None,
        }
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        let own = std::mem::size_of::<Self>();
        match &self.kind {
            MeasuredLayoutKind::Terminal => own,
            MeasuredLayoutKind::Child(child) => own.saturating_add(child.retained_bytes()),
            MeasuredLayoutKind::Children(children) => {
                own.saturating_add(children.iter().map(Self::retained_bytes).sum::<usize>())
            }
            MeasuredLayoutKind::Hbox { widths, children } => own
                .saturating_add(widths.capacity().saturating_mul(std::mem::size_of::<u16>()))
                .saturating_add(children.iter().map(Self::retained_bytes).sum::<usize>()),
        }
    }
}

pub(crate) fn measure_layout_ir_plan(
    layout: &LayoutIr,
    width: u16,
    inline_options: &InlineOptions,
) -> MeasuredLayout {
    measure_layout_ir_plan_with_gutter(layout, width, 0, inline_options)
}

pub(crate) fn refresh_layout_ir_content_measurements(
    layout: &LayoutIr,
    measured: &mut MeasuredLayout,
    width: u16,
    inline_options: &InlineOptions,
) -> bool {
    matches!(
        refresh_layout_ir_content_measurements_inner(layout, measured, width, inline_options),
        ContentMeasurementRefresh::Updated
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContentMeasurementRefresh {
    Unchanged,
    Updated,
    Incompatible,
}

impl ContentMeasurementRefresh {
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Incompatible, _) | (_, Self::Incompatible) => Self::Incompatible,
            (Self::Updated, _) | (_, Self::Updated) => Self::Updated,
            (Self::Unchanged, Self::Unchanged) => Self::Unchanged,
        }
    }
}

fn refresh_layout_ir_content_measurements_inner(
    layout: &LayoutIr,
    measured: &mut MeasuredLayout,
    width: u16,
    inline_options: &InlineOptions,
) -> ContentMeasurementRefresh {
    match (layout, &mut measured.kind) {
        (BlockLayout::Empty, MeasuredLayoutKind::Terminal)
        | (BlockLayout::Leaf(_), MeasuredLayoutKind::Terminal) => {
            let BlockLayout::Leaf(IrLeaf::Content(spec)) = layout else {
                return ContentMeasurementRefresh::Unchanged;
            };
            measured.rows = measure_content_spec(spec, width, 0, inline_options);
            ContentMeasurementRefresh::Updated
        }
        (BlockLayout::Vbox(items), MeasuredLayoutKind::Children(children)) => {
            if items.len() != children.len() {
                return ContentMeasurementRefresh::Incompatible;
            }
            let refresh = items.iter().zip(children.iter_mut()).fold(
                ContentMeasurementRefresh::Unchanged,
                |refresh, (child, measured_child)| {
                    refresh.merge(refresh_layout_ir_content_measurements_inner(
                        child,
                        measured_child,
                        width,
                        inline_options,
                    ))
                },
            );
            if refresh == ContentMeasurementRefresh::Updated {
                measured.rows = children
                    .iter()
                    .fold(0usize, |rows, child| rows.saturating_add(child.rows));
            }
            refresh
        }
        (BlockLayout::Hbox(items), MeasuredLayoutKind::Hbox { widths, children }) => {
            if items.len() != widths.len() || items.len() != children.len() {
                return ContentMeasurementRefresh::Incompatible;
            }
            let refresh = items
                .iter()
                .zip(widths.iter().copied())
                .zip(children.iter_mut())
                .fold(
                    ContentMeasurementRefresh::Unchanged,
                    |refresh, ((item, child_width), measured_child)| {
                        if child_width == 0 {
                            refresh
                        } else {
                            refresh.merge(refresh_layout_ir_content_measurements_inner(
                                &item.layout,
                                measured_child,
                                child_width,
                                inline_options,
                            ))
                        }
                    },
                );
            if refresh == ContentMeasurementRefresh::Updated {
                measured.rows = children.iter().map(|child| child.rows).max().unwrap_or(0);
            }
            refresh
        }
        (BlockLayout::Gutter { child, spec }, MeasuredLayoutKind::Child(measured_child)) => {
            let spec_width = display_width_u16(&spec.text);
            let refresh = refresh_layout_ir_content_measurements_inner(
                child,
                measured_child,
                child_width_after_gutter(width, spec_width),
                inline_options,
            );
            if refresh == ContentMeasurementRefresh::Updated {
                measured.rows = measured_child.rows;
            }
            refresh
        }
        (BlockLayout::RowPrefix { child, spec }, MeasuredLayoutKind::Child(measured_child)) => {
            let prefix_width = row_prefix_width(spec);
            let refresh = refresh_layout_ir_content_measurements_inner(
                child,
                measured_child,
                child_width_after_gutter(width, prefix_width),
                inline_options,
            );
            if refresh == ContentMeasurementRefresh::Updated {
                measured.rows = measure_row_prefix_special(child, spec, width, inline_options)
                    .unwrap_or(measured_child.rows);
            }
            refresh
        }
        (BlockLayout::Panel { child, spec }, MeasuredLayoutKind::Child(measured_child)) => {
            let refresh = refresh_layout_ir_content_measurements_inner(
                child,
                measured_child,
                panel_child_width(width, spec.padding),
                inline_options,
            );
            if refresh == ContentMeasurementRefresh::Updated {
                measured.rows = measured_child
                    .rows
                    .saturating_add(usize::from(spec.padding.saturating_mul(2)));
            }
            refresh
        }
        (
            BlockLayout::Style { child, .. } | BlockLayout::Refresh { child, .. },
            MeasuredLayoutKind::Child(measured_child),
        ) => {
            let refresh = refresh_layout_ir_content_measurements_inner(
                child,
                measured_child,
                width,
                inline_options,
            );
            if refresh == ContentMeasurementRefresh::Updated {
                measured.rows = measured_child.rows;
            }
            refresh
        }
        (BlockLayout::Cap { child, spec }, MeasuredLayoutKind::Terminal) => {
            measured.rows = measure_bounded_cap_rows(child, spec, width, inline_options)
                .expect("every cap has a bounded measurement policy");
            ContentMeasurementRefresh::Updated
        }
        _ => ContentMeasurementRefresh::Incompatible,
    }
}

fn measure_layout_ir_plan_with_gutter(
    layout: &LayoutIr,
    width: u16,
    gutter_cells: u16,
    inline_options: &InlineOptions,
) -> MeasuredLayout {
    match layout {
        BlockLayout::Empty => MeasuredLayout {
            rows: 0,
            kind: MeasuredLayoutKind::Terminal,
        },
        BlockLayout::Leaf(leaf) => MeasuredLayout {
            rows: measure_ir_leaf(leaf, width, gutter_cells, inline_options),
            kind: MeasuredLayoutKind::Terminal,
        },
        BlockLayout::Vbox(items) => {
            let children = items
                .iter()
                .map(|child| {
                    measure_layout_ir_plan_with_gutter(child, width, gutter_cells, inline_options)
                })
                .collect::<Vec<_>>();
            let rows = children
                .iter()
                .fold(0usize, |rows, child| rows.saturating_add(child.rows));
            MeasuredLayout {
                rows,
                kind: MeasuredLayoutKind::Children(children),
            }
        }
        BlockLayout::Hbox(items) => {
            let widths = solve_ir_hbox_widths(items, width);
            let children = items
                .iter()
                .zip(widths.iter().copied())
                .map(|(item, child_width)| {
                    if child_width == 0 {
                        MeasuredLayout {
                            rows: 0,
                            kind: MeasuredLayoutKind::Terminal,
                        }
                    } else {
                        measure_layout_ir_plan(&item.layout, child_width, inline_options)
                    }
                })
                .collect::<Vec<_>>();
            let rows = children.iter().map(|child| child.rows).max().unwrap_or(0);
            MeasuredLayout {
                rows,
                kind: MeasuredLayoutKind::Hbox { widths, children },
            }
        }
        BlockLayout::Gutter { child, spec } => {
            let spec_width = display_width_u16(&spec.text);
            let child = measure_layout_ir_plan_with_gutter(
                child,
                child_width_after_gutter(width, spec_width),
                spec_width,
                inline_options,
            );
            MeasuredLayout {
                rows: child.rows,
                kind: MeasuredLayoutKind::Child(Box::new(child)),
            }
        }
        BlockLayout::RowPrefix { child, spec } => {
            let prefix_width = row_prefix_width(spec);
            let child_width = child_width_after_gutter(width, prefix_width);
            let measured_child = measure_layout_ir_plan_with_gutter(
                child,
                child_width,
                gutter_cells,
                inline_options,
            );
            let rows = measure_row_prefix_special(child, spec, width, inline_options)
                .unwrap_or(measured_child.rows);
            MeasuredLayout {
                rows,
                kind: MeasuredLayoutKind::Child(Box::new(measured_child)),
            }
        }
        BlockLayout::Panel { child, spec } => {
            let child = measure_layout_ir_plan(
                child,
                panel_child_width(width, spec.padding),
                inline_options,
            );
            MeasuredLayout {
                rows: child
                    .rows
                    .saturating_add(usize::from(spec.padding.saturating_mul(2))),
                kind: MeasuredLayoutKind::Child(Box::new(child)),
            }
        }
        BlockLayout::Style { child, .. } | BlockLayout::Refresh { child, .. } => {
            let child =
                measure_layout_ir_plan_with_gutter(child, width, gutter_cells, inline_options);
            MeasuredLayout {
                rows: child.rows,
                kind: MeasuredLayoutKind::Child(Box::new(child)),
            }
        }
        BlockLayout::Cap { child, spec } => MeasuredLayout {
            rows: measure_bounded_cap_rows(child, spec, width, inline_options)
                .expect("every cap has a bounded measurement policy"),
            kind: MeasuredLayoutKind::Terminal,
        },
    }
}

fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_layout_ir_range_into_measured(
    out: &mut LineBuilder,
    layout: &LayoutIr,
    measured: &MeasuredLayout,
    width: u16,
    row_start: usize,
    row_count: usize,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
) -> u16 {
    render_layout_ir_range_measured(
        out,
        layout,
        width,
        row_start,
        row_count,
        None,
        history,
        inline_options,
        RenderMeasurement::Measured(measured),
    )
}

#[allow(clippy::too_many_arguments)]
fn render_layout_ir_range_measured(
    out: &mut LineBuilder,
    layout: &LayoutIr,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
    measurement: RenderMeasurement<'_>,
) -> u16 {
    if row_count == 0 {
        return 0;
    }
    match layout {
        BlockLayout::Empty => 0,
        BlockLayout::Leaf(leaf) => render_ir_leaf(
            out,
            leaf,
            width,
            row_start,
            row_count,
            gutter,
            history,
            inline_options,
        ),
        BlockLayout::Vbox(items) => render_ir_vbox(
            out,
            items,
            width,
            row_start,
            row_count,
            gutter,
            history,
            inline_options,
            measurement,
        ),
        BlockLayout::Hbox(items) => render_ir_hbox(
            out,
            items,
            width,
            row_start,
            row_count,
            gutter,
            history,
            inline_options,
            measurement,
        ),
        BlockLayout::Gutter { child, spec } => {
            let spec_width = display_width_u16(&spec.text);
            let child_width = child_width_after_gutter(width, spec_width);
            render_layout_ir_range_measured(
                out,
                child,
                child_width,
                row_start,
                row_count,
                Some(spec),
                history,
                inline_options,
                measurement.child(),
            )
        }
        BlockLayout::RowPrefix { child, spec } => render_ir_row_prefix(
            out,
            child,
            spec,
            width,
            row_start,
            row_count,
            gutter,
            history,
            inline_options,
            measurement.child(),
        ),
        BlockLayout::Panel { child, spec } => render_ir_panel(
            out,
            child,
            spec,
            width,
            row_start,
            row_count,
            gutter,
            history,
            inline_options,
            measurement.child(),
        ),
        BlockLayout::Style { child, spec } => render_ir_style(
            out,
            child,
            spec,
            width,
            row_start,
            row_count,
            gutter,
            history,
            inline_options,
            measurement.child(),
        ),
        BlockLayout::Cap { child, spec } => render_ir_cap(
            out,
            child,
            spec,
            width,
            row_start,
            row_count,
            gutter,
            inline_options,
        ),
        BlockLayout::Refresh { child, .. } => render_layout_ir_range_measured(
            out,
            child,
            width,
            row_start,
            row_count,
            gutter,
            history,
            inline_options,
            measurement.child(),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_ir_vbox(
    out: &mut LineBuilder,
    items: &[LayoutIr],
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
    measurement: RenderMeasurement<'_>,
) -> u16 {
    let mut written = 0u16;
    let mut skipped = row_start;
    let measured_children = match measurement {
        RenderMeasurement::Complete => None,
        RenderMeasurement::Measured(measured) => Some(
            measured
                .children()
                .expect("measured layout must match its vbox"),
        ),
    };
    if let Some(children) = measured_children {
        assert_eq!(children.len(), items.len(), "measured vbox child count");
    }
    for (index, child) in items.iter().enumerate() {
        let child_measurement = measured_children.map_or(RenderMeasurement::Complete, |children| {
            RenderMeasurement::Measured(&children[index])
        });
        if usize::from(written) >= row_count {
            break;
        }
        let child_rows = match child_measurement {
            RenderMeasurement::Complete => measure_layout_ir_full_with_gutter(
                child,
                width,
                gutter_width(gutter),
                inline_options,
            ),
            RenderMeasurement::Measured(measured) => measured.rows(),
        };
        if skipped >= child_rows {
            skipped = skipped.saturating_sub(child_rows);
            continue;
        }
        let child_count = row_count
            .saturating_sub(usize::from(written))
            .min(child_rows - skipped);
        written = written.saturating_add(render_layout_ir_range_measured(
            out,
            child,
            width,
            skipped,
            child_count,
            gutter,
            history,
            inline_options,
            child_measurement,
        ));
        skipped = 0;
    }
    written
}

fn measure_layout_ir_full(layout: &LayoutIr, width: u16, inline_options: &InlineOptions) -> usize {
    measure_layout_ir_full_with_gutter(layout, width, 0, inline_options)
}

fn measure_layout_ir_full_with_gutter(
    layout: &LayoutIr,
    width: u16,
    gutter_cells: u16,
    inline_options: &InlineOptions,
) -> usize {
    match layout {
        BlockLayout::Empty => 0,
        BlockLayout::Leaf(leaf) => measure_ir_leaf(leaf, width, gutter_cells, inline_options),
        BlockLayout::Vbox(items) => items
            .iter()
            .map(|child| {
                measure_layout_ir_full_with_gutter(child, width, gutter_cells, inline_options)
            })
            .sum(),
        BlockLayout::Hbox(items) => {
            let widths = solve_ir_hbox_widths(items, width);
            items
                .iter()
                .zip(widths)
                .filter(|(_, child_width)| *child_width > 0)
                .map(|(item, child_width)| {
                    measure_layout_ir_full(&item.layout, child_width, inline_options)
                })
                .max()
                .unwrap_or(0)
        }
        BlockLayout::Gutter { child, spec } => {
            let spec_width = display_width_u16(&spec.text);
            let child_width = child_width_after_gutter(width, spec_width);
            measure_layout_ir_full_with_gutter(child, child_width, spec_width, inline_options)
        }
        BlockLayout::RowPrefix { child, spec } => {
            if let Some(rows) = measure_row_prefix_special(child, spec, width, inline_options) {
                rows
            } else {
                let prefix_width = row_prefix_width(spec);
                let child_width = child_width_after_gutter(width, prefix_width);
                measure_layout_ir_full_with_gutter(child, child_width, gutter_cells, inline_options)
            }
        }
        BlockLayout::Panel { child, spec } => measure_ir_panel(child, spec, width, inline_options),
        BlockLayout::Style { child, .. } => {
            measure_layout_ir_full_with_gutter(child, width, gutter_cells, inline_options)
        }
        BlockLayout::Cap { child, spec } => {
            measure_bounded_cap_rows(child, spec, width, inline_options)
                .expect("every cap has a bounded measurement policy")
        }
        BlockLayout::Refresh { child, .. } => {
            measure_layout_ir_full_with_gutter(child, width, gutter_cells, inline_options)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_ir_leaf(
    out: &mut LineBuilder,
    leaf: &IrLeaf,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
    _history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
) -> u16 {
    match leaf {
        IrLeaf::Text(spec) => render_text_spec(out, spec, width, row_start, row_count, gutter),
        IrLeaf::Content(spec) => render_content_spec(
            out,
            spec,
            width,
            row_start,
            row_count,
            gutter,
            inline_options,
        ),
        IrLeaf::Runs(spec) => render_runs_spec(out, spec, width, row_start, row_count, gutter),
        IrLeaf::Line(spec) => render_line_spec(out, spec, row_start, row_count, gutter),
        IrLeaf::Markdown(spec) => render_markdown_spec(
            out,
            spec,
            width,
            row_start,
            row_count,
            gutter,
            inline_options,
        ),
        IrLeaf::Code(spec) => render_code_spec(out, spec, width, row_start, row_count, gutter),
        IrLeaf::Separator(spec) => {
            render_separator_spec(out, spec, width, row_start, row_count, gutter)
        }
        IrLeaf::SourceView(smelt_core::content::block_layout::SourceViewIr::Diff(cache)) => {
            let indent = gutter_width(gutter);
            let target =
                SourceViewTarget::new(indent, width.saturating_add(indent), row_start, row_count);
            render_source_view(out, SourceView::DiffIr(cache), target)
        }
    }
}

fn measure_ir_leaf(
    leaf: &IrLeaf,
    width: u16,
    gutter_cells: u16,
    inline_options: &InlineOptions,
) -> usize {
    match leaf {
        IrLeaf::Text(spec) => measure_text_spec(spec, width),
        IrLeaf::Content(spec) => measure_content_spec(spec, width, gutter_cells, inline_options),
        IrLeaf::Runs(spec) => measure_runs_spec(spec, width),
        IrLeaf::Line(spec) => measure_line_spec(spec),
        IrLeaf::Markdown(spec) => measure_markdown_spec(spec, width, inline_options),
        IrLeaf::Code(spec) => measure_code_spec(spec, width),
        IrLeaf::Separator(spec) => measure_separator_spec(spec),
        IrLeaf::SourceView(smelt_core::content::block_layout::SourceViewIr::Diff(cache)) => {
            smelt_core::content::highlight::measure_diff_ir(
                cache,
                width.saturating_add(gutter_cells),
                smelt_core::content::highlight::GutterStyle::InlineLineNumbers,
                gutter_cells,
            )
        }
    }
}

fn expand_tabs(line: &str) -> Cow<'_, str> {
    if line.contains('\t') {
        Cow::Owned(line.replace('\t', "    "))
    } else {
        Cow::Borrowed(line)
    }
}

fn wrap_plain_line_ranges(line: &str, width: u16) -> Vec<(usize, usize)> {
    smelt_buffer::wrap::wrap_line_ranges(line, width as usize)
}

fn count_plain_line_ranges(line: &str, width: u16) -> usize {
    smelt_buffer::wrap::count_line_ranges(line, width as usize)
}

fn render_content_spec(
    out: &mut LineBuilder,
    spec: &RetainedContentSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
    inline_options: &InlineOptions,
) -> u16 {
    match &spec.render {
        ContentRenderSpec::Text { hl_group, ansi } => render_content_text_spec(
            out,
            spec,
            hl_group.as_deref(),
            *ansi,
            width,
            row_start,
            row_count,
            gutter,
        ),
        ContentRenderSpec::Markdown {
            dim,
            italic,
            inline,
        } => render_retained_markdown_spec(
            out,
            spec,
            *dim,
            *italic,
            *inline,
            width,
            row_start,
            row_count,
            gutter,
            inline_options,
        ),
        ContentRenderSpec::Code { lang, cache } => {
            render_retained_code_spec(out, spec, lang, cache, width, row_start, row_count, gutter)
        }
        ContentRenderSpec::File { path, lang, cache } => {
            let indent = gutter_width(gutter);
            let syntax_ext = lang
                .as_deref()
                .map(smelt_core::content::highlight::lang_to_ext)
                .or_else(|| {
                    std::path::Path::new(path)
                        .extension()
                        .and_then(|ext| ext.to_str())
                })
                .unwrap_or("txt");
            smelt_core::content::highlight::print_retained_file_view(
                out,
                &spec.content,
                cache,
                syntax_ext,
                smelt_core::content::highlight::GutterStyle::InlineLineNumbers,
                indent,
                width.saturating_add(indent),
                row_start,
                row_count,
            )
        }
    }
}

fn measure_content_spec(
    spec: &RetainedContentSpec,
    width: u16,
    gutter_cells: u16,
    inline_options: &InlineOptions,
) -> usize {
    match &spec.render {
        ContentRenderSpec::Text { ansi, .. } => measure_content_text_spec(spec, *ansi, width),
        ContentRenderSpec::Markdown {
            dim,
            italic,
            inline,
        } => measure_retained_markdown_spec(spec, *dim, *italic, *inline, width, inline_options),
        ContentRenderSpec::Code { .. } => {
            smelt_core::content::highlight::measure_retained_code_block(&spec.content, width)
        }
        ContentRenderSpec::File { .. } => {
            smelt_core::content::highlight::measure_retained_file_view(
                &spec.content,
                width.saturating_add(gutter_cells),
                smelt_core::content::highlight::GutterStyle::InlineLineNumbers,
                gutter_cells,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_retained_code_spec(
    out: &mut LineBuilder,
    spec: &RetainedContentSpec,
    lang: &str,
    cache: &smelt_core::content::highlight::RetainedFileViewCache,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
) -> u16 {
    if row_count == 0 {
        return 0;
    }
    if gutter.is_none() {
        return smelt_core::content::highlight::print_retained_code_block(
            out,
            &spec.content,
            cache,
            lang,
            width,
            row_start,
            row_count,
        );
    }

    let inherited_style = out.current_style();
    let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
    let outcome = {
        let mut col = LineBuilder::new(&mut buf, out.theme(), width.max(1));
        col.push(None, inherited_style);
        smelt_core::content::highlight::print_retained_code_block(
            &mut col,
            &spec.content,
            cache,
            lang,
            width,
            row_start,
            row_count,
        );
        col.finish()
    };
    if outcome.was_wrapped {
        out.mark_wrapped();
    }
    let gutter = gutter.expect("guttered retained code render requires gutter");
    let rows = outcome.line_count.min(u16::MAX as usize) as u16;
    for row in 0..rows {
        print_row_gutter(out, gutter);
        apply_temp_decoration(out, &buf, row as usize, true);
        emit_buffer_row_clipped(&buf, row, width, out, None);
        out.newline();
    }
    rows
}

#[allow(clippy::too_many_arguments)]
fn render_retained_markdown_spec(
    out: &mut LineBuilder,
    spec: &RetainedContentSpec,
    dim: bool,
    italic: bool,
    inline: bool,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
    inline_options: &InlineOptions,
) -> u16 {
    if inline {
        return render_retained_inline_markdown_spec(
            out,
            spec,
            dim,
            italic,
            width,
            row_start,
            row_count,
            gutter,
            inline_options,
        );
    }
    if let Some(rows) = render_retained_plain_markdown_line(
        out, spec, dim, italic, width, row_start, row_count, gutter,
    ) {
        return rows;
    }

    if gutter.is_none() {
        if italic {
            out.save_style();
            out.set_italic();
        }
        let rows = render_retained_markdown_inner_range_with_options(
            out,
            &spec.content,
            width as usize,
            "",
            dim,
            None,
            inline_options,
            row_start,
            row_count,
        );
        if italic {
            out.pop_style();
        }
        return rows;
    }

    let gutter = gutter.expect("guttered retained Markdown render requires gutter");
    let inherited_style = out.current_style();
    let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
    let outcome = {
        let mut col = LineBuilder::new(&mut buf, out.theme(), width.max(1));
        col.push(None, inherited_style);
        if italic {
            col.save_style();
            col.set_italic();
        }
        render_retained_markdown_inner_range_with_options(
            &mut col,
            &spec.content,
            width as usize,
            "",
            dim,
            None,
            inline_options,
            row_start,
            row_count,
        );
        if italic {
            col.pop_style();
        }
        col.finish()
    };
    if outcome.was_wrapped {
        out.mark_wrapped();
    }
    let style_overlay = gutter.styled.then_some((dim, italic));
    let rows = outcome.line_count.min(u16::MAX as usize) as u16;
    for row in 0..rows {
        print_row_gutter(out, gutter);
        apply_temp_decoration(out, &buf, row as usize, true);
        emit_buffer_row_clipped(&buf, row, width, out, style_overlay);
        out.newline();
    }
    rows
}

fn measure_retained_markdown_spec(
    spec: &RetainedContentSpec,
    dim: bool,
    italic: bool,
    inline: bool,
    width: u16,
    inline_options: &InlineOptions,
) -> usize {
    if inline {
        return measure_retained_inline_markdown_spec(spec, dim, italic, width, inline_options);
    }
    if let Some(rows) = measure_retained_plain_markdown_line(spec, width) {
        return rows;
    }
    measure_retained_markdown_inner_with_options(
        &spec.content,
        width as usize,
        "",
        dim,
        None,
        inline_options,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_retained_inline_markdown_spec(
    out: &mut LineBuilder,
    spec: &RetainedContentSpec,
    dim: bool,
    italic: bool,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
    inline_options: &InlineOptions,
) -> u16 {
    let read = spec.content.read();
    let max_cols = width.max(1) as usize;
    let mut seen = 0usize;
    let mut rows = 0u16;
    'outer: for line_index in 0..read.logical_line_count() {
        let Some(line) = read.line(line_index) else {
            continue;
        };
        let spans = retained_inline_markdown_spans(&line, dim, italic, inline_options);
        let wrapped = wrap_inline_spans(&spans, max_cols);
        for segment in wrapped_segments(out, &wrapped) {
            if seen < row_start {
                seen = seen.saturating_add(1);
                continue;
            }
            if usize::from(rows) >= row_count {
                break 'outer;
            }
            segment.emit(out, |out, row_spans, _| {
                if let Some(gutter) = gutter {
                    if gutter.styled {
                        out.save_style();
                        if dim {
                            out.set_dim();
                        }
                        if italic {
                            out.set_italic();
                        }
                        out.print_gutter(&gutter.text);
                        out.pop_style();
                    } else {
                        out.print_gutter(&gutter.text);
                    }
                }
                emit_inline_spans(out, row_spans);
            });
            out.newline();
            rows = rows.saturating_add(1);
            seen = seen.saturating_add(1);
        }
    }
    rows
}

fn measure_retained_inline_markdown_spec(
    spec: &RetainedContentSpec,
    dim: bool,
    italic: bool,
    width: u16,
    inline_options: &InlineOptions,
) -> usize {
    let read = spec.content.read();
    let max_cols = width.max(1) as usize;
    (0..read.logical_line_count())
        .filter_map(|line| read.line(line))
        .map(|line| {
            let spans = retained_inline_markdown_spans(&line, dim, italic, inline_options);
            wrap_inline_spans(&spans, max_cols).len()
        })
        .sum()
}

fn retained_inline_markdown_spans(
    line: &str,
    dim: bool,
    italic: bool,
    inline_options: &InlineOptions,
) -> Vec<InlineSpan> {
    let mut spans = parse_inline_spans_with_options(line, dim, inline_options);
    if italic {
        for span in &mut spans {
            span.style.italic = true;
        }
    }
    spans
}

#[allow(clippy::too_many_arguments)]
fn render_retained_plain_markdown_line(
    out: &mut LineBuilder,
    spec: &RetainedContentSpec,
    dim: bool,
    italic: bool,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
) -> Option<u16> {
    let read = spec.content.read();
    let plain_markdown =
        read.logical_line_count() == 1 && read.line_is_plain_markdown(0) && !read.is_empty();
    drop(read);
    if !plain_markdown {
        return None;
    }

    let mut rows = 0u16;
    spec.content.visit_text_layout_rows(
        width,
        false,
        row_start..row_start.saturating_add(row_count),
        |row| {
            render_retained_plain_markdown_row(out, row, dim, italic, gutter);
            rows = rows.saturating_add(1);
        },
    );
    Some(rows)
}

fn measure_retained_plain_markdown_line(spec: &RetainedContentSpec, width: u16) -> Option<usize> {
    let read = spec.content.read();
    let plain_markdown =
        read.logical_line_count() == 1 && read.line_is_plain_markdown(0) && !read.is_empty();
    drop(read);
    plain_markdown.then(|| spec.content.text_layout_rows(width, false))
}

fn render_retained_plain_markdown_row(
    out: &mut LineBuilder,
    row: smelt_core::transcript_content::ContentTextRow<'_>,
    dim: bool,
    italic: bool,
    gutter: Option<&GutterSpec>,
) {
    if row.wrapped() {
        out.mark_wrapped();
    }
    if row.continuation() {
        out.mark_soft_wrap_continuation();
    }
    let text = row.text();
    emit_retained_plain_markdown_slice(out, &text, dim, italic, gutter);
    out.newline();
}

fn emit_retained_plain_markdown_slice(
    out: &mut LineBuilder,
    text: &str,
    dim: bool,
    italic: bool,
    gutter: Option<&GutterSpec>,
) {
    if let Some(gutter) = gutter.filter(|gutter| !gutter.styled) {
        out.print_gutter(&gutter.text);
    }
    if dim {
        out.push_dim();
    }
    if italic {
        out.save_style();
        out.set_italic();
    }
    if let Some(gutter) = gutter.filter(|gutter| gutter.styled) {
        out.print_gutter(&gutter.text);
    }
    out.print(text);
    if italic {
        out.pop_style();
    }
    if dim {
        out.pop_style();
    }
}

#[allow(clippy::too_many_arguments)]
fn render_content_text_spec(
    out: &mut LineBuilder,
    spec: &RetainedContentSpec,
    hl_group: Option<&str>,
    ansi: bool,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:layout:render_content_text");
    #[cfg(test)]
    TEST_RETAINED_TEXT_RENDER_COUNT.with(|count| count.set(count.get().saturating_add(1)));
    let row_end = row_start.saturating_add(row_count);
    smelt_perf::perf::record_value(
        "render:layout:render_content_text_bytes",
        spec.content.len() as u64,
    );
    let hl = hl_group.map(intern);
    let mut rows = 0u16;
    spec.content
        .visit_text_layout_rows(width, ansi, row_start..row_end, |row| {
            render_content_text_row(out, row, hl, ansi, gutter);
            rows = rows.saturating_add(1);
        });
    rows
}

fn render_content_text_row(
    out: &mut LineBuilder,
    row: smelt_core::transcript_content::ContentTextRow<'_>,
    hl: Option<smelt_core::theme::HlGroup>,
    ansi: bool,
    gutter: Option<&GutterSpec>,
) {
    if row.wrapped() {
        out.mark_wrapped();
    }
    if row.continuation() {
        out.mark_soft_wrap_continuation();
    }
    if let Some(gutter) = gutter.filter(|gutter| !gutter.styled) {
        out.print_gutter(&gutter.text);
    }
    match hl {
        Some(group) => out.push_hl(group),
        None => out.push_dim(),
    }
    if let Some(gutter) = gutter.filter(|gutter| gutter.styled) {
        out.print_gutter(&gutter.text);
    }
    if ansi && !row.spans().is_empty() {
        let text = row.text();
        let mut cursor = 0usize;
        for span in row.spans() {
            if cursor < span.byte_range.start {
                out.print(smelt_buffer::text::slice(
                    &text,
                    cursor..span.byte_range.start,
                ));
            }
            let span_text = smelt_buffer::text::slice(&text, span.byte_range.clone());
            if span.style == smelt_core::style::Style::default() {
                out.print(span_text);
            } else {
                out.push(None, span.style);
                out.print(span_text);
                out.pop_style();
            }
            cursor = span.byte_range.end;
        }
        if cursor < text.len() {
            out.print(smelt_buffer::text::slice(&text, cursor..text.len()));
        }
    } else {
        row.visit_text(|text| out.print(text));
    }
    out.pop_style();
    out.newline();
}

fn measure_content_text_spec(spec: &RetainedContentSpec, ansi: bool, width: u16) -> usize {
    let _perf = smelt_perf::perf::begin("render:layout:measure_content_text");
    spec.content.text_layout_rows(width, ansi)
}

fn render_text_spec(
    out: &mut LineBuilder,
    spec: &TextSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:layout:render_text");
    smelt_perf::perf::record_value("render:layout:render_text_bytes", spec.content.len() as u64);
    smelt_perf::perf::record_value("render:layout:render_text_row_start", row_start as u64);
    let hl = spec.hl_group.as_deref().map(intern);
    let mut seen = 0usize;
    let mut rows = 0u16;
    'outer: for line in spec.content.lines() {
        let expanded = expand_tabs(line);
        let ansi_wrapped = spec.ansi.then(|| wrap_ansi(&expanded, width as usize));
        let plain_ranges;
        let ranges: &[(usize, usize)] = if let Some((_, ranges, _)) = &ansi_wrapped {
            ranges
        } else {
            let line_rows = count_plain_line_ranges(&expanded, width);
            if seen.saturating_add(line_rows) <= row_start {
                if line_rows > 1 {
                    out.mark_wrapped();
                }
                seen = seen.saturating_add(line_rows);
                continue;
            }
            plain_ranges = wrap_plain_line_ranges(&expanded, width);
            &plain_ranges
        };
        for segment in wrapped_segments(out, ranges) {
            if seen < row_start {
                seen = seen.saturating_add(1);
                continue;
            }
            if usize::from(rows) >= row_count {
                break 'outer;
            }
            segment.emit(out, |out, &(ws, we), _| {
                if let Some(gutter) = gutter.filter(|g| !g.styled) {
                    out.print_gutter(&gutter.text);
                }
                match hl {
                    Some(group) => out.push_hl(group),
                    None => out.push_dim(),
                }
                if let Some(gutter) = gutter.filter(|g| g.styled) {
                    out.print_gutter(&gutter.text);
                }
                if let Some((spans, _, boundaries)) = &ansi_wrapped {
                    emit_ansi_row(out, spans, boundaries, ws, we);
                } else {
                    out.print(smelt_buffer::text::slice(&expanded, ws..we));
                }
                out.pop_style();
            });
            out.newline();
            rows = rows.saturating_add(1);
            seen = seen.saturating_add(1);
        }
    }
    rows
}

fn measure_text_spec(spec: &TextSpec, width: u16) -> usize {
    let _perf = smelt_perf::perf::begin("render:layout:measure_text");
    smelt_perf::perf::record_value(
        "render:layout:measure_text_bytes",
        spec.content.len() as u64,
    );
    spec.content
        .lines()
        .map(|line| {
            let expanded = expand_tabs(line);
            if spec.ansi {
                let (_, ranges, _) = wrap_ansi(&expanded, width as usize);
                ranges.len()
            } else {
                count_plain_line_ranges(&expanded, width)
            }
        })
        .sum()
}

struct StyledLine {
    styles: Arc<[protocol::StyledSpan]>,
    inline: InlineLine<usize>,
}

impl StyledLine {
    fn new(spans: &[protocol::StyledSpan]) -> Self {
        let (styles, runs) = styled_line_parts(spans);
        Self {
            styles: styles.into(),
            inline: InlineLine::new(runs),
        }
    }

    fn from_parts(styles: Vec<protocol::StyledSpan>, runs: Vec<InlineRun<usize>>) -> Self {
        Self {
            styles: styles.into(),
            inline: InlineLine { runs },
        }
    }

    fn run(&self, index: usize) -> Option<(&protocol::StyledSpan, &InlineRun<usize>)> {
        let run = self.inline.runs.get(index)?;
        Some((self.styles.get(run.meta)?, run))
    }
}

fn styled_line_parts(
    spans: &[protocol::StyledSpan],
) -> (Vec<protocol::StyledSpan>, Vec<InlineRun<usize>>) {
    let mut styles = Vec::with_capacity(spans.len());
    let mut runs = Vec::with_capacity(spans.len());
    for (index, span) in spans.iter().enumerate() {
        let mut style = span.clone();
        let text = std::mem::take(&mut style.text);
        styles.push(style);
        runs.push(InlineRun::new(text, index, BreakPolicy::BreakOnSpaces));
    }
    (styles, runs)
}

fn wrap_styled_line(
    line: &StyledLine,
    width: u16,
    continuation_indent: u16,
) -> Vec<Vec<WrappedRun<usize>>> {
    let width = width.max(1) as usize;
    let indent = continuation_indent as usize;
    let continuation_width = width
        .saturating_sub(indent.min(width.saturating_sub(1)))
        .max(1);
    line.inline
        .wrap_fragments_with_widths(width, continuation_width)
}

fn wrap_styled_runs(
    spans: &[protocol::StyledSpan],
    width: u16,
    continuation_indent: u16,
) -> Vec<Vec<WrappedRun<usize>>> {
    wrap_styled_line(&StyledLine::new(spans), width, continuation_indent)
}

fn runs_continuation_indent(spec: &RunsSpec, width: u16) -> u16 {
    spec.continuation_indent.min(width.saturating_sub(1))
}

fn render_runs_spec(
    out: &mut LineBuilder,
    spec: &RunsSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
) -> u16 {
    let default_hl = spec.hl_group.as_deref();
    let continuation_indent = runs_continuation_indent(spec, width);
    let mut seen = 0usize;
    let mut rows = 0u16;
    'outer: for (line_index, spans) in spec.lines.0.iter().enumerate() {
        let line = StyledLine::new(spans);
        let wrapped = wrap_styled_line(&line, width, continuation_indent);
        for segment in wrapped_segments(out, &wrapped) {
            if seen < row_start {
                seen = seen.saturating_add(1);
                continue;
            }
            if usize::from(rows) >= row_count {
                break 'outer;
            }
            segment.emit(out, |out, row_fragments, kind| {
                if let Some(gutter) = gutter {
                    out.print_gutter(&gutter.text);
                }
                if kind.is_continuation() && continuation_indent > 0 {
                    out.print_with_meta(
                        &" ".repeat(continuation_indent as usize),
                        SpanMeta::unselectable(),
                    );
                }
                for fragment in row_fragments {
                    let Some((style, run)) = line.run(fragment.run_index) else {
                        continue;
                    };
                    print_styled_text_range(
                        out,
                        style,
                        &run.text,
                        default_hl,
                        fragment.range.clone(),
                        spec.syntax_highlights.spans(line_index, run.meta),
                    );
                }
            });
            out.newline();
            rows = rows.saturating_add(1);
            seen = seen.saturating_add(1);
        }
    }
    rows
}

fn measure_runs_spec(spec: &RunsSpec, width: u16) -> usize {
    let continuation_indent = runs_continuation_indent(spec, width);
    spec.lines
        .0
        .iter()
        .map(|spans| wrap_styled_runs(spans, width, continuation_indent).len())
        .sum()
}

fn render_line_spec(
    out: &mut LineBuilder,
    spec: &LineSpec,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
) -> u16 {
    if row_start > 0 || row_count == 0 {
        return 0;
    }
    if let Some(gutter) = gutter {
        out.print_gutter(&gutter.text);
    }
    if !spec.spans.is_empty() && spec.spans.iter().all(|span| !span.selectable) {
        out.exclude_from_copy();
    }
    let default_hl = spec.hl_group.as_deref();
    print_styled_spans(out, &spec.spans, default_hl, Some(&spec.syntax_highlights));
    out.newline();
    1
}

fn measure_line_spec(_spec: &LineSpec) -> usize {
    1
}

fn render_markdown_spec(
    out: &mut LineBuilder,
    spec: &MarkdownSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
    inline_options: &InlineOptions,
) -> u16 {
    if spec.inline {
        return render_inline_markdown_spec(
            out,
            spec,
            width,
            row_start,
            row_count,
            gutter,
            inline_options,
        );
    }
    if can_render_markdown_as_plain(&spec.content) {
        return render_plain_markdown_spec(out, spec, width, row_start, row_count, gutter);
    }

    if gutter.is_none() {
        if spec.italic {
            out.save_style();
            out.set_italic();
        }
        let rows = render_markdown_inner_range_with_options(
            out,
            &spec.content,
            width as usize,
            "",
            spec.dim,
            None,
            inline_options,
            row_start,
            row_count,
        );
        if spec.italic {
            out.pop_style();
        }
        return rows;
    }

    render_markdown_spec_with_gutter(
        out,
        spec,
        width,
        row_start,
        row_count,
        gutter.expect("guttered markdown render requires gutter"),
        inline_options,
    )
}

fn render_markdown_spec_with_gutter(
    out: &mut LineBuilder,
    spec: &MarkdownSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: &GutterSpec,
    inline_options: &InlineOptions,
) -> u16 {
    let inherited_style = out.current_style();
    let mut buf = smelt_core::buffer::Buffer::new(
        smelt_core::buffer::BufId(0),
        smelt_core::buffer::BufCreateOpts::default(),
    );
    let outcome = {
        let mut col = LineBuilder::new(&mut buf, out.theme(), width.max(1));
        col.push(None, inherited_style);
        if spec.italic {
            col.save_style();
            col.set_italic();
        }
        render_markdown_inner_range_with_options(
            &mut col,
            &spec.content,
            width as usize,
            "",
            spec.dim,
            None,
            inline_options,
            row_start,
            row_count,
        );
        if spec.italic {
            col.pop_style();
        }
        col.finish()
    };
    if outcome.was_wrapped {
        out.mark_wrapped();
    }
    let style_overlay = gutter.styled.then_some((spec.dim, spec.italic));
    let rows = outcome.line_count.min(u16::MAX as usize) as u16;
    for row in 0..rows {
        print_row_gutter(out, gutter);
        apply_temp_decoration(out, &buf, row as usize, true);
        emit_buffer_row_clipped(&buf, row, width, out, style_overlay);
        out.newline();
    }
    rows
}

fn measure_markdown_spec(spec: &MarkdownSpec, width: u16, inline_options: &InlineOptions) -> usize {
    if spec.inline {
        return measure_inline_markdown_spec(spec, width, inline_options);
    }
    if can_render_markdown_as_plain(&spec.content) {
        return measure_plain_markdown_spec(spec, width);
    }
    measure_markdown_inner_with_options(
        &spec.content,
        width as usize,
        "",
        spec.dim,
        None,
        inline_options,
    )
}

fn can_render_markdown_as_plain(content: &str) -> bool {
    !content.is_empty()
        && !content.contains('\n')
        && !content.as_bytes().iter().any(|byte| {
            matches!(
                byte,
                b'\\'
                    | b'`'
                    | b'*'
                    | b'_'
                    | b'['
                    | b']'
                    | b'('
                    | b')'
                    | b'#'
                    | b'>'
                    | b'-'
                    | b'+'
                    | b'='
                    | b'|'
                    | b'~'
                    | b'!'
                    | b'&'
                    | b'<'
            )
        })
}

fn render_plain_markdown_spec(
    out: &mut LineBuilder,
    spec: &MarkdownSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
) -> u16 {
    if plain_markdown_is_single_ascii_word(&spec.content) {
        return render_plain_markdown_ascii_word(out, spec, width, row_start, row_count, gutter);
    }
    let mut seen = 0usize;
    let mut rows = 0u16;
    'outer: for line in spec.content.lines() {
        let expanded = expand_tabs(line);
        let line_rows = count_plain_line_ranges(&expanded, width);
        if seen.saturating_add(line_rows) <= row_start {
            if line_rows > 1 {
                out.mark_wrapped();
            }
            seen = seen.saturating_add(line_rows);
            continue;
        }
        let ranges = wrap_plain_line_ranges(&expanded, width);
        for segment in wrapped_segments(out, &ranges) {
            if seen < row_start {
                seen = seen.saturating_add(1);
                continue;
            }
            if usize::from(rows) >= row_count {
                break 'outer;
            }
            segment.emit(out, |out, &(ws, we), _| {
                if let Some(gutter) = gutter.filter(|g| !g.styled) {
                    out.print_gutter(&gutter.text);
                }
                if spec.dim {
                    out.push_dim();
                }
                if spec.italic {
                    out.save_style();
                    out.set_italic();
                }
                if let Some(gutter) = gutter.filter(|g| g.styled) {
                    out.print_gutter(&gutter.text);
                }
                out.print(smelt_buffer::text::slice(&expanded, ws..we));
                if spec.italic {
                    out.pop_style();
                }
                if spec.dim {
                    out.pop_style();
                }
            });
            out.newline();
            rows = rows.saturating_add(1);
            seen = seen.saturating_add(1);
        }
    }
    rows
}

fn measure_plain_markdown_spec(spec: &MarkdownSpec, width: u16) -> usize {
    if plain_markdown_is_single_ascii_word(&spec.content) {
        return ascii_word_rows(spec.content.len(), width);
    }
    spec.content
        .lines()
        .map(|line| count_plain_line_ranges(&expand_tabs(line), width))
        .sum()
}

fn plain_markdown_is_single_ascii_word(content: &str) -> bool {
    content
        .as_bytes()
        .iter()
        .all(|byte| byte.is_ascii_graphic() && *byte != b' ')
}

fn ascii_word_rows(len: usize, width: u16) -> usize {
    let width = usize::from(width.max(1));
    len.max(1).div_ceil(width)
}

fn render_plain_markdown_ascii_word(
    out: &mut LineBuilder,
    spec: &MarkdownSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
) -> u16 {
    if row_count == 0 {
        return 0;
    }
    let width = usize::from(width.max(1));
    let total_rows = ascii_word_rows(spec.content.len(), width as u16);
    if total_rows > 1 {
        out.mark_wrapped();
    }
    let mut rows = 0u16;
    let end_row = row_start.saturating_add(row_count).min(total_rows);
    for row in row_start..end_row {
        let start = row.saturating_mul(width).min(spec.content.len());
        let end = start.saturating_add(width).min(spec.content.len());
        emit_plain_markdown_slice(
            out,
            spec,
            smelt_buffer::text::slice(&spec.content, start..end),
            gutter,
        );
        out.newline();
        rows = rows.saturating_add(1);
    }
    rows
}

fn emit_plain_markdown_slice(
    out: &mut LineBuilder,
    spec: &MarkdownSpec,
    text: &str,
    gutter: Option<&GutterSpec>,
) {
    if let Some(gutter) = gutter.filter(|g| !g.styled) {
        out.print_gutter(&gutter.text);
    }
    if spec.dim {
        out.push_dim();
    }
    if spec.italic {
        out.save_style();
        out.set_italic();
    }
    if let Some(gutter) = gutter.filter(|g| g.styled) {
        out.print_gutter(&gutter.text);
    }
    out.print(text);
    if spec.italic {
        out.pop_style();
    }
    if spec.dim {
        out.pop_style();
    }
}

fn render_inline_markdown_spec(
    out: &mut LineBuilder,
    spec: &MarkdownSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
    inline_options: &InlineOptions,
) -> u16 {
    let max_cols = width.max(1) as usize;
    let mut seen = 0usize;
    let mut rows = 0u16;
    'outer: for line in spec.content.lines() {
        let spans = inline_markdown_spans(line, spec.dim, spec.italic, inline_options);
        let wrapped = wrap_inline_spans(&spans, max_cols);
        for segment in wrapped_segments(out, &wrapped) {
            if seen < row_start {
                seen = seen.saturating_add(1);
                continue;
            }
            if usize::from(rows) >= row_count {
                break 'outer;
            }
            segment.emit(out, |out, row_spans, _| {
                if let Some(gutter) = gutter {
                    if gutter.styled {
                        out.save_style();
                        if spec.dim {
                            out.set_dim();
                        }
                        if spec.italic {
                            out.set_italic();
                        }
                        out.print_gutter(&gutter.text);
                        out.pop_style();
                    } else {
                        out.print_gutter(&gutter.text);
                    }
                }
                emit_inline_spans(out, row_spans);
            });
            out.newline();
            rows = rows.saturating_add(1);
            seen = seen.saturating_add(1);
        }
    }
    rows
}

fn measure_inline_markdown_spec(
    spec: &MarkdownSpec,
    width: u16,
    inline_options: &InlineOptions,
) -> usize {
    let max_cols = width.max(1) as usize;
    spec.content
        .lines()
        .map(|line| {
            let spans = inline_markdown_spans(line, spec.dim, spec.italic, inline_options);
            wrap_inline_spans(&spans, max_cols).len()
        })
        .sum()
}

fn inline_markdown_spans(
    line: &str,
    dim: bool,
    italic: bool,
    inline_options: &InlineOptions,
) -> Vec<InlineSpan> {
    let mut spans = parse_inline_spans_with_options(line, dim, inline_options);
    if italic {
        for span in &mut spans {
            span.style.italic = true;
        }
    }
    spans
}

fn render_code_spec(
    out: &mut LineBuilder,
    spec: &CodeSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
) -> u16 {
    let block = code_block_from_spec(spec);
    let total = measure_code_block(&block, width as usize);
    if row_start == 0 && row_count >= total && gutter.is_none() {
        return render_code_block(out, &block, width as usize, false, None, false);
    }

    render_via_temp(out, width, row_start, row_count, gutter, None, |col| {
        render_code_block(col, &block, width as usize, false, None, false)
    })
}

fn measure_code_spec(spec: &CodeSpec, width: u16) -> usize {
    measure_code_block(&code_block_from_spec(spec), width as usize)
}

fn code_block_from_spec(spec: &CodeSpec) -> smelt_core::content::code_block::CodeBlock {
    let lines: Vec<&str> = if spec.content.is_empty() {
        vec![""]
    } else {
        spec.content.lines().collect()
    };
    parse_code_block(&lines, &spec.lang)
}

fn render_separator_spec(
    out: &mut LineBuilder,
    spec: &SeparatorSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
) -> u16 {
    if row_start > 0 || row_count == 0 {
        return 0;
    }
    if let Some(gutter) = gutter {
        out.print_gutter(&gutter.text);
    }
    if spec.dim {
        out.push_dim();
    }
    let label_width = styled_spans_width(&spec.label);
    let remaining = (width as usize).saturating_sub(label_width);
    let left = remaining / 2;
    let right = remaining - left;
    let fill_meta = SpanMeta {
        selectable: spec.selectable,
        copy_as: None,
        action: None,
    };
    out.print_with_meta(&"─".repeat(left), fill_meta.clone());
    print_styled_spans(out, &spec.label, None, None);
    out.print_with_meta(&"─".repeat(right), fill_meta);
    if spec.dim {
        out.pop_style();
    }
    out.newline();
    1
}

fn measure_separator_spec(_spec: &SeparatorSpec) -> usize {
    1
}

fn render_via_temp(
    out: &mut LineBuilder,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
    styled_gutter: Option<(bool, bool)>,
    render: impl FnOnce(&mut LineBuilder) -> u16,
) -> u16 {
    let inherited_style = out.current_style();
    let mut buf = smelt_core::buffer::Buffer::new(
        smelt_core::buffer::BufId(0),
        smelt_core::buffer::BufCreateOpts::default(),
    );
    let outcome = {
        let mut col = LineBuilder::new(&mut buf, out.theme(), width.max(1));
        col.push(None, inherited_style);
        render(&mut col);
        col.finish()
    };
    if outcome.was_wrapped {
        out.mark_wrapped();
    }
    let total = outcome.line_count;
    let end = row_start.saturating_add(row_count).min(total);
    let mut rows = 0u16;
    for row in row_start..end {
        let Ok(buffer_row) = u16::try_from(row) else {
            break;
        };
        if let Some(gutter) = gutter {
            print_row_gutter(out, gutter);
        }
        apply_temp_decoration(out, &buf, row, true);
        emit_buffer_row_clipped(&buf, buffer_row, width, out, styled_gutter);
        out.newline();
        rows = rows.saturating_add(1);
    }
    rows
}

fn print_row_gutter(out: &mut LineBuilder, gutter: &GutterSpec) {
    if gutter.styled {
        out.print_gutter(&gutter.text);
    } else {
        out.save_style();
        out.reset_style();
        out.print_gutter(&gutter.text);
        out.pop_style();
    }
}

fn print_row_prefix(out: &mut LineBuilder, prefix: &StyledLine, max_cols: u16, row_cols: u16) {
    let start_width = out.current_line_width();
    let prefix_limit = start_width.saturating_add(max_cols);
    let row_limit = start_width.saturating_add(row_cols);
    let mut at_append_boundary = true;

    for index in 0..prefix.inline.runs.len() {
        let Some((style, run)) = prefix.run(index) else {
            continue;
        };
        let mut offset = 0usize;
        if at_append_boundary && !run.text.is_empty() {
            let boundary_len = out.boundary_grapheme_prefix_len(&run.text);
            if boundary_len > 0 {
                let boundary = smelt_buffer::text::slice(&run.text, 0..boundary_len);
                if out.fitting_prefix_len(boundary, row_limit) < boundary_len {
                    break;
                }
                print_styled_text_range(out, style, &run.text, None, 0..boundary_len, None);
                offset = boundary_len;
            }
            at_append_boundary = false;
        }

        let remaining = smelt_buffer::text::slice(&run.text, offset..run.text.len());
        let keep = out.fitting_prefix_len(remaining, prefix_limit);
        print_styled_text_range(out, style, &run.text, None, offset..offset + keep, None);
        if keep < remaining.len() {
            break;
        }
    }
}

fn row_prefix_child_widths(spec: &RowPrefixSpec, width: u16) -> (u16, u16) {
    let first = styled_spans_width(&spec.first).min(u16::MAX as usize) as u16;
    let rest = styled_spans_width(&spec.rest).min(u16::MAX as usize) as u16;
    (
        child_width_after_gutter(width, first),
        child_width_after_gutter(width, rest),
    )
}

struct RowPrefixRunLine {
    first_prefix: StyledLine,
    first_prefix_cols: u16,
    rest_prefix_cols: u16,
    child: StyledLine,
    wrapped: Vec<Vec<WrappedRun<usize>>>,
}

fn compose_styled_lines(
    prefix: &[protocol::StyledSpan],
    child: &[protocol::StyledSpan],
) -> (StyledLine, StyledLine, usize) {
    let (prefix_styles, prefix_runs) = styled_line_parts(prefix);
    let (child_styles, child_runs) = styled_line_parts(child);
    let mut runs = InlineLine::new(prefix_runs).runs;
    let prefix_style_count = prefix_styles.len();
    let child_bytes = child_runs.iter().map(|run| run.text.len()).sum::<usize>();
    runs.extend(InlineLine::new(child_runs).runs.into_iter().map(|mut run| {
        run.meta = run.meta.saturating_add(prefix_style_count);
        run
    }));

    let mut runs = InlineLine::new(runs).runs;
    let child_start = runs.partition_point(|run| run.meta < prefix_style_count);
    let mut child_runs = runs.split_off(child_start);
    for run in &mut child_runs {
        run.meta = run.meta.saturating_sub(prefix_style_count);
    }
    let retained_child_bytes = child_runs.iter().map(|run| run.text.len()).sum::<usize>();
    (
        StyledLine::from_parts(prefix_styles, runs),
        StyledLine::from_parts(child_styles, child_runs),
        child_bytes.saturating_sub(retained_child_bytes),
    )
}

fn row_prefix_run_line(
    row_prefix: &[protocol::StyledSpan],
    rest_prefix: &StyledLine,
    source_spans: &[protocol::StyledSpan],
    width: u16,
) -> RowPrefixRunLine {
    let total_cells = width.max(1);
    let (first_prefix, child, moved) = compose_styled_lines(row_prefix, source_spans);
    let reserved_prefix_cells = if child.inline.is_empty() {
        total_cells
    } else {
        total_cells.saturating_sub(1)
    };
    let first_prefix_cols = if moved > 0 {
        total_cells
    } else {
        reserved_prefix_cells
    };
    let first_occupied = first_prefix
        .inline
        .measure_unwrapped()
        .min(first_prefix_cols as usize);
    let rest_occupied = rest_prefix
        .inline
        .measure_unwrapped()
        .min(reserved_prefix_cells as usize);
    let wrapped = child.inline.wrap_fragments_with_occupied_widths(
        total_cells as usize,
        first_occupied,
        total_cells as usize,
        rest_occupied,
    );
    RowPrefixRunLine {
        first_prefix,
        first_prefix_cols,
        rest_prefix_cols: reserved_prefix_cells,
        child,
        wrapped,
    }
}

fn measure_row_prefix_runs(spec: &RunsSpec, prefix: &RowPrefixSpec, width: u16) -> usize {
    let rest_prefix = StyledLine::new(&prefix.rest);
    let mut rows = 0usize;
    for spans in &spec.lines.0 {
        let row_prefix = if rows == 0 {
            &prefix.first
        } else {
            &prefix.rest
        };
        rows = rows.saturating_add(
            row_prefix_run_line(row_prefix, &rest_prefix, spans, width)
                .wrapped
                .len(),
        );
    }
    rows
}

fn measure_row_prefix_special(
    child: &LayoutIr,
    prefix: &RowPrefixSpec,
    width: u16,
    _inline_options: &InlineOptions,
) -> Option<usize> {
    match child {
        BlockLayout::Leaf(IrLeaf::Runs(spec)) => Some(measure_row_prefix_runs(spec, prefix, width)),
        BlockLayout::Cap { child, spec } => {
            let BlockLayout::Leaf(IrLeaf::Runs(runs)) = child.as_ref() else {
                return None;
            };
            if !layout_fits_exact_measure_budget(child, &mut ExactLayoutBudget::default()) {
                return None;
            }
            let child_rows = measure_row_prefix_runs(runs, prefix, width);
            Some(cap_rows(child_rows, spec).row_count())
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_row_prefix_runs(
    out: &mut LineBuilder,
    spec: &RunsSpec,
    prefix: &RowPrefixSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
) -> u16 {
    if row_count == 0 {
        return 0;
    }
    let default_hl = spec.hl_group.as_deref();
    let rest_prefix = StyledLine::new(&prefix.rest);
    let row_end = row_start.saturating_add(row_count);
    let mut source_row = 0usize;
    let mut written = 0u16;
    'lines: for (line_index, spans) in spec.lines.0.iter().enumerate() {
        let row_prefix = if source_row == 0 {
            &prefix.first
        } else {
            &prefix.rest
        };
        let line = row_prefix_run_line(row_prefix, &rest_prefix, spans, width);
        for (segment_index, fragments) in line.wrapped.iter().enumerate() {
            if source_row >= row_end {
                break 'lines;
            }
            if source_row >= row_start {
                if let Some(gutter) = gutter {
                    print_row_gutter(out, gutter);
                }
                let max_line_cols = out.current_line_width().saturating_add(width);
                let (row_prefix, prefix_cols) = if segment_index == 0 {
                    (&line.first_prefix, line.first_prefix_cols)
                } else {
                    (&rest_prefix, line.rest_prefix_cols)
                };
                print_row_prefix(out, row_prefix, prefix_cols, width);
                WrappedSegmentKind::from_index(segment_index).apply(out);
                for fragment in fragments {
                    let Some((style, run)) = line.child.run(fragment.run_index) else {
                        continue;
                    };
                    if !print_styled_text_range_clipped(
                        out,
                        style,
                        &run.text,
                        default_hl,
                        fragment.range.clone(),
                        spec.syntax_highlights.spans(line_index, run.meta),
                        max_line_cols,
                    ) {
                        break;
                    }
                }
                out.newline();
                written = written.saturating_add(1);
            }
            source_row = source_row.saturating_add(1);
        }
    }
    written
}

#[allow(clippy::too_many_arguments)]
fn render_row_prefix_runs_cap(
    out: &mut LineBuilder,
    runs: &RunsSpec,
    cap: &smelt_core::content::block_layout::CapSpec,
    prefix: &RowPrefixSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
) -> u16 {
    let child_rows = measure_row_prefix_runs(runs, prefix, width);
    let (first_width, rest_width) = row_prefix_child_widths(prefix, width);
    let rows = cap_rows(child_rows, cap);
    let mut written = 0u16;
    let mut selected_row = 0usize;
    while selected_row < row_count {
        let Some(output_row) = row_start.checked_add(selected_row) else {
            break;
        };
        let Some(row) = rows.row_at(output_row) else {
            break;
        };
        match row {
            CapRow::Child(child_row) => {
                let mut child_count = 1usize;
                while child_count < row_count.saturating_sub(selected_row) {
                    let Some(next_output) = output_row.checked_add(child_count) else {
                        break;
                    };
                    let expected_child = child_row.saturating_add(child_count);
                    if !matches!(rows.row_at(next_output), Some(CapRow::Child(row)) if row == expected_child)
                    {
                        break;
                    }
                    child_count = child_count.saturating_add(1);
                }
                written = written.saturating_add(render_row_prefix_runs(
                    out,
                    runs,
                    prefix,
                    width,
                    child_row,
                    child_count,
                    gutter,
                ));
                selected_row = selected_row.saturating_add(child_count);
                continue;
            }
            CapRow::Marker {
                skipped,
                kept,
                total,
                direction,
            } => {
                if let Some(gutter) = gutter {
                    print_row_gutter(out, gutter);
                }
                let row_limit = out.current_line_width().saturating_add(width);
                let (row_prefix, child_width) = if output_row == 0 {
                    (&prefix.first, first_width)
                } else {
                    (&prefix.rest, rest_width)
                };
                let row_prefix = StyledLine::new(row_prefix);
                print_row_prefix(out, &row_prefix, width.saturating_sub(child_width), width);
                render_cap_marker_text(
                    out,
                    skipped,
                    kept,
                    total,
                    direction,
                    row_limit.saturating_sub(out.current_line_width()),
                );
                written = written.saturating_add(1);
            }
        }
        selected_row = selected_row.saturating_add(1);
    }
    written
}

fn render_cap_marker_text(
    out: &mut LineBuilder,
    skipped: usize,
    kept: usize,
    total: Option<u64>,
    direction: &str,
    max_cols: u16,
) {
    out.push_dim();
    let text = cap_marker_text(skipped, kept, total, direction);
    let max_line_cols = out.current_line_width().saturating_add(max_cols);
    let keep = out.fitting_prefix_len(&text, max_line_cols);
    out.print(smelt_buffer::text::slice(&text, 0..keep));
    out.pop_style();
    out.newline();
}

// Row prefixes are applied after the child has chosen and capped its rows, so
// the renderer first materializes only the requested child rows into a scratch
// buffer, then replays those rows with chrome attached. The replay preserves row
// decorations and copy/source metadata via `apply_temp_decoration`.
#[allow(clippy::too_many_arguments)]
fn render_ir_row_prefix(
    out: &mut LineBuilder,
    child: &LayoutIr,
    spec: &RowPrefixSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
    child_measurement: RenderMeasurement<'_>,
) -> u16 {
    if let BlockLayout::Leaf(IrLeaf::Runs(runs)) = child {
        return render_row_prefix_runs(out, runs, spec, width, row_start, row_count, gutter);
    }
    if let BlockLayout::Cap {
        child: cap_child,
        spec: cap_spec,
    } = child
    {
        if let BlockLayout::Leaf(IrLeaf::Runs(runs)) = cap_child.as_ref() {
            if layout_fits_exact_measure_budget(cap_child, &mut ExactLayoutBudget::default()) {
                return render_row_prefix_runs_cap(
                    out, runs, cap_spec, spec, width, row_start, row_count, gutter,
                );
            }
        }
    }

    let prefix_width = row_prefix_width(spec);
    let child_width = child_width_after_gutter(width, prefix_width);
    let inherited_style = out.current_style();
    let mut buf = smelt_core::buffer::Buffer::new(
        smelt_core::buffer::BufId(0),
        smelt_core::buffer::BufCreateOpts::default(),
    );
    let outcome = {
        let mut col = LineBuilder::new(&mut buf, out.theme(), child_width);
        col.push(None, inherited_style);
        render_layout_ir_range_measured(
            &mut col,
            child,
            child_width,
            row_start,
            row_count,
            None,
            history,
            inline_options,
            child_measurement,
        );
        col.finish()
    };
    if outcome.was_wrapped {
        out.mark_wrapped();
    }

    let total = outcome.line_count;
    let first_prefix = StyledLine::new(&spec.first);
    let rest_prefix = StyledLine::new(&spec.rest);
    let mut rows = 0u16;
    for row in 0..total {
        let Ok(buffer_row) = u16::try_from(row) else {
            break;
        };
        if let Some(gutter) = gutter {
            print_row_gutter(out, gutter);
        }
        let row_limit = out.current_line_width().saturating_add(width);
        let source_row = row_start.saturating_add(row);
        let prefix = if source_row == 0 {
            &first_prefix
        } else {
            &rest_prefix
        };
        print_row_prefix(out, prefix, width.saturating_sub(child_width), width);
        apply_temp_decoration(out, &buf, row, true);
        emit_buffer_row_clipped(
            &buf,
            buffer_row,
            row_limit.saturating_sub(out.current_line_width()),
            out,
            None,
        );
        out.newline();
        rows = rows.saturating_add(1);
    }
    rows
}

fn styled_spans_width(spans: &[protocol::StyledSpan]) -> usize {
    smelt_buffer::cell_width::joined_text_width(spans.iter().map(|span| span.text.as_str()))
}

fn print_styled_spans(
    out: &mut LineBuilder,
    spans: &[protocol::StyledSpan],
    default_hl: Option<&str>,
    syntax_highlights: Option<&RetainedInlineSyntax>,
) {
    let line = StyledLine::new(spans);
    for index in 0..line.inline.runs.len() {
        let Some((style, run)) = line.run(index) else {
            continue;
        };
        print_styled_text_range(
            out,
            style,
            &run.text,
            default_hl,
            0..run.text.len(),
            syntax_highlights.and_then(|highlights| highlights.spans(0, run.meta)),
        );
    }
}

fn print_styled_text_range_clipped(
    out: &mut LineBuilder,
    style: &protocol::StyledSpan,
    text: &str,
    default_hl: Option<&str>,
    range: std::ops::Range<usize>,
    syntax_highlights: Option<&[InlineSyntaxSpan]>,
    max_line_cols: u16,
) -> bool {
    let start = smelt_buffer::text::snap(text, range.start);
    let piece = smelt_buffer::text::slice(text, start..range.end);
    let keep = out.fitting_prefix_len(piece, max_line_cols);
    print_styled_text_range(
        out,
        style,
        text,
        default_hl,
        start..start + keep,
        syntax_highlights,
    );
    keep == piece.len()
}

fn print_styled_text_range(
    out: &mut LineBuilder,
    span: &protocol::StyledSpan,
    text: &str,
    default_hl: Option<&str>,
    range: std::ops::Range<usize>,
    syntax_highlights: Option<&[InlineSyntaxSpan]>,
) {
    let piece = smelt_buffer::text::slice(text, range.clone());
    if piece.is_empty() {
        return;
    }
    let fg_color = span.fg.as_deref().and_then(|name| out.theme().get(name).fg);
    let bg_color = span.bg.as_deref().and_then(|name| out.theme().get(name).bg);
    out.save_style();
    if let Some(group) = span.hl.as_deref().or(default_hl) {
        out.set_hl(intern(group));
    }
    if let Some(c) = fg_color {
        out.set_fg(c);
    }
    if let Some(c) = bg_color {
        out.set_bg(c);
    }
    if span.dim {
        out.set_dim();
    }
    if span.bold {
        out.set_bold();
    }
    if span.italic {
        out.set_italic();
    }
    match span.syntax.as_deref() {
        Some(_) if span.selectable && syntax_highlights.is_some() => {
            print_retained_inline_syntax(
                out,
                text,
                range,
                syntax_highlights.expect("syntax highlights checked above"),
            );
        }
        Some(lang) if span.selectable => {
            let _perf = smelt_perf::perf::begin("render:layout:inline_syntax");
            let mut highlighter = InlineSyntax::new(lang, out.theme());
            highlighter.print_line_range(out, text, range);
        }
        _ if span.selectable => out.print(piece),
        _ => out.print_with_meta(piece, SpanMeta::unselectable()),
    }
    out.pop_style();
}

fn print_retained_inline_syntax(
    out: &mut LineBuilder<'_>,
    source: &str,
    range: std::ops::Range<usize>,
    highlights: &[InlineSyntaxSpan],
) {
    let mut cursor = range.start;
    for highlight in highlights {
        let byte_start = highlight.byte_start.max(range.start);
        let byte_end = highlight.byte_end.min(range.end);
        if byte_start >= byte_end {
            continue;
        }
        if cursor < byte_start {
            out.print(smelt_buffer::text::slice(source, cursor..byte_start));
        }
        let [r, g, b] = highlight.foreground;
        out.save_style();
        out.set_fg(smelt_core::style::Color::Rgb { r, g, b });
        out.print(smelt_buffer::text::slice(source, byte_start..byte_end));
        out.pop_style();
        cursor = byte_end;
    }
    if cursor < range.end {
        out.print(smelt_buffer::text::slice(source, cursor..range.end));
    }
}

mod chrome;
use chrome::{
    apply_style_spec, measure_ir_panel, panel_child_width, render_ir_panel, render_ir_style,
};

const MAX_BOUNDED_MARKDOWN_PARSE_BYTES: usize = 64 * 1024;
const BOUNDED_MARKDOWN_OMITTED: &str = "[large Markdown content omitted]";

#[derive(Clone, Copy, PartialEq, Eq)]
enum RetainedMarkdownEdgeMode {
    Parsed,
    Literal,
    Omitted,
}

fn retained_markdown_edge_mode(
    content: &RetainedContentSpec,
    inline: bool,
) -> RetainedMarkdownEdgeMode {
    let read = content.content.read();
    if read.is_empty() {
        return RetainedMarkdownEdgeMode::Parsed;
    }
    if inline {
        return if read.len() <= MAX_BOUNDED_MARKDOWN_PARSE_BYTES {
            RetainedMarkdownEdgeMode::Parsed
        } else if read.large_lines_have_ascii_cells() {
            RetainedMarkdownEdgeMode::Literal
        } else {
            RetainedMarkdownEdgeMode::Omitted
        };
    }
    if read.logical_line_count() == 1 && read.line_is_plain_markdown(0) {
        let bounded = read
            .line_range(0)
            .is_none_or(|range| range.len() <= MAX_BOUNDED_MARKDOWN_PARSE_BYTES)
            || read.line_has_ascii_cells(0);
        return if bounded {
            RetainedMarkdownEdgeMode::Literal
        } else {
            RetainedMarkdownEdgeMode::Omitted
        };
    }
    if !read.markdown_has_range_larger_than(MAX_BOUNDED_MARKDOWN_PARSE_BYTES) {
        RetainedMarkdownEdgeMode::Parsed
    } else if read.large_lines_have_ascii_cells() {
        RetainedMarkdownEdgeMode::Literal
    } else {
        RetainedMarkdownEdgeMode::Omitted
    }
}

fn measure_retained_markdown_edge(
    content: &RetainedContentSpec,
    dim: bool,
    italic: bool,
    inline: bool,
    width: u16,
    inline_options: &InlineOptions,
    max_rows: usize,
) -> smelt_core::transcript_content::ContentTextWindow {
    match retained_markdown_edge_mode(content, inline) {
        RetainedMarkdownEdgeMode::Literal => {
            return content
                .content
                .visit_text_layout_head_rows(width, false, max_rows, |_| {});
        }
        RetainedMarkdownEdgeMode::Omitted => {
            return smelt_core::transcript_content::ContentTextWindow {
                row_count: max_rows.min(1),
                truncated: max_rows == 0,
            };
        }
        RetainedMarkdownEdgeMode::Parsed => {}
    }

    let total = if inline {
        measure_retained_inline_markdown_spec(content, dim, italic, width, inline_options)
    } else {
        return measure_retained_markdown_inner_edge_with_options(
            &content.content,
            width as usize,
            "",
            dim,
            None,
            inline_options,
            max_rows,
        );
    };
    smelt_core::transcript_content::ContentTextWindow {
        row_count: total.min(max_rows),
        truncated: total > max_rows,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_retained_markdown_edge(
    out: &mut LineBuilder,
    content: &RetainedContentSpec,
    dim: bool,
    italic: bool,
    inline: bool,
    width: u16,
    inline_options: &InlineOptions,
    max_rows: usize,
    tail: bool,
) -> smelt_core::transcript_content::ContentTextWindow {
    let mode = retained_markdown_edge_mode(content, inline);
    let window = measure_retained_markdown_edge(
        content,
        dim,
        italic,
        inline,
        width,
        inline_options,
        max_rows,
    );
    match mode {
        RetainedMarkdownEdgeMode::Literal => {
            if tail {
                content.content.visit_text_layout_tail_rows(
                    width,
                    false,
                    window.row_count,
                    |row| render_retained_plain_markdown_row(out, row, dim, italic, None),
                );
            } else {
                content.content.visit_text_layout_head_rows(
                    width,
                    false,
                    window.row_count,
                    |row| render_retained_plain_markdown_row(out, row, dim, italic, None),
                );
            }
            return window;
        }
        RetainedMarkdownEdgeMode::Omitted => {
            if window.row_count > 0 {
                out.save_style();
                if dim {
                    out.set_dim();
                }
                if italic {
                    out.set_italic();
                }
                out.print_with_meta(BOUNDED_MARKDOWN_OMITTED, SpanMeta::unselectable());
                out.pop_style();
                out.newline();
            }
            return window;
        }
        RetainedMarkdownEdgeMode::Parsed => {}
    }

    if inline {
        let total =
            measure_retained_inline_markdown_spec(content, dim, italic, width, inline_options);
        let row_start = if tail {
            total.saturating_sub(window.row_count)
        } else {
            0
        };
        render_retained_inline_markdown_spec(
            out,
            content,
            dim,
            italic,
            width,
            row_start,
            window.row_count,
            None,
            inline_options,
        );
        return window;
    }

    if italic {
        out.save_style();
        out.set_italic();
    }
    render_retained_markdown_inner_edge_with_options(
        out,
        &content.content,
        width as usize,
        "",
        dim,
        None,
        inline_options,
        max_rows,
        tail,
    );
    if italic {
        out.pop_style();
    }
    window
}

const MAX_BOUNDED_EDGE_EXACT_BYTES: usize = 64 * 1024;
const MAX_BOUNDED_EDGE_EXACT_NODES: usize = 4 * 1024;
const MAX_BOUNDED_EDGE_EXACT_SPANS: usize = 4 * 1024;
const MAX_BOUNDED_EDGE_DEPTH: usize = 32;
const BOUNDED_TRANSIENT_OMITTED: &str = "[large transient layout omitted]";

fn measure_retained_text_edge(
    content: &smelt_core::transcript_content::TranscriptContent,
    width: u16,
    ansi: bool,
    max_rows: usize,
) -> smelt_core::transcript_content::ContentTextWindow {
    let logical_lines = content.read().logical_line_count();
    if max_rows != 0 && logical_lines > max_rows {
        return smelt_core::transcript_content::ContentTextWindow {
            row_count: max_rows,
            truncated: true,
        };
    }
    content.visit_text_layout_head_rows(width, ansi, max_rows, |_| {})
}

fn measure_retained_content_edge(
    content: &RetainedContentSpec,
    width: u16,
    max_rows: usize,
    inline_options: &InlineOptions,
) -> smelt_core::transcript_content::ContentTextWindow {
    match &content.render {
        ContentRenderSpec::Text { ansi, .. } => {
            measure_retained_text_edge(&content.content, width, *ansi, max_rows)
        }
        ContentRenderSpec::Markdown {
            dim,
            italic,
            inline,
        } => measure_retained_markdown_edge(
            content,
            *dim,
            *italic,
            *inline,
            width,
            inline_options,
            max_rows,
        ),
        ContentRenderSpec::Code { .. } => {
            smelt_core::content::highlight::measure_retained_code_block_edge(
                &content.content,
                width,
                max_rows,
            )
        }
        ContentRenderSpec::File { .. } => {
            smelt_core::content::highlight::measure_retained_file_view_edge(
                &content.content,
                width,
                smelt_core::content::highlight::GutterStyle::InlineLineNumbers,
                0,
                max_rows,
            )
        }
    }
}

#[derive(Default)]
struct ExactLayoutBudget {
    bytes: usize,
    nodes: usize,
    spans: usize,
}

impl ExactLayoutBudget {
    fn add_node(&mut self) -> bool {
        self.nodes = self.nodes.saturating_add(1);
        self.nodes <= MAX_BOUNDED_EDGE_EXACT_NODES
    }

    fn add_bytes(&mut self, bytes: usize) -> bool {
        self.bytes = self.bytes.saturating_add(bytes);
        self.bytes <= MAX_BOUNDED_EDGE_EXACT_BYTES
    }

    fn add_spans<'a>(&mut self, spans: impl Iterator<Item = &'a protocol::StyledSpan>) -> bool {
        for span in spans {
            self.spans = self.spans.saturating_add(1);
            if self.spans > MAX_BOUNDED_EDGE_EXACT_SPANS || !self.add_bytes(span.text.len()) {
                return false;
            }
        }
        true
    }
}

fn include_leaf_in_exact_measure_budget(leaf: &IrLeaf, budget: &mut ExactLayoutBudget) -> bool {
    match leaf {
        IrLeaf::Text(spec) => budget.add_bytes(spec.content.len()),
        IrLeaf::Runs(spec) => budget.add_spans(spec.lines.0.iter().flatten()),
        IrLeaf::Line(spec) => budget.add_spans(spec.spans.iter()),
        IrLeaf::Markdown(spec) => budget.add_bytes(spec.content.len()),
        IrLeaf::Code(spec) => budget.add_bytes(spec.content.len()),
        IrLeaf::Separator(spec) => budget.add_spans(spec.label.iter()),
        IrLeaf::Content(spec) => match &spec.render {
            ContentRenderSpec::Markdown { .. } => budget.add_bytes(spec.content.len()),
            _ => false,
        },
        IrLeaf::SourceView(_) => false,
    }
}

fn layout_fits_exact_measure_budget(layout: &LayoutIr, budget: &mut ExactLayoutBudget) -> bool {
    layout_fits_exact_measure_budget_inner(layout, budget, 0)
}

fn layout_fits_exact_measure_budget_inner(
    layout: &LayoutIr,
    budget: &mut ExactLayoutBudget,
    depth: usize,
) -> bool {
    if depth > MAX_BOUNDED_EDGE_DEPTH || !budget.add_node() {
        return false;
    }
    match layout {
        BlockLayout::Empty => true,
        BlockLayout::Leaf(leaf) => include_leaf_in_exact_measure_budget(leaf, budget),
        BlockLayout::Vbox(items) => items.iter().all(|child| {
            layout_fits_exact_measure_budget_inner(child, budget, depth.saturating_add(1))
        }),
        BlockLayout::Hbox(items) => items.iter().all(|item| {
            layout_fits_exact_measure_budget_inner(&item.layout, budget, depth.saturating_add(1))
        }),
        BlockLayout::Gutter { child, .. }
        | BlockLayout::RowPrefix { child, .. }
        | BlockLayout::Panel { child, .. }
        | BlockLayout::Style { child, .. }
        | BlockLayout::Cap { child, .. }
        | BlockLayout::Refresh { child, .. } => {
            layout_fits_exact_measure_budget_inner(child, budget, depth.saturating_add(1))
        }
    }
}

fn leaf_fits_exact_measure_budget(leaf: &IrLeaf) -> bool {
    include_leaf_in_exact_measure_budget(leaf, &mut ExactLayoutBudget::default())
}

fn bounded_exact_layout_rows(
    layout: &LayoutIr,
    width: u16,
    inline_options: &InlineOptions,
) -> Option<usize> {
    layout_fits_exact_measure_budget(layout, &mut ExactLayoutBudget::default())
        .then(|| measure_layout_ir_full(layout, width, inline_options))
}

fn measure_bounded_layout_edge_inner(
    layout: &LayoutIr,
    width: u16,
    max_rows: usize,
    inline_options: &InlineOptions,
    depth: usize,
) -> Option<smelt_core::transcript_content::ContentTextWindow> {
    use smelt_core::transcript_content::ContentTextWindow;

    if depth > MAX_BOUNDED_EDGE_DEPTH {
        return Some(ContentTextWindow {
            row_count: max_rows.min(1),
            truncated: true,
        });
    }

    match layout {
        BlockLayout::Empty => Some(ContentTextWindow {
            row_count: 0,
            truncated: false,
        }),
        BlockLayout::Leaf(IrLeaf::Content(content)) => Some(measure_retained_content_edge(
            content,
            width,
            max_rows,
            inline_options,
        )),
        BlockLayout::Leaf(leaf) => {
            if !leaf_fits_exact_measure_budget(leaf) {
                return Some(ContentTextWindow {
                    row_count: max_rows.min(1),
                    truncated: true,
                });
            }
            let total = measure_ir_leaf(leaf, width, 0, inline_options);
            Some(ContentTextWindow {
                row_count: total.min(max_rows),
                truncated: total > max_rows,
            })
        }
        BlockLayout::Vbox(items) => {
            let mut row_count = 0usize;
            for child in items {
                let remaining = max_rows.saturating_sub(row_count);
                if remaining == 0 {
                    return Some(ContentTextWindow {
                        row_count,
                        truncated: true,
                    });
                }
                let child = measure_bounded_layout_edge_inner(
                    child,
                    width,
                    remaining,
                    inline_options,
                    depth.saturating_add(1),
                )?;
                row_count = row_count.saturating_add(child.row_count);
                if child.truncated {
                    return Some(ContentTextWindow {
                        row_count,
                        truncated: true,
                    });
                }
            }
            Some(ContentTextWindow {
                row_count,
                truncated: false,
            })
        }
        BlockLayout::Gutter { child, spec } => {
            let gutter_width = display_width_u16(&spec.text);
            measure_bounded_layout_edge_inner(
                child,
                child_width_after_gutter(width, gutter_width),
                max_rows,
                inline_options,
                depth.saturating_add(1),
            )
        }
        BlockLayout::RowPrefix { child, spec } => measure_bounded_layout_edge_inner(
            child,
            child_width_after_gutter(width, row_prefix_width(spec)),
            max_rows,
            inline_options,
            depth.saturating_add(1),
        ),
        BlockLayout::Panel { child, spec } => {
            let padding = usize::from(spec.padding);
            let top_rows = padding.min(max_rows);
            if top_rows == max_rows {
                return Some(ContentTextWindow {
                    row_count: top_rows,
                    truncated: true,
                });
            }
            let child = measure_bounded_layout_edge_inner(
                child,
                panel_child_width(width, spec.padding),
                max_rows.saturating_sub(top_rows),
                inline_options,
                depth.saturating_add(1),
            )?;
            let row_count = top_rows.saturating_add(child.row_count);
            if child.truncated {
                return Some(ContentTextWindow {
                    row_count,
                    truncated: true,
                });
            }
            let total = row_count.saturating_add(padding);
            Some(ContentTextWindow {
                row_count: total.min(max_rows),
                truncated: total > max_rows,
            })
        }
        BlockLayout::Style { child, .. } | BlockLayout::Refresh { child, .. } => {
            measure_bounded_layout_edge_inner(
                child,
                width,
                max_rows,
                inline_options,
                depth.saturating_add(1),
            )
        }
        BlockLayout::Cap { child, spec } => {
            let rows = measure_bounded_cap_rows_inner(
                child,
                spec,
                width,
                inline_options,
                depth.saturating_add(1),
            )?;
            Some(ContentTextWindow {
                row_count: rows.min(max_rows),
                truncated: rows > max_rows,
            })
        }
        BlockLayout::Hbox(items) => {
            let widths = solve_ir_hbox_widths(items, width);
            let mut row_count = 0usize;
            let mut truncated = false;
            for (item, child_width) in items.iter().zip(widths) {
                if child_width == 0 {
                    continue;
                }
                let child = measure_bounded_layout_edge_inner(
                    &item.layout,
                    child_width,
                    max_rows,
                    inline_options,
                    depth.saturating_add(1),
                )?;
                row_count = row_count.max(child.row_count);
                truncated |= child.truncated;
            }
            Some(ContentTextWindow {
                row_count,
                truncated,
            })
        }
    }
}

fn retained_text_cap_leaf(
    child: &LayoutIr,
) -> Option<(
    &RetainedContentSpec,
    Option<smelt_core::theme::HlGroup>,
    bool,
)> {
    let BlockLayout::Leaf(IrLeaf::Content(content)) = child else {
        return None;
    };
    let ContentRenderSpec::Text { hl_group, ansi } = &content.render else {
        return None;
    };
    Some((content, hl_group.as_deref().map(intern), *ansi))
}

fn measure_bounded_cap_rows(
    child: &LayoutIr,
    spec: &smelt_core::content::block_layout::CapSpec,
    width: u16,
    inline_options: &InlineOptions,
) -> Option<usize> {
    measure_bounded_cap_rows_inner(child, spec, width, inline_options, 0)
}

fn measure_bounded_cap_rows_inner(
    child: &LayoutIr,
    spec: &smelt_core::content::block_layout::CapSpec,
    width: u16,
    inline_options: &InlineOptions,
    depth: usize,
) -> Option<usize> {
    use smelt_core::content::block_layout::CapKeep;

    if depth <= MAX_BOUNDED_EDGE_DEPTH {
        if let Some(child_rows) = bounded_exact_layout_rows(child, width, inline_options) {
            return Some(
                cap_rows_for_child(child_rows, spec, child, width, None, inline_options, None)
                    .row_count(),
            );
        }
    }
    let probe_rows = usize::from(spec.rows).saturating_add(1);
    let window =
        measure_bounded_layout_edge_inner(child, width, probe_rows, inline_options, depth)?;
    let truncated = window.truncated || window.row_count > usize::from(spec.rows);
    let kept = window.row_count.min(usize::from(spec.rows));
    let marker = truncated
        && match spec.keep {
            CapKeep::Head { marker } | CapKeep::Tail { marker } => marker.is_some(),
            CapKeep::HeadTail { marker, .. } => marker,
        };
    Some(kept.saturating_add(usize::from(marker)))
}

mod cap_rows;
use cap_rows::*;

fn cap_rows_for_child(
    child_rows: usize,
    spec: &smelt_core::content::block_layout::CapSpec,
    child: &LayoutIr,
    width: u16,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
    theme: Option<&Theme>,
) -> CapRows {
    let mut rows = cap_rows(child_rows, spec);
    if rows.omitted_range().is_some_and(|(start, end)| {
        !omitted_rows_have_visible_text(child, width, start, end, history, inline_options, theme)
    }) {
        rows.remove_omitted_marker();
    }
    rows
}

fn omitted_rows_have_visible_text(
    child: &LayoutIr,
    width: u16,
    start: usize,
    end: usize,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
    theme: Option<&Theme>,
) -> bool {
    if start >= end {
        return false;
    }
    if let Some(visible) = layout_range_has_visible_text(child, width, start, end, inline_options) {
        return visible;
    }
    let fallback_theme;
    let theme = if let Some(theme) = theme {
        theme
    } else {
        fallback_theme = Theme::default();
        &fallback_theme
    };
    let measured = measure_layout_ir_plan(child, width, inline_options);
    let mut buf = smelt_core::buffer::Buffer::new(smelt_core::buffer::BufId(0), Default::default());
    let mut out = LineBuilder::new(&mut buf, theme, width);
    let rows = render_layout_ir_range_measured(
        &mut out,
        child,
        width,
        start,
        end.saturating_sub(start),
        None,
        history,
        inline_options,
        RenderMeasurement::Measured(&measured),
    );
    out.finish();
    rows > 0 && (0..rows as usize).any(|row| !buf.get_line(row).unwrap_or("").trim().is_empty())
}

fn layout_range_has_visible_text(
    layout: &LayoutIr,
    width: u16,
    start: usize,
    end: usize,
    inline_options: &InlineOptions,
) -> Option<bool> {
    if start >= end {
        return Some(false);
    }
    match layout {
        BlockLayout::Empty => Some(false),
        BlockLayout::Leaf(IrLeaf::Markdown(spec)) if !spec.inline => {
            Some(markdown_range_has_visible_text_with_options(
                &spec.content,
                width as usize,
                "",
                spec.dim,
                None,
                inline_options,
                start,
                end.saturating_sub(start),
            ))
        }
        BlockLayout::Leaf(IrLeaf::Content(spec)) => match &spec.render {
            ContentRenderSpec::Text { ansi, .. } => Some(
                spec.content
                    .text_layout_range_has_visible_text(width, *ansi, start..end),
            ),
            _ => None,
        },
        BlockLayout::Vbox(items) => {
            vbox_range_has_visible_text(items, width, start, end, inline_options)
        }
        BlockLayout::Hbox(items) => {
            let widths = solve_ir_hbox_widths(items, width);
            let mut saw_unknown = false;
            for (item, item_width) in items.iter().zip(widths) {
                if item_width == 0 {
                    continue;
                }
                match layout_range_has_visible_text(
                    &item.layout,
                    item_width,
                    start,
                    end,
                    inline_options,
                ) {
                    Some(true) => return Some(true),
                    Some(false) => {}
                    None => saw_unknown = true,
                }
            }
            (!saw_unknown).then_some(false)
        }
        BlockLayout::Gutter { child, spec } => {
            let child_width = child_width_after_gutter(width, display_width_u16(&spec.text));
            layout_range_has_visible_text(child, child_width, start, end, inline_options)
        }
        BlockLayout::Style { child, .. } => {
            layout_range_has_visible_text(child, width, start, end, inline_options)
        }
        _ => None,
    }
}

fn vbox_range_has_visible_text(
    items: &[LayoutIr],
    width: u16,
    start: usize,
    end: usize,
    inline_options: &InlineOptions,
) -> Option<bool> {
    let mut base = 0usize;
    let mut saw_unknown = false;
    for child in items {
        if base >= end {
            break;
        }
        let rows = measure_layout_ir_full(child, width, inline_options);
        let child_end = base.saturating_add(rows);
        if child_end > start && base < end {
            let local_start = start.saturating_sub(base);
            let local_end = end.saturating_sub(base).min(rows);
            match layout_range_has_visible_text(
                child,
                width,
                local_start,
                local_end,
                inline_options,
            ) {
                Some(true) => return Some(true),
                Some(false) => {}
                None => saw_unknown = true,
            }
        }
        base = child_end;
    }
    (!saw_unknown).then_some(false)
}

struct CapRenderRange {
    start: usize,
    end: usize,
    output_row: usize,
    written: u16,
}

impl CapRenderRange {
    fn includes_current(&self) -> bool {
        self.output_row >= self.start && self.output_row < self.end
    }

    fn advance(&mut self) {
        self.output_row = self.output_row.saturating_add(1);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_retained_text_cap_marker(
    out: &mut LineBuilder,
    state: &mut CapRenderRange,
    spec: &smelt_core::content::block_layout::CapSpec,
    kept: usize,
    direction: &'static str,
    total: Option<u64>,
    gutter: Option<&GutterSpec>,
) {
    if state.includes_current() {
        let estimated_total = total
            .or(spec.total_rows)
            .unwrap_or(kept.saturating_add(1) as u64);
        let skipped = estimated_total
            .saturating_sub(kept as u64)
            .min(usize::MAX as u64) as usize;
        render_cap_marker(out, skipped, kept, total, direction, gutter);
        state.written = state.written.saturating_add(1);
    }
    state.advance();
}

#[allow(clippy::too_many_arguments)]
fn render_retained_text_cap_edge(
    out: &mut LineBuilder,
    state: &mut CapRenderRange,
    content: &RetainedContentSpec,
    hl: Option<smelt_core::theme::HlGroup>,
    ansi: bool,
    width: u16,
    tail: bool,
    count: usize,
    gutter: Option<&GutterSpec>,
) {
    if tail {
        content
            .content
            .visit_text_layout_tail_rows(width, ansi, count, |row| {
                if state.includes_current() {
                    render_content_text_row(out, row, hl, ansi, gutter);
                    state.written = state.written.saturating_add(1);
                }
                state.advance();
            });
    } else {
        content
            .content
            .visit_text_layout_head_rows(width, ansi, count, |row| {
                if state.includes_current() {
                    render_content_text_row(out, row, hl, ansi, gutter);
                    state.written = state.written.saturating_add(1);
                }
                state.advance();
            });
    }
}

#[allow(clippy::too_many_arguments)]
fn render_retained_text_cap(
    out: &mut LineBuilder,
    child: &LayoutIr,
    spec: &smelt_core::content::block_layout::CapSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
) -> Option<u16> {
    use smelt_core::content::block_layout::{CapKeep, CapMarker};

    let (content, hl, ansi) = retained_text_cap_leaf(child)?;
    let probe_rows = usize::from(spec.rows).saturating_add(1);
    let probe = measure_retained_text_edge(&content.content, width, ansi, probe_rows);
    let truncated = probe.truncated || probe.row_count > usize::from(spec.rows);
    let kept = probe.row_count.min(usize::from(spec.rows));
    let mut state = CapRenderRange {
        start: row_start,
        end: row_start.saturating_add(row_count),
        output_row: 0,
        written: 0,
    };

    match spec.keep {
        CapKeep::Head { marker } => {
            if truncated && marker == Some(CapMarker::Above) {
                render_retained_text_cap_marker(out, &mut state, spec, kept, "above", None, gutter);
            }
            render_retained_text_cap_edge(
                out, &mut state, content, hl, ansi, width, false, kept, gutter,
            );
            if truncated && marker == Some(CapMarker::Below) {
                render_retained_text_cap_marker(out, &mut state, spec, kept, "below", None, gutter);
            }
        }
        CapKeep::Tail { marker } => {
            if truncated && marker == Some(CapMarker::Above) {
                render_retained_text_cap_marker(
                    out,
                    &mut state,
                    spec,
                    kept,
                    "above",
                    spec.total_rows.filter(|total| *total > kept as u64),
                    gutter,
                );
            }
            render_retained_text_cap_edge(
                out, &mut state, content, hl, ansi, width, true, kept, gutter,
            );
            if truncated && marker == Some(CapMarker::Below) {
                render_retained_text_cap_marker(out, &mut state, spec, kept, "below", None, gutter);
            }
        }
        CapKeep::HeadTail { head, marker } => {
            if !truncated {
                render_retained_text_cap_edge(
                    out, &mut state, content, hl, ansi, width, false, kept, gutter,
                );
            } else {
                let head_rows = usize::from(head).min(kept);
                let tail_rows = kept.saturating_sub(head_rows);
                render_retained_text_cap_edge(
                    out, &mut state, content, hl, ansi, width, false, head_rows, gutter,
                );
                if marker {
                    render_retained_text_cap_marker(
                        out, &mut state, spec, kept, "omitted", None, gutter,
                    );
                }
                render_retained_text_cap_edge(
                    out, &mut state, content, hl, ansi, width, true, tail_rows, gutter,
                );
            }
        }
    }
    Some(state.written)
}

fn render_retained_cap_marker(
    out: &mut LineBuilder,
    spec: &smelt_core::content::block_layout::CapSpec,
    kept: usize,
    direction: &'static str,
    known_rows: Option<usize>,
    total: Option<u64>,
) {
    let estimated_total = known_rows
        .map(|rows| rows.min(u64::MAX as usize) as u64)
        .or(total)
        .or(spec.total_rows)
        .unwrap_or(kept.saturating_add(1) as u64);
    let skipped = estimated_total
        .saturating_sub(kept as u64)
        .min(usize::MAX as u64) as usize;
    render_cap_marker(out, skipped, kept, total, direction, None);
}

fn render_retained_content_edge(
    out: &mut LineBuilder,
    content: &RetainedContentSpec,
    width: u16,
    count: usize,
    tail: bool,
    inline_options: &InlineOptions,
) {
    match &content.render {
        ContentRenderSpec::Text { hl_group, ansi } => {
            let hl = hl_group.as_deref().map(intern);
            if tail {
                content
                    .content
                    .visit_text_layout_tail_rows(width, *ansi, count, |row| {
                        render_content_text_row(out, row, hl, *ansi, None);
                    });
            } else {
                content
                    .content
                    .visit_text_layout_head_rows(width, *ansi, count, |row| {
                        render_content_text_row(out, row, hl, *ansi, None);
                    });
            }
        }
        ContentRenderSpec::Code { lang, cache } => {
            smelt_core::content::highlight::print_retained_code_block_edge(
                out,
                &content.content,
                cache,
                lang,
                width,
                count,
                tail,
            );
        }
        ContentRenderSpec::File { path, lang, cache } => {
            let syntax_ext = lang
                .as_deref()
                .map(smelt_core::content::highlight::lang_to_ext)
                .or_else(|| {
                    std::path::Path::new(path)
                        .extension()
                        .and_then(|ext| ext.to_str())
                })
                .unwrap_or("txt");
            smelt_core::content::highlight::print_retained_file_view_edge(
                out,
                &content.content,
                cache,
                syntax_ext,
                smelt_core::content::highlight::GutterStyle::InlineLineNumbers,
                0,
                width,
                count,
                tail,
            );
        }
        ContentRenderSpec::Markdown {
            dim,
            italic,
            inline,
        } => {
            render_retained_markdown_edge(
                out,
                content,
                *dim,
                *italic,
                *inline,
                width,
                inline_options,
                count,
                tail,
            );
        }
    }
}

fn replay_bounded_edge_buffer(out: &mut LineBuilder, buf: &Buffer, outcome: Outcome, width: u16) {
    if outcome.was_wrapped {
        out.mark_wrapped();
    }
    for row in 0..outcome.line_count {
        apply_temp_decoration(out, buf, row, true);
        let row = u16::try_from(row).expect("bounded edge exceeds the temporary buffer");
        emit_buffer_row_clipped(buf, row, width, out, None);
        out.newline();
    }
}

#[allow(clippy::too_many_arguments)]
fn render_bounded_layout_edge_to_buffer(
    layout: &LayoutIr,
    theme: &Theme,
    inherited_style: smelt_core::style::Style,
    width: u16,
    max_rows: usize,
    tail: bool,
    inline_options: &InlineOptions,
    depth: usize,
) -> Option<(
    Buffer,
    Outcome,
    smelt_core::transcript_content::ContentTextWindow,
)> {
    let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
    let mut col = LineBuilder::new(&mut buf, theme, width.max(1));
    col.push(None, inherited_style);
    let window = render_bounded_layout_edge_inner(
        &mut col,
        layout,
        width,
        max_rows,
        tail,
        inline_options,
        depth,
    )?;
    let outcome = col.finish();
    Some((buf, outcome, window))
}

fn render_panel_edge_row(
    out: &mut LineBuilder,
    child: Option<(&Buffer, usize)>,
    child_width: u16,
    panel_hl: smelt_core::theme::HlGroup,
    panel_bg: smelt_core::style::Color,
    padding: u16,
) {
    out.set_hl(panel_hl);
    if padding > 0 {
        out.print_with_meta(&" ".repeat(usize::from(padding)), SpanMeta::unselectable());
    }
    if let Some((buf, row)) = child {
        apply_temp_decoration(out, buf, row, false);
        let row = u16::try_from(row).expect("bounded panel edge exceeds the temporary buffer");
        emit_buffer_row_clipped(buf, row, child_width, out, None);
    }
    out.fill_line_bg(panel_bg);
    out.reset_style();
    out.newline();
}

fn render_bounded_layout_edge_inner(
    out: &mut LineBuilder,
    layout: &LayoutIr,
    width: u16,
    max_rows: usize,
    tail: bool,
    inline_options: &InlineOptions,
    depth: usize,
) -> Option<smelt_core::transcript_content::ContentTextWindow> {
    if depth > MAX_BOUNDED_EDGE_DEPTH {
        let window = smelt_core::transcript_content::ContentTextWindow {
            row_count: max_rows.min(1),
            truncated: true,
        };
        if window.row_count > 0 {
            out.print_with_meta(BOUNDED_TRANSIENT_OMITTED, SpanMeta::unselectable());
            out.newline();
        }
        return Some(window);
    }

    let window = measure_bounded_layout_edge_inner(layout, width, max_rows, inline_options, depth)?;
    match layout {
        BlockLayout::Empty => {}
        BlockLayout::Leaf(IrLeaf::Content(content)) => {
            render_retained_content_edge(
                out,
                content,
                width,
                window.row_count,
                tail,
                inline_options,
            );
        }
        BlockLayout::Leaf(leaf) => {
            if !leaf_fits_exact_measure_budget(leaf) {
                if window.row_count > 0 {
                    out.print_with_meta(BOUNDED_TRANSIENT_OMITTED, SpanMeta::unselectable());
                    out.newline();
                }
            } else {
                let total = measure_ir_leaf(leaf, width, 0, inline_options);
                let row_start = if tail {
                    total.saturating_sub(window.row_count)
                } else {
                    0
                };
                render_ir_leaf(
                    out,
                    leaf,
                    width,
                    row_start,
                    window.row_count,
                    None,
                    None,
                    inline_options,
                );
            }
        }
        BlockLayout::Vbox(items) => {
            if !tail || !window.truncated {
                let mut remaining = window.row_count;
                for child in items {
                    if remaining == 0 {
                        break;
                    }
                    let child_window = render_bounded_layout_edge_inner(
                        out,
                        child,
                        width,
                        remaining,
                        false,
                        inline_options,
                        depth.saturating_add(1),
                    )?;
                    remaining = remaining.saturating_sub(child_window.row_count);
                    if child_window.truncated {
                        break;
                    }
                }
            } else {
                let inherited_style = out.current_style();
                let mut remaining = window.row_count;
                let mut rendered = Vec::new();
                for child in items.iter().rev() {
                    if remaining == 0 {
                        break;
                    }
                    let (buf, outcome, child_window) = render_bounded_layout_edge_to_buffer(
                        child,
                        out.theme(),
                        inherited_style,
                        width,
                        remaining,
                        true,
                        inline_options,
                        depth.saturating_add(1),
                    )?;
                    remaining = remaining.saturating_sub(child_window.row_count);
                    rendered.push((buf, outcome));
                    if child_window.truncated {
                        break;
                    }
                }
                for (buf, outcome) in rendered.into_iter().rev() {
                    replay_bounded_edge_buffer(out, &buf, outcome, width);
                }
            }
        }
        BlockLayout::Gutter { child, spec } => {
            let gutter_width = display_width_u16(&spec.text);
            let child_width = child_width_after_gutter(width, gutter_width);
            let (buf, outcome, _) = render_bounded_layout_edge_to_buffer(
                child,
                out.theme(),
                out.current_style(),
                child_width,
                window.row_count,
                tail,
                inline_options,
                depth.saturating_add(1),
            )?;
            if outcome.was_wrapped {
                out.mark_wrapped();
            }
            for row in 0..outcome.line_count {
                print_row_gutter(out, spec);
                apply_temp_decoration(out, &buf, row, true);
                let row = u16::try_from(row).expect("bounded gutter exceeds temporary buffer");
                emit_buffer_row_clipped(&buf, row, child_width, out, None);
                out.newline();
            }
        }
        BlockLayout::RowPrefix { child, spec } => {
            let prefix_width = row_prefix_width(spec);
            let child_width = child_width_after_gutter(width, prefix_width);
            let (buf, outcome, _) = render_bounded_layout_edge_to_buffer(
                child,
                out.theme(),
                out.current_style(),
                child_width,
                window.row_count,
                tail,
                inline_options,
                depth.saturating_add(1),
            )?;
            if outcome.was_wrapped {
                out.mark_wrapped();
            }
            let starts_at_head = !tail || !window.truncated;
            for row in 0..outcome.line_count {
                let prefix = if starts_at_head && row == 0 {
                    &spec.first
                } else {
                    &spec.rest
                };
                let prefix = StyledLine::new(prefix);
                print_row_prefix(out, &prefix, prefix_width, width);
                apply_temp_decoration(out, &buf, row, true);
                let row = u16::try_from(row).expect("bounded row prefix exceeds temporary buffer");
                emit_buffer_row_clipped(&buf, row, child_width, out, None);
                out.newline();
            }
        }
        BlockLayout::Panel { child, spec } => {
            let padding = usize::from(spec.padding);
            let child_width = panel_child_width(width, spec.padding);
            let panel_hl = intern(&spec.hl_group);
            let panel_bg = out
                .theme()
                .resolve(panel_hl)
                .bg
                .unwrap_or(smelt_core::style::Color::Reset);
            let (top_rows, child_limit, bottom_rows) = if tail {
                let bottom = padding.min(window.row_count);
                let child_limit = window.row_count.saturating_sub(bottom);
                let child_window = measure_bounded_layout_edge_inner(
                    child,
                    child_width,
                    child_limit,
                    inline_options,
                    depth.saturating_add(1),
                )?;
                let top = if child_window.truncated {
                    0
                } else {
                    child_limit
                        .saturating_sub(child_window.row_count)
                        .min(padding)
                };
                (top, child_window.row_count, bottom)
            } else {
                let top = padding.min(window.row_count);
                let child_limit = window.row_count.saturating_sub(top);
                let child_window = measure_bounded_layout_edge_inner(
                    child,
                    child_width,
                    child_limit,
                    inline_options,
                    depth.saturating_add(1),
                )?;
                let bottom = if child_window.truncated {
                    0
                } else {
                    child_limit
                        .saturating_sub(child_window.row_count)
                        .min(padding)
                };
                (top, child_window.row_count, bottom)
            };
            for _ in 0..top_rows {
                render_panel_edge_row(out, None, child_width, panel_hl, panel_bg, spec.padding);
            }
            if child_limit > 0 {
                let (buf, outcome, _) = render_bounded_layout_edge_to_buffer(
                    child,
                    out.theme(),
                    out.current_style(),
                    child_width,
                    child_limit,
                    tail,
                    inline_options,
                    depth.saturating_add(1),
                )?;
                if outcome.was_wrapped {
                    out.mark_wrapped();
                }
                for row in 0..outcome.line_count {
                    render_panel_edge_row(
                        out,
                        Some((&buf, row)),
                        child_width,
                        panel_hl,
                        panel_bg,
                        spec.padding,
                    );
                }
            }
            for _ in 0..bottom_rows {
                render_panel_edge_row(out, None, child_width, panel_hl, panel_bg, spec.padding);
            }
        }
        BlockLayout::Style { child, spec } => {
            out.save_style();
            apply_style_spec(out, spec);
            render_bounded_layout_edge_inner(
                out,
                child,
                width,
                window.row_count,
                tail,
                inline_options,
                depth.saturating_add(1),
            )?;
            out.pop_style();
        }
        BlockLayout::Refresh { child, .. } => {
            render_bounded_layout_edge_inner(
                out,
                child,
                width,
                window.row_count,
                tail,
                inline_options,
                depth.saturating_add(1),
            )?;
        }
        BlockLayout::Cap { child, spec } => {
            let child_depth = depth.saturating_add(1);
            let rows =
                measure_bounded_cap_rows_inner(child, spec, width, inline_options, child_depth)?;
            let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
            let outcome = {
                let mut col = LineBuilder::new(&mut buf, out.theme(), width.max(1));
                col.push(None, out.current_style());
                render_bounded_layout_cap_inner(
                    &mut col,
                    child,
                    spec,
                    width,
                    0,
                    rows,
                    None,
                    inline_options,
                    child_depth,
                )?;
                col.finish()
            };
            if outcome.was_wrapped {
                out.mark_wrapped();
            }
            let start = if tail {
                outcome.line_count.saturating_sub(window.row_count)
            } else {
                0
            };
            let end = start
                .saturating_add(window.row_count)
                .min(outcome.line_count);
            for row in start..end {
                apply_temp_decoration(out, &buf, row, true);
                let row = u16::try_from(row).expect("nested cap exceeds temporary buffer");
                emit_buffer_row_clipped(&buf, row, width, out, None);
                out.newline();
            }
        }
        BlockLayout::Hbox(items) => {
            let widths = solve_ir_hbox_widths(items, width);
            let inherited_style = out.current_style();
            let mut columns = Vec::with_capacity(items.len());
            for (item, child_width) in items.iter().zip(widths.iter().copied()) {
                if child_width == 0 {
                    columns.push(None);
                    continue;
                }
                let (buf, outcome, _) = render_bounded_layout_edge_to_buffer(
                    &item.layout,
                    out.theme(),
                    inherited_style,
                    child_width,
                    window.row_count,
                    tail,
                    inline_options,
                    depth.saturating_add(1),
                )?;
                if outcome.was_wrapped {
                    out.mark_wrapped();
                }
                columns.push(Some((buf, outcome)));
            }

            let copy_owner = items.iter().position(|item| item.copy_owner).unwrap_or(0);
            let bottom_align = tail && window.truncated;
            for row in 0..window.row_count {
                for (index, (column, child_width)) in
                    columns.iter().zip(widths.iter().copied()).enumerate()
                {
                    if child_width == 0 {
                        continue;
                    }
                    let Some((buf, outcome)) = column else {
                        continue;
                    };
                    let child_row = if bottom_align {
                        row.checked_sub(window.row_count.saturating_sub(outcome.line_count))
                    } else {
                        (row < outcome.line_count).then_some(row)
                    };
                    let emitted = if let Some(child_row) = child_row {
                        if index == copy_owner {
                            apply_temp_decoration(out, buf, child_row, false);
                        }
                        let child_row = u16::try_from(child_row)
                            .expect("bounded hbox exceeds the temporary buffer");
                        emit_buffer_row_clipped(buf, child_row, child_width, out, None)
                    } else {
                        0
                    };
                    print_hbox_padding(out, child_width.saturating_sub(emitted));
                }
                out.newline();
            }
        }
    }
    Some(window)
}

#[allow(clippy::too_many_arguments)]
fn render_bounded_layout_cap(
    out: &mut LineBuilder,
    child: &LayoutIr,
    spec: &smelt_core::content::block_layout::CapSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
    inline_options: &InlineOptions,
) -> Option<u16> {
    render_bounded_layout_cap_inner(
        out,
        child,
        spec,
        width,
        row_start,
        row_count,
        gutter,
        inline_options,
        0,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_bounded_layout_cap_inner(
    out: &mut LineBuilder,
    child: &LayoutIr,
    spec: &smelt_core::content::block_layout::CapSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
    inline_options: &InlineOptions,
    depth: usize,
) -> Option<u16> {
    use smelt_core::content::block_layout::{CapKeep, CapMarker};

    let cap = usize::from(spec.rows);
    let exact_child_rows = (depth <= MAX_BOUNDED_EDGE_DEPTH)
        .then(|| bounded_exact_layout_rows(child, width, inline_options))
        .flatten();
    let probe = measure_bounded_layout_edge_inner(
        child,
        width,
        cap.saturating_add(1),
        inline_options,
        depth,
    )?;
    let truncated =
        exact_child_rows.map_or(probe.truncated || probe.row_count > cap, |rows| rows > cap);
    let kept = exact_child_rows.unwrap_or(probe.row_count).min(cap);
    let marker_visible = if let Some(child_rows) = exact_child_rows {
        cap_rows_for_child(
            child_rows,
            spec,
            child,
            width,
            None,
            inline_options,
            Some(out.theme()),
        )
        .row_count()
            > kept
    } else {
        truncated
            && match spec.keep {
                CapKeep::Head { marker } | CapKeep::Tail { marker } => marker.is_some(),
                CapKeep::HeadTail { marker, .. } => marker,
            }
    };
    if !truncated && gutter.is_none() && row_start == 0 {
        let requested = row_count.min(kept);
        let window = render_bounded_layout_edge_inner(
            out,
            child,
            width,
            requested,
            false,
            inline_options,
            depth,
        )?;
        return Some(u16::try_from(window.row_count).expect("capped output is terminal-bounded"));
    }
    let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
    let outcome = {
        let mut col = LineBuilder::new(&mut buf, out.theme(), width);
        col.push(None, out.current_style());
        match spec.keep {
            CapKeep::Head { marker } => {
                if marker_visible && marker == Some(CapMarker::Above) {
                    render_retained_cap_marker(
                        &mut col,
                        spec,
                        kept,
                        "above",
                        exact_child_rows,
                        None,
                    );
                }
                render_bounded_layout_edge_inner(
                    &mut col,
                    child,
                    width,
                    kept,
                    false,
                    inline_options,
                    depth,
                )?;
                if marker_visible && marker == Some(CapMarker::Below) {
                    render_retained_cap_marker(
                        &mut col,
                        spec,
                        kept,
                        "below",
                        exact_child_rows,
                        None,
                    );
                }
            }
            CapKeep::Tail { marker } => {
                if marker_visible && marker == Some(CapMarker::Above) {
                    render_retained_cap_marker(
                        &mut col,
                        spec,
                        kept,
                        "above",
                        exact_child_rows,
                        spec.total_rows.filter(|total| *total > kept as u64),
                    );
                }
                render_bounded_layout_edge_inner(
                    &mut col,
                    child,
                    width,
                    kept,
                    true,
                    inline_options,
                    depth,
                )?;
                if marker_visible && marker == Some(CapMarker::Below) {
                    render_retained_cap_marker(
                        &mut col,
                        spec,
                        kept,
                        "below",
                        exact_child_rows,
                        None,
                    );
                }
            }
            CapKeep::HeadTail { head, marker } => {
                if truncated {
                    let head_rows = usize::from(head).min(kept);
                    let tail_rows = kept.saturating_sub(head_rows);
                    render_bounded_layout_edge_inner(
                        &mut col,
                        child,
                        width,
                        head_rows,
                        false,
                        inline_options,
                        depth,
                    )?;
                    if marker_visible && marker {
                        render_retained_cap_marker(
                            &mut col,
                            spec,
                            kept,
                            "omitted",
                            exact_child_rows,
                            None,
                        );
                    }
                    render_bounded_layout_edge_inner(
                        &mut col,
                        child,
                        width,
                        tail_rows,
                        true,
                        inline_options,
                        depth,
                    )?;
                } else {
                    render_bounded_layout_edge_inner(
                        &mut col,
                        child,
                        width,
                        kept,
                        false,
                        inline_options,
                        depth,
                    )?;
                }
            }
        }
        col.finish()
    };
    if outcome.was_wrapped {
        out.mark_wrapped();
    }
    let end = row_start.saturating_add(row_count).min(outcome.line_count);
    let mut written = 0u16;
    for row in row_start..end {
        if let Some(gutter) = gutter {
            print_bounded_cap_gutter(
                out,
                gutter,
                child,
                spec,
                truncated,
                marker_visible,
                kept,
                row,
            );
        }
        apply_temp_decoration(out, &buf, row, true);
        let buffer_row = u16::try_from(row).expect("capped output is terminal-bounded");
        emit_buffer_row_clipped(&buf, buffer_row, width, out, None);
        out.newline();
        written = written.saturating_add(1);
    }
    Some(written)
}

#[allow(clippy::too_many_arguments)]
fn print_bounded_cap_gutter(
    out: &mut LineBuilder,
    gutter: &GutterSpec,
    child: &LayoutIr,
    spec: &smelt_core::content::block_layout::CapSpec,
    truncated: bool,
    marker_visible: bool,
    kept: usize,
    row: usize,
) {
    if !gutter.styled {
        print_row_gutter(out, gutter);
        return;
    }
    out.save_style();
    if bounded_cap_row_is_marker(spec, truncated, marker_visible, kept, row) {
        out.set_dim();
    } else {
        apply_outer_layout_styles(out, child);
    }
    out.print_gutter(&gutter.text);
    out.pop_style();
}

fn bounded_cap_row_is_marker(
    spec: &smelt_core::content::block_layout::CapSpec,
    truncated: bool,
    marker_visible: bool,
    kept: usize,
    row: usize,
) -> bool {
    use smelt_core::content::block_layout::{CapKeep, CapMarker};

    if !truncated || !marker_visible {
        return false;
    }
    match spec.keep {
        CapKeep::Head {
            marker: Some(CapMarker::Above),
        }
        | CapKeep::Tail {
            marker: Some(CapMarker::Above),
        } => row == 0,
        CapKeep::Head {
            marker: Some(CapMarker::Below),
        }
        | CapKeep::Tail {
            marker: Some(CapMarker::Below),
        } => row == kept,
        CapKeep::HeadTail { head, marker: true } => row == usize::from(head).min(kept),
        _ => false,
    }
}

fn apply_outer_layout_styles(out: &mut LineBuilder, layout: &LayoutIr) {
    match layout {
        BlockLayout::Style { child, spec } => {
            apply_style_spec(out, spec);
            apply_outer_layout_styles(out, child);
        }
        BlockLayout::Refresh { child, .. } => apply_outer_layout_styles(out, child),
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn render_ir_cap(
    out: &mut LineBuilder,
    child: &LayoutIr,
    spec: &smelt_core::content::block_layout::CapSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
    inline_options: &InlineOptions,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:layout:cap");
    if let Some(rows) =
        render_retained_text_cap(out, child, spec, width, row_start, row_count, gutter)
    {
        return rows;
    }
    render_bounded_layout_cap(
        out,
        child,
        spec,
        width,
        row_start,
        row_count,
        gutter,
        inline_options,
    )
    .expect("every cap has a bounded rendering policy")
}

fn render_cap_marker(
    out: &mut LineBuilder,
    skipped: usize,
    kept: usize,
    total: Option<u64>,
    direction: &str,
    gutter: Option<&GutterSpec>,
) {
    if let Some(gutter) = gutter.filter(|g| !g.styled) {
        out.print_gutter(&gutter.text);
    }
    out.push_dim();
    if let Some(gutter) = gutter.filter(|g| g.styled) {
        out.print_gutter(&gutter.text);
    }
    let text = cap_marker_text(skipped, kept, total, direction);
    out.print(&text);
    out.pop_style();
    out.newline();
}

fn cap_marker_text(skipped: usize, kept: usize, total: Option<u64>, direction: &str) -> String {
    if direction == "above" {
        if let Some(total) = total {
            format!(
                "… showing last {} of {}",
                kept,
                pluralize(total.min(usize::MAX as u64) as usize, "line", "lines")
            )
        } else {
            format!("… {} {direction}", pluralize(skipped, "line", "lines"))
        }
    } else if direction == "omitted" {
        format!("… {} omitted …", pluralize(skipped, "line", "lines"))
    } else {
        format!("… {} {direction}", pluralize(skipped, "line", "lines"))
    }
}

mod hbox;
use hbox::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smelt_edit::{BufCreateOpts, BufId, Buffer, Theme};
    use smelt_core::content::block_layout::{
        CapKeep, CapMarker, CapSpec, Constraint, GutterSpec, HboxItem, LayoutLeaf, LineSpec,
        MarkdownSpec, RowPrefixSpec, SeparatorSpec, TextSpec,
    };

    fn render_buffer(layout: &LayoutIr, width: u16) -> Buffer {
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        {
            let mut out = LineBuilder::new(&mut buf, &theme, width);
            render_layout_ir_into(&mut out, layout, width);
            out.finish();
        }
        buf
    }

    fn render_lines(layout: &LayoutIr, width: u16) -> Vec<String> {
        render_buffer(layout, width).lines().to_vec()
    }

    fn copy_rendered_text(layout: &LayoutIr, width: u16) -> String {
        let buf = render_buffer(layout, width);
        let text = buf.text();
        smelt_buffer::coords::copy_byte_range(&buf, 0, text.len())
    }

    fn retained_text_layout(content: impl Into<String>) -> LayoutIr {
        BlockLayout::Leaf(IrLeaf::Content(RetainedContentSpec {
            content: smelt_core::transcript_content::TranscriptContent::from(content.into()),
            render: ContentRenderSpec::Text {
                hl_group: None,
                ansi: true,
            },
        }))
    }

    fn panel_layout(child: LayoutIr, padding: u16) -> LayoutIr {
        BlockLayout::Panel {
            child: Box::new(child),
            spec: PanelSpec {
                hl_group: "Normal".into(),
                padding,
            },
        }
    }

    #[test]
    fn panel_renders_requested_child_range_once() {
        let layout = panel_layout(retained_text_layout("one\ntwo\nthree\nfour"), 1);
        let options = InlineOptions::default();
        let measured = measure_layout_ir_plan(&layout, 12, &options);
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());

        TEST_RETAINED_TEXT_RENDER_COUNT.with(|count| count.set(0));
        {
            let mut out = LineBuilder::new(&mut buf, &theme, 12);
            assert_eq!(
                render_layout_ir_range_measured(
                    &mut out,
                    &layout,
                    12,
                    1,
                    3,
                    None,
                    None,
                    &options,
                    RenderMeasurement::Measured(&measured),
                ),
                3
            );
            out.finish();
        }

        assert_eq!(buf.lines(), &[" one", " two", " three"]);
        TEST_RETAINED_TEXT_RENDER_COUNT.with(|count| assert_eq!(count.get(), 1));
    }

    #[test]
    fn panel_preserves_padding_background_copy_and_soft_wrap_metadata() {
        let layout = panel_layout(retained_text_layout("abcdef"), 1);
        let buf = render_buffer(&layout, 5);

        assert_eq!(buf.lines(), &[" ", " abc", " def", " "]);
        assert!((0..buf.line_count())
            .all(|row| buf.decoration_at(row).fill_bg == Some(smelt_core::style::Color::Reset)));
        assert!(buf.decoration_at(2).soft_wrapped);
        let text = buf.text();
        assert_eq!(
            smelt_buffer::coords::copy_byte_range(&buf, 0, text.len()),
            "\nabcdef\n"
        );
    }

    #[test]
    fn panel_renders_child_ranges_past_terminal_row_limits() {
        let layout = panel_layout(retained_text_layout("x".repeat(70_001)), 1);
        let options = InlineOptions::default();
        let measured = measure_layout_ir_plan(&layout, 3, &options);
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        {
            let mut out = LineBuilder::new(&mut buf, &theme, 3);
            assert_eq!(
                render_layout_ir_range_measured(
                    &mut out,
                    &layout,
                    3,
                    70_000,
                    2,
                    None,
                    None,
                    &options,
                    RenderMeasurement::Measured(&measured),
                ),
                2
            );
            out.finish();
        }

        assert_eq!(buf.lines(), &[" x", " x"]);
    }

    #[test]
    fn retained_measurement_refresh_matches_full_rebuild_without_reallocating_tree() {
        let content = smelt_core::transcript_content::TranscriptContent::from("one".to_string());
        let content_leaf = || {
            BlockLayout::Leaf(IrLeaf::Content(RetainedContentSpec {
                content: content.clone(),
                render: ContentRenderSpec::Text {
                    hl_group: None,
                    ansi: true,
                },
            }))
        };
        let layout = BlockLayout::Vbox(vec![
            BlockLayout::Panel {
                child: Box::new(BlockLayout::Style {
                    child: Box::new(BlockLayout::Gutter {
                        child: Box::new(BlockLayout::Cap {
                            child: Box::new(content_leaf()),
                            spec: CapSpec {
                                rows: 3,
                                keep: CapKeep::Tail {
                                    marker: Some(CapMarker::Above),
                                },
                                total_rows: None,
                            },
                        }),
                        spec: GutterSpec {
                            text: "│ ".into(),
                            styled: true,
                        },
                    }),
                    spec: StyleSpec::default(),
                }),
                spec: PanelSpec {
                    hl_group: "Normal".into(),
                    padding: 1,
                },
            },
            BlockLayout::Hbox(vec![
                HboxItem {
                    constraint: Constraint::Length(3),
                    layout: BlockLayout::Leaf(IrLeaf::Text(TextSpec {
                        content: "log".into(),
                        hl_group: None,
                        ansi: false,
                    })),
                    copy_owner: true,
                },
                HboxItem {
                    constraint: Constraint::Fill(1),
                    layout: BlockLayout::RowPrefix {
                        child: Box::new(content_leaf()),
                        spec: RowPrefixSpec {
                            first: vec![],
                            rest: vec![],
                        },
                    },
                    copy_owner: false,
                },
            ]),
        ]);
        let options = InlineOptions::default();
        let mut measured = measure_layout_ir_plan(&layout, 12, &options);
        let retained_bytes = measured.retained_bytes();
        let child_storage = match &measured.kind {
            MeasuredLayoutKind::Children(children) => children.as_ptr(),
            _ => panic!("expected root vbox measurement"),
        };

        content.append_owned("\ntwo two two\nthree three three\nfour".into());

        assert!(refresh_layout_ir_content_measurements(
            &layout,
            &mut measured,
            12,
            &options,
        ));
        assert_eq!(measured, measure_layout_ir_plan(&layout, 12, &options));
        assert_eq!(measured.retained_bytes(), retained_bytes);
        match &measured.kind {
            MeasuredLayoutKind::Children(children) => {
                assert_eq!(children.as_ptr(), child_storage);
            }
            _ => panic!("expected root vbox measurement"),
        }
    }

    #[test]
    fn retained_code_matches_transient_code_layout_and_copy_text() {
        let source = "fn main() {\n\tprintln!(\"hello world\");\n}\n";
        let transient = BlockLayout::Leaf(IrLeaf::Code(CodeSpec {
            content: source.to_string(),
            lang: "rust".into(),
        }));
        let content =
            smelt_core::transcript_content::TranscriptContent::from("fn main() {\n".to_string());
        content.append_owned("\tprintln!(\"hello world\");\n}\n".to_string());
        let retained = BlockLayout::Leaf(IrLeaf::Content(RetainedContentSpec {
            content,
            render: ContentRenderSpec::Code {
                lang: "rust".into(),
                cache: Default::default(),
            },
        }));

        assert_eq!(
            measure_layout_ir(&retained, 18),
            measure_layout_ir(&transient, 18)
        );
        assert_eq!(render_lines(&retained, 18), render_lines(&transient, 18));
        assert_eq!(
            copy_rendered_text(&retained, 18),
            copy_rendered_text(&transient, 18)
        );
    }

    #[test]
    fn retained_code_tail_cap_does_not_build_a_full_file_layout() {
        let content = smelt_core::transcript_content::TranscriptContent::from("x".repeat(70_001));
        let retained_bytes = content.retained_bytes();
        let layout = BlockLayout::Cap {
            child: Box::new(BlockLayout::Leaf(IrLeaf::Content(RetainedContentSpec {
                content: content.clone(),
                render: ContentRenderSpec::Code {
                    lang: "text".into(),
                    cache: Default::default(),
                },
            }))),
            spec: CapSpec {
                rows: 2,
                keep: CapKeep::Tail {
                    marker: Some(CapMarker::Above),
                },
                total_rows: Some(70_001),
            },
        };
        let options = InlineOptions::default();
        let measured = measure_layout_ir_plan(&layout, 40, &options);

        assert_eq!(measured.rows(), 3);
        assert!(matches!(measured.kind, MeasuredLayoutKind::Terminal));
        assert_eq!(content.retained_bytes(), retained_bytes);
        let expected = [
            "… showing last 2 of 70001 lines".to_string(),
            "x".repeat(40),
            "x".to_string(),
        ];
        assert_eq!(render_lines(&layout, 40), expected);
        let retained_after = content.retained_bytes();
        assert!(retained_after <= retained_bytes.saturating_add(4 * 1024));
        assert_eq!(render_lines(&layout, 40), expected);
        assert_eq!(content.retained_bytes(), retained_after);
    }

    #[test]
    fn retained_file_tail_cap_does_not_build_a_full_file_layout() {
        let content = smelt_core::transcript_content::TranscriptContent::from("x".repeat(70_001));
        let retained_bytes = content.retained_bytes();
        let layout = BlockLayout::Cap {
            child: Box::new(BlockLayout::Leaf(IrLeaf::Content(RetainedContentSpec {
                content: content.clone(),
                render: ContentRenderSpec::File {
                    path: "output.txt".into(),
                    lang: None,
                    cache: Default::default(),
                },
            }))),
            spec: CapSpec {
                rows: 2,
                keep: CapKeep::Tail { marker: None },
                total_rows: None,
            },
        };
        let options = InlineOptions::default();
        let measured = measure_layout_ir_plan(&layout, 10, &options);

        assert_eq!(measured.rows(), 2);
        assert!(matches!(measured.kind, MeasuredLayoutKind::Terminal));
        assert_eq!(content.retained_bytes(), retained_bytes);
        assert_eq!(render_lines(&layout, 10).len(), 2);
        let retained_after = content.retained_bytes();
        assert!(retained_after <= retained_bytes.saturating_add(4 * 1024));
        assert_eq!(render_lines(&layout, 10).len(), 2);
        assert_eq!(content.retained_bytes(), retained_after);
    }

    #[test]
    fn retained_code_renders_ranges_past_terminal_row_limits() {
        let content = smelt_core::transcript_content::TranscriptContent::from("x".repeat(70_001));
        let layout = BlockLayout::Leaf(IrLeaf::Content(RetainedContentSpec {
            content,
            render: ContentRenderSpec::Code {
                lang: "text".into(),
                cache: Default::default(),
            },
        }));
        let options = InlineOptions::default();

        let measured = measure_layout_ir_plan(&layout, 1, &options);
        assert_eq!(measured.rows(), 70_001);
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        {
            let mut out = LineBuilder::new(&mut buf, &theme, 1);
            assert_eq!(
                render_layout_ir_range_into_measured(
                    &mut out, &layout, &measured, 1, 69_999, 2, None, &options,
                ),
                2
            );
            out.finish();
        }
        assert_eq!(buf.lines(), &["x", "x"]);
    }

    #[test]
    fn retained_markdown_ascii_word_renders_deep_ranges_without_snapshotting() {
        let content = smelt_core::transcript_content::TranscriptContent::from("x".repeat(70_001));
        let layout = BlockLayout::Leaf(IrLeaf::Content(RetainedContentSpec {
            content,
            render: ContentRenderSpec::Markdown {
                dim: false,
                italic: false,
                inline: false,
            },
        }));
        let options = InlineOptions::default();

        let measured = measure_layout_ir_plan(&layout, 1, &options);
        assert_eq!(measured.rows(), 70_001);
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        {
            let mut out = LineBuilder::new(&mut buf, &theme, 1);
            assert_eq!(
                render_layout_ir_range_into_measured(
                    &mut out, &layout, &measured, 1, 69_999, 2, None, &options,
                ),
                2
            );
            out.finish();
        }
        assert_eq!(buf.lines(), &["x", "x"]);
    }

    #[test]
    fn markdown_range_render_does_not_materialize_full_block() {
        let content = (0..2_000)
            .map(|i| format!("Paragraph {i}: {}", "alpha beta gamma delta ".repeat(6)))
            .collect::<Vec<_>>()
            .join("\n\n");
        let layout = BlockLayout::Leaf(LayoutLeaf::Markdown(MarkdownSpec {
            content,
            dim: false,
            italic: false,
            inline: false,
        }));
        let theme = Theme::default();
        let options = InlineOptions::default();
        let measured = measure_layout_ir_plan(&layout, 60, &options);
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());

        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        {
            let mut out = LineBuilder::new(&mut buf, &theme, 60);
            render_layout_ir_range_into_measured(
                &mut out, &layout, &measured, 60, 300, 3, None, &options,
            );
            out.finish();
        }
        let snapshot = smelt_perf::perf::snapshot();
        smelt_perf::perf::set_enabled(false);
        smelt_perf::perf::clear();

        assert_eq!(buf.line_count(), 3);
        let full_renders = snapshot
            .durations
            .iter()
            .find(|row| row.label == "render:markdown")
            .map_or(0, |row| row.count);
        assert_eq!(
            full_renders, 0,
            "range render should not use full markdown rendering"
        );
        let range_renders = snapshot
            .durations
            .iter()
            .find(|row| row.label == "render:markdown:range")
            .map_or(0, |row| row.count);
        assert!(
            range_renders > 0,
            "range render should use markdown range rendering"
        );
    }

    #[test]
    fn large_markdown_tail_range_matches_measured_rows() {
        let layout = BlockLayout::Leaf(LayoutLeaf::Markdown(MarkdownSpec {
            content: "x".repeat(2_100_000),
            dim: false,
            italic: false,
            inline: false,
        }));
        let width = 51;
        let options = InlineOptions::default();
        let measured = measure_layout_ir_plan(&layout, width, &options);
        let row_count = 43;
        let row_start = measured.rows() - row_count;
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());

        let rendered = {
            let mut out = LineBuilder::new(&mut buf, &theme, width);
            let rendered = render_layout_ir_range_into_measured(
                &mut out, &layout, &measured, width, row_start, row_count, None, &options,
            );
            let outcome = out.finish();
            assert_eq!(outcome.line_count, rendered as usize);
            rendered
        };

        assert_eq!(usize::from(rendered), row_count);
    }

    #[test]
    fn markdown_entities_use_markdown_renderer() {
        let layout = BlockLayout::Leaf(LayoutLeaf::Markdown(MarkdownSpec {
            content: "alpha &amp; beta".into(),
            dim: false,
            italic: false,
            inline: false,
        }));

        assert!(!can_render_markdown_as_plain("alpha &amp; beta"));
        assert_eq!(render_lines(&layout, 80), vec!["alpha & beta"]);
    }

    #[test]
    fn guttered_markdown_range_render_stays_bounded() {
        let content = (0..2_000)
            .map(|i| format!("Paragraph {i}: {}", "alpha beta gamma delta ".repeat(6)))
            .collect::<Vec<_>>()
            .join("\n\n");
        let layout = BlockLayout::Gutter {
            child: Box::new(BlockLayout::Leaf(LayoutLeaf::Markdown(MarkdownSpec {
                content,
                dim: true,
                italic: true,
                inline: false,
            }))),
            spec: GutterSpec {
                text: "│ ".into(),
                styled: true,
            },
        };
        let theme = Theme::default();
        let options = InlineOptions::default();
        let measured = measure_layout_ir_plan(&layout, 60, &options);
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());

        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        {
            let mut out = LineBuilder::new(&mut buf, &theme, 60);
            render_layout_ir_range_into_measured(
                &mut out, &layout, &measured, 60, 300, 3, None, &options,
            );
            out.finish();
        }
        let snapshot = smelt_perf::perf::snapshot();
        smelt_perf::perf::set_enabled(false);
        smelt_perf::perf::clear();

        assert_eq!(buf.line_count(), 3);
        for row in 0..buf.line_count() {
            assert!(
                buf.get_line(row).is_some_and(|line| line.starts_with("│ ")),
                "row {row} missing gutter: {:?}",
                buf.get_line(row)
            );
        }
        let full_renders = snapshot
            .durations
            .iter()
            .find(|row| row.label == "render:markdown")
            .map_or(0, |row| row.count);
        assert_eq!(
            full_renders, 0,
            "guttered range render should not use full markdown rendering"
        );
        let range_renders = snapshot
            .durations
            .iter()
            .find(|row| row.label == "render:markdown:range")
            .map_or(0, |row| row.count);
        assert!(
            range_renders > 0,
            "range render should use markdown range rendering"
        );
    }

    #[test]
    fn clipped_rows_count_controls_as_cells() {
        let theme = Theme::default();
        let mut src = Buffer::new(BufId(1), BufCreateOpts::default());
        src.set_all_lines(vec!["abcdefghi\u{7f}j".into()]);
        let mut dst = Buffer::new(BufId(2), BufCreateOpts::default());

        let emitted = {
            let mut out = LineBuilder::new(&mut dst, &theme, 10);
            let emitted = emit_buffer_row_clipped(&src, 0, 10, &mut out, None);
            out.finish();
            emitted
        };

        let line = dst.get_line(0).expect("clipped row");
        assert_eq!(emitted, 10);
        assert_eq!(display_width(line), 10);
        assert_eq!(line, "abcdefghi\u{7f}");
    }

    #[test]
    fn row_prefix_clips_long_control_chrome_to_viewport() {
        let prefix = protocol::StyledSpan {
            text: format!("* {}", "\u{1c}".repeat(8)),
            ..Default::default()
        };
        let layout = BlockLayout::RowPrefix {
            child: Box::new(BlockLayout::Leaf(LayoutLeaf::Line(LineSpec {
                spans: vec![protocol::StyledSpan {
                    text: "x".into(),
                    ..Default::default()
                }],
                hl_group: None,
                syntax_highlights: Default::default(),
            }))),
            spec: RowPrefixSpec {
                first: vec![prefix.clone()],
                rest: vec![prefix],
            },
        };

        let lines = render_lines(&layout, 10);
        assert_eq!(lines, vec![format!("* {}x", "\u{1c}".repeat(7))]);
        assert!(lines.iter().all(|line| display_width(line) <= 10));
    }

    #[test]
    fn replaying_temp_rows_with_wide_chars_uses_display_columns() {
        let layout = BlockLayout::RowPrefix {
            child: Box::new(BlockLayout::Leaf(LayoutLeaf::Line(LineSpec {
                spans: vec![
                    protocol::StyledSpan {
                        text: "aaaaaaaaaaaaaaaaaaaaaaaaaaa界界".into(),
                        ..Default::default()
                    },
                    protocol::StyledSpan {
                        text: "x".into(),
                        hl: Some("accent".into()),
                        ..Default::default()
                    },
                ],
                hl_group: None,
                syntax_highlights: Default::default(),
            }))),
            spec: RowPrefixSpec {
                first: Vec::new(),
                rest: Vec::new(),
            },
        };

        assert_eq!(
            render_lines(&layout, 80),
            vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaa界界x"]
        );
    }

    #[test]
    fn text_copy_omits_soft_wraps_and_preserves_source_newlines() {
        let source = "alpha beta gamma delta\nsecond logical line";
        let layout = BlockLayout::Leaf(LayoutLeaf::Text(TextSpec {
            content: source.into(),
            hl_group: None,
            ansi: false,
        }));

        assert!(render_lines(&layout, 10).len() > 2);
        assert_eq!(copy_rendered_text(&layout, 10), source);
    }

    #[test]
    fn inline_markdown_copy_omits_soft_wraps_and_preserves_source_newlines() {
        let layout = BlockLayout::Leaf(LayoutLeaf::Markdown(MarkdownSpec {
            content: "alpha **beta** gamma delta\nsecond *logical* line".into(),
            dim: false,
            italic: false,
            inline: true,
        }));

        assert!(render_lines(&layout, 10).len() > 2);
        assert_eq!(
            copy_rendered_text(&layout, 10),
            "alpha beta gamma delta\nsecond logical line"
        );
    }

    #[test]
    fn hbox_copy_preserves_owner_soft_wraps() {
        let source = "alpha beta gamma delta\nsecond logical line";
        let layout = BlockLayout::Hbox(vec![
            HboxItem {
                constraint: Constraint::Length(4),
                layout: BlockLayout::Leaf(LayoutLeaf::Line(LineSpec {
                    spans: vec![protocol::StyledSpan {
                        text: "time".into(),
                        selectable: false,
                        ..Default::default()
                    }],
                    hl_group: None,
                    syntax_highlights: Default::default(),
                })),
                copy_owner: false,
            },
            HboxItem {
                constraint: Constraint::Fill(1),
                layout: BlockLayout::Leaf(LayoutLeaf::Runs(RunsSpec {
                    lines: protocol::StyledLines(
                        source
                            .lines()
                            .map(|line| {
                                vec![protocol::StyledSpan {
                                    text: line.into(),
                                    ..Default::default()
                                }]
                            })
                            .collect(),
                    ),
                    hl_group: None,
                    continuation_indent: 0,
                    syntax_highlights: Default::default(),
                })),
                copy_owner: true,
            },
        ]);

        assert!(render_lines(&layout, 16).len() > 2);
        assert_eq!(copy_rendered_text(&layout, 16), source);
    }

    #[test]
    fn hbox_measurement_ignores_zero_width_columns() {
        let layout = BlockLayout::Hbox(vec![
            HboxItem {
                constraint: Constraint::Fill(1),
                layout: BlockLayout::Leaf(LayoutLeaf::Runs(RunsSpec {
                    lines: protocol::StyledLines(vec![vec![protocol::StyledSpan {
                        text: "hidden content".into(),
                        ..Default::default()
                    }]]),
                    hl_group: None,
                    continuation_indent: 0,
                    syntax_highlights: Default::default(),
                })),
                copy_owner: true,
            },
            HboxItem {
                constraint: Constraint::Fit,
                layout: BlockLayout::Leaf(LayoutLeaf::Line(LineSpec {
                    spans: vec![protocol::StyledSpan {
                        text: "x".into(),
                        ..Default::default()
                    }],
                    hl_group: None,
                    syntax_highlights: Default::default(),
                })),
                copy_owner: false,
            },
        ]);

        let lines = render_lines(&layout, 1);
        assert_eq!(lines, vec!["x"]);
        assert_eq!(measure_layout_ir(&layout, 1), lines.len());
        assert_eq!(
            measure_layout_ir_plan(&layout, 1, &InlineOptions::default()).rows(),
            lines.len()
        );
    }

    #[test]
    fn runs_continuation_indent_aligns_soft_wrapped_rows() {
        let layout = BlockLayout::Leaf(LayoutLeaf::Runs(RunsSpec {
            lines: protocol::StyledLines(vec![vec![
                protocol::StyledSpan {
                    text: "* grep ".into(),
                    selectable: false,
                    ..Default::default()
                },
                protocol::StyledSpan {
                    text: "alpha beta gamma delta epsilon".into(),
                    ..Default::default()
                },
            ]]),
            hl_group: None,
            continuation_indent: 7,
            syntax_highlights: Default::default(),
        }));

        assert_eq!(
            render_lines(&layout, 20),
            vec![
                "* grep alpha beta ",
                "       gamma delta ",
                "       epsilon"
            ]
        );
        assert_eq!(measure_layout_ir(&layout, 20), 3);
    }

    #[test]
    fn retained_text_tail_cap_keeps_measurement_and_rendering_bounded() {
        let content = smelt_core::transcript_content::TranscriptContent::from(
            (0..100_000)
                .map(|line| format!("line {line:06}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let retained_before = content.retained_bytes();
        let layout = BlockLayout::Cap {
            child: Box::new(BlockLayout::Leaf(IrLeaf::Content(RetainedContentSpec {
                content: content.clone(),
                render: ContentRenderSpec::Text {
                    hl_group: None,
                    ansi: true,
                },
            }))),
            spec: CapSpec {
                rows: 3,
                keep: CapKeep::Tail {
                    marker: Some(CapMarker::Above),
                },
                total_rows: Some(100_000),
            },
        };

        let measured = measure_layout_ir_plan(&layout, 80, &InlineOptions::default());
        assert_eq!(measured.rows(), 4);
        assert!(matches!(measured.kind, MeasuredLayoutKind::Terminal));
        assert_eq!(
            render_lines(&layout, 80),
            [
                "… showing last 3 of 100000 lines",
                "line 099997",
                "line 099998",
                "line 099999",
            ]
        );
        let retained_after = content.retained_bytes();
        assert!(retained_after <= retained_before.saturating_add(4 * 1024));
        assert_eq!(
            render_lines(&layout, 80),
            [
                "… showing last 3 of 100000 lines",
                "line 099997",
                "line 099998",
                "line 099999",
            ]
        );
        assert_eq!(content.retained_bytes(), retained_after);
    }

    #[test]
    fn retained_text_edge_measurement_preserves_wrapping_ansi_and_truncation() {
        let wrapped = smelt_core::transcript_content::TranscriptContent::from("abcdefgh");
        assert_eq!(
            measure_retained_text_edge(&wrapped, 4, false, 3),
            smelt_core::transcript_content::ContentTextWindow {
                row_count: 2,
                truncated: false,
            }
        );

        let ansi =
            smelt_core::transcript_content::TranscriptContent::from("\u{1b}[31mabcdefgh\u{1b}[0m");
        assert_eq!(
            measure_retained_text_edge(&ansi, 4, true, 3),
            smelt_core::transcript_content::ContentTextWindow {
                row_count: 2,
                truncated: false,
            }
        );

        let multiline =
            smelt_core::transcript_content::TranscriptContent::from("one\ntwo\nthree\nfour");
        assert_eq!(
            measure_retained_text_edge(&multiline, 80, false, 3),
            smelt_core::transcript_content::ContentTextWindow {
                row_count: 3,
                truncated: true,
            }
        );

        let equal_lines_with_wrap =
            smelt_core::transcript_content::TranscriptContent::from("abcdefgh\nx\ny");
        assert_eq!(
            measure_retained_text_edge(&equal_lines_with_wrap, 4, false, 3),
            smelt_core::transcript_content::ContentTextWindow {
                row_count: 3,
                truncated: true,
            }
        );
    }

    #[test]
    fn thinking_peek_cap_queries_wrapped_retained_markdown_edges() {
        let content = smelt_core::transcript_content::TranscriptContent::from(format!(
            "{}x",
            "word ".repeat(14_000)
        ));
        let retained_before = content.retained_bytes();
        let thinking = BlockLayout::Style {
            child: Box::new(BlockLayout::Vbox(vec![
                BlockLayout::Leaf(IrLeaf::Markdown(MarkdownSpec {
                    content: "**Title**".into(),
                    dim: false,
                    italic: false,
                    inline: false,
                })),
                BlockLayout::Leaf(IrLeaf::Content(RetainedContentSpec {
                    content: content.clone(),
                    render: ContentRenderSpec::Markdown {
                        dim: false,
                        italic: false,
                        inline: false,
                    },
                })),
            ])),
            spec: StyleSpec {
                dim: true,
                italic: true,
                ..Default::default()
            },
        };
        let layout = BlockLayout::Gutter {
            child: Box::new(BlockLayout::Cap {
                child: Box::new(thinking),
                spec: CapSpec {
                    rows: 4,
                    keep: CapKeep::HeadTail {
                        head: 1,
                        marker: true,
                    },
                    total_rows: Some(7_002),
                },
            }),
            spec: GutterSpec {
                text: "│ ".into(),
                styled: true,
            },
        };
        let options = InlineOptions::default();
        let measured = measure_layout_ir_plan(&layout, 40, &options);

        assert_eq!(measured.rows(), 5);
        assert_eq!(content.retained_bytes(), retained_before);
        let expected = [
            "│ Title".to_string(),
            "│ … 6998 lines omitted …".to_string(),
            "│ word word word word word word word wor".to_string(),
            "│ d word word word word word word word w".to_string(),
            "│ ord x".to_string(),
        ];
        assert_eq!(render_lines(&layout, 40), expected);
        let retained_after = content.retained_bytes();
        assert!(retained_after <= retained_before.saturating_add(4 * 1024));
        assert_eq!(render_lines(&layout, 40), expected);
        assert_eq!(content.retained_bytes(), retained_after);
    }

    #[test]
    fn pathological_retained_markdown_nodes_use_bounded_literal_edges() {
        let content = smelt_core::transcript_content::TranscriptContent::from(format!(
            "# {}",
            "word ".repeat(14_000)
        ));
        let retained_before = content.retained_bytes();
        let layout = BlockLayout::Cap {
            child: Box::new(BlockLayout::Leaf(IrLeaf::Content(RetainedContentSpec {
                content: content.clone(),
                render: ContentRenderSpec::Markdown {
                    dim: false,
                    italic: false,
                    inline: false,
                },
            }))),
            spec: CapSpec {
                rows: 2,
                keep: CapKeep::Tail {
                    marker: Some(CapMarker::Above),
                },
                total_rows: Some(1_751),
            },
        };
        let measured = measure_layout_ir_plan(&layout, 40, &InlineOptions::default());

        assert_eq!(measured.rows(), 3);
        assert!(matches!(measured.kind, MeasuredLayoutKind::Terminal));
        assert_eq!(content.retained_bytes(), retained_before);
        let expected = [
            "… showing last 2 of 1751 lines",
            "d word word word word word word word wor",
            "d ",
        ];
        assert_eq!(render_lines(&layout, 40), expected);
        let retained_after = content.retained_bytes();
        assert!(retained_after <= retained_before.saturating_add(4 * 1024));
        assert_eq!(render_lines(&layout, 40), expected);
        assert_eq!(content.retained_bytes(), retained_after);
    }

    #[test]
    fn pathological_non_ascii_markdown_uses_bounded_omission() {
        let content = smelt_core::transcript_content::TranscriptContent::from("界".repeat(30_000));
        let retained_before = content.retained_bytes();
        let layout = BlockLayout::Cap {
            child: Box::new(BlockLayout::Leaf(IrLeaf::Content(RetainedContentSpec {
                content: content.clone(),
                render: ContentRenderSpec::Markdown {
                    dim: true,
                    italic: false,
                    inline: false,
                },
            }))),
            spec: CapSpec {
                rows: 4,
                keep: CapKeep::Tail {
                    marker: Some(CapMarker::Above),
                },
                total_rows: None,
            },
        };
        let measured = measure_layout_ir_plan(&layout, 80, &InlineOptions::default());

        assert_eq!(measured.rows(), 1);
        assert!(matches!(measured.kind, MeasuredLayoutKind::Terminal));
        assert_eq!(render_lines(&layout, 80), [BOUNDED_MARKDOWN_OMITTED]);
        assert_eq!(content.retained_bytes(), retained_before);
    }

    #[test]
    fn pathological_multiline_markdown_nodes_use_bounded_literal_edges() {
        let content = smelt_core::transcript_content::TranscriptContent::from(format!(
            "```\n{}\n```",
            (0..10_000)
                .map(|line| format!("line {line:05}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
        let retained_before = content.retained_bytes();
        let layout = BlockLayout::Cap {
            child: Box::new(BlockLayout::Leaf(IrLeaf::Content(RetainedContentSpec {
                content: content.clone(),
                render: ContentRenderSpec::Markdown {
                    dim: false,
                    italic: false,
                    inline: false,
                },
            }))),
            spec: CapSpec {
                rows: 2,
                keep: CapKeep::Tail {
                    marker: Some(CapMarker::Above),
                },
                total_rows: Some(10_002),
            },
        };
        let measured = measure_layout_ir_plan(&layout, 80, &InlineOptions::default());

        assert_eq!(measured.rows(), 3);
        assert!(matches!(measured.kind, MeasuredLayoutKind::Terminal));
        assert_eq!(content.retained_bytes(), retained_before);
        let expected = ["… showing last 2 of 10002 lines", "line 09999", "```"];
        assert_eq!(render_lines(&layout, 80), expected);
        let retained_after = content.retained_bytes();
        assert!(retained_after <= retained_before.saturating_add(4 * 1024));
        assert_eq!(render_lines(&layout, 80), expected);
        assert_eq!(content.retained_bytes(), retained_after);
    }

    #[test]
    fn oversized_transient_caps_never_measure_or_render_the_payload() {
        let layout = BlockLayout::Cap {
            child: Box::new(BlockLayout::Leaf(IrLeaf::Text(TextSpec {
                content: "x".repeat(MAX_BOUNDED_EDGE_EXACT_BYTES + 1),
                hl_group: None,
                ansi: false,
            }))),
            spec: CapSpec {
                rows: 2,
                keep: CapKeep::HeadTail {
                    head: 1,
                    marker: true,
                },
                total_rows: None,
            },
        };
        let options = InlineOptions::default();

        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        let measured = measure_layout_ir_plan(&layout, 40, &options);
        let lines = render_lines(&layout, 40);
        let snapshot = smelt_perf::perf::snapshot();
        smelt_perf::perf::set_enabled(false);
        smelt_perf::perf::clear();

        assert!(matches!(measured.kind, MeasuredLayoutKind::Terminal));
        assert_eq!(lines, vec![BOUNDED_TRANSIENT_OMITTED, "… 1 line omitted …"]);
        for forbidden in ["render:layout:measure_text", "render:layout:render_text"] {
            assert_eq!(
                snapshot
                    .durations
                    .iter()
                    .find(|row| row.label == forbidden)
                    .map_or(0, |row| row.count),
                0,
                "oversized transient cap used {forbidden}"
            );
        }
    }

    #[test]
    fn transient_span_budget_bounds_zero_width_layouts() {
        let spans = (0..=MAX_BOUNDED_EDGE_EXACT_SPANS)
            .map(|_| protocol::StyledSpan::default())
            .collect();
        let leaf = IrLeaf::Line(LineSpec {
            spans,
            hl_group: None,
            syntax_highlights: Default::default(),
        });
        assert!(!leaf_fits_exact_measure_budget(&leaf));

        let runs = RunsSpec {
            lines: protocol::StyledLines(vec![(0..=MAX_BOUNDED_EDGE_EXACT_SPANS)
                .map(|_| protocol::StyledSpan::default())
                .collect()]),
            hl_group: None,
            continuation_indent: 0,
            syntax_highlights: Default::default(),
        };
        let layout = BlockLayout::RowPrefix {
            child: Box::new(BlockLayout::Cap {
                child: Box::new(BlockLayout::Leaf(IrLeaf::Runs(runs))),
                spec: CapSpec {
                    rows: 1,
                    keep: CapKeep::Head { marker: None },
                    total_rows: None,
                },
            }),
            spec: RowPrefixSpec {
                first: vec![protocol::StyledSpan {
                    text: "F ".into(),
                    ..Default::default()
                }],
                rest: vec![protocol::StyledSpan {
                    text: "R ".into(),
                    ..Default::default()
                }],
            },
        };
        assert_eq!(
            render_lines(&layout, 40),
            vec![format!("F {BOUNDED_TRANSIENT_OMITTED}")]
        );
    }

    #[test]
    fn transient_depth_budget_bounds_deeply_nested_caps() {
        let mut child = BlockLayout::Leaf(IrLeaf::Text(TextSpec {
            content: "payload".into(),
            hl_group: None,
            ansi: false,
        }));
        for _ in 0..=MAX_BOUNDED_EDGE_DEPTH {
            child = BlockLayout::Style {
                child: Box::new(child),
                spec: StyleSpec::default(),
            };
        }
        assert!(!layout_fits_exact_measure_budget(
            &child,
            &mut ExactLayoutBudget::default()
        ));

        let layout = BlockLayout::Cap {
            child: Box::new(child),
            spec: CapSpec {
                rows: 1,
                keep: CapKeep::HeadTail {
                    head: 1,
                    marker: true,
                },
                total_rows: None,
            },
        };
        assert_eq!(
            render_lines(&layout, 40),
            vec![BOUNDED_TRANSIENT_OMITTED, "… 1 line omitted …"]
        );
    }

    #[test]
    fn bounded_caps_render_retained_content_through_recursive_wrappers() {
        let prefix = |text: &str| {
            vec![protocol::StyledSpan {
                text: text.into(),
                selectable: false,
                ..Default::default()
            }]
        };
        let tail_cap = |child, total_rows| BlockLayout::Cap {
            child: Box::new(child),
            spec: CapSpec {
                rows: 2,
                keep: CapKeep::Tail {
                    marker: Some(CapMarker::Above),
                },
                total_rows: Some(total_rows),
            },
        };
        let cases = [
            (
                "style",
                tail_cap(
                    BlockLayout::Style {
                        child: Box::new(retained_text_layout("one\ntwo\nthree\nfour\nfive")),
                        spec: StyleSpec {
                            dim: true,
                            ..Default::default()
                        },
                    },
                    5,
                ),
                vec!["… showing last 2 of 5 lines", "four", "five"],
            ),
            (
                "vbox",
                tail_cap(
                    BlockLayout::Vbox(vec![
                        BlockLayout::Leaf(IrLeaf::Text(TextSpec {
                            content: "title".into(),
                            hl_group: None,
                            ansi: false,
                        })),
                        retained_text_layout("one\ntwo\nthree\nfour\nfive"),
                    ]),
                    6,
                ),
                vec!["… showing last 2 of 6 lines", "four", "five"],
            ),
            (
                "gutter",
                tail_cap(
                    BlockLayout::Gutter {
                        child: Box::new(retained_text_layout("one\ntwo\nthree\nfour\nfive")),
                        spec: GutterSpec {
                            text: "│ ".into(),
                            styled: false,
                        },
                    },
                    5,
                ),
                vec!["… showing last 2 of 5 lines", "│ four", "│ five"],
            ),
            (
                "row prefix",
                tail_cap(
                    BlockLayout::RowPrefix {
                        child: Box::new(retained_text_layout("one\ntwo\nthree\nfour\nfive")),
                        spec: RowPrefixSpec {
                            first: prefix("F "),
                            rest: prefix("R "),
                        },
                    },
                    5,
                ),
                vec!["… showing last 2 of 5 lines", "R four", "R five"],
            ),
            (
                "panel",
                tail_cap(
                    panel_layout(retained_text_layout("one\ntwo\nthree\nfour\nfive"), 1),
                    7,
                ),
                vec!["… showing last 2 of 7 lines", " five", " "],
            ),
            (
                "hbox",
                tail_cap(
                    BlockLayout::Hbox(vec![
                        HboxItem {
                            constraint: Constraint::Length(5),
                            layout: retained_text_layout("one\ntwo\nthree\nfour\nfive"),
                            copy_owner: true,
                        },
                        HboxItem {
                            constraint: Constraint::Length(5),
                            layout: retained_text_layout("six\nseven\neight\nnine\nten"),
                            copy_owner: false,
                        },
                    ]),
                    5,
                ),
                vec!["… showing last 2 of 5 lines", "four nine ", "five ten  "],
            ),
            (
                "nested cap",
                tail_cap(
                    BlockLayout::Cap {
                        child: Box::new(retained_text_layout("one\ntwo\nthree\nfour\nfive")),
                        spec: CapSpec {
                            rows: 3,
                            keep: CapKeep::Tail {
                                marker: Some(CapMarker::Above),
                            },
                            total_rows: Some(5),
                        },
                    },
                    4,
                ),
                vec!["… showing last 2 of 4 lines", "four", "five"],
            ),
        ];

        for (name, layout, expected) in cases {
            let measured = measure_layout_ir_plan(&layout, 40, &InlineOptions::default());
            assert!(
                matches!(measured.kind, MeasuredLayoutKind::Terminal),
                "{name} used a full child measurement"
            );
            assert_eq!(render_lines(&layout, 40), expected, "{name}");
        }
    }

    #[test]
    fn bounded_cap_preserves_styled_gutters_for_content_and_markers() {
        let layout = BlockLayout::Gutter {
            child: Box::new(BlockLayout::Cap {
                child: Box::new(BlockLayout::Style {
                    child: Box::new(retained_text_layout("one\ntwo\nthree\nfour\nfive")),
                    spec: StyleSpec {
                        dim: true,
                        italic: true,
                        ..Default::default()
                    },
                }),
                spec: CapSpec {
                    rows: 2,
                    keep: CapKeep::HeadTail {
                        head: 1,
                        marker: true,
                    },
                    total_rows: Some(5),
                },
            }),
            spec: GutterSpec {
                text: "│ ".into(),
                styled: true,
            },
        };
        let buffer = render_buffer(&layout, 40);
        assert_eq!(
            buffer.lines(),
            &["│ one", "│ … 3 lines omitted …", "│ five"]
        );

        let theme = Theme::default();
        let gutter_style = |row| {
            let span = buffer
                .highlights_at(row)
                .into_iter()
                .find(|span| span.col_start == 0 && span.col_end >= 2)
                .expect("gutter should be styled");
            theme.resolve(span.hl)
        };
        let head = gutter_style(0);
        assert!(head.dim && head.italic);
        let marker = gutter_style(1);
        assert!(marker.dim && !marker.italic);
        let tail = gutter_style(2);
        assert!(tail.dim && tail.italic);
    }

    #[test]
    fn bounded_cap_wrappers_preserve_copy_and_soft_wrap_metadata() {
        let prefix = vec![protocol::StyledSpan {
            text: "R ".into(),
            selectable: false,
            ..Default::default()
        }];
        let layout = BlockLayout::Cap {
            child: Box::new(BlockLayout::RowPrefix {
                child: Box::new(retained_text_layout("abcdef")),
                spec: RowPrefixSpec {
                    first: prefix.clone(),
                    rest: prefix,
                },
            }),
            spec: CapSpec {
                rows: 2,
                keep: CapKeep::Tail { marker: None },
                total_rows: Some(3),
            },
        };
        let buffer = render_buffer(&layout, 4);
        let text = buffer.text();

        assert_eq!(buffer.lines(), &["R cd", "R ef"]);
        assert!(buffer.decoration_at(0).soft_wrapped);
        assert_eq!(
            smelt_buffer::coords::copy_byte_range(&buffer, 0, text.len()),
            "cdef"
        );
    }

    #[test]
    fn retained_text_range_renders_past_terminal_row_limits_through_wrappers() {
        let content = smelt_core::transcript_content::TranscriptContent::from(
            (0..70_000)
                .map(|line| format!("line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let layout = BlockLayout::Gutter {
            child: Box::new(BlockLayout::Style {
                child: Box::new(BlockLayout::Leaf(IrLeaf::Content(RetainedContentSpec {
                    content,
                    render: ContentRenderSpec::Text {
                        hl_group: None,
                        ansi: false,
                    },
                }))),
                spec: StyleSpec::default(),
            }),
            spec: GutterSpec {
                text: "│ ".into(),
                styled: false,
            },
        };
        let options = InlineOptions::default();
        let measured = measure_layout_ir_plan(&layout, 80, &options);
        assert_eq!(measured.rows(), 70_000);

        let theme = Theme::default();
        let mut buffer = Buffer::new(BufId(0), BufCreateOpts::default());
        let rendered = {
            let mut out = LineBuilder::new(&mut buffer, &theme, 80);
            let rendered = render_layout_ir_range_into_measured(
                &mut out, &layout, &measured, 80, 69_999, 1, None, &options,
            );
            out.finish();
            rendered
        };
        assert_eq!(rendered, 1);
        assert_eq!(buffer.get_line(0), Some("│ line 69999"));
    }

    #[test]
    fn cap_head_tail_uses_three_bounded_segments() {
        let rows = cap_rows(
            10,
            &CapSpec {
                rows: 4,
                keep: CapKeep::HeadTail {
                    head: 2,
                    marker: true,
                },
                total_rows: None,
            },
        );

        assert_eq!(rows.row_count(), 5);
        assert_eq!(rows.row_at(0), Some(CapRow::Child(0)));
        assert_eq!(rows.row_at(1), Some(CapRow::Child(1)));
        assert_eq!(
            rows.row_at(2),
            Some(CapRow::Marker {
                skipped: 6,
                kept: 4,
                total: None,
                direction: "omitted",
            })
        );
        assert_eq!(rows.row_at(3), Some(CapRow::Child(8)));
        assert_eq!(rows.row_at(4), Some(CapRow::Child(9)));
        assert_eq!(rows.row_at(5), None);
    }

    #[test]
    fn retained_cap_head_tail_marks_truncation_even_when_omitted_rows_are_blank() {
        let layout = BlockLayout::Cap {
            child: Box::new(retained_text_layout("head\n\n\n\ntail")),
            spec: CapSpec {
                rows: 2,
                keep: CapKeep::HeadTail {
                    head: 1,
                    marker: true,
                },
                total_rows: Some(5),
            },
        };

        assert_eq!(
            render_lines(&layout, 80),
            vec!["head", "… 3 lines omitted …", "tail"]
        );
        assert_eq!(measure_layout_ir(&layout, 80), 3);
    }

    #[test]
    fn retained_markdown_cap_suppresses_marker_for_blank_omitted_row() {
        let content = smelt_core::transcript_content::TranscriptContent::from(
            "first paragraph\n\nsecond paragraph\nthird paragraph\nfourth paragraph".to_string(),
        );
        let layout = BlockLayout::Cap {
            child: Box::new(BlockLayout::Leaf(IrLeaf::Content(RetainedContentSpec {
                content,
                render: ContentRenderSpec::Markdown {
                    dim: true,
                    italic: true,
                    inline: false,
                },
            }))),
            spec: CapSpec {
                rows: 4,
                keep: CapKeep::HeadTail {
                    head: 1,
                    marker: true,
                },
                total_rows: Some(5),
            },
        };

        assert_eq!(
            render_lines(&layout, 80),
            vec![
                "first paragraph",
                "second paragraph",
                "third paragraph",
                "fourth paragraph",
            ]
        );
        assert_eq!(measure_layout_ir(&layout, 80), 4);
    }

    #[test]
    fn cap_head_tail_suppresses_marker_for_blank_omitted_rows() {
        let layout = BlockLayout::Cap {
            child: Box::new(BlockLayout::Leaf(LayoutLeaf::Text(TextSpec {
                content: "head\n\n\n\ntail".into(),
                hl_group: None,
                ansi: false,
            }))),
            spec: CapSpec {
                rows: 2,
                keep: CapKeep::HeadTail {
                    head: 1,
                    marker: true,
                },
                total_rows: None,
            },
        };

        assert_eq!(render_lines(&layout, 80), vec!["head", "tail"]);
        assert_eq!(measure_layout_ir(&layout, 80), 2);
    }

    #[test]
    fn cap_head_tail_keeps_marker_for_visible_omitted_rows() {
        let layout = BlockLayout::Cap {
            child: Box::new(BlockLayout::Leaf(LayoutLeaf::Text(TextSpec {
                content: "head\n\nmiddle\n\ntail".into(),
                hl_group: None,
                ansi: false,
            }))),
            spec: CapSpec {
                rows: 2,
                keep: CapKeep::HeadTail {
                    head: 1,
                    marker: true,
                },
                total_rows: None,
            },
        };

        assert_eq!(
            render_lines(&layout, 80),
            vec!["head", "… 3 lines omitted …", "tail"]
        );
        assert_eq!(measure_layout_ir(&layout, 80), 3);
    }

    #[test]
    fn protocol_styled_runs_keep_cross_span_graphemes_atomic() {
        for (parts, grapheme) in [
            (vec!["e", "\u{301}"], "e\u{301}"),
            (vec!["👩", "\u{200d}", "💻"], "👩\u{200d}💻"),
            (vec!["9", "\u{fe0f}"], "9\u{fe0f}"),
            (vec!["⌚", "\u{fe0e}"], "⌚\u{fe0e}"),
            (vec!["🇨", "🇦"], "🇨🇦"),
        ] {
            let mut spans: Vec<_> = parts
                .iter()
                .enumerate()
                .map(|(index, text)| protocol::StyledSpan {
                    text: (*text).into(),
                    fg: Some(format!("style-{index}")),
                    ..Default::default()
                })
                .collect();
            spans.push(protocol::StyledSpan {
                text: "x".into(),
                ..Default::default()
            });

            let line = StyledLine::new(&spans);
            let (grapheme_style, _) = line
                .inline
                .runs
                .iter()
                .enumerate()
                .find(|(_, run)| run.text == grapheme)
                .and_then(|(index, _)| line.run(index))
                .expect("complete grapheme assigned to one span");
            assert_eq!(grapheme_style.fg.as_deref(), Some("style-0"));

            let layout = BlockLayout::Leaf(LayoutLeaf::Runs(RunsSpec {
                lines: protocol::StyledLines(vec![spans]),
                hl_group: None,
                continuation_indent: 0,
                syntax_highlights: Default::default(),
            }));
            let width = display_width(grapheme).max(1) as u16;
            let lines = render_lines(&layout, width);
            assert_eq!(lines, vec![grapheme, "x"]);
            assert_eq!(measure_layout_ir(&layout, width) as usize, lines.len());
            assert_eq!(intrinsic_layout_width(&layout, 80), width + 1);
        }
    }

    #[test]
    fn hbox_children_share_grapheme_width_without_overflow() {
        let line = |text: &str| {
            BlockLayout::Leaf(LayoutLeaf::Line(LineSpec {
                spans: vec![protocol::StyledSpan {
                    text: text.into(),
                    ..Default::default()
                }],
                hl_group: None,
                syntax_highlights: Default::default(),
            }))
        };
        let layout = BlockLayout::Hbox(vec![
            HboxItem {
                constraint: Constraint::Length(1),
                layout: line("9"),
                copy_owner: false,
            },
            HboxItem {
                constraint: Constraint::Length(1),
                layout: line("\u{fe0f}x"),
                copy_owner: true,
            },
        ]);

        let lines = render_lines(&layout, 2);
        assert_eq!(lines, vec!["9\u{fe0f}"]);
        assert!(lines.iter().all(|line| display_width(line) <= 2));
    }

    #[test]
    fn row_prefix_and_child_share_grapheme_width() {
        let layout = BlockLayout::RowPrefix {
            child: Box::new(BlockLayout::Leaf(LayoutLeaf::Runs(RunsSpec {
                lines: protocol::StyledLines(vec![vec![protocol::StyledSpan {
                    text: "\u{fe0f}x".into(),
                    ..Default::default()
                }]]),
                hl_group: None,
                continuation_indent: 0,
                syntax_highlights: Default::default(),
            }))),
            spec: RowPrefixSpec {
                first: vec![protocol::StyledSpan {
                    text: "9".into(),
                    ..Default::default()
                }],
                rest: Vec::new(),
            },
        };

        let lines = render_lines(&layout, 2);
        assert_eq!(lines, vec!["9\u{fe0f}", "x"]);
        assert!(lines.iter().all(|line| display_width(line) <= 2));
        assert_eq!(measure_layout_ir(&layout, 2) as usize, lines.len());

        let clipped_line = BlockLayout::RowPrefix {
            child: Box::new(BlockLayout::Leaf(LayoutLeaf::Line(LineSpec {
                spans: vec![protocol::StyledSpan {
                    text: "\u{fe0f}x".into(),
                    ..Default::default()
                }],
                hl_group: None,
                syntax_highlights: Default::default(),
            }))),
            spec: RowPrefixSpec {
                first: vec![protocol::StyledSpan {
                    text: "9".into(),
                    ..Default::default()
                }],
                rest: Vec::new(),
            },
        };
        assert_eq!(render_lines(&clipped_line, 2), vec!["9\u{fe0f}"]);

        let ordinary_prefix = BlockLayout::RowPrefix {
            child: Box::new(BlockLayout::Leaf(LayoutLeaf::Runs(RunsSpec {
                lines: protocol::StyledLines(vec![vec![protocol::StyledSpan {
                    text: "x".into(),
                    ..Default::default()
                }]]),
                hl_group: None,
                continuation_indent: 0,
                syntax_highlights: Default::default(),
            }))),
            spec: RowPrefixSpec {
                first: vec![protocol::StyledSpan {
                    text: "**".into(),
                    ..Default::default()
                }],
                rest: vec![protocol::StyledSpan {
                    text: "**".into(),
                    ..Default::default()
                }],
            },
        };
        assert_eq!(render_lines(&ordinary_prefix, 2), vec!["*x"]);

        let gutter_join = BlockLayout::Gutter {
            child: Box::new(BlockLayout::RowPrefix {
                child: Box::new(BlockLayout::Leaf(LayoutLeaf::Runs(RunsSpec {
                    lines: protocol::StyledLines(vec![vec![protocol::StyledSpan {
                        text: "x".into(),
                        ..Default::default()
                    }]]),
                    hl_group: None,
                    continuation_indent: 0,
                    syntax_highlights: Default::default(),
                }))),
                spec: RowPrefixSpec {
                    first: vec![protocol::StyledSpan {
                        text: "\u{fe0f}".into(),
                        ..Default::default()
                    }],
                    rest: Vec::new(),
                },
            }),
            spec: GutterSpec {
                text: "9".into(),
                styled: false,
            },
        };
        let lines = render_lines(&gutter_join, 2);
        assert_eq!(lines, vec!["9\u{fe0f}"]);
        assert!(lines.iter().all(|line| display_width(line) <= 2));
    }

    #[test]
    fn line_prefix_and_separator_share_cross_span_grapheme_widths() {
        let split = || {
            vec![
                protocol::StyledSpan {
                    text: "⌚".into(),
                    fg: Some("first".into()),
                    ..Default::default()
                },
                protocol::StyledSpan {
                    text: "\u{fe0e}".into(),
                    fg: Some("second".into()),
                    ..Default::default()
                },
            ]
        };

        let line = BlockLayout::Leaf(LayoutLeaf::Line(LineSpec {
            spans: split(),
            hl_group: None,
            syntax_highlights: Default::default(),
        }));
        assert_eq!(render_lines(&line, 10), vec!["⌚\u{fe0e}"]);
        assert_eq!(intrinsic_layout_width(&line, 10), 1);

        let prefixed = BlockLayout::RowPrefix {
            child: Box::new(BlockLayout::Leaf(LayoutLeaf::Line(LineSpec {
                spans: vec![protocol::StyledSpan {
                    text: "x".into(),
                    ..Default::default()
                }],
                hl_group: None,
                syntax_highlights: Default::default(),
            }))),
            spec: RowPrefixSpec {
                first: split(),
                rest: split(),
            },
        };
        assert_eq!(render_lines(&prefixed, 2), vec!["⌚\u{fe0e}x"]);
        assert_eq!(intrinsic_layout_width(&prefixed, 10), 2);

        let separator = BlockLayout::Leaf(LayoutLeaf::Separator(SeparatorSpec {
            label: split(),
            dim: false,
            selectable: true,
        }));
        assert_eq!(render_lines(&separator, 1), vec!["⌚\u{fe0e}"]);
        assert_eq!(intrinsic_layout_width(&separator, 10), 1);
    }

    #[test]
    fn cap_tail_marker_uses_total_rows_when_available() {
        let layout = BlockLayout::Cap {
            child: Box::new(BlockLayout::Leaf(LayoutLeaf::Text(TextSpec {
                content: "one\ntwo\nthree\nfour".into(),
                hl_group: None,
                ansi: false,
            }))),
            spec: CapSpec {
                rows: 2,
                keep: CapKeep::Tail {
                    marker: Some(CapMarker::Above),
                },
                total_rows: Some(100),
            },
        };

        assert_eq!(
            render_lines(&layout, 80),
            vec!["… showing last 2 of 100 lines", "three", "four"]
        );
    }
}
