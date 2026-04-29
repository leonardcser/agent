use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn render_agent_block(
    out: &mut SpanCollector,
    agent_id: &str,
    slug: Option<&str>,
    blocking: bool,
    tool_calls: &[crate::app::AgentToolEntry],
    status: AgentBlockStatus,
    elapsed: Option<Duration>,
    width: usize,
) -> u16 {
    let mut rows = 0u16;

    // Header: " + agent_id · slug [✓/✗] [elapsed]"
    out.push_style(SpanStyle {
        fg: Some(crate::theme::AGENT.into()),
        bold: true,
        ..Default::default()
    });
    out.print_string(format!("+ {agent_id}"));

    if !blocking {
        out.push_fg(ColorValue::Role(ColorRole::Muted));
        out.print(" started");
        out.pop_style(); // muted fg
        out.pop_style(); // bold+agent fg
        out.newline();
        return rows + 1;
    }

    if let Some(slug) = slug {
        out.push_dim();
        out.print_string(format!(" \u{00b7} {slug}"));
        out.pop_style();
    }

    match status {
        AgentBlockStatus::Done => {
            out.push_fg(crate::theme::SUCCESS.into());
            out.print(" \u{2713}"); // ✓
            out.pop_style();
        }
        AgentBlockStatus::Error => {
            out.push_fg(crate::theme::ERROR.into());
            out.print(" \u{2717}"); // ✗
            out.pop_style();
        }
        AgentBlockStatus::Running => {}
    }

    if let Some(d) = elapsed {
        if d.as_secs_f64() >= 0.1 {
            let meta = SpanMeta {
                selectable: false,
                copy_as: None,
            };
            out.push_style(SpanStyle {
                fg: Some(ColorValue::Role(ColorRole::Muted)),
                dim: true,
                ..Default::default()
            });
            out.print_with_meta(&format!("  {}", format_duration(d.as_secs())), meta);
            out.pop_style();
        }
    }

    out.pop_style(); // bold+agent fg
    out.newline();
    rows += 1;

    // Blocking: show last 3 tool calls with left border.
    let visible = tool_calls.iter().rev().take(3).collect::<Vec<_>>();
    for entry in visible.iter().rev() {
        out.push_fg(crate::theme::AGENT.into());
        out.print_gutter("\u{2502} "); // │
        out.pop_style();

        out.push_dim();
        out.print(&entry.tool_name);
        out.pop_style();

        // Reserve space for elapsed time so the summary doesn't push it off-screen.
        let time_str = entry
            .elapsed
            .filter(|d| d.as_secs_f64() >= 0.1)
            .map(|d| format!("  {}", format_duration(d.as_secs())));
        let time_w = time_str.as_ref().map_or(0, |s| s.len());
        // 6 = " │ " (3) + space before summary (1) + padding (2)
        let max_summary = width.saturating_sub(6 + entry.tool_name.len() + time_w);
        let summary = truncate_str(&entry.summary, max_summary);
        out.print_string(format!(" {summary}"));

        if let Some(ref ts) = time_str {
            let meta = SpanMeta {
                selectable: false,
                copy_as: None,
            };
            out.push_dim();
            out.print_with_meta(ts, meta);
            out.pop_style();
        }

        out.newline();
        rows += 1;
    }

    // Bottom border
    let border_w = width.saturating_sub(1);
    out.push_fg(crate::theme::AGENT.into());
    out.print_gutter(&format!("\u{2570}{}", "\u{2500}".repeat(border_w)));
    out.pop_style();
    out.newline();
    rows += 1;

    rows
}
