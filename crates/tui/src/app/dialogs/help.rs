use super::super::App;
use crate::app::ops::AppOp;
use ui::{Callback, CallbackResult, WinEvent};

pub(in crate::app) fn open(app: &mut App) {
    use crate::keymap::hints;
    use crossterm::event::{KeyCode, KeyModifiers};

    let vim_enabled = app.input.vim_enabled();
    let sections = hints::help_sections(vim_enabled);

    let label_col = sections
        .iter()
        .flat_map(|(_, entries)| entries.iter().map(|(k, _)| k.len()))
        .max()
        .unwrap_or(0)
        + 4;

    let mut content_lines: Vec<String> = Vec::new();
    for (si, (_, entries)) in sections.iter().enumerate() {
        for &(label, detail) in entries {
            let padding = " ".repeat(label_col.saturating_sub(label.len()));
            content_lines.push(format!("{label}{padding}{detail}"));
        }
        if si + 1 < sections.len() {
            content_lines.push(String::new());
        }
    }

    let title_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if let Some(buf) = app.ui.buf_mut(title_buf) {
        buf.set_all_lines(vec!["help".into(), String::new()]);
        buf.add_highlight(0, 0, 4, ui::buffer::SpanStyle::dim());
    }

    let content_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if let Some(buf) = app.ui.buf_mut(content_buf) {
        buf.set_all_lines(content_lines);
        let muted = ui::buffer::SpanStyle {
            fg: Some(crate::theme::muted()),
            ..Default::default()
        };
        let dim = ui::buffer::SpanStyle::dim();
        for (i, line) in buf.lines().to_vec().iter().enumerate() {
            let len = line.chars().count();
            if len == 0 {
                continue;
            }
            let label_end = line
                .char_indices()
                .take(label_col)
                .last()
                .map(|(_, _)| label_col as u16)
                .unwrap_or(len as u16);
            buf.add_highlight(i, 0, label_end, muted.clone());
            buf.add_highlight(i, label_end, len as u16, dim.clone());
        }
    }

    let hints_text = hints::join(&[
        hints::CLOSE,
        hints::nav(vim_enabled),
        hints::scroll(vim_enabled),
    ]);

    let dialog_config = app.builtin_dialog_config(
        Some(hints_text),
        vec![
            (KeyCode::Char('q'), KeyModifiers::NONE),
            (KeyCode::Char('?'), KeyModifiers::NONE),
        ],
    );

    let panels = vec![
        ui::PanelSpec::content(title_buf, ui::PanelHeight::Fixed(2)).focusable(false),
        ui::PanelSpec::content(content_buf, ui::PanelHeight::Fill)
            .with_pad_left(2)
            .focusable(true),
    ];

    let Some(win_id) = app.ui.dialog_open(
        ui::FloatConfig {
            title: None,
            border: ui::Border::None,
            placement: ui::Placement::dock_bottom_full_width(ui::Constraint::Pct(60)),
            ..Default::default()
        },
        dialog_config,
        panels,
    ) else {
        return;
    };

    let ops = app.lua.ops_handle();
    app.ui.win_on_event(
        win_id,
        WinEvent::Dismiss,
        Callback::Rust(Box::new(move |ctx| {
            ops.push(AppOp::CloseFloat(ctx.win));
            CallbackResult::Consumed
        })),
    );
}
