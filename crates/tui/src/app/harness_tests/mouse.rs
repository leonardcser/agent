use super::*;

#[test]
fn generic_win_cursor_setter_cannot_repark_prompt_cursor() {
    let mut app = TestApp::builder().with_vim(false).build();
    assert!(app.run_lua(r#"smelt.prompt.set_text("hel\nlo")"#));
    app.render();
    assert!(app.run_lua("smelt.prompt.win():cursor(0)"));
    app.type_text("!");
    assert_eq!(app.state().prompt_text, "hel\nlo!");
}

#[test]
fn generic_win_reveal_cannot_repark_prompt_cursor() {
    let mut app = TestApp::builder().with_vim(false).build();
    assert!(app.run_lua(r#"smelt.prompt.set_text("hel\nlo")"#));
    let before = app.prompt_cpos();

    assert!(app.run_lua("smelt.prompt.win():reveal(0)"));

    assert_eq!(app.prompt_cpos(), before);
}

#[test]
fn prompt_bottom_chrome_click_focuses_prompt() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().with_vim(false).build();
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: "transcript content".into(),
    });
    app.render_silent();

    let transcript_rect = app
        .ui_probe()
        .split_rect(crate::app::TRANSCRIPT_WIN)
        .expect("transcript rect after render");
    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row: transcript_rect.top,
        column: transcript_rect.left,
        modifiers: KeyModifiers::empty(),
    })));
    assert_eq!(app.state().app_focus, AppFocus::Content);

    let bottom_win = app
        .ui_probe()
        .named_win("smelt.prompt_bar.bottom")
        .expect("default prompt bottom bar window");
    let bottom_rect = app
        .ui_probe()
        .split_rect(bottom_win)
        .expect("bottom prompt chrome rect after render");
    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row: bottom_rect.top,
        column: bottom_rect.left,
        modifiers: KeyModifiers::empty(),
    })));

    assert_eq!(app.state().app_focus, AppFocus::Prompt);
    assert_eq!(app.ui_probe().focus(), Some(crate::app::PROMPT_WIN));
}

#[test]
fn top_scroll_pill_click_preserves_focus_and_cursor_and_advances_target() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().with_ephemeral(true).build();
    app.set_terminal_size(80, 16);
    for turn in 0..4 {
        app.push_transcript_block(smelt_core::transcript_model::Block::User {
            text: format!("user turn {turn}"),
            image_labels: Vec::new(),
            command: false,
            sent_at_ms: None,
        });
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: (0..18)
                .map(|line| format!("assistant turn {turn} line {line:02}"))
                .collect::<Vec<_>>()
                .join("\n")
                .into(),
        });
    }
    app.render_silent();
    app.focus_prompt();
    app.render_silent();

    let cursor_before = app.transcript_window().document_view_state().cursor;

    for click in 1..=2 {
        let top_win = app
            .ui_probe()
            .named_win("smelt.scroll_pills.top.win")
            .expect("visible top scroll pill");
        let top_buf = app
            .ui_probe()
            .named_buf("smelt.scroll_pills.top.buf")
            .expect("top scroll pill buffer");
        let label_before = app
            .ui_probe()
            .buf(top_buf)
            .and_then(|buf| buf.get_line(0))
            .expect("top scroll pill label before click")
            .to_string();
        let pill_rect = app
            .ui_probe()
            .win(top_win)
            .and_then(|win| win.viewport)
            .expect("top scroll pill viewport")
            .rect;

        assert_eq!(app.state().app_focus, AppFocus::Prompt);
        assert_eq!(app.ui_probe().focus(), Some(crate::app::PROMPT_WIN));
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: pill_rect.top,
            column: pill_rect.left.saturating_add(1),
            modifiers: KeyModifiers::empty(),
        })));

        app.render_silent();
        assert_eq!(app.state().app_focus, AppFocus::Prompt);
        assert_eq!(app.ui_probe().focus(), Some(crate::app::PROMPT_WIN));
        assert_eq!(
            app.transcript_window().document_view_state().cursor,
            cursor_before
        );
        let viewport_lines = transcript_viewport_lines(&app);
        let target_screen_row = viewport_lines
            .iter()
            .position(|line| line.contains(label_before.trim()))
            .unwrap_or_else(|| {
                panic!(
                    "click {click} should reveal its user message: label={label_before:?}, lines={viewport_lines:?}"
                )
            });
        assert_eq!(
            target_screen_row, 2,
            "click {click} should clear the top pill and panel padding: {viewport_lines:?}"
        );
        let top_buf = app
            .ui_probe()
            .named_buf("smelt.scroll_pills.top.buf")
            .expect("top scroll pill should remain visible after click");
        let label_after = app
            .ui_probe()
            .buf(top_buf)
            .and_then(|buf| buf.get_line(0))
            .expect("top scroll pill label after click")
            .to_string();
        assert_ne!(
            label_after, label_before,
            "click {click} should advance to the previous user message"
        );
    }
}

