//! Span-tree collector used by block renderers during the layout stage.
//!
//! `SpanCollector` is the in-memory builder for a `DisplayBlock`. It
//! exposes a small surface via the `LayoutSink` trait so renderers and
//! shared helpers can be generic over either sink. Block renderers always
//! layout into a `SpanCollector`; dialogs continue to write directly to a
//! `RenderOut`.

use super::display::{ColorValue, DisplayBlock, DisplayLine, DisplaySpan, SpanStyle};
use super::RenderOut;
use crossterm::style::Color;
use unicode_width::UnicodeWidthStr;

/// Display-column width of a string slice. Used for visible-width
/// tracking inside `SpanCollector` and the `RenderOut` sink.
pub(crate) fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Common surface used by block renderers and shared helpers. Both
/// `RenderOut` (immediate byte emission for dialogs) and `SpanCollector`
/// (span-tree capture for cached block layouts) implement it.
///
/// The trait is intentionally narrow: each impl provides only a handful
/// of required methods, and all per-attribute `set_*` / `push_*` helpers
/// are default-implemented on top of `snapshot_style` + `apply_style` /
/// `push_style`.
pub(crate) trait LayoutSink {
    // ── Required ─────────────────────────────────────────────────────
    fn print(&mut self, text: &str);
    fn newline(&mut self);

    /// Mark the layout as width-pinned: it cannot be replayed at a
    /// different terminal width without re-laying-out from the source
    /// `Block`. Wrap helpers call this when they break a line.
    fn mark_wrapped(&mut self);

    /// Fill the remainder of the current visual row with `bg` so the row
    /// extends to the right edge of the terminal (minus `right_margin`
    /// columns reserved on the right). Used by diff and code rows.
    /// Both sinks emit/store the fill before the next `newline()` call.
    fn fill_line_bg(&mut self, bg: ColorValue, right_margin: u16);

    /// Set the gutter background for the current line. Paint-time gutter
    /// padding will be filled with this color instead of blank spaces.
    fn set_gutter_bg(&mut self, _bg: ColorValue) {}

    /// Mark the current line as a soft-wrap continuation of the previous
    /// logical line. `copy_range` suppresses `\n` before these rows.
    fn mark_soft_wrap_continuation(&mut self) {}

    /// Set the raw source text for the current line. Used by
    /// `render_markdown_inner` so `copy_range` can emit raw markdown
    /// instead of display text for fully-selected rows.
    fn set_source_text(&mut self, _text: &str) {}

    /// Snapshot the current style by value. Default helpers use this to
    /// derive a mutated style before `apply_style` / `push_style`.
    fn snapshot_style(&self) -> SpanStyle;

    /// Replace the current style without growing the push/pop stack.
    fn apply_style(&mut self, style: SpanStyle);

    /// Push the current style onto the stack and apply `style`.
    fn push_style(&mut self, style: SpanStyle);

    /// Pop back to the previously-pushed style.
    fn pop_style(&mut self);

    /// Print text with explicit `SpanMeta`. Block renderers use this
    /// for padding and decorations that should not be selectable or
    /// should have custom copy behavior.
    fn print_with_meta(&mut self, text: &str, meta: crate::content::display::SpanMeta) {
        let _ = meta;
        self.print(text);
    }

    // ── Default helpers ──────────────────────────────────────────────

    fn print_gutter(&mut self, text: &str) {
        self.print_with_meta(
            text,
            crate::content::display::SpanMeta {
                selectable: false,
                copy_as: None,
            },
        );
    }

    fn print_string(&mut self, s: String) {
        self.print(&s);
    }

    fn reset_style(&mut self) {
        self.apply_style(SpanStyle::default());
    }

