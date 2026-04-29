//! Span-emitting renderers shared across the transcript and dialogs.
//!
//! - [`syntax`] — code-block + syntax-file rendering, `BashHighlighter`.
//! - [`diff`] — inline diff rendering and the persisted `CachedInlineDiff`.
//! - [`inline`] — markdown tables and inline emphasis (the markdown
//!   inline grammar lives here; block-level lives in
//!   `app::transcript_present::markdown`).
//! - [`util`] — helpers shared by inline and table rendering.

use std::sync::LazyLock;
use syntect::parsing::SyntaxSet;

mod diff;
mod inline;
mod syntax;
mod util;

pub(super) static SYNTAX_SET: LazyLock<SyntaxSet> =
    LazyLock::new(SyntaxSet::load_defaults_newlines);
pub(super) static THEME_SET: LazyLock<two_face::theme::EmbeddedLazyThemeSet> =
    LazyLock::new(two_face::theme::extra);

/// Force eager initialization of the syntect syntax and theme sets. Call
/// once at startup from a background thread so the first tool render
/// doesn't pay the ~30ms deserialization cost mid-frame.
pub fn warm_up_syntect() {
    LazyLock::force(&SYNTAX_SET);
    LazyLock::force(&THEME_SET);
}

pub(super) fn syntax_theme() -> &'static syntect::highlighting::Theme {
    if crate::theme::is_light() {
        &THEME_SET[two_face::theme::EmbeddedThemeName::MonokaiExtendedLight]
    } else {
        &THEME_SET[two_face::theme::EmbeddedThemeName::MonokaiExtended]
    }
}

pub(crate) use diff::{
    build_inline_diff_cache_ext, print_cached_inline_diff, print_inline_diff, CachedInlineDiff,
};
pub(crate) use inline::{
    emit_inline_spans, inline_spans_width, parse_inline_spans, render_markdown_table,
    wrap_inline_spans, InlineSpan, InlineStyle,
};
pub(crate) use syntax::{
    print_syntax_file, print_syntax_file_ext, render_code_block, BashHighlighter,
};
