//! `LineBuilder` is the single layout primitive for all block renderers.
//! Renderers call `print` / `newline` / `push_style` etc.; the builder resolves styles
//! against the supplied [`Theme`] and writes lines + highlights + decorations into a [`Buffer`].
//! On [`LineBuilder::finish`] the trailing incomplete line is flushed and an [`Outcome`] returned.

use crate::buffer::{Buffer, LineDecoration, RenderedBufferRebuild, SourceLine, SpanMeta};
use crate::style::{Color, Style};
use crate::theme::{intern_anonymous_style, HlGroup, Theme};
use smelt_buffer::{cell_width, text};

/// Display-column width of a string slice.
pub fn display_width(s: &str) -> usize {
    cell_width::text_width(s)
}

/// Position of a visual segment within one logical source line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrappedSegmentKind {
    First,
    Continuation,
}

impl WrappedSegmentKind {
    pub fn from_index(index: usize) -> Self {
        if index == 0 {
            Self::First
        } else {
            Self::Continuation
        }
    }

    pub fn is_continuation(self) -> bool {
        matches!(self, Self::Continuation)
    }

    pub fn apply(self, out: &mut LineBuilder<'_>) {
        if self.is_continuation() {
            out.mark_soft_wrap_continuation();
        }
    }
}

/// A visual row whose hard or soft relationship to the previous row is explicit.
pub struct WrappedSegment<'a, T> {
    value: &'a T,
    kind: WrappedSegmentKind,
}

impl<'a, T> WrappedSegment<'a, T> {
    pub fn emit<F>(self, out: &mut LineBuilder<'_>, emit: F)
    where
        F: FnOnce(&mut LineBuilder<'_>, &'a T, WrappedSegmentKind),
    {
        self.kind.apply(out);
        emit(out, self.value, self.kind);
    }

    pub fn emit_with_source<F>(self, out: &mut LineBuilder<'_>, source: &str, emit: F)
    where
        F: FnOnce(&mut LineBuilder<'_>, &'a T, WrappedSegmentKind),
    {
        if self.kind.is_continuation() {
            self.kind.apply(out);
        } else {
            out.set_source_text(source);
        }
        emit(out, self.value, self.kind);
    }
}

pub struct WrappedSegments<'a, T> {
    iter: std::iter::Enumerate<std::slice::Iter<'a, T>>,
}

impl<'a, T> Iterator for WrappedSegments<'a, T> {
    type Item = WrappedSegment<'a, T>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(|(index, value)| WrappedSegment {
            value,
            kind: WrappedSegmentKind::from_index(index),
        })
    }
}

/// Mark a logical line as wrapped and iterate its visual segments with explicit
/// hard/soft row semantics.
pub fn wrapped_segments<'a, T>(
    out: &mut LineBuilder<'_>,
    segments: &'a [T],
) -> WrappedSegments<'a, T> {
    if segments.len() > 1 {
        out.mark_wrapped();
    }
    WrappedSegments {
        iter: segments.iter().enumerate(),
    }
}

/// Outcome returned by [`LineBuilder::finish`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Outcome {
    pub line_count: usize,
    pub layout_width: u16,
    /// True when the layout wrapped at least one line; false means replayable at any width >= `max_line_width`.
    pub was_wrapped: bool,
    pub max_line_width: u16,
}

impl Outcome {
    pub fn is_valid_at(&self, new_width: u16) -> bool {
        if self.was_wrapped {
            new_width == self.layout_width
        } else {
            new_width >= self.max_line_width
        }
    }
}

pub struct LineBuilder<'a> {
    buf: &'a mut Buffer,
    theme: &'a Theme,
    layout_width: u16,

    // Per-line accumulator
    cur_text: String,
    replacement: Option<RenderedBufferRebuild>,
    cur_highlights: Vec<(u16, u16, HlGroup, SpanMeta)>,
    cur_decoration: LineDecoration,
    cur_visible_cols: u16,

    // Line counters
    starting_line: usize,
    lines_committed: usize,
    has_pending_content: bool,
    overwrote_blank_seed: bool,

    cur_group: Option<HlGroup>,
    cur_style: Style,
    style_stack: Vec<(Option<HlGroup>, Style)>,

    // Source-text plumbing
    pending_source_text: Option<String>,
    auto_soft_wrap_continuation: bool,

    // Outcome flags
    was_wrapped: bool,
    max_line_width: u16,
}

impl<'a> LineBuilder<'a> {
    pub fn new(buf: &'a mut Buffer, theme: &'a Theme, layout_width: u16) -> Self {
        // The first committed line replaces the trailing empty seed line when present.
        let starting_line = buf.line_count();
        let trailing_seed_blank = buf
            .get_line(starting_line.saturating_sub(1))
            .map(|s| s.is_empty())
            .unwrap_or(false);
        let starting_line = if trailing_seed_blank && starting_line > 0 {
            starting_line - 1
        } else {
            starting_line
        };
        Self {
            buf,
            theme,
            layout_width,
            cur_text: String::new(),
            replacement: None,
            cur_highlights: Vec::new(),
            cur_decoration: LineDecoration::default(),
            cur_visible_cols: 0,
            starting_line,
            lines_committed: 0,
            has_pending_content: false,
            overwrote_blank_seed: false,
            cur_group: None,
            cur_style: Style::default(),
            style_stack: Vec::new(),
            pending_source_text: None,
            auto_soft_wrap_continuation: false,
            was_wrapped: false,
            max_line_width: 0,
        }
    }

    /// Replace the buffer's rendered rows while reusing their text allocations.
    /// This is the retained-rendering constructor for buffers that are wholly
    /// owned by one layout node.
    pub fn replacing(buf: &'a mut Buffer, theme: &'a Theme, layout_width: u16) -> Self {
        let replacement = buf.begin_rendered_lines_rebuild();
        let mut builder = Self::new(buf, theme, layout_width);
        builder.replacement = Some(replacement);
        builder.cur_text = builder.take_replacement_line(0);
        builder
    }

    fn take_replacement_line(&mut self, row: usize) -> String {
        let mut line = self
            .replacement
            .as_mut()
            .and_then(|replacement| replacement.lines.get_mut(row))
            .map(std::mem::take)
            .unwrap_or_default();
        line.clear();
        line
    }