#[test]
fn wheel_scroll_in_visual_mode_preserves_cursor_screen_row() {
    let mut app = row_document_transcript_app(100, true);

    // Move cursor to row 5.
    app.type_char('5');
    app.type_char('G');
    let row_before = transcript_row_cursor_row(&app);
    assert_eq!(row_before, 4);

    // Enter visual mode.
    app.type_char('v');

    // Scroll down with the mouse wheel (coalesced delta is 3 rows per tick).
    use crossterm::event::{MouseEvent, MouseEventKind};
    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        row: 5,
        column: 10,
        modifiers: crossterm::event::KeyModifiers::empty(),
    })));
    app.render_silent();

    let row_after = transcript_row_cursor_row(&app);
    // With the fix the cursor follows the viewport, so it should have
    // moved down by 3 rows (screen row preserved).
    assert_eq!(
        row_after,
        row_before + 3,
        "cursor should follow viewport in visual mode"
    );
}

#[test]
fn mouse_drag_clears_visual_line_mode() {
    let mut app = row_document_transcript_app(100, true);

    // Enter visual-line mode.
    app.type_char('V');
    assert!(
        matches!(app.transcript_window().vim_mode, VimMode::VisualLine),
        "should start in visual-line mode"
    );

    // Start a mouse drag on the transcript.
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row: 5,
        column: 10,
        modifiers: crossterm::event::KeyModifiers::empty(),
    })));
    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        row: 6,
        column: 10,
        modifiers: crossterm::event::KeyModifiers::empty(),
    })));

    // Visual-line mode should have been exited by the drag.
    assert!(
        matches!(app.transcript_window().vim_mode, VimMode::Normal),
        "mouse drag should exit visual-line mode, got {:?}",
        app.transcript_window().vim_mode
    );
}

#[test]
fn transcript_shift_selection_copy_copies_document_range() {
    let mut app = row_document_transcript_app(80, false);

    app.press_mod(KeyCode::Up, KeyModifiers::SUPER);
    for _ in 0..30 {
        app.press_mod(KeyCode::Down, KeyModifiers::SHIFT);
    }
    app.press_mod(KeyCode::Char('c'), KeyModifiers::SUPER);

    let yank = app.core_probe().clipboard.kill_ring.current();
    assert!(yank.contains("row 000 alpha beta"), "yank was {yank:?}");
    assert!(yank.contains("row 014 alpha beta"), "yank was {yank:?}");
}

#[test]
fn transcript_triple_click_event_pipeline_yanks_clicked_display_line() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().with_vim(false).build();
    let expected = "Bestellung 10500 besta\u{308}tigt.pdf 日本 👩\u{200d}💻 🇨🇦";
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: format!("```text\nalpha\nbeta\ngamma\n```\n\n{expected}").into(),
    });
    app.render_silent();

    let transcript_win = app.transcript_window();
    let vp = transcript_win
        .viewport
        .expect("transcript viewport after render");
    let pad_left = transcript_win.gutter_pad_left;
    let scroll_top = transcript_win.scroll_top() as usize;
    let buf = app
        .ui_probe()
        .buf(transcript_win.buf)
        .expect("transcript buffer");
    let line_idx = buf
        .lines()
        .iter()
        .position(|line| line.contains("Bestellung 10500"))
        .expect("target line rendered");
    assert!(line_idx >= scroll_top, "target line should be visible");
    let row = vp.rect.top + (line_idx - scroll_top) as u16;
    let column = vp
        .rect
        .left
        .saturating_add(vp.gutter_width)
        .saturating_add(pad_left)
        .saturating_add(3);

    for _ in 0..3 {
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row,
            column,
            modifiers: KeyModifiers::empty(),
        })));
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            row,
            column,
            modifiers: KeyModifiers::empty(),
        })));
    }

    assert_eq!(app.core_probe().clipboard.kill_ring.current(), expected);
}

#[test]
fn decorative_copy_owner_does_not_exclude_adjacent_content() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.set_terminal_size(44, 18);
    assert!(app.run_lua(
        r#"
        smelt.transcript.set_renderer(function(block)
          local l = smelt.layout
          return l.hbox({
            { l.line({ { text = "icon", selectable = false } }), cols = 5 },
            l.line("Keep this selectable message.")
          })
        end)
    "#
    ));
    app.push_user_block("source");
    app.render_silent();
    drag_transcript_text(&mut app, "Keep", "message.");
    assert_eq!(
        app.core_probe().clipboard.kill_ring.current(),
        "Keep this selectable message."
    );
}

