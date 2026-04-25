//! Paint stage: walk a `DisplayBlock`'s span tree and emit SGR + text.
//!
//! Theme colors are resolved here against the `Theme` snapshot in
//! `PaintContext`, so a single redraw stays internally consistent and
//! cached layouts survive theme changes without invalidation.

use super::display::{ColorRole, ColorValue};
use crate::theme::Theme;
use crossterm::style::Color;

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