    fn install_replacement(&mut self) {
        let Some(mut replacement) = self.replacement.take() else {
            return;
        };
        replacement.lines.truncate(self.lines_committed);
        self.buf.finish_rendered_lines_rebuild(replacement);
    }

    pub fn theme(&self) -> &Theme {
        self.theme
    }

    /// The width this builder was constructed with. Renderers use this to drive
    /// wrap math and bg-fill so the layout sizes the buffer it actually writes
    /// into rather than a global terminal width.
    pub fn layout_width(&self) -> u16 {
        self.layout_width
    }

    /// Display width of the incomplete row, including graphemes completed by
    /// text appended across style and renderer boundaries.
    pub fn current_line_width(&self) -> u16 {
        self.cur_visible_cols
    }

    /// Number of leading bytes in `text` that complete a grapheme started by
    /// the current row. Returns zero when the append boundary is already a
    /// grapheme boundary.
    pub fn boundary_grapheme_prefix_len(&self, text: &str) -> usize {
        smelt_buffer::text::joining_grapheme_prefix_len(&self.cur_text, text)
    }

    /// Return the longest byte prefix of `text` that keeps the incomplete row
    /// within `max_cols`. The prefix ends on a grapheme boundary in the joined
    /// row, which can fall inside a grapheme as segmented in `text` alone.
    pub fn fitting_prefix_len(&self, text: &str, max_cols: u16) -> usize {
        if text.is_empty() || self.cur_visible_cols > max_cols {
            return 0;
        }
        if self.boundary_grapheme_prefix_len(text) == 0 {
            let mut cells = self.cur_visible_cols as usize;
            let mut fit = 0usize;
            for (start, grapheme) in cell_width::grapheme_indices(text) {
                cells = cells.saturating_add(display_width(grapheme));
                if cells > max_cols as usize {
                    break;
                }
                fit = start + grapheme.len();
            }
            return fit;
        }

        let boundary = self.cur_text.len();
        let mut joined = String::with_capacity(boundary + text.len());
        joined.push_str(&self.cur_text);
        joined.push_str(text);

        let mut cells = 0usize;
        let mut fit = 0usize;
        for (start, grapheme) in cell_width::grapheme_indices(&joined) {
            cells = cells.saturating_add(display_width(grapheme));
            let end = start + grapheme.len();
            if end <= boundary {
                continue;
            }
            if cells > max_cols as usize {
                break;
            }
            fit = end - boundary;
        }
        fit.min(text.len())
    }

    /// Commit any pending line and return rendering metadata.
    pub fn finish(mut self) -> Outcome {
        if self.has_pending_content || self.cur_decoration_present() || self.cur_visible_cols > 0 {
            self.commit_line();
        }
        self.install_replacement();
        Outcome {
            line_count: self.lines_committed,
            layout_width: self.layout_width,
            was_wrapped: self.was_wrapped,
            max_line_width: self.max_line_width,
        }
    }

    /// Lines accumulated so far, including the current incomplete line if it has content.
    pub fn line_count(&self) -> usize {
        self.lines_committed
            + if self.has_pending_content
                || self.cur_decoration_present()
                || self.cur_visible_cols > 0
            {
                1
            } else {
                0
            }
    }

    // ── Text emission ───────────────────────────────────────────────

    pub fn print(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.append_span_styled(text, SpanMeta::default());
    }

    pub fn print_string(&mut self, s: String) {
        self.print(&s);
    }

    pub fn print_with_meta(&mut self, text: &str, meta: SpanMeta) {
        if text.is_empty() {
            return;
        }
        self.append_span_styled(text, meta);
    }

    pub fn print_gutter(&mut self, text: &str) {
        self.print_with_meta(text, SpanMeta::unselectable());
    }

    pub fn newline(&mut self) {
        if let Some(src) = self.pending_source_text.take() {
            self.cur_decoration.source_text = Some(src);
        } else if self.auto_soft_wrap_continuation {
            self.cur_decoration.soft_wrapped = true;
            self.cur_decoration.copy_continuation = true;
        }
        if self.cur_visible_cols > self.max_line_width {
            self.max_line_width = self.cur_visible_cols;
        }
        self.commit_line();
    }

    // ── Per-line decorations ────────────────────────────────────────

    /// Mark the layout as width-pinned (not replayable at a different viewport width).
    pub fn mark_wrapped(&mut self) {
        self.was_wrapped = true;
    }

    /// Emit trailing spaces with `meta` and the currently active style until
    /// the row reaches `layout_width()`. The primitive for "fill row to the
    /// right edge with the active bg" - callers don't recompute trailing
    /// widths per row, and the cells inherit whatever style is set (bg, dim,
    /// …). Pass `SpanMeta::unselectable()` for chrome pad so
    /// cursor/selection/copy skip it.
    ///
    /// Use this when the row text needs the pad cells *in the buffer* (so
    /// inline highlights/selection cover them). For buffers whose text must
    /// stay clean - set the bg via `fill_line_bg` and let paint draw it.
    pub fn pad_row_to_layout_width(&mut self, meta: SpanMeta) {
        let remaining = self.layout_width.saturating_sub(self.cur_visible_cols) as usize;
        if remaining == 0 {
            return;
        }
        self.print_with_meta(&" ".repeat(remaining), meta);
    }

    /// Record a row-level bg fill: paint draws `bg` across the layout
    /// content region (same span as `pad_row_to_layout_width`) without
    /// writing trailing cells into the buffer text. Gutter and right-margin
    /// columns keep the row's cursor/normal style so chrome stays uniform
    /// across rows. Pick this when the buffer text needs to stay free of
    /// trailing pad (e.g. split-diff rows that are read back as logical
    /// content, or blank padding rows whose clipboard yank should be empty).
    pub fn fill_line_bg(&mut self, bg: Color) {
        debug_assert!(
            self.cur_decoration.fill_bg.is_none(),
            "fill_line_bg called twice on the same row"
        );
        self.cur_decoration.fill_bg = Some(bg);
    }

    pub fn mark_soft_wrap_continuation(&mut self) {
        self.cur_decoration.soft_wrapped = true;
        self.cur_decoration.copy_continuation = true;
    }