#[test]
fn user_timestamp_is_excluded_from_drag_copy_without_changing_wrapping() {
    let mut app = TestApp::builder()
        .with_vim(false)
        .with_wall_time(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_742_567_823))
        .build();
    app.set_terminal_size(44, 18);
    assert!(app.run_lua("smelt.time.format = smelt.time.format_utc"));
    let message = "Preserve Unicode 語界 and long lines that wrap across the panel.\n\nKeep hard newlines at the end.";
    app.push_user_block(message);
    app.render_silent();

    let selected_rows = drag_transcript_text(&mut app, "14:37:03", "end.");
    assert!(selected_rows.count() >= 4);
    assert_eq!(app.core_probe().clipboard.kill_ring.current(), message);
    assert_eq!(
        app.core_probe().clipboard.kill_ring.last_clipboard_write(),
        Some(message)
    );
}

#[test]
fn transcript_drag_copy_preserves_bash_command_hard_newlines_only() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.set_terminal_size(52, 16);
    app.start_turn(42);
    let command = "printf 'first-logical-line alpha beta gamma delta epsilon zeta eta theta'\nprintf 'second-logical-line iota kappa lambda mu nu xi omicron pi'";
    app.feed_one(SourceEvent::engine(EngineEvent::ToolStarted {
        invocation_id: protocol::InvocationId::new(1),
        call_id: "wrapped-bash-copy".into(),
        tool_name: "bash".into(),
        args: std::collections::HashMap::from([("command".into(), serde_json::json!(command))]),
        called_at_ms: 0,
    }));
    app.render_silent();

    let selected_rows = drag_transcript_text(&mut app, "printf", "pi'");
    assert!(
        selected_rows.clone().count() >= 3,
        "command must span at least three visual rows"
    );
    assert_eq!(
        app.core_probe().clipboard.kill_ring.last_clipboard_write(),
        Some(command)
    );
    assert_eq!(app.core_probe().clipboard.kill_ring.current(), command);
}

#[test]
fn transcript_triple_click_wrapped_markdown_highlights_and_copies_paragraph() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().with_vim(false).build();
    app.set_terminal_size(52, 16);
    for i in 0..40 {
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: format!("filler row {i:02}").into(),
        });
    }
    let expected = "This paragraph includes **markdown** and a curly ’ quote before beta so selection copy keeps beta aligned across wraps and highlights every soft-wrapped row in the paragraph.";
    app
        .push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: "This paragraph includes **markdown** and a curly ’ quote before beta so selection copy keeps beta aligned across wraps and highlights every soft-wrapped row in the paragraph.".into(),
        });
    for i in 0..20 {
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: format!("trailing row {i:02}").into(),
        });
    }
    app.render_silent();
    pin_transcript_top_to_line_containing(&mut app, "curly");
    assert!(
        app.transcript_window().has_materialized_rows(),
        "regression should exercise row-backed transcript selection"
    );

    let (row, column) = {
        let win = app.transcript_window();
        let vp = win.viewport.expect("transcript viewport");
        let materialized = win.materialized_rows().expect("row-backed transcript");
        let scroll_top = win.scroll_top();
        let pad_left = win.gutter_pad_left;
        let buf = app.ui_probe().buf(win.buf).expect("transcript buffer");
        let local_scroll = win.local_visual_row(scroll_top) as usize;
        let visible_end = local_scroll
            .saturating_add(vp.rect.height as usize)
            .min(buf.line_count());
        let line_idx = (local_scroll..visible_end)
            .find(|&idx| buf.lines()[idx].contains("curly"))
            .expect("wrapped markdown row visible");
        let abs_row = materialized.absolute_row(line_idx as crate::smelt_edit::RowIndex);
        let line = &buf.lines()[line_idx];
        let hit_col = smelt_buffer::text::byte_to_cell(line, line.find("curly").unwrap()) as u16;
        (
            vp.rect.top + abs_row.saturating_sub(scroll_top) as u16,
            vp.rect
                .left
                .saturating_add(vp.gutter_width)
                .saturating_add(pad_left)
                .saturating_add(hit_col),
        )
    };

    for _ in 0..3 {
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row,
            column,
            modifiers: KeyModifiers::empty(),
        })));
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            row,
            column,
            modifiers: KeyModifiers::empty(),
        })));
    }

    let copied = app.core_probe().clipboard.kill_ring.current();
    assert_eq!(copied, expected);
    assert_eq!(
        app.core_probe().clipboard.kill_ring.last_clipboard_write(),
        Some(expected)
    );
    assert!(
        copied.contains("quote before beta"),
        "multibyte quote must not shift copied text: {copied:?}"
    );

    let (scroll_top, viewport_rows, highlighted_lines, highlights) = {
        let (buf_id, scroll_top, viewport_rows) = {
            let win = app.transcript_window();
            (win.buf, win.scroll_top(), win.viewport.unwrap().rect.height)
        };
        let highlights = app.transcript_selection_highlights(scroll_top, 0, viewport_rows);
        let buf = app.ui_probe().buf(buf_id).expect("transcript buffer");
        let lines = highlights
            .iter()
            .filter_map(|(line, _, _)| buf.get_line(*line))
            .filter(|line| {
                line.contains("paragraph") || line.contains("curly") || line.contains("beta")
            })
            .count();
        (scroll_top, viewport_rows, lines, highlights)
    };
    assert!(
        highlighted_lines >= 2,
        "yank flash should cover multiple wrapped rows at scroll_top={scroll_top}, viewport_rows={viewport_rows}; highlights={highlights:?}"
    );
}

