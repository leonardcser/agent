use crate::BufId;
use crossterm::style::Color;
use std::sync::Arc;

/// Formatter plugged into a `Buffer` to turn a plain-text `source`
/// into styled lines + decorations at a given terminal width.
///
/// Host crates implement this trait for every "content kind" a buffer
/// can display (markdown, bash, syntax-highlighted file, inline diff,
/// plain wrap, …). The `ui` crate knows nothing about any specific
/// format — it just calls `render` when the source or width change.
///
/// The formatter's `render` is free to call the usual mutators on
/// `Buffer` (`set_all_lines`, `add_highlight`, `set_decoration`, …):
/// the buffer's `source` is read-only from the formatter's point of
/// view and lives on `Buffer` untouched across renders.
pub trait BufferFormatter: Send + Sync {
    fn render(&self, buf: &mut Buffer, source: &str, width: u16);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub col_start: u16,
    pub col_end: u16,
    pub style: SpanStyle,
    pub meta: SpanMeta,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpanMeta {
    pub selectable: bool,
    pub copy_as: Option<String>,
}

impl SpanMeta {
    pub fn selectable() -> Self {
        Self {
            selectable: true,
            copy_as: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LineDecoration {
    pub gutter_bg: Option<Color>,
    pub fill_bg: Option<Color>,
    pub fill_right_margin: u16,
    pub soft_wrapped: bool,
    pub source_text: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpanStyle {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
}

impl SpanStyle {
    pub fn fg(color: Color) -> Self {
        Self {
            fg: Some(color),
            ..Default::default()
        }
    }

    pub fn dim() -> Self {
        Self {
            dim: true,
            ..Default::default()
        }
    }

    pub fn bold() -> Self {
        Self {
            bold: true,
            ..Default::default()
        }
    }

    pub fn bg(color: Color) -> Self {
        Self {
            bg: Some(color),
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BufType {
    Normal,
    Nofile,
    Prompt,
    Scratch,
}

#[derive(Clone, Debug)]
pub struct VirtualText {
    pub line: usize,
    pub col: usize,
    pub text: String,
    pub hl_group: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Mark {
    pub line: usize,
    pub col: usize,
}

pub struct BufCreateOpts {
    pub modifiable: bool,
    pub buftype: BufType,
}

impl Default for BufCreateOpts {
    fn default() -> Self {
        Self {
            modifiable: true,
            buftype: BufType::Normal,
        }
    }
}

#[derive(Clone)]
pub struct Buffer {
    pub(crate) id: BufId,
    /// `Arc`-wrapped so `Buffer::clone()` and sync-to-view become
    /// refcount bumps; mutators use `Arc::make_mut` which only deep-
    /// copies when the Arc is actually shared.
    lines: Arc<Vec<String>>,
    highlights: Arc<Vec<Vec<Span>>>,
    decorations: Arc<Vec<LineDecoration>>,
    modifiable: bool,
    buftype: BufType,
    virtual_text: Vec<VirtualText>,
    marks: std::collections::HashMap<String, Mark>,
    changedtick: u64,
    /// When set, `source` drives the visible lines: the formatter
    /// re-renders into this buffer lazily when `ensure_rendered_at`
    /// is called with a different `(source_tick, width)` than the
    /// last render.
    formatter: Option<Arc<dyn BufferFormatter>>,
    source: String,
    source_tick: u64,
    last_render: Option<(u64, u16)>,
}

impl Buffer {
    pub fn new(id: BufId, opts: BufCreateOpts) -> Self {
        Self {
            id,
            lines: Arc::new(vec![String::new()]),
            highlights: Arc::new(vec![Vec::new()]),
            decorations: Arc::new(vec![LineDecoration::default()]),
            modifiable: opts.modifiable,
            buftype: opts.buftype,
            virtual_text: Vec::new(),
            marks: std::collections::HashMap::new(),
            changedtick: 0,
            formatter: None,
            source: String::new(),
            source_tick: 0,
            last_render: None,
        }
    }

    /// Install a formatter. The next `ensure_rendered_at` call
    /// re-renders from the current `source`. Replaces any prior
    /// formatter.
    pub fn set_formatter(&mut self, formatter: Arc<dyn BufferFormatter>) {
        self.formatter = Some(formatter);
        self.last_render = None;
    }

    /// Builder equivalent of `set_formatter`.
    pub fn with_formatter(mut self, formatter: Arc<dyn BufferFormatter>) -> Self {
        self.set_formatter(formatter);
        self
    }

    pub fn has_formatter(&self) -> bool {
        self.formatter.is_some()
    }

    /// Update the source driving the formatter. The next
    /// `ensure_rendered_at` will re-render; without a formatter
    /// attached, the source is held but never consulted.
    pub fn set_source(&mut self, source: String) {
        if source == self.source {
            return;
        }
        self.source = source;
        self.source_tick = self.source_tick.wrapping_add(1);
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Re-run the formatter if `(source, width)` differs from the last
    /// render. No-op without a formatter or when nothing changed.
    /// Returns `true` when a render actually happened.
    pub fn ensure_rendered_at(&mut self, width: u16) -> bool {
        let Some(formatter) = self.formatter.clone() else {
            return false;
        };
        let fresh = match self.last_render {
            Some((tick, w)) => tick == self.source_tick && w == width,
            None => false,
        };
        if fresh {
            return false;
        }
        let source = std::mem::take(&mut self.source);
        formatter.render(self, &source, width);
        self.source = source;
        self.last_render = Some((self.source_tick, width));
        true
    }

    pub fn id(&self) -> BufId {
        self.id
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn get_lines(&self, start: usize, end: usize) -> &[String] {
        let end = end.min(self.lines.len());
        let start = start.min(end);
        &self.lines[start..end]
    }

    pub fn get_line(&self, idx: usize) -> Option<&str> {
        self.lines.get(idx).map(|s| s.as_str())
    }

    pub fn set_lines(&mut self, start: usize, end: usize, replacement: Vec<String>) {
        let end = end.min(self.lines.len());
        let start = start.min(end);
        let new_count = replacement.len();
        let lines = Arc::make_mut(&mut self.lines);
        lines.splice(start..end, replacement);
        let lines_empty = lines.is_empty();
        let empty_spans: Vec<Vec<Span>> = vec![Vec::new(); new_count];
        let highlights = Arc::make_mut(&mut self.highlights);
        let hl_end = end.min(highlights.len());
        let hl_start = start.min(hl_end);
        highlights.splice(hl_start..hl_end, empty_spans);
        let decorations = Arc::make_mut(&mut self.decorations);
        let dec_end = end.min(decorations.len());
        let dec_start = start.min(dec_end);
        decorations.splice(
            dec_start..dec_end,
            std::iter::repeat_with(LineDecoration::default).take(new_count),
        );
        if lines_empty {
            lines.push(String::new());
            *highlights = vec![Vec::new()];
            *decorations = vec![LineDecoration::default()];
        }
        self.changedtick += 1;
    }

    pub fn set_all_lines(&mut self, lines: Vec<String>) {
        let (new_lines, count) = if lines.is_empty() {
            (vec![String::new()], 1)
        } else {
            let n = lines.len();
            (lines, n)
        };
        self.lines = Arc::new(new_lines);
        self.highlights = Arc::new(vec![Vec::new(); count]);
        self.decorations = Arc::new(vec![LineDecoration::default(); count]);
        self.changedtick += 1;
    }

    pub fn append_line(&mut self, line: String) {
        Arc::make_mut(&mut self.lines).push(line);
        Arc::make_mut(&mut self.highlights).push(Vec::new());
        Arc::make_mut(&mut self.decorations).push(LineDecoration::default());
        self.changedtick += 1;
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn is_modifiable(&self) -> bool {
        self.modifiable
    }

    pub fn set_modifiable(&mut self, modifiable: bool) {
        self.modifiable = modifiable;
    }

    pub fn buftype(&self) -> &BufType {
        &self.buftype
    }

    pub fn changedtick(&self) -> u64 {
        self.changedtick
    }

    pub fn set_virtual_text(&mut self, line: usize, text: String, hl_group: Option<String>) {
        self.virtual_text.retain(|vt| vt.line != line);
        self.virtual_text.push(VirtualText {
            line,
            col: 0,
            text,
            hl_group,
        });
    }

    pub fn clear_virtual_text(&mut self, line: usize) {
        self.virtual_text.retain(|vt| vt.line != line);
    }

    pub fn virtual_text_at(&self, line: usize) -> Option<&VirtualText> {
        self.virtual_text.iter().find(|vt| vt.line == line)
    }

    pub fn virtual_text(&self) -> &[VirtualText] {
        &self.virtual_text
    }

    pub fn set_mark(&mut self, name: String, line: usize, col: usize) {
        self.marks.insert(name, Mark { line, col });
    }

    pub fn get_mark(&self, name: &str) -> Option<&Mark> {
        self.marks.get(name)
    }

    pub fn delete_mark(&mut self, name: &str) {
        self.marks.remove(name);
    }

    pub fn add_highlight(&mut self, line: usize, col_start: u16, col_end: u16, style: SpanStyle) {
        self.add_highlight_with_meta(line, col_start, col_end, style, SpanMeta::default());
    }

    pub fn add_highlight_with_meta(
        &mut self,
        line: usize,
        col_start: u16,
        col_end: u16,
        style: SpanStyle,
        meta: SpanMeta,
    ) {
        let highlights = Arc::make_mut(&mut self.highlights);
        if line >= highlights.len() {
            highlights.resize_with(line + 1, Vec::new);
        }
        highlights[line].push(Span {
            col_start,
            col_end,
            style,
            meta,
        });
    }

    pub fn clear_highlights(&mut self, start_line: usize, end_line: usize) {
        let highlights = Arc::make_mut(&mut self.highlights);
        let end = end_line.min(highlights.len());
        for spans in highlights.iter_mut().take(end).skip(start_line) {
            spans.clear();
        }
    }

    pub fn highlights_at(&self, line: usize) -> &[Span] {
        self.highlights.get(line).map_or(&[], |v| v.as_slice())
    }

    /// Shared handle to the full highlights vec — used by views that
    /// want to `Arc::clone` instead of rebuilding their own copy.
    pub fn highlights_arc(&self) -> &Arc<Vec<Vec<Span>>> {
        &self.highlights
    }

    pub fn lines_arc(&self) -> &Arc<Vec<String>> {
        &self.lines
    }

    pub fn decorations_arc(&self) -> &Arc<Vec<LineDecoration>> {
        &self.decorations
    }

    pub fn set_decoration(&mut self, line: usize, decoration: LineDecoration) {
        let decorations = Arc::make_mut(&mut self.decorations);
        if line >= decorations.len() {
            decorations.resize_with(line + 1, LineDecoration::default);
        }
        decorations[line] = decoration;
    }

    pub fn decoration_at(&self, line: usize) -> &LineDecoration {
        static DEFAULT: LineDecoration = LineDecoration {
            gutter_bg: None,
            fill_bg: None,
            fill_right_margin: 0,
            soft_wrapped: false,
            source_text: None,
        };
        self.decorations.get(line).unwrap_or(&DEFAULT)
    }

    pub fn decorations(&self) -> &[LineDecoration] {
        &self.decorations
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_buf() -> Buffer {
        Buffer::new(BufId(1), BufCreateOpts::default())
    }

    #[test]
    fn new_buffer_has_one_empty_line() {
        let buf = make_buf();
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.get_line(0), Some(""));
    }

    #[test]
    fn set_lines_replaces_range() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["a".into(), "b".into(), "c".into()]);
        buf.set_lines(1, 2, vec!["x".into(), "y".into()]);
        assert_eq!(buf.lines(), &["a", "x", "y", "c"]);
    }

    #[test]
    fn set_lines_clamps_range() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["a".into()]);
        buf.set_lines(0, 100, vec!["replaced".into()]);
        assert_eq!(buf.lines(), &["replaced"]);
    }

    #[test]
    fn set_all_lines_empty_keeps_one_line() {
        let mut buf = make_buf();
        buf.set_all_lines(vec![]);
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.get_line(0), Some(""));
    }

    #[test]
    fn nonmodifiable_buffer_still_accepts_api_writes() {
        // `modifiable` guards user edits via windows, not framework
        // API calls. Dialog buffers are created with modifiable=false
        // but still need to be populated by `set_all_lines`.
        let mut buf = Buffer::new(
            BufId(1),
            BufCreateOpts {
                modifiable: false,
                buftype: BufType::Nofile,
            },
        );
        buf.set_all_lines(vec!["hello".into(), "world".into()]);
        assert_eq!(buf.line_count(), 2);
        assert_eq!(buf.get_line(0), Some("hello"));
    }

    #[test]
    fn changedtick_increments() {
        let mut buf = make_buf();
        let t0 = buf.changedtick();
        buf.set_all_lines(vec!["a".into()]);
        assert!(buf.changedtick() > t0);
        let t1 = buf.changedtick();
        buf.append_line("b".into());
        assert!(buf.changedtick() > t1);
    }

    #[test]
    fn virtual_text_lifecycle() {
        let mut buf = make_buf();
        buf.set_virtual_text(0, "ghost".into(), None);
        assert!(buf.virtual_text_at(0).is_some());
        buf.clear_virtual_text(0);
        assert!(buf.virtual_text_at(0).is_none());
    }

    #[test]
    fn marks_lifecycle() {
        let mut buf = make_buf();
        buf.set_mark("a".into(), 0, 5);
        assert_eq!(buf.get_mark("a").unwrap().col, 5);
        buf.delete_mark("a");
        assert!(buf.get_mark("a").is_none());
    }

    #[test]
    fn text_joins_lines() {
        let mut buf = make_buf();
        buf.set_all_lines(vec!["hello".into(), "world".into()]);
        assert_eq!(buf.text(), "hello\nworld");
    }

    /// Drives [`Buffer::ensure_rendered_at`] with a stub formatter so
    /// we can assert the caching + re-render semantics without
    /// pulling the full markdown pipeline into the ui crate.
    struct StubFormatter {
        calls: std::sync::Mutex<Vec<(String, u16)>>,
    }

    impl StubFormatter {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn call_log(&self) -> Vec<(String, u16)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl BufferFormatter for StubFormatter {
        fn render(&self, buf: &mut Buffer, source: &str, width: u16) {
            self.calls.lock().unwrap().push((source.to_string(), width));
            buf.set_all_lines(vec![format!("{source}@{width}")]);
        }
    }

    #[test]
    fn formatter_runs_once_per_source_width() {
        let fmt = StubFormatter::new();
        let mut buf = make_buf().with_formatter(fmt.clone());
        buf.set_source("x".into());
        assert!(buf.ensure_rendered_at(10));
        assert!(!buf.ensure_rendered_at(10));
        assert!(buf.ensure_rendered_at(20));
        buf.set_source("y".into());
        assert!(buf.ensure_rendered_at(20));
        assert_eq!(
            fmt.call_log(),
            vec![
                ("x".to_string(), 10),
                ("x".to_string(), 20),
                ("y".to_string(), 20),
            ]
        );
        assert_eq!(buf.get_line(0), Some("y@20"));
    }

    #[test]
    fn setting_same_source_does_not_re_render() {
        let fmt = StubFormatter::new();
        let mut buf = make_buf().with_formatter(fmt.clone());
        buf.set_source("abc".into());
        buf.ensure_rendered_at(10);
        buf.set_source("abc".into());
        assert!(!buf.ensure_rendered_at(10));
        assert_eq!(fmt.call_log().len(), 1);
    }

    #[test]
    fn installing_formatter_invalidates_render_cache() {
        let first = StubFormatter::new();
        let mut buf = make_buf().with_formatter(first.clone());
        buf.set_source("s".into());
        buf.ensure_rendered_at(10);
        let second = StubFormatter::new();
        buf.set_formatter(second.clone());
        assert!(buf.ensure_rendered_at(10));
        assert_eq!(second.call_log().len(), 1);
    }
}