    /// Mark the current line as a copy continuation: it coalesces with the
    /// previous line during `copy_byte_range` (no newline, skipped if
    /// `source_text` was already emitted). Use this for table rows after the
    /// first so copy emits the raw markdown while each row remains a hard
    /// selection boundary.
    pub fn mark_copy_continuation(&mut self) {
        self.cur_decoration.copy_continuation = true;
    }

    /// Omit this decorative row on copy unless composition adds copyable content.
    pub fn exclude_from_copy(&mut self) {
        self.cur_decoration.copy_excluded = true;
    }

    /// Mark the current row as a chrome-delimited selection row. Double-click
    /// selection may choose the selectable run between neighboring
    /// non-selectable spans (for example, a table cell).
    pub fn mark_cell_selectable(&mut self) {
        self.cur_decoration.cell_selectable = true;
    }

    /// Mark the current row as part of a contiguous block-selectable structure.
    /// Triple-click selection may expand through adjacent rows with this bit.
    pub fn mark_block_selectable(&mut self) {
        self.cur_decoration.block_selectable = true;
    }

    /// Attach raw source text to the current line so copy emits markdown rather than display text.
    pub fn set_source_text(&mut self, text: &str) {
        self.cur_decoration.source_text = Some(text.to_string());
    }

    /// Attach alternate source text used when a selection spans outside the current copy group.
    pub fn set_external_source_text(&mut self, text: &str) {
        self.cur_decoration.external_source_text = Some(text.to_string());
    }

    /// Set `source_text` on the first row in `[start, end)` and `copy_continuation`
    /// on the remaining rows. Used by table renderers that want copy-coalescing
    /// without `soft_wrapped`.
    pub fn stamp_copy_group(&mut self, start: usize, source_text: &str) {
        let end = self.buf.line_count();
        if start >= end {
            return;
        }
        let mut first = self.buf.decoration_at(start).clone();
        first.source_text = Some(source_text.to_string());
        self.buf.set_decoration(start, first);
        for r in (start + 1)..end {
            let mut dec = self.buf.decoration_at(r).clone();
            dec.copy_continuation = true;
            self.buf.set_decoration(r, dec);
        }
    }

    /// Mark all rows emitted since `start` as a chrome-delimited selectable
    /// block. This is used by structured renderers such as Markdown tables.
    pub fn stamp_chrome_delimited_block(&mut self, start: usize) {
        let end = self.buf.line_count();
        for r in start..end {
            let mut dec = self.buf.decoration_at(r).clone();
            dec.cell_selectable = true;
            dec.block_selectable = true;
            self.buf.set_decoration(r, dec);
        }
    }

    /// Stamp the current line's logical source-line mapping. Gutter providers
    /// like `LineNumberGutter` read this to render per-row line numbers.
    pub fn set_source_line(&mut self, source_line: SourceLine) {
        self.cur_decoration.source_line = Some(source_line);
    }

    /// Stamp `head` on the first visual chunk of a wrapped logical line and
    /// `SourceLine::Synthetic` on every continuation chunk. Used by the syntax
    /// and diff renderers - `chunk_idx == 0` carries the lineno, the rest fill
    /// blank in the gutter so the column stays aligned.
    pub fn stamp_chunk(&mut self, chunk_idx: usize, head: SourceLine) {
        self.set_source_line(if chunk_idx == 0 {
            head
        } else {
            SourceLine::Synthetic
        });
    }

    /// Attach `source` to the next committed line; subsequent `newline()` calls become soft-wrap
    /// continuations until `disarm_source_text` is called.
    pub fn arm_source_text(&mut self, source: String) {
        self.pending_source_text = Some(source);
        self.auto_soft_wrap_continuation = true;
    }

    pub fn disarm_source_text(&mut self) {
        self.pending_source_text = None;
        self.auto_soft_wrap_continuation = false;
    }

    // ── Style state ─────────────────────────────────────────────────

    /// Push the current (group, style) onto the stack and merge the supplied pair into it.
    pub fn push(&mut self, group: Option<HlGroup>, style: Style) {
        self.style_stack.push((self.cur_group, self.cur_style));
        self.cur_group = group.or(self.cur_group);
        self.cur_style = merge_style(self.cur_style, style);
    }

    pub fn current_style(&self) -> Style {
        self.resolve_current()
    }

    /// Snapshot the current (group, style) onto the stack without changing it. Pair with
    /// `pop_style` to restore. Use this when emitting a run that mutates style state
    /// (e.g. per-region syntax colors) without clobbering what the caller had set.
    pub fn save_style(&mut self) {
        self.style_stack.push((self.cur_group, self.cur_style));
    }

    fn push_clone(&mut self) {
        self.save_style();
    }

    pub fn pop_style(&mut self) {
        if let Some((g, s)) = self.style_stack.pop() {
            self.cur_group = g;
            self.cur_style = s;
        }
    }

    pub fn reset_style(&mut self) {
        self.cur_group = None;
        self.cur_style = Style::default();
    }

    pub fn set_fg(&mut self, c: Color) {
        self.cur_style.fg = Some(c);
    }

    pub fn set_bg(&mut self, c: Color) {
        self.cur_style.bg = Some(c);
    }

    pub fn set_hl(&mut self, group: HlGroup) {
        self.cur_group = Some(group);
    }

    pub fn set_bold(&mut self) {
        self.cur_style.bold = true;
    }

    pub fn set_dim(&mut self) {
        self.cur_style.dim = true;
    }

    pub fn set_italic(&mut self) {
        self.cur_style.italic = true;
    }

    pub fn set_crossedout(&mut self) {
        self.cur_style.crossedout = true;
    }

    pub fn set_reverse(&mut self) {
        self.cur_style.reverse = true;
    }

    pub fn set_underline(&mut self) {
        self.cur_style.underline = true;
    }

    pub fn set_dim_italic(&mut self) {
        self.cur_style.dim = true;
        self.cur_style.italic = true;
    }

    pub fn push_fg(&mut self, c: Color) {
        self.push_clone();
        self.cur_style.fg = Some(c);
    }

    pub fn push_hl(&mut self, group: HlGroup) {
        self.push_clone();
        self.cur_group = Some(group);
    }