    fn set_fg(&mut self, c: ColorValue) {
        let mut s = self.snapshot_style();
        s.fg = Some(c);
        self.apply_style(s);
    }
    fn set_bg(&mut self, c: ColorValue) {
        let mut s = self.snapshot_style();
        s.bg = Some(c);
        self.apply_style(s);
    }
    fn set_bold(&mut self) {
        let mut s = self.snapshot_style();
        s.bold = true;
        self.apply_style(s);
    }
    fn set_dim(&mut self) {
        let mut s = self.snapshot_style();
        s.dim = true;
        self.apply_style(s);
    }
    fn set_dim_italic(&mut self) {
        let mut s = self.snapshot_style();
        s.dim = true;
        s.italic = true;
        self.apply_style(s);
    }

    fn push_fg(&mut self, c: ColorValue) {
        let mut s = self.snapshot_style();
        s.fg = Some(c);
        self.push_style(s);
    }
    fn push_bold(&mut self) {
        let mut s = self.snapshot_style();
        s.bold = true;
        self.push_style(s);
    }
    fn push_dim(&mut self) {
        let mut s = self.snapshot_style();
        s.dim = true;
        self.push_style(s);
    }
    fn push_italic(&mut self) {
        let mut s = self.snapshot_style();
        s.italic = true;
        self.push_style(s);
    }
    fn push_crossedout(&mut self) {
        let mut s = self.snapshot_style();
        s.crossedout = true;
        self.push_style(s);
    }
    fn push_dim_italic(&mut self) {
        let mut s = self.snapshot_style();
        s.dim = true;
        s.italic = true;
        self.push_style(s);
    }
}

// ── SpanCollector ──────────────────────────────────────────────────────

pub(crate) struct SpanCollector {
    block: DisplayBlock,
    cur_line: DisplayLine,
    cur_style: SpanStyle,
    style_stack: Vec<SpanStyle>,
    cur_visible_cols: u16,
}

impl SpanCollector {
    pub(crate) fn new(layout_width: u16) -> Self {
        Self {
            block: DisplayBlock {
                lines: Vec::new(),
                layout_width,
                was_wrapped: false,
                max_line_width: 0,
            },
            cur_line: DisplayLine::default(),
            cur_style: SpanStyle::default(),
            style_stack: Vec::new(),
            cur_visible_cols: 0,
        }
    }

    pub(crate) fn finish(mut self) -> DisplayBlock {
        if !self.cur_line.spans.is_empty()
            || self.cur_line.fill_bg.is_some()
            || self.cur_visible_cols > 0
        {
            if self.cur_visible_cols > self.block.max_line_width {
                self.block.max_line_width = self.cur_visible_cols;
            }
            self.block.lines.push(std::mem::take(&mut self.cur_line));
        }
        self.block
    }
}

impl LayoutSink for SpanCollector {
    fn print(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let w = display_width(text) as u16;
        self.cur_visible_cols = self.cur_visible_cols.saturating_add(w);
        if let Some(last) = self.cur_line.spans.last_mut() {
            if last.style == self.cur_style && last.meta == Default::default() {
                last.text.push_str(text);
                return;
            }
        }
        self.cur_line.spans.push(DisplaySpan {
            text: text.to_string(),
            style: self.cur_style.clone(),
            meta: Default::default(),
        });
    }

    fn print_with_meta(&mut self, text: &str, meta: crate::content::display::SpanMeta) {
        if text.is_empty() {
            return;
        }
        let w = display_width(text) as u16;
        self.cur_visible_cols = self.cur_visible_cols.saturating_add(w);
        if let Some(last) = self.cur_line.spans.last_mut() {
            if last.style == self.cur_style && last.meta == meta {
                last.text.push_str(text);
                return;
            }
        }
        self.cur_line.spans.push(DisplaySpan {
            text: text.to_string(),
            style: self.cur_style.clone(),
            meta,
        });
    }

    fn newline(&mut self) {
        if self.cur_visible_cols > self.block.max_line_width {
            self.block.max_line_width = self.cur_visible_cols;
        }
        self.block.lines.push(std::mem::take(&mut self.cur_line));
        self.cur_visible_cols = 0;
    }

