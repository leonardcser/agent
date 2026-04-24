//! Simple read-only text dialog. One content panel, Esc / q dismiss.
//! Used by `/stats` and `/cost` — popovers that just show static text.

use super::super::App;
use crossterm::event::{KeyCode, KeyModifiers};
use ui::buffer::BufCreateOpts;
use ui::{Constraint, PanelHeight, PanelSpec, Placement};

/// Open a dialog with one scrollable content panel showing `body`.
/// Dismisses on Esc, q, or Ctrl+C.
pub fn open(app: &mut App, title: impl Into<String>, body: &str) {
    let buf = app.ui.buf_create(BufCreateOpts::default());
    if let Some(b) = app.ui.buf_mut(buf) {
        let lines: Vec<String> = if body.is_empty() {
            vec![String::new()]
        } else {
            body.lines().map(|s| s.to_string()).collect()
        };
        b.set_all_lines(lines);
    }

    let dialog_config = app.builtin_dialog_config(
        None,
        vec![
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::Char('q'), KeyModifiers::NONE),
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
        ],
    );

    let panels = vec![PanelSpec::content(buf, PanelHeight::Fill).focusable(false)];

    let _ = app.ui.dialog_open(
        ui::FloatConfig {
            title: Some(title.into()),
            placement: Placement::Centered {
                width: Constraint::Pct(70),
                height: Constraint::Pct(60),
            },
            ..Default::default()
        },
        dialog_config,
        panels,
    );
}
