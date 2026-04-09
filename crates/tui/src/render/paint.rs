//! Paint stage: walk a `DisplayBlock`'s span tree and emit SGR + text.
//!
//! Theme colors are resolved here against the `Theme` snapshot in
//! `PaintContext`, so a single redraw stays internally consistent and
//! cached layouts survive theme changes without invalidation.

use super::display::{ColorRole, ColorValue, DisplayBlock, DisplayLine, SpanStyle};
use super::layout_out::display_width;
use super::{crlf, PaintContext, RenderOut, StyleState};
use crate::theme::Theme;
use crossterm::style::{Color, Print};
use crossterm::QueueableCommand;

/// Static buffer of spaces sliced (and looped) for end-of-line bg
/// padding. Avoids a per-line `String::with_capacity + repeat`
/// allocation in the paint hot path. 256 covers any common terminal
/// width in one slice; ultrawide displays past that loop the buffer.
const PAD_SPACES: &str =
    "                                                                                                                                                                                                                                                                ";

/// Paint a block, optionally dropping the first `skip` lines. The
/// skip path is used by the purge/redraw flow when a single block
/// exceeds the redraw budget — its head is cropped so the tail still
/// fits, tmux-style. `skip = 0` is the common case.
pub(super) fn paint_block(
    out: &mut RenderOut,
    block: &DisplayBlock,
    ctx: &PaintContext,
    skip: usize,
) {
    if skip >= block.lines.len() {
        return;
    }
    for line in &block.lines[skip..] {
        paint_line(out, line, ctx);
    }
}

/// Paint a single `DisplayLine`: emit its spans, fill the row bg if
/// requested, then advance via `crlf`. Drops the bg before `crlf` so
/// `Clear::UntilNewLine` doesn't bleed the fill color into scrollback.
pub(super) fn paint_line(out: &mut RenderOut, line: &DisplayLine, ctx: &PaintContext) {
    let mut visible_cols: u16 = 0;
    for span in &line.spans {
        apply_style(out, &span.style, ctx.theme);
        let _ = out.queue(Print(&span.text));
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
                let _ = out.queue(Print(&PAD_SPACES[..chunk]));
                remaining -= chunk;
            }
        }
    }
    out.set_bg_only(None);
    crlf(out);
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
///
/// `is_bg = false` (foreground): RGB values are mapped to the nearest
/// xterm-256 cube entry so the terminal emits 10-byte `\x1b[38;5;Nm`
/// escapes instead of 17-byte `\x1b[38;2;R;G;Bm`. Minor accuracy loss,
/// invisible for syntax highlighting in practice.
///
/// `is_bg = true` (background): RGB stays 24-bit. Dark, low-saturation
/// diff bgs (e.g. `rgb(60,20,20)`) lose their hue when snapped to the
/// 6×6×6 cube — the nearest cube entry is usually a much brighter pure-
/// channel color, which reads wrong on the diff background.
#[inline]
pub(super) fn resolve(c: ColorValue, theme: &Theme, is_bg: bool) -> Color {
    match c {
        ColorValue::Rgb(r, g, b) => {
            if is_bg {
                Color::Rgb { r, g, b }
            } else {
                Color::AnsiValue(rgb_to_cube(r, g, b))
            }
        }
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

/// Map an RGB triple to the closest xterm-256 6×6×6 cube entry
/// (palette indices 16–231). The cube uses level values
/// `[0, 95, 135, 175, 215, 255]` per channel.
///
/// Deliberately ignores the grayscale ramp (indices 232–255): for dark
/// colors with one dominant channel (e.g. `rgb(60,20,20)`), the gray ramp
/// is closer in Euclidean distance but loses the hue, which reads as a
/// regression for syntax highlighting. The cube alone is "good enough"
/// and never surprises.
#[inline]
fn rgb_to_cube(r: u8, g: u8, b: u8) -> u8 {
    16 + 36 * nearest_level(r) + 6 * nearest_level(g) + nearest_level(b)
}

#[inline]
fn nearest_level(v: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let mut best = 0u8;
    let mut best_d = u32::MAX;
    for (i, &lv) in LEVELS.iter().enumerate() {
        let d = (v as i32 - lv as i32).unsigned_abs();
        if d < best_d {
            best_d = d;
            best = i as u8;
        }
    }
    best
}
