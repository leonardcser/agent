//! Span-tree collector used by block renderers during the layout stage.
//!
//! `SpanCollector` is the in-memory builder for a `DisplayBlock`.
//! Renderers (markdown, diff, syntax, transcript blocks, dialog
//! previews) all write into one of these; the finished block is
//! projected into a `ui::Buffer` by `to_buffer::render_into_buffer`.

use super::display::{ColorValue, DisplayBlock, DisplayLine, DisplaySpan, SpanMeta, SpanStyle};
use unicode_width::UnicodeWidthStr;

/// Display-column width of a string slice. Used for visible-width
/// tracking inside `SpanCollector`.
pub(crate) fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

pub(crate) struct SpanCollector {
    block: DisplayBlock,
    cur_line: DisplayLine,
    cur_style: SpanStyle,
    style_stack: Vec<SpanStyle>,
    cur_visible_cols: u16,
    /// Source text to attach to the next line closed by `newline()`.
    /// Set by `arm_source_text`; consumed once and cleared.
    pending_source_text: Option<String>,
    /// While true, every `newline()` after the source-text injection
    /// tags the closed line as a soft-wrap continuation. Cleared by
    /// `disarm_source_text` when the multi-row construct ends.
    auto_soft_wrap_continuation: bool,
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
            pending_source_text: None,
            auto_soft_wrap_continuation: false,
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

    // ── Text emission ───────────────────────────────────────────────

    pub(crate) fn print(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let w = display_width(text) as u16;
        self.cur_visible_cols = self.cur_visible_cols.saturating_add(w);
        if let Some(last) = self.cur_line.spans.last_mut() {
            if last.style == self.cur_style && last.meta == SpanMeta::default() {
                last.text.push_str(text);
                return;
            }
        }
        self.cur_line.spans.push(DisplaySpan {
            text: text.to_string(),
            style: self.cur_style.clone(),
            meta: SpanMeta::default(),
        });
    }

    pub(crate) fn print_string(&mut self, s: String) {
        self.print(&s);
    }

    pub(crate) fn print_with_meta(&mut self, text: &str, meta: SpanMeta) {
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

    pub(crate) fn print_gutter(&mut self, text: &str) {
        self.print_with_meta(
            text,
            SpanMeta {
                selectable: false,
                copy_as: None,
            },
        );
    }

    pub(crate) fn newline(&mut self) {
        if let Some(src) = self.pending_source_text.take() {
            self.cur_line.source_text = Some(src);
        } else if self.auto_soft_wrap_continuation {
            self.cur_line.soft_wrapped = true;
        }
        if self.cur_visible_cols > self.block.max_line_width {
            self.block.max_line_width = self.cur_visible_cols;
        }
        self.block.lines.push(std::mem::take(&mut self.cur_line));
        self.cur_visible_cols = 0;
    }

    // ── Per-line decorations ────────────────────────────────────────

    /// Mark the layout as width-pinned: it cannot be replayed at a
    /// different terminal width without re-laying-out from the source
    /// `Block`. Wrap helpers call this when they break a line.
    pub(crate) fn mark_wrapped(&mut self) {
        self.block.was_wrapped = true;
    }

    /// Fill the remainder of the current visual row with `bg` so the row
    /// extends to the right edge of the terminal (minus `right_margin`
    /// columns reserved on the right). Used by diff and code rows.
    pub(crate) fn fill_line_bg(&mut self, bg: ColorValue, right_margin: u16) {
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

    /// Set the gutter background for the current line. Paint-time gutter
    /// padding will be filled with this color instead of blank spaces.
    pub(crate) fn set_gutter_bg(&mut self, bg: ColorValue) {
        self.cur_line.gutter_bg = Some(bg);
    }

    /// Mark the current line as a soft-wrap continuation of the previous
    /// logical line. `copy_range` suppresses `\n` before these rows.
    pub(crate) fn mark_soft_wrap_continuation(&mut self) {
        self.cur_line.soft_wrapped = true;
    }

    /// Set the raw source text for the current line. Used by
    /// `render_markdown_inner` so `copy_range` can emit raw markdown
    /// instead of display text for fully-selected rows.
    pub(crate) fn set_source_text(&mut self, text: &str) {
        self.cur_line.source_text = Some(text.to_string());
    }

    /// Tag the next-closed line with `source` and turn every following
    /// `newline()` into a soft-wrap continuation until
    /// `disarm_source_text` is called. Used to attach a raw markdown
    /// source string to the first visual row of a multi-row rendered
    /// construct (tables) where per-row source mapping is impractical.
    pub(crate) fn arm_source_text(&mut self, source: String) {
        self.pending_source_text = Some(source);
        self.auto_soft_wrap_continuation = true;
    }

    pub(crate) fn disarm_source_text(&mut self) {
        self.pending_source_text = None;
        self.auto_soft_wrap_continuation = false;
    }

    // ── Style state ─────────────────────────────────────────────────

    pub(crate) fn snapshot_style(&self) -> SpanStyle {
        self.cur_style.clone()
    }

    pub(crate) fn apply_style(&mut self, style: SpanStyle) {
        self.cur_style = style;
    }

    pub(crate) fn push_style(&mut self, style: SpanStyle) {
        self.style_stack.push(self.cur_style.clone());
        self.cur_style = style;
    }

    pub(crate) fn pop_style(&mut self) {
        if let Some(prev) = self.style_stack.pop() {
            self.cur_style = prev;
        }
    }

    pub(crate) fn reset_style(&mut self) {
        self.apply_style(SpanStyle::default());
    }

    pub(crate) fn set_fg(&mut self, c: ColorValue) {
        let mut s = self.snapshot_style();
        s.fg = Some(c);
        self.apply_style(s);
    }

    pub(crate) fn set_bg(&mut self, c: ColorValue) {
        let mut s = self.snapshot_style();
        s.bg = Some(c);
        self.apply_style(s);
    }

    pub(crate) fn set_bold(&mut self) {
        let mut s = self.snapshot_style();
        s.bold = true;
        self.apply_style(s);
    }

    pub(crate) fn set_dim(&mut self) {
        let mut s = self.snapshot_style();
        s.dim = true;
        self.apply_style(s);
    }

    pub(crate) fn set_dim_italic(&mut self) {
        let mut s = self.snapshot_style();
        s.dim = true;
        s.italic = true;
        self.apply_style(s);
    }

    pub(crate) fn push_fg(&mut self, c: ColorValue) {
        let mut s = self.snapshot_style();
        s.fg = Some(c);
        self.push_style(s);
    }

    pub(crate) fn push_bold(&mut self) {
        let mut s = self.snapshot_style();
        s.bold = true;
        self.push_style(s);
    }

    pub(crate) fn push_dim(&mut self) {
        let mut s = self.snapshot_style();
        s.dim = true;
        self.push_style(s);
    }

    pub(crate) fn push_italic(&mut self) {
        let mut s = self.snapshot_style();
        s.italic = true;
        self.push_style(s);
    }

    pub(crate) fn push_crossedout(&mut self) {
        let mut s = self.snapshot_style();
        s.crossedout = true;
        self.push_style(s);
    }

    pub(crate) fn push_dim_italic(&mut self) {
        let mut s = self.snapshot_style();
        s.dim = true;
        s.italic = true;
        self.push_style(s);
    }
}
