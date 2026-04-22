use super::super::App;
use crate::app::ops::AppOp;
use crossterm::event::{KeyCode, KeyModifiers};
use std::cell::RefCell;
use std::rc::Rc;
use ui::{Callback, CallbackResult, KeyBind, WinEvent};

struct PsState {
    registry: engine::tools::ProcessRegistry,
    killed: Vec<String>,
    list_buf: ui::BufId,
}

pub(in crate::app) fn open(app: &mut App) {
    use crate::keymap::hints;

    let registry = app.engine.processes.clone();
    let procs = registry.list();

    let list_lines: Vec<String> = procs.iter().map(format_proc).collect();

    let title_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if let Some(buf) = app.ui.buf_mut(title_buf) {
        buf.set_all_lines(vec!["processes".into(), String::new()]);
        buf.add_highlight(0, 0, 9, ui::buffer::SpanStyle::dim());
    }

    let list_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if let Some(buf) = app.ui.buf_mut(list_buf) {
        buf.set_all_lines(list_lines);
    }

    let hint_text = hints::join(&[hints::CLOSE, hints::KILL_PROC]);
    let dialog_config = app.builtin_dialog_config(
        Some(hint_text),
        vec![(KeyCode::Char('q'), KeyModifiers::NONE)],
    );

    let Some(win_id) = app.ui.dialog_open(
        ui::FloatConfig {
            title: None,
            border: ui::Border::None,
            placement: ui::Placement::dock_bottom_full_width(ui::Constraint::Pct(50)),
            ..Default::default()
        },
        dialog_config,
        vec![
            ui::PanelSpec::content(title_buf, ui::PanelHeight::Fixed(2)).focusable(false),
            ui::PanelSpec::list(list_buf, ui::PanelHeight::Fill),
        ],
    ) else {
        return;
    };

    let state = Rc::new(RefCell::new(PsState {
        registry,
        killed: Vec::new(),
        list_buf,
    }));

    let state_backspace = state.clone();
    app.ui.win_set_keymap(
        win_id,
        KeyBind::plain(KeyCode::Backspace),
        Callback::Rust(Box::new(move |ctx| {
            let idx = ctx.ui.dialog_mut(ctx.win).and_then(|d| d.selected_index());
            let Some(idx) = idx else {
                return CallbackResult::Consumed;
            };
            let mut s = state_backspace.borrow_mut();
            let procs: Vec<_> = s
                .registry
                .list()
                .into_iter()
                .filter(|p| !s.killed.contains(&p.id))
                .collect();
            if let Some(p) = procs.get(idx) {
                s.killed.push(p.id.clone());
                let fresh: Vec<_> = s
                    .registry
                    .list()
                    .into_iter()
                    .filter(|p| !s.killed.contains(&p.id))
                    .collect();
                let lines: Vec<String> = fresh.iter().map(format_proc).collect();
                if let Some(buf) = ctx.ui.buf_mut(s.list_buf) {
                    buf.set_all_lines(lines);
                }
            }
            CallbackResult::Consumed
        })),
    );

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

fn format_proc(p: &engine::tools::ProcessInfo) -> String {
    let time = crate::utils::format_duration(p.started_at.elapsed().as_secs());
    format!("{} — {time} {}", p.command, p.id)
}