    pub fn push_bold(&mut self) {
        self.push_clone();
        self.cur_style.bold = true;
    }

    pub fn push_dim(&mut self) {
        self.push_clone();
        self.cur_style.dim = true;
    }

    pub fn push_italic(&mut self) {
        self.push_clone();
        self.cur_style.italic = true;
    }

    pub fn push_crossedout(&mut self) {
        self.push_clone();
        self.cur_style.crossedout = true;
    }

    pub fn push_reverse(&mut self) {
        self.push_clone();
        self.cur_style.reverse = true;
    }

    pub fn push_underline(&mut self) {
        self.push_clone();
        self.cur_style.underline = true;
    }

    // ── Internals ───────────────────────────────────────────────────

    fn append_span_styled(&mut self, text: &str, meta: SpanMeta) {
        let resolved = self.resolve_current();
        let style_default = style_is_default(&resolved);
        let meta_default = meta == SpanMeta::default();
        if style_default && meta_default {
            self.append_text(text);
            return;
        }
        let hl = self.current_hl(resolved);
        self.append_span_with_hl(text, hl, meta);
    }

    fn append_span_resolved(&mut self, text: &str, style: Style, meta: SpanMeta) {
        let style_default = style_is_default(&style);
        let meta_default = meta == SpanMeta::default();
        if style_default && meta_default {
            self.append_text(text);
            return;
        }
        let hl = intern_anonymous_style(style);
        self.append_span_with_hl(text, hl, meta);
    }

    fn append_text(&mut self, text: &str) {
        let old_len = self.cur_text.len();
        let old_cols = self.cur_visible_cols;
        self.cur_text.push_str(text);
        self.cur_visible_cols = display_width(&self.cur_text).min(u16::MAX as usize) as u16;
        if text.is_empty() {
            return;
        }
        self.has_pending_content = true;

        let boundary_cols = cell_width::grapheme_indices(&self.cur_text)
            .find(|(byte, grapheme)| *byte < old_len && *byte + grapheme.len() > old_len)
            .map(|(byte, grapheme)| {
                display_width(&self.cur_text[..byte + grapheme.len()]).min(u16::MAX as usize) as u16
            });
        if let Some(boundary_cols) = boundary_cols {
            self.resize_trailing_highlight(old_cols, boundary_cols);
        }
    }

    fn append_span_with_hl(&mut self, text: &str, hl: HlGroup, meta: SpanMeta) {
        let old_len = self.cur_text.len();
        let old_cols = self.cur_visible_cols;
        self.cur_text.push_str(text);
        let cols_after = display_width(&self.cur_text).min(u16::MAX as usize) as u16;
        self.cur_visible_cols = cols_after;
        if text.is_empty() {
            return;
        }
        self.has_pending_content = true;

        // A style boundary can land inside a grapheme when streaming or when
        // independently styled producers emit its scalars. The complete
        // cluster belongs to the style of its first scalar, so begin this
        // highlight at the next grapheme boundary.
        let mut style_byte_start = old_len;
        for (byte, grapheme) in cell_width::grapheme_indices(&self.cur_text) {
            let end = byte + grapheme.len();
            if byte < old_len && end > old_len {
                style_byte_start = end;
                break;
            }
            if byte >= old_len {
                break;
            }
        }
        let cols_before =
            display_width(&self.cur_text[..style_byte_start]).min(u16::MAX as usize) as u16;

        // Completing a boundary grapheme can expand or contract its width.
        // Keep the preceding style attached to the complete terminal glyph.
        self.resize_trailing_highlight(old_cols, cols_before);
        if cols_after == cols_before {
            return;
        }
        // Coalesce with the previous highlight if it has the same
        // hl+meta and was contiguous.
        if let Some(last) = self.cur_highlights.last_mut() {
            if last.1 == cols_before && last.2 == hl && last.3 == meta {
                last.1 = cols_after;
                return;
            }
        }
        self.cur_highlights
            .push((cols_before, cols_after, hl, meta));
    }

    fn resize_trailing_highlight(&mut self, old_end: u16, new_end: u16) {
        if old_end == new_end {
            return;
        }
        let mut remove_empty = false;
        if let Some(last) = self.cur_highlights.last_mut() {
            if last.1 == old_end {
                last.1 = new_end;
                remove_empty = last.0 >= last.1;
            }
        }
        if remove_empty {
            self.cur_highlights.pop();
        }
    }

    /// Map the active (group, style) to an interned [`HlGroup`].
    /// A plain group reference flows the id directly (live theme updates); compound styles intern anonymously.
    fn current_hl(&self, resolved: Style) -> HlGroup {
        if let Some(group) = self.cur_group {
            if !style_has_axis_mods(&self.cur_style) && self.theme.contains(group) {
                return group;
            }
        }
        intern_anonymous_style(resolved)
    }

    fn commit_line(&mut self) {
        let target_row = self.starting_line + self.lines_committed;
        let mut decoration = std::mem::take(&mut self.cur_decoration);
        // LineBuilder output is intrinsically pre-formatted: callers (parsers,
        // markdown, code, diff) have already laid this row out at the chosen
        // width. The host window's `WrappedLayout` keys off this so it doesn't
        // re-wrap parser-produced rows.
        decoration.pre_formatted = true;

        if self.replacement.is_some() {
            let text = std::mem::take(&mut self.cur_text);
            let replacement = self
                .replacement
                .as_mut()
                .expect("replacement checked above");
            if target_row < replacement.lines.len() {
                replacement.lines[target_row] = text;
            } else {
                debug_assert_eq!(target_row, replacement.lines.len());
                replacement.lines.push(text);
            }
            for (col_start, col_end, hl, meta) in self.cur_highlights.drain(..) {
                replacement
                    .metadata
                    .push_highlight(target_row, col_start, col_end, hl, meta);
            }
            replacement.metadata.push_decoration(target_row, decoration);
            self.lines_committed += 1;
            self.cur_text = self.take_replacement_line(self.lines_committed);
        } else {
            let text = std::mem::take(&mut self.cur_text);
            let buf_len = self.buf.line_count();
            if target_row < buf_len {
                self.buf.set_lines(target_row, target_row + 1, vec![text]);
                if target_row == self.starting_line && !self.overwrote_blank_seed {
                    self.overwrote_blank_seed = true;
                }
            } else {
                self.buf.set_lines(buf_len, buf_len, vec![text]);
            }

            for (col_start, col_end, hl, meta) in self.cur_highlights.drain(..) {
                self.buf
                    .add_highlight_group_with_meta(target_row, col_start, col_end, hl, meta);
            }
            self.buf.set_decoration(target_row, decoration);
            self.lines_committed += 1;
        }

        self.has_pending_content = false;
        self.cur_visible_cols = 0;
    }

