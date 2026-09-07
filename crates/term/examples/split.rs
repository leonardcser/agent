//! Run with `cargo run -p smelt-term --example split`.
//! Keys and persistence belong to the host; geometry and dragging belong to term.

use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use smelt_term::{
    Axis, Color, DividerStyles, LayoutStyle, LayoutTree, PaintId, Split, SplitInteraction,
    SplitOptions, SplitPane, SplitResizeMode, SplitSize, Style, Surface, TerminalSession,
};

fn main() -> std::io::Result<()> {
    let mut terminal = TerminalSession::builder()
        .focus_events(true)
        .enter_stdout()?;
    let (width, height) = terminal.size()?;
    let mut surface = Surface::new(width, height);
    let split = Split::new(
        Axis::Horizontal,
        SplitOptions {
            size: SplitSize::Cells(30),
            minimum: [12, 20],
            resize_mode: SplitResizeMode::Cells,
            ..SplitOptions::default()
        },
    );
    surface.set_layout(LayoutTree::split(
        split.clone(),
        LayoutTree::leaf(PaintId(1)),
        LayoutTree::leaf(PaintId(2)),
    ));
    let mut interaction = SplitInteraction::default();
    let mut saved = split.preferred_size();
    loop {
        let layout = surface.resolve_layout();
        surface.set_layout_style(LayoutStyle {
            dividers: DividerStyles {
                normal: Style::new().fg(Color::DarkGrey),
                active: Style::new().fg(Color::Cyan),
            },
            active_split: interaction.active_id(),
            ..LayoutStyle::default()
        });
        surface.render_resolved(terminal.writer(), &layout, |id, slice, _| {
            let heading = if id == PaintId(1) {
                "sidebar"
            } else {
                "preview"
            };
            slice.put_str(1, 1, heading, Style::new().bold());
            if id == PaintId(2) {
                for (row, text) in [
                    "drag the divider or use H/L",
                    "= equalize; r reset; q quit",
                    "shrink and restore the terminal",
                    "cell preferences survive clamping",
                ]
                .iter()
                .enumerate()
                {
                    slice.put_str(1, 3 + row as u16, text, Style::new());
                }
                slice.put_str(1, 8, &format!("saved size: {saved:?}"), Style::new().dim());
            }
        })?;
        let response = match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                interaction.cancel();
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('H' | 'L') => {
                        if let Some(resolved) = layout.split(split.id()) {
                            resolved.resize(
                                SplitPane::First,
                                if key.code == KeyCode::Char('H') {
                                    -2
                                } else {
                                    2
                                },
                            );
                        }
                    }
                    KeyCode::Char('=') => {
                        if let Some(resolved) = layout.split(split.id()) {
                            resolved.equalize();
                        }
                    }
                    KeyCode::Char('r') => {
                        split.reset();
                    }
                    _ => {}
                }
                saved = split.preferred_size();
                None
            }
            Event::Mouse(mouse) => {
                let active = interaction.active_id().and_then(|id| layout.split(id));
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => layout
                        .divider_at(mouse.row, mouse.column)
                        .and_then(|resolved| interaction.begin(resolved, mouse.row, mouse.column)),
                    MouseEventKind::Drag(MouseButton::Left) => {
                        interaction.update(active, mouse.row, mouse.column)
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        interaction.release(active, mouse.row, mouse.column)
                    }
                    _ => None,
                }
            }
            Event::Resize(width, height) => {
                surface.set_terminal_size(width, height);
                None
            }
            Event::FocusLost => interaction.cancel(),
            _ => None,
        };
        if response.is_some_and(|response| response.ended() && response.changed) {
            // Serialize this value in the host's settings, not the handle's id.
            saved = split.preferred_size();
        }
    }
    Ok(())
}
