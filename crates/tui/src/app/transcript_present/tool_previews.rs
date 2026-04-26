use super::markdown::render_markdown_inner;
use super::tools::{print_dim, print_dim_count, render_default_output};
use super::*;

pub(super) fn render_edit_output(
    out: &mut SpanCollector,
    output: &ToolOutput,
    args: &HashMap<String, serde_json::Value>,
) -> u16 {
    let old = args
        .get("old_string")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let new = args
        .get("new_string")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
    if new.is_empty() {
        print_dim_count(out, old.lines().count(), "line deleted", "lines deleted")
    } else if let Some(crate::app::transcript_cache::ToolOutputRenderCache::InlineDiff(cache)) =
        output.render_cache.as_ref()
    {
        print_cached_inline_diff(out, cache, 0, 0)
    } else {
        print_inline_diff(out, old, new, path, new, 0, 0)
    }
}

pub(super) fn render_write_output(
    out: &mut SpanCollector,
    args: &HashMap<String, serde_json::Value>,
) -> u16 {
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
    print_syntax_file(out, content, path, 0, 0)
}

pub(super) fn render_notebook_output(
    out: &mut SpanCollector,
    output: &ToolOutput,
    width: usize,
) -> u16 {
    let Some(meta) = output.metadata.as_ref() else {
        return render_default_output(out, &output.content, output.is_error, width);
    };
    let Ok(data) = serde_json::from_value::<NotebookRenderData>(meta.clone()) else {
        return render_default_output(out, &output.content, output.is_error, width);
    };

    let mut rows = 0u16;
    print_dim(out, &format!("  {}", data.title()));
    out.newline();
    rows += 1;

    if data.edit_mode == "insert" {
        rows += print_syntax_file_ext(
            out,
            &data.new_source,
            &data.path,
            Some(data.syntax_ext()),
            0,
            0,
        );
    } else if let Some(crate::app::transcript_cache::ToolOutputRenderCache::NotebookEdit(ref nb)) =
        output.render_cache
    {
        if let Some(ref cache) = nb.diff {
            rows += print_cached_inline_diff(out, cache, 0, 0);
        } else {
            rows += print_inline_diff(
                out,
                &data.old_source,
                &data.new_source,
                &data.path,
                &data.old_source,
                0,
                0,
            );
        }
    } else {
        rows += print_inline_diff(
            out,
            &data.old_source,
            &data.new_source,
            &data.path,
            &data.old_source,
            0,
            0,
        );
    }
    rows
}
pub(super) fn render_plan_output(
    out: &mut SpanCollector,
    args: &HashMap<String, serde_json::Value>,
    width: usize,
) -> u16 {
    let body = args
        .get("plan_summary")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if body.is_empty() {
        return 0;
    }

    // Box geometry: "   │ " (5) + content + " │" (2) = 7 overhead
    let inner_w = width.saturating_sub(7);
    let mut rows = 0u16;

    // Top border: "   ┌─ Plan ──...──┐"
    // 3 + 1(┌) + 1(─) + 6(label) + fill + 1(┐) = 5 + inner_w + 2
    let label = " Plan ";
    let fill = inner_w.saturating_sub(label.len()).saturating_add(1);
    out.push_fg(theme::PLAN.into());
    out.print_gutter(&format!(
        "  \u{250c}\u{2500}{label}{}\u{2510}",
        "\u{2500}".repeat(fill)
    ));
    out.pop_style();
    out.newline();
    rows += 1;

    // Body: markdown rendering inside the plan box.
    let bctx = crate::content::BoxContext {
        left: "  \u{2502} ",
        right: " \u{2502}",
        color: theme::PLAN.into(),
        inner_w,
    };
    rows += render_markdown_inner(out, body, width, bctx.left, false, Some(&bctx));

    // Bottom border: "   └──...──┘"
    // 3 + 1(└) + dashes + 1(┘) = 5 + inner_w + 2 → dashes = inner_w + 2
    out.push_fg(theme::PLAN.into());
    out.print_gutter(&format!(
        "  \u{2514}{}\u{2518}",
        "\u{2500}".repeat(inner_w + 2)
    ));
    out.pop_style();
    out.newline();
    rows += 1;

    rows
}