    fn cur_decoration_present(&self) -> bool {
        has_decoration(&self.cur_decoration)
    }

    fn resolve_current(&self) -> Style {
        let (group_fg, group_bg) = match self.cur_group {
            Some(g) => {
                let s = self.theme.resolve(g);
                // Unregistered group: force Color::Reset so the extmark bypasses the `style_is_default` short-circuit.
                let fg = s.fg.or(if s.bg.is_none() {
                    Some(Color::Reset)
                } else {
                    None
                });
                (fg, s.bg)
            }
            None => (None, None),
        };
        Style {
            fg: self.cur_style.fg.or(group_fg),
            bg: self.cur_style.bg.or(group_bg),
            bold: self.cur_style.bold,
            dim: self.cur_style.dim,
            italic: self.cur_style.italic,
            underline: self.cur_style.underline,
            crossedout: self.cur_style.crossedout,
            reverse: self.cur_style.reverse,
        }
    }
}

fn merge_style(parent: Style, child: Style) -> Style {
    Style {
        fg: child.fg.or(parent.fg),
        bg: child.bg.or(parent.bg),
        bold: parent.bold || child.bold,
        dim: parent.dim || child.dim,
        italic: parent.italic || child.italic,
        underline: parent.underline || child.underline,
        crossedout: parent.crossedout || child.crossedout,
        reverse: parent.reverse || child.reverse,
    }
}

fn style_has_axis_mods(s: &Style) -> bool {
    s.fg.is_some()
        || s.bg.is_some()
        || s.bold
        || s.dim
        || s.italic
        || s.underline
        || s.crossedout
        || s.reverse
}

fn has_decoration(dec: &LineDecoration) -> bool {
    dec.fill_bg.is_some()
        || dec.soft_wrapped
        || dec.cell_selectable
        || dec.block_selectable
        || dec.copy_continuation
        || dec.source_text.is_some()
}

fn style_is_default(s: &Style) -> bool {
    s.fg.is_none()
        && s.bg.is_none()
        && !s.bold
        && !s.dim
        && !s.italic
        && !s.underline
        && !s.crossedout
        && !s.reverse
}

/// Build a fresh `Buffer`, render into it, and return the outcome.
pub fn render_into_fresh(
    width: u16,
    theme: &Theme,
    fill: impl FnOnce(&mut LineBuilder),
) -> (Buffer, Outcome) {
    use crate::buffer::{BufCreateOpts, BufId};
    let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
    let outcome = render_into(&mut buf, width, theme, fill);
    (buf, outcome)
}

pub fn render_into(
    buf: &mut Buffer,
    width: u16,
    theme: &Theme,
    fill: impl FnOnce(&mut LineBuilder),
) -> Outcome {
    let mut col = LineBuilder::new(buf, theme, width);
    fill(&mut col);
    col.finish()
}

/// Replay a previously-rendered `Buffer` into `out` as styled spans.
pub fn replay_buffer_into(buf: &Buffer, out: &mut LineBuilder) {
    let n = buf.line_count();
    for i in 0..n {
        replay_buffer_row_into(buf, i as u16, out);
        out.newline();
    }
    let _ = buf;
}

/// Replay one row of `buf` into `out` as styled spans without a trailing newline.
pub fn replay_buffer_row_into(buf: &Buffer, row: u16, out: &mut LineBuilder) {
    let text = buf.get_line(row as usize).unwrap_or("");
    let mut highlights = buf.highlights_at(row as usize);
    highlights.sort_by_key(|h| h.col_start);

    let mut col_idx: u16 = 0;
    for h in &highlights {
        if h.col_end <= col_idx {
            continue;
        }
        if h.col_start > col_idx {
            let plain = text::slice_cells(text, col_idx as usize, h.col_start as usize).to_string();
            out.print(&plain);
            col_idx = h.col_start;
        }
        let end = h.col_end.min(display_width(text) as u16);
        if end <= col_idx {
            continue;
        }
        let segment = text::slice_cells(text, col_idx as usize, end as usize).to_string();
        let style = out.theme.resolve(h.hl);
        out.append_resolved_span(&segment, style, h.meta.clone());
        col_idx = end;
    }
    if (col_idx as usize) < display_width(text) {
        let tail = text::slice_cells(text, col_idx as usize, display_width(text)).to_string();
        out.print(&tail);
    }
}

impl<'a> LineBuilder<'a> {
    /// Append a span with a pre-resolved style (no theme lookup). Used by replay paths.
    pub fn append_resolved_span(&mut self, text: &str, style: Style, meta: SpanMeta) {
        if text.is_empty() {
            return;
        }
        self.append_span_resolved(text, style, meta);
    }
}

pub mod test_util {
    use super::*;
    use crate::buffer::{BufCreateOpts, BufId};

    pub struct TestSpan {
        pub text: String,
        pub style: Style,
        pub meta: SpanMeta,
    }

    pub struct TestLine {
        pub text: String,
        pub source_text: Option<String>,
        pub external_source_text: Option<String>,
        pub soft_wrapped: bool,
        pub cell_selectable: bool,
        pub block_selectable: bool,
        pub copy_continuation: bool,
        pub spans: Vec<TestSpan>,
    }

    pub struct TestBlock {
        pub lines: Vec<TestLine>,
        pub outcome: Outcome,
    }

    pub fn render_test(width: u16, fill: impl FnOnce(&mut LineBuilder)) -> TestBlock {
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        let outcome = render_into(&mut buf, width, &theme, fill);
        let lines = read_buffer(&buf, &theme, outcome.line_count);
        TestBlock { lines, outcome }
    }

