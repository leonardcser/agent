//! Span-emitting renderers shared across the transcript and dialogs.

use std::sync::LazyLock;
use syntect::parsing::SyntaxSet;
use two_face::theme::{EmbeddedLazyThemeSet, EmbeddedThemeName};

mod action_refs;
pub mod diff;
pub mod inline;
pub mod syntax;

/// How the highlight renderers paint the left margin / gutter.
///
/// The set is closed and exhaustive - each variant corresponds to a real consumer:
/// - `None` - minimalist render (snippets, inline previews) with no gutter at all.
/// - `InlineLineNumbers` - paint a single ` N ` line-number column as text inside
///   the content area; the host window needs no gutter config. Used by transcript
///   tool blocks (`write_file`, `edit_file`, file/diff previews).
/// - `Stamped` - emits `SourceLine` metadata only; a host window with a
///   `LineNumberGutter` paints the actual column. Used by file-viewer panes
///   (`BufFormat::Code`) where the gutter belongs to the window chrome.
///
/// `print_syntax_file*` understands `None | Stamped` (inline-gutter callers
/// route through `build_file_view_ir` + `print_diff_ir` instead).
/// `print_diff_ir` understands `None | InlineLineNumbers | Stamped`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GutterStyle {
    None,
    InlineLineNumbers,
    Stamped,
}

pub(super) static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);
pub(super) static THEME_SET: LazyLock<two_face::theme::EmbeddedLazyThemeSet> =
    LazyLock::new(two_face::theme::extra);

/// Eagerly initialize syntect sets to avoid ~30ms deserialization cost on the first render.
pub fn warm_up_syntect() {
    let _perf = smelt_perf::perf::begin("warmup:syntect");
    LazyLock::force(&SYNTAX_SET);
    LazyLock::force(&THEME_SET);
}

/// Every syntax theme bundled by `two-face`, exposed with the exact names
/// accepted by `ThemeSpec.syntax`.
pub fn syntax_theme_names() -> impl Iterator<Item = &'static str> {
    EmbeddedLazyThemeSet::theme_names()
        .iter()
        .map(|theme| theme.as_name())
}

pub fn syntax_theme_name_is_valid(name: &str) -> bool {
    embedded_syntax_theme(name).is_some()
}

fn embedded_syntax_theme(name: &str) -> Option<EmbeddedThemeName> {
    EmbeddedLazyThemeSet::theme_names()
        .iter()
        .copied()
        .find(|theme| theme.as_name() == name)
}

/// Pick the requested syntax theme, falling back to the matching Monokai
/// dark/light palette when the colorscheme omits `syntax`.
pub(super) fn syntax_theme(theme: &crate::theme::Theme) -> &'static syntect::highlighting::Theme {
    if let Some(name) = theme.syntax_theme() {
        if let Some(theme) = embedded_syntax_theme(name) {
            return &THEME_SET[theme];
        }
    }
    if theme.is_light() {
        &THEME_SET[EmbeddedThemeName::MonokaiExtendedLight]
    } else {
        &THEME_SET[EmbeddedThemeName::MonokaiExtended]
    }
}

pub use diff::{
    build_diff_ir_ext, build_diff_ir_ext_with_base, build_diff_ir_ext_with_source,
    build_file_view_ir, build_retained_diff_ir, compute_split_diff, measure_diff_ir,
    measure_retained_code_block, measure_retained_code_block_edge, measure_retained_file_view,
    measure_retained_file_view_edge, print_diff_ir, print_diff_ir_with_width, print_inline_diff,
    print_inline_diff_ext, print_retained_code_block, print_retained_code_block_edge,
    print_retained_file_view, print_retained_file_view_edge, print_split_diff,
    print_split_diff_side, DiffIr, RetainedFileViewCache, SplitDiffPlan, SplitSide,
};
pub use inline::{
    emit_inline_spans, inline_spans_width, lower_inline_event_lines,
    lower_inline_event_lines_with_options, lower_inline_events, lower_inline_events_with_options,
    measure_markdown_table, measure_markdown_table_with_options, parse_inline_spans,
    parse_inline_spans_with_options, render_markdown_table, render_markdown_table_with_options,
    wrap_inline_spans, InlineOptions, InlineSpan, InlineStyle,
};
pub use syntax::{
    lang_to_ext, print_code_lines, print_syntax_file, print_syntax_file_ext, render_code_block,
    syntax_for_lang, InlineSyntax, InlineSyntaxSpan,
};
