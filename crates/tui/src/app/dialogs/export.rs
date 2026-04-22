use super::super::App;
use crate::app::ops::AppOp;
use ui::{Callback, CallbackResult, Payload, WinEvent};

pub(in crate::app) fn open(app: &mut App) {
    use crate::keymap::hints;

    let title_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if let Some(buf) = app.ui.buf_mut(title_buf) {
        buf.set_all_lines(vec!["export".into(), String::new()]);
        buf.add_highlight(0, 0, 6, ui::buffer::SpanStyle::dim());
    }

    let list_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if let Some(buf) = app.ui.buf_mut(list_buf) {
        buf.set_all_lines(vec![
            "1. Copy to clipboard".into(),
            "2. Write to file".into(),
        ]);
    }

    let hint_text = hints::join(&[hints::SELECT, hints::CANCEL]);
    let dialog_config = app.builtin_dialog_config(Some(hint_text), vec![]);

    let Some(win_id) = app.ui.dialog_open(
        ui::FloatConfig {
            title: None,
            border: ui::Border::None,
            placement: ui::Placement::dock_bottom_full_width(ui::Constraint::Fixed(8)),
            ..Default::default()
        },
        dialog_config,
        vec![
            ui::PanelSpec::content(title_buf, ui::PanelHeight::Fixed(2)).focusable(false),
            ui::PanelSpec::list(list_buf, ui::PanelHeight::Fit),
        ],
    ) else {
        return;
    };

    let ops = app.lua.ops_handle();
    let ops_submit = ops.clone();
    app.ui.win_on_event(
        win_id,
        WinEvent::Submit,
        Callback::Rust(Box::new(move |ctx| {
            if let Payload::Selection { index } = ctx.payload {
                match index {
                    0 => ops_submit.push(AppOp::ExportClipboard),
                    1 => ops_submit.push(AppOp::ExportFile),
                    _ => {}
                }
            }
            ops_submit.push(AppOp::CloseFloat(ctx.win));
            CallbackResult::Consumed
        })),
    );
    app.ui.win_on_event(
        win_id,
        WinEvent::Dismiss,
        Callback::Rust(Box::new(move |ctx| {
            ops.push(AppOp::CloseFloat(ctx.win));
            CallbackResult::Consumed
        })),
    );
}