#[test]
fn user_message_padding_click_snaps_cursor_after_left_pad() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().with_vim(true).build();
    app.push_transcript_block(smelt_core::transcript_model::Block::User {
        text: "hello".into(),
        image_labels: vec![],
        command: false,
        sent_at_ms: None,
    });
    app.render_silent();

    let transcript_win = app.transcript_window();
    assert!(
        !transcript_win.has_materialized_rows(),
        "single user message should exercise the byte-backed path"
    );
    let vp = transcript_win
        .viewport
        .expect("transcript viewport after render");
    let column = vp
        .rect
        .left
        .saturating_add(vp.gutter_width)
        .saturating_add(transcript_win.gutter_pad_left);

    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row: vp.rect.top,
        column,
        modifiers: KeyModifiers::empty(),
    })));

    let win = app.transcript_window();
    let buf = app.ui_probe().buf(win.buf).expect("transcript buffer");
    assert_eq!(
        buf.display_cursor_pos(win.effective_endpoint()),
        (0, 1),
        "cursor should snap after the user-message left padding cell"
    );
}

#[test]
fn user_message_drag_to_line_end_does_not_select_bottom_padding_row() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().with_vim(true).build();
    app.push_transcript_block(smelt_core::transcript_model::Block::User {
        text: "hello".into(),
        image_labels: vec![],
        command: false,
        sent_at_ms: None,
    });
    app.render_silent();

    let transcript_win = app.transcript_window();
    assert!(
        !transcript_win.has_materialized_rows(),
        "single user message should exercise the byte-backed path"
    );
    let vp = transcript_win
        .viewport
        .expect("transcript viewport after render");
    let pad_left = transcript_win.gutter_pad_left;
    let buf = app
        .ui_probe()
        .buf(transcript_win.buf)
        .expect("transcript buffer");
    let text_row = buf
        .lines()
        .iter()
        .position(|line| line.contains("hello"))
        .expect("rendered user text row");
    let text_col = smelt_buffer::text::byte_to_cell(
        &buf.lines()[text_row],
        buf.lines()[text_row].find("hello").unwrap(),
    ) as u16;
    let row = vp.rect.top + text_row as u16;
    let column = vp
        .rect
        .left
        .saturating_add(vp.gutter_width)
        .saturating_add(pad_left)
        .saturating_add(text_col);

    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row,
        column,
        modifiers: KeyModifiers::empty(),
    })));
    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        row,
        column: column + "hello".len() as u16,
        modifiers: KeyModifiers::empty(),
    })));
    app.render_silent();

    let scroll_top = app.transcript_window().scroll_top;
    let highlights = app.transcript_selection_highlights(scroll_top, 0, vp.rect.height);
    assert!(
        highlights.iter().any(|(line, start, end)| {
            *line == text_row && *start == text_col && *end == text_col + "hello".len() as u16
        }),
        "text row should be highlighted, got {highlights:?}"
    );
    assert!(
            !highlights.iter().any(|(line, _, _)| *line > text_row),
            "selection ending after the last character must not include bottom padding, got {highlights:?}"
        );
}

#[test]
fn transcript_click_drag_in_vim_survives_multiple_drag_events() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().with_vim(true).build();
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: "hello world".into(),
    });
    app.render_silent();

    let transcript_win = app.transcript_window();
    assert!(
        !transcript_win.has_materialized_rows(),
        "fresh/small transcript should exercise the byte-backed path"
    );
    let vp = transcript_win
        .viewport
        .expect("transcript viewport after render");
    let pad_left = transcript_win.gutter_pad_left;
    let row = vp.rect.top;
    let column = vp
        .rect
        .left
        .saturating_add(vp.gutter_width)
        .saturating_add(pad_left);

    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row,
        column,
        modifiers: KeyModifiers::empty(),
    })));
    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        row,
        column: column + 2,
        modifiers: KeyModifiers::empty(),
    })));
    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        row,
        column: column + 4,
        modifiers: KeyModifiers::empty(),
    })));
    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        row,
        column: column + 4,
        modifiers: KeyModifiers::empty(),
    })));

    assert_eq!(
        app.core_probe().clipboard.kill_ring.current(),
        "hello",
        "vim drag selection should stay active across every Drag event"
    );
}