    pub fn read_buffer(buf: &Buffer, theme: &Theme, line_count: usize) -> Vec<TestLine> {
        let n = line_count.min(buf.line_count());
        (0..n)
            .map(|i| {
                let text = buf.get_line(i).unwrap_or("").to_string();
                let dec = buf.decoration_at(i).clone();
                let mut highlights = buf.highlights_at(i);
                highlights.sort_by_key(|h| h.col_start);
                let mut spans = Vec::new();
                let mut col: u16 = 0;
                let width = display_width(&text) as u16;
                for h in &highlights {
                    if h.col_end <= col {
                        continue;
                    }
                    if h.col_start > col {
                        let plain = text::slice_cells(&text, col as usize, h.col_start as usize)
                            .to_string();
                        spans.push(TestSpan {
                            text: plain,
                            style: Style::default(),
                            meta: SpanMeta::default(),
                        });
                        col = h.col_start;
                    }
                    let end = h.col_end.min(width);
                    if end <= col {
                        continue;
                    }
                    let segment = text::slice_cells(&text, col as usize, end as usize).to_string();
                    let style = theme.resolve(h.hl);
                    spans.push(TestSpan {
                        text: segment,
                        style,
                        meta: h.meta.clone(),
                    });
                    col = end;
                }
                if col < width {
                    let tail = text::slice_cells(&text, col as usize, width as usize).to_string();
                    spans.push(TestSpan {
                        text: tail,
                        style: Style::default(),
                        meta: SpanMeta::default(),
                    });
                }
                TestLine {
                    text,
                    source_text: dec.source_text,
                    external_source_text: dec.external_source_text,
                    soft_wrapped: dec.soft_wrapped,
                    cell_selectable: dec.cell_selectable,
                    block_selectable: dec.block_selectable,
                    copy_continuation: dec.copy_continuation,
                    spans,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{BufCreateOpts, BufId};
    use crate::theme::intern;

    fn fresh_buf() -> Buffer {
        Buffer::new(BufId(0), BufCreateOpts::default())
    }

    #[test]
    fn display_width_counts_visible_columns() {
        assert_eq!(display_width(""), 0);
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("日本"), 4);
    }

    #[test]
    fn styled_prints_keep_cross_run_graphemes_atomic() {
        let block = test_util::render_test(80, |out| {
            out.push_fg(Color::Red);
            out.print("9");
            out.pop_style();
            out.push_fg(Color::Blue);
            out.print("\u{fe0f}");
            out.pop_style();
            out.print("X");
            out.newline();
        });

        assert_eq!(block.lines[0].text, "9\u{fe0f}X");
        assert_eq!(block.lines[0].spans[0].text, "9\u{fe0f}");
        assert_eq!(block.lines[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(block.lines[0].spans[1].text, "X");
        assert_eq!(block.outcome.max_line_width, 3);
    }

    #[test]
    fn styled_prints_use_first_scalar_style_for_complex_graphemes() {
        let block = test_util::render_test(80, |out| {
            for parts in [
                ("e", "\u{301}", ""),
                ("👩", "\u{200d}", "💻"),
                ("🇨", "🇦", ""),
            ] {
                out.push_fg(Color::Red);
                out.print(parts.0);
                out.pop_style();
                out.push_fg(Color::Blue);
                out.print(parts.1);
                out.pop_style();
                out.push_fg(Color::Green);
                out.print(parts.2);
                out.pop_style();
                out.print("X");
                out.newline();
            }
        });

        for (line, grapheme) in block.lines.iter().zip(["e\u{301}", "👩\u{200d}💻", "🇨🇦"])
        {
            assert_eq!(line.spans[0].text, grapheme);
            assert_eq!(line.spans[0].style.fg, Some(Color::Red));
            assert_eq!(line.spans[1].text, "X");
            assert_eq!(line.spans[1].style, Style::default());
        }
    }

    #[test]
    fn completing_grapheme_repairs_contracting_highlight_width() {
        assert_eq!(display_width("⌚"), 2);
        assert_eq!(display_width("⌚\u{fe0e}"), 1);
        let block = test_util::render_test(80, |out| {
            out.push_fg(Color::Red);
            out.print("⌚");
            out.pop_style();
            out.print("\u{fe0e}");
            out.print("X");
            out.newline();
        });

        assert_eq!(block.lines[0].spans[0].text, "⌚\u{fe0e}");
        assert_eq!(block.lines[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(block.lines[0].spans[1].text, "X");
        assert_eq!(block.lines[0].spans[1].style, Style::default());
        assert_eq!(block.outcome.max_line_width, 2);
    }

    #[test]
    fn fitting_prefix_len_respects_clipping_and_joined_graphemes() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let mut lb = LineBuilder::new(&mut buf, &theme, 80);
        lb.print("ab");
        assert_eq!(lb.fitting_prefix_len("c中d", 4), "c".len());

        let cases = [
            ("e", "\u{301}x", 1, "\u{301}".len()),
            ("9", "\u{fe0f}x", 2, "\u{fe0f}".len()),
            ("👩", "\u{200d}💻x", 2, "\u{200d}💻".len()),
            ("👩\u{200d}", "💻x", 2, "💻".len()),
            ("🇨", "🇦x", 2, "🇦".len()),
            ("\u{600}", " x", 2, " ".len()),
            ("\r", "\nx", 1, "\n".len()),
        ];
        for (current, text, max_cols, expected) in cases {
            let mut buf = fresh_buf();
            let mut lb = LineBuilder::new(&mut buf, &theme, 80);
            lb.print(current);
            assert_eq!(
                lb.boundary_grapheme_prefix_len(text),
                expected,
                "current={current:?} text={text:?}"
            );
            assert_eq!(
                lb.fitting_prefix_len(text, max_cols),
                expected,
                "current={current:?} text={text:?}"
            );
        }
    }

    #[test]
    fn plain_action_metadata_is_not_discarded() {
        let action = crate::buffer::SpanAction::OpenUrl("https://example.com".into());
        let block = test_util::render_test(80, |out| {
            out.print_with_meta("link", SpanMeta::action(action.clone()));
        });

        assert_eq!(block.lines[0].spans[0].meta.action, Some(action));
    }

    #[test]
    fn outcome_is_valid_at_pinned_when_wrapped() {
        let o = Outcome {
            line_count: 1,
            layout_width: 40,
            was_wrapped: true,
            max_line_width: 20,
        };
        assert!(o.is_valid_at(40));
        assert!(!o.is_valid_at(39));
        assert!(!o.is_valid_at(41));
    }

    #[test]
    fn outcome_is_valid_at_replayable_when_not_wrapped() {
        let o = Outcome {
            line_count: 1,
            layout_width: 40,
            was_wrapped: false,
            max_line_width: 20,
        };
        assert!(o.is_valid_at(20));
        assert!(o.is_valid_at(80));
        assert!(!o.is_valid_at(19));
    }

    #[test]
    fn line_count_zero_when_no_content() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let lb = LineBuilder::new(&mut buf, &theme, 80);
        assert_eq!(lb.line_count(), 0);
    }

    #[test]
    fn line_count_one_when_pending_text() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let mut lb = LineBuilder::new(&mut buf, &theme, 80);
        lb.print("hi");
        assert_eq!(lb.line_count(), 1);
    }

    #[test]
    fn line_count_one_when_decoration_present_but_no_text() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let mut lb = LineBuilder::new(&mut buf, &theme, 80);
        lb.fill_line_bg(Color::Red);
        assert_eq!(lb.line_count(), 1);
    }

    #[test]
    fn replacing_reuses_line_text_allocations() {
        let theme = Theme::default();
        let mut buf = fresh_buf();
        let mut line = String::with_capacity(128);
        line.push_str("old retained row");
        buf.set_all_lines(vec![line]);
        let old_ptr = buf.lines()[0].as_ptr();
        let old_capacity = buf.lines()[0].capacity();

        let mut out = LineBuilder::replacing(&mut buf, &theme, 80);
        out.print("new row");
        let outcome = out.finish();

        assert_eq!(outcome.line_count, 1);
        assert_eq!(buf.lines(), &["new row"]);
        assert_eq!(buf.lines()[0].as_ptr(), old_ptr);
        assert_eq!(buf.lines()[0].capacity(), old_capacity);
    }

    #[test]
    fn replacing_removes_surplus_rows() {
        let theme = Theme::default();
        let mut buf = fresh_buf();
        buf.set_all_lines(vec!["one".into(), "two".into(), "three".into()]);

        let mut out = LineBuilder::replacing(&mut buf, &theme, 80);
        out.print("only");
        let outcome = out.finish();

        assert_eq!(outcome.line_count, 1);
        assert_eq!(buf.lines(), &["only"]);
    }

    #[test]
    fn replacing_clears_old_highlights_and_decorations() {
        let mut theme = Theme::default();
        theme.set(
            "ReplaceBuilderHighlight",
            Style {
                fg: Some(Color::Red),
                ..Style::default()
            },
        );
        let group = theme.id_for("ReplaceBuilderHighlight");
        let mut buf = fresh_buf();
        let mut initial = LineBuilder::new(&mut buf, &theme, 80);
        initial.set_hl(group);
        initial.set_source_text("old source");
        initial.print("old row");
        initial.newline();
        initial.finish();
        assert!(!buf.highlights_at(0).is_empty());
        assert_eq!(
            buf.decoration_at(0).source_text.as_deref(),
            Some("old source")
        );

        let mut replacement = LineBuilder::replacing(&mut buf, &theme, 80);
        replacement.print("plain row");
        replacement.finish();

        assert!(buf.highlights_at(0).is_empty());
        assert_eq!(buf.decoration_at(0).source_text, None);
        assert!(!buf.decoration_at(0).soft_wrapped);
        assert_eq!(buf.get_line(0), Some("plain row"));
    }

    #[test]
    fn empty_replacement_keeps_buffer_seed_line() {
        let theme = Theme::default();
        let mut buf = fresh_buf();
        buf.set_all_lines(vec!["old".into(), "rows".into()]);

        let outcome = LineBuilder::replacing(&mut buf, &theme, 80).finish();

        assert_eq!(outcome.line_count, 0);
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.get_line(0), Some(""));
        assert!(buf.highlights_at(0).is_empty());
    }

    #[test]
    fn print_empty_string_is_noop() {
        let block = test_util::render_test(80, |out| {
            out.print("");
        });
        assert_eq!(block.outcome.line_count, 0);
    }

    #[test]
    fn print_with_meta_attaches_custom_copy_as() {
        let block = test_util::render_test(80, |out| {
            out.print_with_meta("click", SpanMeta::copy_as("real"));
        });
        assert_eq!(block.lines.len(), 1);
        let meta_copy = block.lines[0]
            .spans
            .iter()
            .find_map(|s| s.meta.copy_as.clone());
        assert_eq!(meta_copy.as_deref(), Some("real"));
    }

    #[test]
    fn mark_soft_wrap_continuation_sets_decoration_flag() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let mut lb = LineBuilder::new(&mut buf, &theme, 80);
        lb.mark_soft_wrap_continuation();
        assert!(lb.cur_decoration.soft_wrapped);
    }

    #[test]
    fn wrapped_segments_stamp_continuations_during_emission() {
        let block = test_util::render_test(80, |out| {
            let segments = ["first", "second"];
            for segment in wrapped_segments(out, &segments) {
                segment.emit(out, |out, text, _| out.print(text));
                out.newline();
            }
        });

        assert!(block.outcome.was_wrapped);
        assert!(!block.lines[0].soft_wrapped);
        assert!(block.lines[1].soft_wrapped);
        assert!(block.lines[1].copy_continuation);
    }

    #[test]
    fn set_source_text_attaches_raw_markdown() {
        let block = test_util::render_test(80, |out| {
            out.print("rendered");
            out.set_source_text("**rendered**");
            out.newline();
        });
        assert_eq!(block.lines[0].source_text.as_deref(), Some("**rendered**"));
    }

    #[test]
    fn arm_source_text_attaches_then_marks_subsequent_wraps_as_continuations() {
        let block = test_util::render_test(80, |out| {
            out.arm_source_text("source line".into());
            out.print("first");
            out.newline();
            out.print("second");
            out.newline();
            out.disarm_source_text();
            out.print("third");
            out.newline();
        });
        assert_eq!(block.lines.len(), 3);
        assert_eq!(block.lines[0].source_text.as_deref(), Some("source line"));
        // Second line: no fresh source, but armed continuation flag set.
        assert_eq!(block.lines[1].source_text, None);
        assert!(block.lines[1].soft_wrapped);
        // Third line: disarmed, so no continuation flag.
        assert!(!block.lines[2].soft_wrapped);
    }

    #[test]
    fn disarm_source_text_clears_pending_and_continuation() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let mut lb = LineBuilder::new(&mut buf, &theme, 80);
        lb.arm_source_text("x".into());
        lb.disarm_source_text();
        assert!(lb.pending_source_text.is_none());
        assert!(!lb.auto_soft_wrap_continuation);
    }

