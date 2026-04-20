//! Paint stage: walk a `DisplayBlock`'s span tree and emit SGR + text.
//!
//! Theme colors are resolved here against the `Theme` snapshot in
//! `PaintContext`, so a single redraw stays internally consistent and
//! cached layouts survive theme changes without invalidation.

use super::display::{ColorRole, ColorValue, DisplayLine, SpanStyle};
use super::layout_out::display_width;
use super::{PaintContext, RenderOut, StyleState};
use crate::theme::Theme;
use crossterm::style::Color;

/// Static buffer of spaces sliced (and looped) for end-of-line bg
/// padding. Avoids a per-line `String::with_capacity + repeat`
/// allocation in the paint hot path. 256 covers any common terminal
/// width in one slice; ultrawide displays past that loop the buffer.
const PAD_SPACES: &str =
    "                                                                                                                                                                                                                                                                ";

/// Paint a single `DisplayLine`: emit its spans, fill the row bg if
/// requested, then advance via `newline`. Drops the bg before `newline`
/// so `Clear::UntilNewLine` doesn't bleed the fill color into scrollback.
pub(super) fn paint_line(
    out: &mut RenderOut,
    line: &DisplayLine,
    ctx: &PaintContext,
    pad_left: u16,
) {
    if pad_left > 0 {
        if let Some(gbg) = line.gutter_bg {
            out.set_bg_only(Some(resolve(gbg, ctx.theme, true)));
        }
        let mut remaining = pad_left as usize;
        while remaining > 0 {
            let chunk = remaining.min(PAD_SPACES.len());
            out.print(&PAD_SPACES[..chunk]);
            remaining -= chunk;
        }
        if line.gutter_bg.is_some() {
            out.set_bg_only(None);
        }
    }
    let mut visible_cols: u16 = pad_left;
    for span in &line.spans {
        apply_style(out, &span.style, ctx.theme);
        out.print(&span.text);
        visible_cols = visible_cols.saturating_add(display_width(&span.text) as u16);
    }
    if let Some(fill) = line.fill_bg {
        let pad = ctx
            .term_width
            .saturating_sub(visible_cols)
            .saturating_sub(line.fill_right_margin);
        if pad > 0 {
            out.set_bg_only(Some(resolve(fill, ctx.theme, true)));
            let mut remaining = pad as usize;
            while remaining > 0 {
                let chunk = remaining.min(PAD_SPACES.len());
                out.print(&PAD_SPACES[..chunk]);
                remaining -= chunk;
            }
        }
    }
    out.set_bg_only(None);
    out.newline();
}

#[inline]
fn apply_style(out: &mut RenderOut, style: &SpanStyle, theme: &Theme) {
    let target = StyleState {
        fg: style.fg.map(|c| resolve(c, theme, false)),
        bg: style.bg.map(|c| resolve(c, theme, true)),
        bold: style.bold,
        dim: style.dim,
        italic: style.italic,
        crossedout: style.crossedout,
        underline: style.underline,
    };
    out.set_state(target);
}

/// Resolve a `ColorValue` against the current theme.
#[inline]
pub(crate) fn resolve(c: ColorValue, theme: &Theme, _is_bg: bool) -> Color {
    match c {
        ColorValue::Rgb(r, g, b) => Color::Rgb { r, g, b },
        ColorValue::Ansi(v) => Color::AnsiValue(v),
        ColorValue::Named(n) => Color::from(n),
        ColorValue::Role(role) => match role {
            ColorRole::Accent => theme.accent,
            ColorRole::Slug => theme.slug,
            ColorRole::UserBg => theme.user_bg,
            ColorRole::CodeBlockBg => theme.code_block_bg,
            ColorRole::Bar => theme.bar,
            ColorRole::ToolPending => theme.tool_pending,
            ColorRole::ReasonOff => theme.reason_off,
            ColorRole::Muted => theme.muted,
        },
    }
}
