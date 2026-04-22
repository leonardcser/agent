//! Residual shell for the transcript subsystem. Block history,
//! streaming state, layout cache, and the transcript projection
//! moved onto `App` in `app/transcript.rs`; what remains here is
//! terminal-direct output + a handful of public types still used
//! across the crate.

pub(crate) struct TranscriptData {
    pub clamped_scroll: u16,
    pub total_rows: u16,
    pub scrollbar_col: u16,
    /// Inner viewport rect (0-based, pre-padding). Caller composes it
    /// with `transcript_gutters.pad_left` when assembling the WindowView.
    pub viewport: super::region::Viewport,
}

pub(crate) struct TranscriptCursor {
    pub clamped_line: u16,
    pub clamped_col: u16,
    pub soft_cursor: Option<super::window_view::SoftCursor>,
}

/// Visual selection in the content pane, captured from vim state.
/// Line indices are 0-based from the top of the full transcript; cols
/// count chars on that line.
#[derive(Clone, Copy, Debug)]
pub struct ContentVisualRange {
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
    pub kind: ContentVisualKind,
}

#[derive(Clone, Copy, Debug)]
pub enum ContentVisualKind {
    Char,
    Line,
}

/// A short ephemeral notification rendered above the prompt bar.
#[derive(Clone)]
pub struct Notification {
    pub message: String,
    pub is_error: bool,
}