    fn mark_wrapped(&mut self) {
        self.block.was_wrapped = true;
    }

    fn fill_line_bg(&mut self, bg: ColorValue, right_margin: u16) {
        // Calling `fill_line_bg` twice on the same row would silently
        // overwrite the first fill. No legitimate caller does this — a
        // row has at most one trailing bg fill — so catch the misuse in
        // debug builds.
        debug_assert!(
            self.cur_line.fill_bg.is_none(),
            "fill_line_bg called twice on the same row"
        );
        self.cur_line.fill_bg = Some(bg);
        self.cur_line.fill_right_margin = right_margin;
    }

    fn set_gutter_bg(&mut self, bg: ColorValue) {
        self.cur_line.gutter_bg = Some(bg);
    }

    fn mark_soft_wrap_continuation(&mut self) {
        self.cur_line.soft_wrapped = true;
    }

    fn set_source_text(&mut self, text: &str) {
        self.cur_line.source_text = Some(text.to_string());
    }

    fn snapshot_style(&self) -> SpanStyle {
        self.cur_style.clone()
    }

    fn apply_style(&mut self, style: SpanStyle) {
        self.cur_style = style;
    }

    fn push_style(&mut self, style: SpanStyle) {
        self.style_stack.push(self.cur_style.clone());
        self.cur_style = style;
    }

    fn pop_style(&mut self) {
        if let Some(prev) = self.style_stack.pop() {
            self.cur_style = prev;
        }
    }
}

// ── RenderOut sink impl ────────────────────────────────────────────────

/// Resolve a `ColorValue` against the live theme atomics. Used by
/// `RenderOut` (which emits SGR immediately for dialogs).
fn resolve_live(c: ColorValue) -> Color {
    match c {
        ColorValue::Rgb(r, g, b) => Color::Rgb { r, g, b },
        ColorValue::Ansi(v) => Color::AnsiValue(v),
        ColorValue::Named(n) => Color::from(n),
        ColorValue::Role(role) => super::resolve_role_live(role),
    }
}

fn span_to_state(style: SpanStyle) -> super::StyleState {
    super::StyleState {
        fg: style.fg.map(resolve_live),
        bg: style.bg.map(resolve_live),
        bold: style.bold,
        dim: style.dim,
        italic: style.italic,
        crossedout: style.crossedout,
        underline: style.underline,
    }
}

fn state_to_span(state: &super::StyleState) -> SpanStyle {
    SpanStyle {
        fg: state.fg.map(ColorValue::from),
        bg: state.bg.map(ColorValue::from),
        bold: state.bold,
        dim: state.dim,
        italic: state.italic,
        crossedout: state.crossedout,
        underline: state.underline,
    }
}

impl LayoutSink for RenderOut {
    fn print(&mut self, text: &str) {
        RenderOut::print(self, text);
    }

    fn newline(&mut self) {
        RenderOut::newline(self);
    }

    fn mark_wrapped(&mut self) {
        // No-op for RenderOut: dialogs and overlay paths don't use the
        // layout cache, so width-pinning is irrelevant.
    }

    fn fill_line_bg(&mut self, bg: ColorValue, right_margin: u16) {
        let tw = super::term_width() as u16;
        let pad = tw
            .saturating_sub(self.line_cols)
            .saturating_sub(right_margin);
        if pad > 0 {
            let c = resolve_live(bg);
            RenderOut::set_bg(self, c);
            self.print(&" ".repeat(pad as usize));
        }
    }

    fn snapshot_style(&self) -> SpanStyle {
        state_to_span(&self.current)
    }

    fn apply_style(&mut self, style: SpanStyle) {
        let state = span_to_state(style);
        RenderOut::set_state(self, state);
    }

    fn push_style(&mut self, style: SpanStyle) {
        let state = span_to_state(style);
        RenderOut::push_style(self, state);
    }

    fn pop_style(&mut self) {
        RenderOut::pop_style(self);
    }
}
