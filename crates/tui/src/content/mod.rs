mod context;
pub(crate) mod display;
pub(crate) mod highlight;
pub(crate) mod layout;
pub(crate) mod layout_out;
pub(crate) mod prompt_data;
pub(crate) mod prompt_wrap;
mod scrollbar;
pub(crate) mod selection;
pub(crate) mod status;
pub(crate) mod stream_parser;
pub(crate) mod to_buffer;
pub(crate) mod transcript;
pub(crate) mod transcript_buf;
pub(crate) mod viewport;
pub(crate) mod window_view;

pub(crate) use layout::HitRegion;
pub use status::StatusItem;
pub use transcript::{SnapshotCell, TranscriptSnapshot};
pub use viewport::ViewportGeom;

pub(crate) use selection::{scan_at_token, truncate_str, try_at_ref, wrap_line};

pub(crate) use status::BarSpan;
pub use status::{StatusPosition, StyleState};

use crate::theme;
use crate::utils::format_duration;
use crossterm::{style::Color, terminal};
use std::collections::HashMap;

pub use context::{LayoutContext, PaintContext};
pub use display::DisplayBlock;
pub use highlight::warm_up_syntect;

pub(crate) const SPINNER_FRAMES: &[&str] = &["✿", "❀", "✾", "❁"];
/// Frame duration for every animated spinner in the app. Callers in
/// the status bar, transcript tool previews, and the Lua `smelt.ui.
/// spinner` API all read this so every on-screen spinner stays in
/// lockstep.
pub(crate) const SPINNER_FRAME_MS: u64 = 150;

/// Current spinner frame glyph, sampled from a process-wide monotonic
/// clock. All call sites that animate the same pill should use this
/// helper rather than reimplementing the `elapsed / SPINNER_FRAME_MS
/// % len` modulo so frames stay coherent across the status bar,
/// transcript, and Lua-driven dialogs.
pub(crate) fn spinner_frame_index(elapsed: std::time::Duration) -> usize {
    ((elapsed.as_millis() / SPINNER_FRAME_MS as u128) as usize) % SPINNER_FRAMES.len()
}

/// Time-based current glyph. Uses a process-start epoch so every
/// caller converges on the same frame without threading an explicit
/// `Instant` around.
pub(crate) fn spinner_glyph() -> &'static str {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = EPOCH.get_or_init(Instant::now);
    SPINNER_FRAMES[spinner_frame_index(epoch.elapsed())]
}

/// A markdown table separator line (e.g. `|---|---|`).
pub(crate) fn is_table_separator(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty()
        && t.chars()
            .all(|c| c == '-' || c == '|' || c == ':' || c == ' ')
}

/// Context for rendering content inside a bordered box.
/// When passed to `render_markdown` and its sub-renderers, each output line
/// gets a colored left border prefix and a right border suffix with padding.
pub(crate) struct BoxContext {
    /// Left border string printed before each line (e.g. "   │ ").
    pub left: &'static str,
    /// Right border string printed after padding (e.g. " │").
    pub right: &'static str,
    /// Color for the border characters.
    pub color: display::ColorValue,
    /// Inner content width (between left and right borders).
    pub inner_w: usize,
}

impl BoxContext {
    /// Print the left border with color.
    pub fn print_left(&self, out: &mut layout_out::SpanCollector) {
        out.push_fg(self.color);
        out.print_gutter(self.left);
        out.pop_style();
    }

    /// Print right-side padding and border for a line that used `cols` content columns.
    pub fn print_right(&self, out: &mut layout_out::SpanCollector, cols: usize) {
        let pad = self.inner_w.saturating_sub(cols);
        if pad > 0 {
            out.print_gutter(&" ".repeat(pad));
        }
        out.push_fg(self.color);
        out.print_gutter(self.right);
        out.pop_style();
    }
}

/// Emit `n` blank rows.
pub(super) fn emit_newlines(out: &mut layout_out::SpanCollector, n: u16) {
    for _ in 0..n {
        out.newline();
    }
}

pub(super) fn reasoning_color(effort: protocol::ReasoningEffort) -> Color {
    match effort {
        protocol::ReasoningEffort::Off => theme::reason_off(),
        protocol::ReasoningEffort::Low => theme::REASON_LOW,
        protocol::ReasoningEffort::Medium => theme::REASON_MED,
        protocol::ReasoningEffort::High => theme::REASON_HIGH,
        protocol::ReasoningEffort::Max => theme::REASON_MAX,
    }
}

pub fn term_width() -> usize {
    terminal::size().map(|(w, _)| w as usize).unwrap_or(80)
}

pub fn term_height() -> usize {
    terminal::size().map(|(_, h)| h as usize).unwrap_or(24)
}

pub fn tool_timeout_label(args: &HashMap<String, serde_json::Value>) -> Option<String> {
    let ms = args.get("timeout_ms").and_then(|v| v.as_u64())?;
    Some(format!("timeout: {}", format_duration(ms / 1000)))
}

pub(super) fn format_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{}", n)
    }
}
