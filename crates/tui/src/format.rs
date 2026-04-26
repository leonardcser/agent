//! Buffer-formatter registry.
//!
//! Every "content kind" a `ui::Buffer` can display — markdown, bash
//! script, syntax-highlighted file, inline diff, soft-wrapped plain
//! text — lives here as a variant of [`BufFormat`]. An installed
//! formatter turns the buffer's `source` string into styled lines +
//! soft-wrap decorations at the terminal `width` it's given. The
//! dialog / window host is responsible for calling
//! [`ui::Buffer::ensure_rendered_at`] with the current content width
//! (after chrome like borders, padding, and scrollbar reservation has
//! been subtracted) before sampling the buffer for display.
//!
//! This is the single unification point for the transcript's markdown
//! pipeline and Lua-driven dialog / window content: both can point a
//! buffer at one of these modes via `ui::BufferFormatter` and reuse
//! the same wrap-aware, copy-friendly rendering the transcript uses.

use std::sync::Arc;
use ui::buffer::{Buffer, BufferFormatter};

use crate::content::highlight::{print_inline_diff, print_syntax_file, BashHighlighter};
use crate::content::layout_out::SpanCollector;
use crate::content::to_buffer::render_into_buffer;

/// Content kind a formatter-backed buffer renders. Constructed from a
/// Lua `mode` string or chosen by a Rust caller, handed to
/// [`BufFormat::into_formatter`] to get a trait object that can be
/// installed on a `ui::Buffer`.
#[derive(Clone, Debug)]
pub enum BufFormat {
    /// Plain text, soft-wrapped to the render width. Copy-friendly:
    /// wrap continuations are marked so vim/visual copies round-trip
    /// without the inserted line breaks.
    Plain,
    /// CommonMark-ish markdown: headings, lists, fenced code blocks
    /// (syntax-highlighted), tables, blockquotes, inline emphasis.
    /// Reuses the exact renderer the transcript response block uses.
    Markdown,
    /// Shell/bash syntax highlighting, soft-wrapped.
    Bash,
    /// Syntax-highlighted source file. Language is inferred from the
    /// `path` extension.
    File { path: String },
    /// Inline unified diff. `old` is the pre-edit content; the
    /// buffer's `source` is treated as the post-edit content.
    Diff { old: String, path: String },
}

impl BufFormat {
    /// Parse a Lua `mode` keyword into the matching variant. Modes
    /// that need an extra payload (`file` → `path`, `diff` → `old +
    /// path`) are constructed via [`BufFormat::from_lua_spec`] which
    /// also reads those fields from the opts table.
    pub fn parse_simple(mode: &str) -> Option<Self> {
        match mode {
            "plain" => Some(Self::Plain),
            "markdown" | "md" => Some(Self::Markdown),
            "bash" | "sh" | "shell" => Some(Self::Bash),
            _ => None,
        }
    }

    /// Resolve a full mode spec from a Lua opts table: `{ mode = "…",
    /// path = "…", old = "…" }`. Unknown modes or missing payloads
    /// produce `Err(msg)`.
    pub fn from_lua_spec(mode: &str, opts: &mlua::Table) -> Result<Self, String> {
        if let Some(simple) = Self::parse_simple(mode) {
            return Ok(simple);
        }
        match mode {
            "file" => {
                let path: String = opts
                    .get("path")
                    .map_err(|_| "buf.create mode=file requires path".to_string())?;
                Ok(Self::File { path })
            }
            "diff" => {
                let path: String = opts
                    .get("path")
                    .map_err(|_| "buf.create mode=diff requires path".to_string())?;
                let old: String = opts.get("old").unwrap_or_default();
                Ok(Self::Diff { old, path })
            }
            other => Err(format!("unknown buffer mode: {other}")),
        }
    }

    /// Wrap this mode in a trait object ready to install on a buffer
    /// via [`ui::Buffer::set_formatter`].
    pub fn into_formatter(self) -> Arc<dyn BufferFormatter> {
        Arc::new(ModeFormatter { mode: self })
    }
}

struct ModeFormatter {
    mode: BufFormat,
}

impl BufferFormatter for ModeFormatter {
    fn render(&self, buf: &mut Buffer, source: &str, width: u16) {
        let theme = crate::theme::snapshot();
        let width = width.max(1);
        match &self.mode {
            BufFormat::Plain => {
                render_into_buffer(buf, width, &theme, |sink| render_plain(sink, source, width));
            }
            BufFormat::Markdown => {
                render_into_buffer(buf, width, &theme, |sink| {
                    crate::app::transcript_present::render_markdown_inner(
                        sink,
                        source,
                        width as usize,
                        "",
                        false,
                        None,
                    );
                });
            }
            BufFormat::Bash => {
                render_into_buffer(buf, width, &theme, |sink| render_bash(sink, source, width));
            }
            BufFormat::File { path } => {
                render_into_buffer(buf, width, &theme, |sink| {
                    print_syntax_file(sink, source, path, 0, u16::MAX);
                });
            }
            BufFormat::Diff { old, path } => {
                render_into_buffer(buf, width, &theme, |sink| {
                    print_inline_diff(sink, old, source, path, old, 0, u16::MAX);
                });
            }
        }
    }
}

