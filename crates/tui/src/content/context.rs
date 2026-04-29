//! Width-independent context plumbed through layout and paint stages.

use crate::app::transcript_model::ViewState;
use crate::theme::Theme;

/// Settings that flow through the layout stage. Layout produces a
/// theme-independent `DisplayBlock` so the only width-relevant inputs
/// are the terminal width, whether thinking blocks are expanded, and
/// the per-block view state (expanded / collapsed / trimmed).
#[derive(Debug, Clone, Copy)]
pub struct LayoutContext {
    pub width: u16,
    pub show_thinking: bool,
    pub view_state: ViewState,
}

/// Context for the paint stage. Carries the active theme snapshot so
/// `ColorRole::*` spans resolve to the same colors for every block in
/// one redraw.
#[derive(Debug, Clone, Copy)]
pub struct PaintContext<'a> {
    pub theme: &'a Theme,
    pub term_width: u16,
}
