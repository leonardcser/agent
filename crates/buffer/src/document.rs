//! Indexed, read-only row sources. Windows retain only a viewport-sized projection.

use std::ops::Range;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use crate::buffer::{Buffer, LineDecoration, SourceLine, Span};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DocumentSnapshot {
    pub generation: u64,
    pub total_rows: u64,
}

#[derive(Clone, Debug, Default)]
pub struct DocumentRow {
    pub text: String,
    pub spans: Vec<Span>,
    pub decoration: LineDecoration,
}

/// Identity of one live viewport subscription, independent of document identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ViewId(u64);

/// Window-owned presentation inputs. Text access does not change these inputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentViewport {
    pub rows: Range<u64>,
    pub width: u16,
    pub cursor: Option<u64>,
}

/// Random-access data behind virtual read-only windows. Row production must be
/// bounded by the requested range, not document size. Generation changes
/// invalidate retained projections. Copying and searching call `rows` and must
/// not replace visible enrichment demand or mutate another window's state.
pub trait RowSource: Send + Sync {
    fn snapshot(&self) -> DocumentSnapshot;
    fn rows(&self, range: Range<u64>, theme: &crate::theme::Theme) -> Vec<DocumentRow>;
    /// Register this view's current demand. Called before materializing its rows,
    /// including when the cursor moves without changing the visible row range.
    fn prepare(&self, _view: ViewId, _viewport: &DocumentViewport) {}
    /// Format rows for this window using its width and owning UI's theme. Like
    /// `rows`, copy/search may call this without changing enrichment demand.
    fn viewport_rows(
        &self,
        viewport: &DocumentViewport,
        theme: &crate::theme::Theme,
    ) -> Vec<DocumentRow> {
        self.rows(viewport.rows.clone(), theme)
    }
    /// Release only this viewport's demand when it detaches or closes.
    fn release(&self, _view: ViewId) {}
    /// Whether visible rows have background enrichment pending. Speculative
    /// prefetching must not keep an otherwise idle UI repainting.
    fn is_pending(&self) -> bool {
        false
    }
    /// Document-wide line-number bounds keep virtual gutters stable while scrolling.
    fn line_number_bounds(&self) -> Option<crate::buffer::SourceLine> {
        None
    }
}

/// A window's subscription to shared row storage. Cloning creates an independent
/// subscription; dropping releases only its own demand, not another window's.
pub struct RowView {
    source: Arc<dyn RowSource>,
    id: ViewId,
}

impl RowView {
    pub fn new(source: Arc<dyn RowSource>) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            source,
            id: ViewId(NEXT_ID.fetch_add(1, Ordering::Relaxed)),
        }
    }

    pub fn source(&self) -> &Arc<dyn RowSource> {
        &self.source
    }

    pub fn prepare(&self, viewport: &DocumentViewport) {
        self.source.prepare(self.id, viewport);
    }

    /// Suspend enrichment while unmounted. The subscription can prepare again
    /// when the same window becomes visible, without replacing its document.
    pub fn clear_demand(&self) {
        self.source.release(self.id);
    }
}

impl Clone for RowView {
    fn clone(&self) -> Self {
        Self::new(Arc::clone(&self.source))
    }
}

impl Drop for RowView {
    fn drop(&mut self) {
        self.source.release(self.id);
    }
}

/// A plain-text document with a compact line-offset index.
pub struct TextDocument {
    text: String,
    lines: Vec<Range<usize>>,
}

impl TextDocument {
    pub fn new(text: String) -> Self {
        let mut offset = 0;
        let lines = text
            .split_inclusive('\n')
            .map(|line| {
                let start = offset;
                offset += line.len();
                start..start + line.strip_suffix('\n').unwrap_or(line).len()
            })
            .collect();
        Self { text, lines }
    }
}

impl RowSource for TextDocument {
    fn snapshot(&self) -> DocumentSnapshot {
        DocumentSnapshot {
            generation: 0,
            total_rows: self.lines.len() as u64,
        }
    }

    fn rows(&self, range: Range<u64>, _theme: &crate::theme::Theme) -> Vec<DocumentRow> {
        let start = range.start.min(self.lines.len() as u64) as usize;
        let end = range.end.min(self.lines.len() as u64) as usize;
        self.lines
            .get(start..end)
            .unwrap_or_default()
            .iter()
            .map(|range| DocumentRow {
                text: crate::text::slice(&self.text, range.clone()).to_string(),
                ..Default::default()
            })
            .collect()
    }
}

/// Install already-materialized rows without retaining any offscreen text.
pub fn install_rows(buf: &mut Buffer, rows: Vec<DocumentRow>, line_bounds: Option<SourceLine>) {
    let mut rebuild = buf.begin_rendered_lines_rebuild();
    rebuild.lines.clear();
    rebuild.metadata.source_line_bounds = line_bounds;
    for (index, row) in rows.into_iter().enumerate() {
        rebuild.lines.push(row.text);
        for span in row.spans {
            rebuild.metadata.push_span(index, span);
        }
        rebuild.metadata.push_decoration(index, row.decoration);
    }
    buf.finish_rendered_lines_rebuild(rebuild);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{BufCreateOpts, BufId};

    #[test]
    fn indexed_text_bounds_utf8_and_projection_metadata_reset() {
        let source = TextDocument::new("a\n\n界\nlast".into());
        let theme = crate::theme::Theme::default();
        assert_eq!(source.snapshot().total_rows, 4);
        assert_eq!(
            source
                .rows(2..u64::MAX, &theme)
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            ["界", "last"]
        );
        assert!(source.rows(u64::MAX..u64::MAX, &theme).is_empty());
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        let bounds = Some(SourceLine::Diff {
            old: Some(1_000_000),
            new: Some(20),
        });
        install_rows(&mut buf, source.rows(2..3, &theme), bounds);
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.source_line_bounds(), bounds);
        install_rows(&mut buf, source.rows(3..4, &theme), None);
        assert_eq!(buf.lines(), &["last"]);
        assert_eq!(buf.source_line_bounds(), None);
        install_rows(&mut buf, Vec::new(), None);
        assert_eq!(buf.lines(), &[""]);
    }
}