fn render_plain(out: &mut SpanCollector, source: &str, width: u16) {
    let width = width.max(1) as usize;
    for line in source.lines() {
        emit_wrapped_line(out, line, width, |sink, segment| {
            sink.print(segment);
        });
    }
}

fn render_bash(out: &mut SpanCollector, source: &str, width: u16) {
    let width = width.max(1) as usize;
    let mut bh = BashHighlighter::new();
    for line in source.lines() {
        emit_wrapped_line(out, line, width, |sink, segment| {
            bh.print_line(sink, segment);
        });
    }
}

/// Soft-wrap `line` to `width` columns, call `emit` for each wrapped
/// segment. Marks the produced rows as a single logical source line:
/// `source_text` on row 0, `soft_wrapped` on every continuation row,
/// `mark_wrapped` on the group if it actually wrapped — all the
/// metadata the transcript copy path and the dialog viewport expect.
fn emit_wrapped_line<F>(out: &mut SpanCollector, line: &str, width: usize, mut emit: F)
where
    F: FnMut(&mut SpanCollector, &str),
{
    let wrapped = ui::text::wrap_line(line, width);
    if wrapped.len() > 1 {
        out.mark_wrapped();
    }
    for (i, segment) in wrapped.iter().enumerate() {
        if i == 0 {
            out.set_source_text(line);
        } else {
            out.mark_soft_wrap_continuation();
        }
        emit(out, segment);
        out.newline();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ui::buffer::{BufCreateOpts, Buffer};
    use ui::BufId;

    fn new_buf() -> Buffer {
        Buffer::new(BufId(1), BufCreateOpts::default())
    }

    #[test]
    fn plain_mode_soft_wraps_long_lines() {
        let mut buf = new_buf().with_formatter(BufFormat::Plain.into_formatter());
        buf.set_source("hello world this is a long line that must wrap".into());
        buf.ensure_rendered_at(10);
        assert!(
            buf.line_count() > 1,
            "expected plain formatter to soft-wrap long line, got {} line(s)",
            buf.line_count()
        );
        // The first row remembers the unwrapped source for copy;
        // continuation rows are flagged so selection copy skips the
        // injected row break.
        assert_eq!(
            buf.decoration_at(0).source_text.as_deref(),
            Some("hello world this is a long line that must wrap")
        );
        assert!(buf.decoration_at(1).soft_wrapped);
    }

    #[test]
    fn markdown_mode_renders_source() {
        let mut buf = new_buf().with_formatter(BufFormat::Markdown.into_formatter());
        buf.set_source("# Heading\n\nbody text".into());
        buf.ensure_rendered_at(40);
        assert!(buf.line_count() >= 2);
        // Heading row should expose its raw markdown for copy-paste.
        assert_eq!(
            buf.decoration_at(0).source_text.as_deref(),
            Some("# Heading")
        );
    }

    #[test]
    fn ensure_rendered_is_idempotent_at_same_width() {
        let mut buf = new_buf().with_formatter(BufFormat::Plain.into_formatter());
        buf.set_source("hi".into());
        assert!(buf.ensure_rendered_at(20));
        assert!(!buf.ensure_rendered_at(20));
    }

    #[test]
    fn ensure_rendered_reruns_on_width_change() {
        let mut buf = new_buf().with_formatter(BufFormat::Plain.into_formatter());
        buf.set_source("hello world".into());
        buf.ensure_rendered_at(20);
        let narrow_rendered = buf.ensure_rendered_at(5);
        assert!(narrow_rendered);
    }

    #[test]
    fn ensure_rendered_reruns_on_source_change() {
        let mut buf = new_buf().with_formatter(BufFormat::Plain.into_formatter());
        buf.set_source("v1".into());
        buf.ensure_rendered_at(40);
        buf.set_source("v2".into());
        assert!(buf.ensure_rendered_at(40));
        assert_eq!(buf.get_line(0), Some("v2"));
    }

    #[test]
    fn no_formatter_is_noop() {
        let mut buf = new_buf();
        buf.set_source("ignored without a formatter".into());
        assert!(!buf.ensure_rendered_at(40));
    }

    #[test]
    fn parse_simple_covers_plain_markdown_bash() {
        assert!(matches!(
            BufFormat::parse_simple("plain"),
            Some(BufFormat::Plain)
        ));
        assert!(matches!(
            BufFormat::parse_simple("markdown"),
            Some(BufFormat::Markdown)
        ));
        assert!(matches!(
            BufFormat::parse_simple("md"),
            Some(BufFormat::Markdown)
        ));
        assert!(matches!(
            BufFormat::parse_simple("bash"),
            Some(BufFormat::Bash)
        ));
        assert!(BufFormat::parse_simple("unknown").is_none());
    }
}