    #[test]
    fn set_style_helpers_set_corresponding_flags() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let mut lb = LineBuilder::new(&mut buf, &theme, 80);
        lb.set_bold();
        assert!(lb.cur_style.bold);
        lb.set_italic();
        assert!(lb.cur_style.italic);
        lb.set_dim();
        assert!(lb.cur_style.dim);
        lb.set_crossedout();
        assert!(lb.cur_style.crossedout);
        lb.reset_style();
        lb.set_dim_italic();
        assert!(lb.cur_style.dim);
        assert!(lb.cur_style.italic);
        assert!(!lb.cur_style.bold);
    }

    #[test]
    fn push_helpers_save_state_and_apply_modification() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let mut lb = LineBuilder::new(&mut buf, &theme, 80);
        lb.push_fg(Color::Red);
        assert_eq!(lb.cur_style.fg, Some(Color::Red));
        lb.pop_style();
        assert_eq!(lb.cur_style.fg, None);

        lb.push_hl(intern("SomeGroup"));
        assert!(lb.cur_group.is_some());
        lb.pop_style();
        assert!(lb.cur_group.is_none());

        lb.push_bold();
        lb.push_italic();
        lb.push_dim();
        lb.push_crossedout();
        assert!(lb.cur_style.bold);
        assert!(lb.cur_style.italic);
        assert!(lb.cur_style.dim);
        assert!(lb.cur_style.crossedout);
        for _ in 0..4 {
            lb.pop_style();
        }
        assert_eq!(lb.cur_style, Style::default());
    }

    #[test]
    fn pop_style_on_empty_stack_is_noop() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let mut lb = LineBuilder::new(&mut buf, &theme, 80);
        // Should not panic with no pushes.
        lb.pop_style();
        assert_eq!(lb.cur_style, Style::default());
        assert!(lb.cur_group.is_none());
    }

    #[test]
    fn set_hl_assigns_group_for_subsequent_prints() {
        let block = test_util::render_test(80, |out| {
            let g = intern("MyGroup");
            out.set_hl(g);
            out.print("x");
            out.reset_style();
            out.newline();
        });
        // The group passes through as a highlight on the span.
        let has_hl = block.lines[0].spans.iter().any(|s| !s.text.is_empty());
        assert!(has_hl);
    }

    #[test]
    fn render_into_fresh_returns_buffer_and_outcome() {
        let theme = Theme::default();
        let (buf, outcome) = render_into_fresh(80, &theme, |out| {
            out.print("hello");
            out.newline();
        });
        assert_eq!(outcome.line_count, 1);
        assert!(!outcome.was_wrapped);
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.get_line(0), Some("hello"));
    }

    #[test]
    fn replay_buffer_into_reproduces_text_lines() {
        let theme = Theme::default();
        let (src_buf, _) = render_into_fresh(80, &theme, |out| {
            out.print("line one");
            out.newline();
            out.set_hl(intern("MyGroup"));
            out.print("line two");
            out.reset_style();
            out.newline();
        });
        let block = test_util::render_test(80, |out| {
            replay_buffer_into(&src_buf, out);
        });
        let texts: Vec<&str> = block.lines.iter().map(|l| l.text.as_str()).collect();
        assert!(texts.contains(&"line one"));
        assert!(texts.contains(&"line two"));
    }

    #[test]
    fn replay_buffer_row_into_emits_one_row_without_trailing_newline() {
        let theme = Theme::default();
        let (src_buf, _) = render_into_fresh(80, &theme, |out| {
            out.print("only one");
            out.newline();
        });
        let block = test_util::render_test(80, |out| {
            replay_buffer_row_into(&src_buf, 0, out);
        });
        assert!(!block.lines.is_empty());
        assert_eq!(block.lines[0].text, "only one");
    }

    #[test]
    fn finish_no_content_returns_empty_outcome() {
        let mut buf = fresh_buf();
        let theme = Theme::default();
        let lb = LineBuilder::new(&mut buf, &theme, 80);
        let outcome = lb.finish();
        assert_eq!(outcome.line_count, 0);
        assert_eq!(outcome.layout_width, 80);
        assert!(!outcome.was_wrapped);
    }

    #[test]
    fn finish_flushes_trailing_pending_content_as_one_line() {
        let block = test_util::render_test(80, |out| {
            out.print("no trailing newline");
        });
        assert_eq!(block.outcome.line_count, 1);
        assert_eq!(block.lines[0].text, "no trailing newline");
    }

    #[test]
    fn newline_after_styled_text_records_max_line_width() {
        let block = test_util::render_test(80, |out| {
            out.print("hello");
            out.newline();
            out.print("hi");
            out.newline();
        });
        assert!(block.outcome.max_line_width >= 5);
    }

    #[test]
    fn current_hl_uses_registered_group_directly_without_axis_mods() {
        // Register the group with a real bg so theme.contains() is true.
        let mut theme = Theme::default();
        theme.set(
            "__BuilderTestGroup_xyz__",
            Style {
                bg: Some(Color::Red),
                ..Default::default()
            },
        );
        let group = theme.id_for("__BuilderTestGroup_xyz__");
        let mut buf = fresh_buf();
        let mut lb = LineBuilder::new(&mut buf, &theme, 80);
        lb.set_hl(group);
        lb.print("x");
        lb.newline();
        let outcome = lb.finish();
        assert_eq!(outcome.line_count, 1);
    }
}
