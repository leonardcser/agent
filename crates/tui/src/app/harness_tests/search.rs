use super::*;

fn searchable_transcript_app() -> TestApp {
    let mut app = TestApp::builder().with_vim(true).build();
    app.set_terminal_size(80, 16);
    app.focus_transcript();
    app.configure_transcript_vim(true, VimMode::Normal);
    app
}

fn sparse_display_only_search_app() -> TestApp {
    let mut app = TestApp::builder().with_vim(true).build();
    app.set_terminal_size(80, 16);
    let session_id = app.session_snapshot().id.clone();
    let sessions_root = app.core_probe().sessions.sessions_dir();
    let mut session =
        smelt_core::session::Session::new(app.core_probe().env.pid(), app.core_probe().env.cwd());
    session.id = session_id.clone();
    session.history = (0..200)
        .map(|idx| protocol::HistoryItem::user(protocol::Content::text(format!("item {idx}"))))
        .collect();
    let mut commit = smelt_core::session::initial_store_commit_from_session(&session).unwrap();
    let records = (0..200)
        .map(|idx| {
            let content = match idx {
                20 => "early needle".to_string(),
                199 => "tail needle".to_string(),
                _ => format!("block {idx}"),
            };
            test_block_record(idx, &content)
        })
        .collect::<Vec<_>>();
    commit.transcript_records = Some(smelt_store::TranscriptRecordSuffix {
        start: smelt_store::TranscriptRecordIndex::ZERO,
        records,
    });
    let mut writer = smelt_store::OwnedLineageWriter::open(&sessions_root, &session_id).unwrap();
    let lineage_id = writer.lineage_id().to_string();
    let receipt = writer.commit_session(&commit).unwrap();
    let store_address = smelt_core::session::SessionStoreAddress::new(
        sessions_root.clone(),
        session_id.clone(),
        writer.lineage_id().to_string(),
    );
    let projector = writer.spawn_search_projector().unwrap();
    projector.request();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !projector.is_idle() {
        assert!(
            std::time::Instant::now() < deadline,
            "search fixture projection did not complete"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(projector.latest_error(), None);
    projector.stop();
    writer.release().unwrap();

    let loaded = crate::app::transcript::LoadedTranscript::tail_from_sqlite(store_address, 80, 16)
        .expect("display-only transcript tail");
    session.history.clear();
    app.load_store_backed_session(
        crate::app::session_document::StoreBackedSessionDocument::new(
            session,
            loaded,
            crate::app::history::store_backed_live_session_for_test(
                session_id.clone(),
                200,
                None,
                smelt_core::session::SessionStoreAddress::new(
                    sessions_root,
                    session_id,
                    lineage_id,
                ),
            ),
            receipt.current,
        ),
    );
    app.focus_transcript();
    app.configure_transcript_vim(true, VimMode::Normal);
    app.render_silent();
    app
}

fn wait_for_agent_turn(app: &mut TestApp) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if app.agent_running() {
            return;
        }
        if let Some(event) = app.try_recv_app_event() {
            app.app.handle_app_event(event);
        }
        app.app.drain_persist_reports();
        app.render_silent();
        assert!(
            std::time::Instant::now() < deadline,
            "deferred canonical turn did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

fn wait_for_live_search(app: &mut TestApp) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if app
            .overlays_probe()
            .search_session()
            .and_then(|session| session.current_range())
            .is_some()
        {
            return;
        }
        if let Some(event) = app.try_recv_app_event() {
            app.app.handle_app_event(event);
            app.render_silent();
            continue;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "live transcript search did not complete"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

fn wait_for_search_query(app: &mut TestApp, query: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if app
            .overlays_probe()
            .search_session()
            .is_some_and(|session| session.query == query)
        {
            return;
        }
        if let Some(event) = app.try_recv_app_event() {
            app.app.handle_app_event(event);
            app.render_silent();
            continue;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "live transcript search for {query:?} did not complete"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

fn drain_app_events_for(app: &mut TestApp, duration: std::time::Duration) {
    let deadline = std::time::Instant::now() + duration;
    while std::time::Instant::now() < deadline {
        if let Some(event) = app.try_recv_app_event() {
            app.app.handle_app_event(event);
            app.render_silent();
        } else {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

fn begin_slow_persisted_search(app: &mut TestApp) {
    app.app
        .set_transcript_search_worker_delay_for_harness(std::time::Duration::from_millis(80));
    app.type_char('/');
    app.type_text("needle");
}

#[test]
fn cancelling_hydration_pending_submit_rolls_back_without_dispatch() {
    let mut app = sparse_display_only_search_app();
    app.focus_prompt();
    app.app
        .conversation
        .require_transcript_record_resave_from_for_harness(0);
    let initial_history_len = app.session_message_count();

    app.type_text("cancel before hydration");
    app.press(KeyCode::Enter);
    assert!(app.app.turn_submission_is_pending());
    assert!(!app.agent_running());
    assert_eq!(app.session_message_count(), initial_history_len + 2);

    app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(!app.app.turn_submission_is_pending());
    assert!(!app.agent_running());
    assert_eq!(app.session_message_count(), initial_history_len + 1);

    drain_app_events_for(&mut app, std::time::Duration::from_millis(100));
    assert!(!app.app.turn_submission_is_pending());
    assert!(!app.agent_running());
    assert!(!app.session_is_read_only());
}

#[test]
fn hydration_preserves_terminal_transition_before_following_submit() {
    let mut app = sparse_display_only_search_app();
    app.focus_prompt();
    app.type_text("first ordered turn");
    app.press(KeyCode::Enter);
    assert!(app.agent_running());
    let _ = app.app.flush_persist();
    app.feed_one(crate::app::test_harness::SourceEvent::engine(
        protocol::EngineEvent::ToolStarted {
            invocation_id: protocol::InvocationId::new(1),
            call_id: "call-1".into(),
            tool_name: "read_file".into(),
            args: std::collections::HashMap::new(),
            called_at_ms: 1,
        },
    ));

    app.app
        .conversation
        .require_transcript_record_resave_from_for_harness(0);
    assert_eq!(
        app.app.discard_turn(crate::app::TurnEnd::Cancelled),
        crate::app::agent::TerminalCommitStatus::Deferred
    );
    assert!(!app.agent_running());
    assert!(app.app.conversation.canonical_operations_are_pending());

    assert!(app
        .app
        .begin_agent_turn(
            "second ordered turn",
            protocol::Content::text("second ordered turn"),
            0,
        )
        .is_none());
    assert!(app.app.turn_submission_is_pending());

    wait_for_agent_turn(&mut app);
    let _ = app.app.flush_persist();
    let reader = smelt_store::LineageSessionReader::open_existing(
        app.app.core.sessions.sessions_dir(),
        &app.app.conversation.session().id,
    )
    .expect("open ordered canonical lineage");
    let turns = reader.turns().expect("read ordered canonical turns");
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].state, smelt_store::TurnState::Cancelled);
    assert!(matches!(
        turns[1].state,
        smelt_store::TurnState::Ready | smelt_store::TurnState::Running
    ));
    assert_eq!(
        turns[1].submitted_revision.get(),
        turns[0].submitted_revision.get() + 3,
        "running and terminal transitions must commit before the following submit"
    );
}

#[test]
fn sparse_transcript_hydrates_interrupted_tool_turn_before_persisting() {
    let mut app = sparse_display_only_search_app();
    app.focus_prompt();
    app.type_text("first turn");
    app.press(KeyCode::Enter);
    assert!(app.agent_running());
    let _ = app.app.flush_persist();
    app.feed_one(crate::app::test_harness::SourceEvent::engine(
        protocol::EngineEvent::ToolStarted {
            invocation_id: protocol::InvocationId::new(1),
            call_id: "call-1".into(),
            tool_name: "read_file".into(),
            args: std::collections::HashMap::new(),
            called_at_ms: 1,
        },
    ));

    app.app
        .conversation
        .require_transcript_record_resave_from_for_harness(0);
    app.type_text("next turn");
    app.press(KeyCode::Enter);
    app.press(KeyCode::Enter);
    assert_eq!(app.state().queued_inputs, vec!["next turn"]);

    app.press(KeyCode::Enter);
    assert!(!app.agent_running());
    assert!(!app.session_is_read_only());
    assert_eq!(app.state().queued_inputs, vec!["next turn"]);

    wait_for_agent_turn(&mut app);
    assert!(!app.session_is_read_only());
    assert!(app.state().queued_inputs.is_empty());

    app.set_session_title(
        "Writable sparse interrupt".into(),
        "writable-sparse-interrupt".into(),
        None,
    );
    let _ = app.app.flush_persist();
    assert_eq!(
        app.session_snapshot().title.as_deref(),
        Some("Writable sparse interrupt")
    );
}

#[test]
fn transcript_search_renders_and_edits_unicode_by_grapheme() {
    let mut app = searchable_transcript_app();
    let input = "e\u{301}👩\u{200d}💻9\u{fe0f}🇨🇦";
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: format!("prefix {input} suffix").into(),
    });
    app.render_silent();
    app.type_char('/');
    app.type_text(input);
    wait_for_search_query(&mut app, input);

    let frame = app.render_to_frame().text();
    for grapheme in ["e\u{301}", "👩\u{200d}💻", "9\u{fe0f}", "🇨🇦"] {
        assert!(frame.contains(grapheme), "frame: {frame}");
    }
    assert!(app
        .overlays_probe()
        .search_session()
        .is_some_and(|session| session.query == input));

    app.press(KeyCode::Left);
    app.press(KeyCode::Backspace);
    app.press(KeyCode::Delete);
    wait_for_search_query(&mut app, "e\u{301}👩\u{200d}💻");

    assert!(app
        .overlays_probe()
        .search_session()
        .is_some_and(|session| session.query == "e\u{301}👩\u{200d}💻"));
}

fn test_block_record(block_idx: u64, content: &str) -> smelt_store::StoredTranscriptBlock {
    smelt_store::StoredTranscriptBlock {
        block_idx,
        history_idx: None,
        kind: "assistant".to_string(),
        tool_call_id: None,
        tool_name: None,
        content_hash: "0".to_string(),
        estimated_text_bytes: content.len() as u64,
        preview_text: content.to_string(),
        indexed_text: content.to_string(),
        block_json: serde_json::to_string(&smelt_core::Block::Text {
            content: content.to_string().into(),
        })
        .unwrap(),
        origin_json: Some(
            serde_json::to_string(&smelt_core::BlockOrigin::History(block_idx as usize)).unwrap(),
        ),
        tool_state_json: None,
        tool_render_revision: 0,
    }
}

#[test]
fn transcript_search_previews_live_and_enter_confirms_without_moving_again() {
    let mut app = row_document_transcript_app(100, true);
    app.type_char('g');
    app.type_char('g');
    let original_row = transcript_row_cursor_row(&app);

    app.type_char('/');
    app.type_text("row 090");
    app.render_silent();

    assert!(app.state().cmdline_open);
    let preview_row = transcript_row_cursor_row(&app);
    assert!(preview_row > original_row);
    let preview = app
        .overlays_probe()
        .search_session()
        .and_then(|session| session.current_range())
        .and_then(|range| range.rows())
        .expect("live search preview");
    assert_eq!(preview.start.row, preview_row);

    app.press(KeyCode::Enter);
    app.render_silent();
    assert!(!app.state().cmdline_open);
    assert_eq!(transcript_row_cursor_row(&app), preview_row);
}

#[test]
fn transcript_search_escape_and_empty_query_restore_original_anchor() {
    for empty_query in [false, true] {
        let mut app = row_document_transcript_app(100, true);
        app.type_char('2');
        app.type_char('0');
        app.type_char('G');
        app.render_silent();
        let original_row = transcript_row_cursor_row(&app);
        let original_scroll_top = app.transcript_window().scroll_top;

        app.type_char('/');
        app.type_text("row 090");
        app.render_silent();
        assert_ne!(transcript_row_cursor_row(&app), original_row);

        if empty_query {
            for _ in 0.."row 090".len() {
                app.press(KeyCode::Backspace);
            }
            app.render_silent();
            assert!(app.state().cmdline_open);
        } else {
            app.press(KeyCode::Esc);
            app.render_silent();
            assert!(!app.state().cmdline_open);
        }
        assert_eq!(transcript_row_cursor_row(&app), original_row);
        assert_eq!(app.transcript_window().scroll_top, original_scroll_top);
    }
}

#[test]
fn transcript_search_paste_and_history_changes_preview_live() {
    let mut app = row_document_transcript_app(100, true);
    app.type_char('g');
    app.type_char('g');
    app.type_char('/');
    app.app
        .cmdline_handle_event(crossterm::event::Event::Paste("row 090".into()));
    app.render_silent();
    let pasted_row = transcript_row_cursor_row(&app);
    assert!(pasted_row > 0);
    app.press(KeyCode::Enter);

    app.type_char('g');
    app.type_char('g');
    app.type_char('/');
    app.press(KeyCode::Up);
    app.render_silent();
    assert_eq!(transcript_row_cursor_row(&app), pasted_row);
    assert!(app.state().cmdline_open);
}

#[test]
fn transcript_search_previews_unloaded_match_before_enter() {
    let mut app = sparse_display_only_search_app();
    app.type_char('G');
    app.type_char('/');
    app.type_text("early needle");

    wait_for_live_search(&mut app);
    assert!(app.state().cmdline_open);
    let matched = app
        .overlays_probe()
        .search_session()
        .and_then(|session| session.current_range())
        .and_then(|range| range.rows())
        .expect("unloaded live search preview");
    let row = app
        .transcript_rows_and_breaks_range(matched.start.row, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(row.contains("early needle"), "previewed row: {row:?}");
}

#[test]
fn transcript_search_no_match_clears_preview_and_restores_original_anchor() {
    let mut app = sparse_display_only_search_app();
    app.type_char('g');
    app.type_char('g');
    app.render_silent();
    let original_row = transcript_row_cursor_row(&app);
    let original_scroll_top = app.transcript_window().scroll_top;

    app.type_char('/');
    app.type_text("tail needle");
    wait_for_live_search(&mut app);
    assert_ne!(transcript_row_cursor_row(&app), original_row);

    for _ in 0.."tail needle".len() {
        app.press(KeyCode::Backspace);
    }
    app.type_text("absent query");
    wait_for_search_query(&mut app, "absent query");

    let session = app
        .overlays_probe()
        .search_session()
        .expect("current no-match search session");
    assert!(session.current_range().is_none());
    assert_eq!(transcript_row_cursor_row(&app), original_row);
    assert_eq!(app.transcript_window().scroll_top, original_scroll_top);
    assert!(app.state().cmdline_open);
}

#[test]
fn transcript_search_rapid_typing_cancels_stale_worker_generations() {
    let mut app = sparse_display_only_search_app();
    app.type_char('G');
    app.app
        .set_transcript_search_worker_delay_for_harness(std::time::Duration::from_millis(80));
    app.type_char('/');

    // Keep lazy worker startup and empty-query anchor restoration outside the
    // steady-state generation handoff samples.
    app.type_char('x');
    app.render_silent();
    let mut samples = Vec::new();
    for character in "tail".chars() {
        let started = std::time::Instant::now();
        app.type_char(character);
        app.render_silent();
        samples.push(started.elapsed());
    }
    for _ in 0..4 {
        let started = std::time::Instant::now();
        app.press(KeyCode::Backspace);
        app.render_silent();
        samples.push(started.elapsed());
    }
    app.press(KeyCode::Backspace);
    app.render_silent();
    for character in "early needle".chars() {
        let started = std::time::Instant::now();
        app.type_char(character);
        app.render_silent();
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    let p95_index = (samples.len() * 95).div_ceil(100).saturating_sub(1);
    let p95 = samples[p95_index];
    eprintln!("TRANSCRIPT_LIVE_SEARCH_FRAME_P95_US {}", p95.as_micros());
    assert!(
        p95 <= std::time::Duration::from_millis(16),
        "live search key handling and redraw p95 was {p95:?}"
    );

    app.press(KeyCode::Enter);
    wait_for_live_search(&mut app);
    let matched = app
        .overlays_probe()
        .search_session()
        .and_then(|session| session.current_range())
        .and_then(|range| range.rows())
        .expect("latest live search result");
    let row = app
        .transcript_rows_and_breaks_range(matched.start.row, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(
        row.contains("early needle"),
        "stale search moved to {row:?}"
    );
}

#[test]
fn transcript_search_session_switch_discards_pending_results() {
    let mut app = sparse_display_only_search_app();
    begin_slow_persisted_search(&mut app);
    let previous_id = app.session_snapshot().id.clone();
    let replacement =
        smelt_core::session::Session::new(app.core_probe().env.pid(), app.core_probe().env.cwd());

    app.load_session(replacement);
    assert_ne!(app.session_snapshot().id, previous_id);
    drain_app_events_for(&mut app, std::time::Duration::from_millis(120));
    assert!(app.overlays_probe().search_session().is_none());
}

#[test]
fn transcript_search_rewind_cancels_pending_results() {
    let mut app = sparse_display_only_search_app();
    begin_slow_persisted_search(&mut app);

    app.rewind_to_history_index(Some(100), false);
    drain_app_events_for(&mut app, std::time::Duration::from_millis(120));
    assert!(app.overlays_probe().search_session().is_none());
}

#[test]
fn transcript_search_session_close_cancels_pending_results() {
    let mut app = sparse_display_only_search_app();
    begin_slow_persisted_search(&mut app);
    let previous_id = app.session_snapshot().id.clone();

    app.reset_session();
    assert_ne!(app.session_snapshot().id, previous_id);
    drain_app_events_for(&mut app, std::time::Duration::from_millis(120));
    assert!(app.overlays_probe().search_session().is_none());
}

#[test]
fn transcript_search_opens_status_input_and_repeats_matches() {
    let mut app = row_document_transcript_app(20, true);
    app.type_char('g');
    app.type_char('g');

    app.type_char('/');
    assert!(app.state().cmdline_open);
    app.type_text("alpha");
    app.press(KeyCode::Enter);
    app.render_silent();

    let first_match = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();
    assert_eq!(transcript_row_cursor_row(&app), first_match.start.row);

    app.type_char('n');
    app.render_silent();
    let next_match = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();
    assert_eq!(transcript_row_cursor_row(&app), next_match.start.row);
    assert!(next_match.start.row > first_match.start.row);
    app.type_char('N');
    app.render_silent();
    assert_eq!(transcript_row_cursor_row(&app), first_match.start.row);
}

#[test]
fn transcript_search_repeat_reaches_unloaded_sparse_matches() {
    let mut app = sparse_display_only_search_app();
    app.type_char('G');

    app.type_char('/');
    app.type_text("needle");
    app.press(KeyCode::Enter);
    wait_for_live_search(&mut app);

    let tail_match = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();
    let tail_row = app
        .transcript_rows_and_breaks_range(tail_match.start.row, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(
        tail_row.contains("tail needle"),
        "matched row: {tail_row:?}"
    );

    app.type_char('n');
    app.render_silent();
    let early_match = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();
    assert!(early_match.start.row < tail_match.start.row);
    assert_eq!(transcript_row_cursor_row(&app), early_match.start.row);
    let early_row = app
        .transcript_rows_and_breaks_range(early_match.start.row, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(
        early_row.contains("early needle"),
        "matched row after repeat: {early_row:?}"
    );
}

#[test]
fn transcript_search_reverse_repeat_reaches_unloaded_sparse_matches() {
    let mut app = sparse_display_only_search_app();
    app.type_char('G');

    app.type_char('/');
    app.type_text("needle");
    app.press(KeyCode::Enter);
    wait_for_live_search(&mut app);
    let tail_match = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();

    app.type_char('N');
    app.render_silent();
    let early_match = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();
    assert!(early_match.start.row < tail_match.start.row);
    assert_eq!(transcript_row_cursor_row(&app), early_match.start.row);
    let early_row = app
        .transcript_rows_and_breaks_range(early_match.start.row, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(
        early_row.contains("early needle"),
        "matched row after reverse repeat: {early_row:?}"
    );
}

#[test]
fn transcript_search_reverse_repeat_returns_to_cached_sparse_match() {
    let mut app = sparse_display_only_search_app();
    app.type_char('g');
    app.type_char('g');
    app.render_silent();

    app.type_char('/');
    app.type_text("needle");
    app.press(KeyCode::Enter);
    wait_for_live_search(&mut app);
    let early_match = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();
    let early_row = app
        .transcript_rows_and_breaks_range(early_match.start.row, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(
        early_row.contains("early needle"),
        "matched row: {early_row:?}"
    );

    app.type_char('n');
    app.render_silent();
    let tail_match = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap();
    assert!(tail_match.start.row > early_match.start.row);
    assert_eq!(transcript_row_cursor_row(&app), tail_match.start.row);

    app.type_char('N');
    app.render_silent();
    assert_eq!(transcript_row_cursor_row(&app), early_match.start.row);
    let row = app
        .transcript_rows_and_breaks_range(early_match.start.row, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(
        row.contains("early needle"),
        "cached reverse repeat did not reveal early match: {row:?}"
    );
}

#[test]
fn transcript_search_reverse_repeat_wraps_from_first_match() {
    let mut app = row_document_transcript_app(20, true);
    app.type_char('g');
    app.type_char('g');

    app.type_char('/');
    app.type_text("alpha");
    app.press(KeyCode::Enter);
    app.render_silent();
    let first_row = transcript_row_cursor_row(&app);

    app.type_char('N');
    app.render_silent();
    assert!(transcript_row_cursor_row(&app) > first_row);
    app.type_char('n');
    app.render_silent();
    assert_eq!(transcript_row_cursor_row(&app), first_row);
}

#[test]
fn transcript_search_jump_keeps_match_below_top_overlay() {
    let mut app = row_document_transcript_app(100, true);
    app.type_char('G');

    app.type_char('/');
    app.type_text("row 010");
    app.press(KeyCode::Enter);
    app.render_silent();

    let match_row = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap()
        .start
        .row;
    assert_eq!(transcript_row_cursor_row(&app), match_row);
    assert_eq!(app.transcript_window().scroll_top, match_row - 1);

    app.render_silent();
    assert_eq!(transcript_row_cursor_row(&app), match_row);
    assert_eq!(app.transcript_window().scroll_top, match_row - 1);
}

#[test]
fn transcript_search_jump_keeps_match_above_bottom_overlay() {
    let mut app = row_document_transcript_app(100, true);
    app.type_char('g');
    app.type_char('g');

    app.type_char('/');
    app.type_text("row 090");
    app.press(KeyCode::Enter);
    app.render_silent();

    let match_row = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap()
        .start
        .row;
    let viewport_rows = app
        .transcript_window()
        .viewport
        .map(|v| v.rect.height as crate::smelt_edit::RowIndex)
        .unwrap_or(1);
    assert_eq!(transcript_row_cursor_row(&app), match_row);
    assert_eq!(
        app.transcript_window().scroll_top,
        match_row.saturating_sub(viewport_rows.saturating_sub(2))
    );

    app.render_silent();
    assert_eq!(transcript_row_cursor_row(&app), match_row);
    assert_eq!(
        app.transcript_window().scroll_top,
        match_row.saturating_sub(viewport_rows.saturating_sub(2))
    );
}

#[test]
fn transcript_reveal_api_keeps_cursor_below_top_padding() {
    let mut app = row_document_transcript_app(100, true);
    app.type_char('G');

    assert!(app.run_lua("smelt.win.transcript():reveal(10, { top_padding = 1 })"));
    app.render_silent();

    assert_eq!(transcript_row_cursor_row(&app), 10);
    assert_eq!(app.transcript_window().scroll_top, 9);
}

#[test]
fn transcript_reveal_api_keeps_cursor_above_bottom_padding() {
    let mut app = row_document_transcript_app(100, true);
    app.type_char('g');
    app.type_char('g');

    assert!(app.run_lua("smelt.win.transcript():reveal(90, { bottom_padding = 1 })"));
    app.render_silent();

    let viewport_rows = app
        .transcript_window()
        .viewport
        .map(|v| v.rect.height as crate::smelt_edit::RowIndex)
        .unwrap_or(1);
    assert_eq!(transcript_row_cursor_row(&app), 90);
    assert_eq!(
        app.transcript_window().scroll_top,
        90u64.saturating_sub(viewport_rows.saturating_sub(2))
    );
}

#[test]
fn transcript_cursor_api_reads_absolute_row_after_scroll() {
    let mut app = row_document_transcript_app(100, true);
    app.type_char('g');
    app.type_char('g');

    assert!(app.run_lua(
        r#"
        smelt.win.transcript():reveal(90)
        _G.transcript_cursor_row = smelt.win.transcript():cursor()
        "#
    ));

    assert_eq!(app.lua_int_global("transcript_cursor_row"), Some(90));
    assert_eq!(
        transcript_row_cursor_row(&app),
        0,
        "physical cursor moved before the transcript projection committed"
    );

    app.render_silent();

    assert_eq!(transcript_row_cursor_row(&app), 90);
}

#[test]
fn transcript_search_paints_visible_matches_and_nohl_aliases_clear() {
    for command in ["nohl", "nohlsearch", "noh"] {
        let mut app = row_document_transcript_app(20, true);
        start_visible_transcript_search(&mut app);

        run_cmdline(&mut app, command);
        app.render_silent();

        let win = app.transcript_window();
        assert!(
            win.search_ranges.is_empty(),
            "{command} should clear search highlights"
        );
        assert!(app.overlays_probe().search_session().is_none());
    }
}

#[test]
fn transcript_search_paints_visible_matches_and_esc_clears() {
    let mut app = row_document_transcript_app(20, true);
    start_visible_transcript_search(&mut app);

    app.press(KeyCode::Esc);
    app.render_silent();
    let win = app.transcript_window();
    assert!(win.search_ranges.is_empty());
    assert!(app.overlays_probe().search_session().is_none());
}

fn start_visible_transcript_search(app: &mut TestApp) {
    app.type_char('g');
    app.type_char('g');
    app.type_char('/');
    app.type_text("row 000");
    app.press(KeyCode::Enter);
    app.render_silent();

    let win = app.transcript_window();
    assert!(
        !win.search_ranges.is_empty(),
        "submitted search should paint visible matches"
    );
}

fn run_cmdline(app: &mut TestApp, command: &str) {
    app.press(KeyCode::Char(':'));
    app.type_text(command);
    app.press(KeyCode::Enter);
}

#[test]
fn transcript_search_finds_compacted_divider_label() {
    let mut app = searchable_transcript_app();
    app.push_transcript_block(smelt_core::transcript_model::Block::Compacted {
        summary: "archived earlier turns".into(),
    });
    app.render_silent();

    app.type_char('/');
    app.type_text("compacted");
    app.press(KeyCode::Enter);

    let range = app
        .overlays_probe()
        .search_session()
        .and_then(|session| session.current_range())
        .and_then(|range| range.rows())
        .expect("compacted divider search match");
    let row = app
        .transcript_rows_and_breaks_range(range.start.row, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(row.contains("compacted"), "matched row: {row:?}");
}

#[test]
fn transcript_search_includes_selectable_compacted_separator_chrome() {
    let mut app = searchable_transcript_app();
    app.push_transcript_block(smelt_core::transcript_model::Block::Compacted {
        summary: "archived earlier turns".into(),
    });
    app.render_silent();

    let row = app
        .transcript_rows_and_breaks_range(0, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(row.contains('─'), "expected separator chrome: {row:?}");

    app.type_char('/');
    app.type_text("─");
    app.press(KeyCode::Enter);

    assert!(app
        .overlays_probe()
        .search_session()
        .and_then(|session| session.current_range())
        .is_some());
}

#[test]
fn transcript_search_finds_lua_rendered_collapsed_tool_detail() {
    let mut app = searchable_transcript_app();
    let mut args = std::collections::HashMap::new();
    args.insert("pattern".to_string(), serde_json::json!("needle"));
    let invocation_id = app.start_tool(
        "glob-call-1".into(),
        "glob".into(),
        protocol::StyledLines::from_plain("**/*.rs"),
        args,
    );
    app.finish_tool(
        invocation_id,
        smelt_core::transcript_model::ToolStatus::Ok,
        Some(Box::new(smelt_core::transcript_model::ToolOutput {
            content: "src/a.rs\nsrc/b.rs".into(),
            is_error: false,
            metadata: Some(serde_json::json!({
                "display_count": { "value": 12, "unit": "file" }
            })),
            content_fields: Vec::new(),
        })),
        None,
    );
    app.render_silent();

    app.type_char('/');
    app.type_text("12 files");
    app.press(KeyCode::Enter);

    let range = app
        .overlays_probe()
        .search_session()
        .and_then(|session| session.current_range())
        .and_then(|range| range.rows())
        .expect("collapsed tool detail search match");
    let row = app
        .transcript_rows_and_breaks_range(range.start.row, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert!(row.contains("12 files"), "matched row: {row:?}");
}

#[test]
fn backward_search_starts_from_previous_match() {
    let mut app = row_document_transcript_app(20, true);
    app.type_char('5');
    app.type_char('G');
    assert_eq!(transcript_row_cursor_row(&app), 4);

    app.press(KeyCode::Char('?'));
    assert!(app.state().cmdline_open);
    app.type_text("alpha");
    app.press(KeyCode::Enter);
    app.render_silent();
    let current_row = app
        .overlays_probe()
        .search_session()
        .unwrap()
        .current_range()
        .unwrap()
        .rows()
        .unwrap()
        .start
        .row;
    assert_eq!(transcript_row_cursor_row(&app), current_row);

    app.type_char('n');
    app.render_silent();
    assert!(transcript_row_cursor_row(&app) < current_row);
    app.type_char('N');
    app.render_silent();
    assert_eq!(transcript_row_cursor_row(&app), current_row);
}

#[test]
fn overlay_viewer_search_targets_focused_overlay() {
    let mut app = TestApp::builder().with_vim(true).build();
    let leaf =
        app.open_readonly_overlay_fixture(vec!["alpha beta".into(), "gamma delta".into()], None);
    app.render_silent();

    app.type_char('/');
    assert!(app.state().cmdline_open);
    app.type_text("gamma");
    app.press(KeyCode::Enter);

    let win = app.ui_probe().win(leaf).expect("overlay window");
    let buf = app.ui_probe().buf(win.buf).expect("overlay buffer");
    assert_eq!(buf.display_byte_pos(win.cpos()), (1, 0));
    assert_eq!(app.overlays_probe().search_session().unwrap().target, leaf);

    app.render_silent();
    let win = app.ui_probe().win(leaf).expect("overlay window");
    let buf_id = win.buf;
    assert!(
        win.range_layer(crate::smelt_edit::RangeLayer::Search)
            .iter()
            .any(|range| range.line == 1),
        "focused overlay search should paint visible matches"
    );

    app.set_window_lines(leaf, vec!["gamma moved".into(), "no match".into()]);
    app.render_silent();
    let win = app.ui_probe().win(leaf).expect("overlay window");
    let ranges = win.range_layer(crate::smelt_edit::RangeLayer::Search);
    assert!(
        ranges.iter().any(|range| range.line == 0),
        "search paint should follow live buffer contents"
    );
    assert!(
        !ranges.iter().any(|range| range.line == 1),
        "stale search ranges should not survive buffer rewrites"
    );

    app.set_window_lines(leaf, vec!["gamma first".into(), "gamma second".into()]);
    app.set_window_cursor(leaf, 0);
    app.type_char('n');
    let win = app.ui_probe().win(leaf).expect("overlay window");
    let buf = app.ui_probe().buf(buf_id).expect("overlay buffer");
    assert_eq!(buf.display_byte_pos(win.cpos()), (0, 0));
}

#[test]
fn viewer_search_ignores_non_selectable_spans() {
    let mut app = TestApp::builder().with_vim(true).build();
    let leaf = app.open_readonly_overlay_fixture(vec!["chrome real".into()], Some(6));
    app.render_silent();

    app.type_char('/');
    app.type_text("chrome");
    app.press(KeyCode::Enter);
    assert!(app
        .overlays_probe()
        .search_session()
        .unwrap()
        .full_matches()
        .is_empty());
    app.render_silent();
    let win = app.ui_probe().win(leaf).expect("overlay window");
    assert!(win
        .range_layer(crate::smelt_edit::RangeLayer::Search)
        .is_empty());

    app.type_char('/');
    app.type_text("real");
    app.press(KeyCode::Enter);
    let session = app.overlays_probe().search_session().unwrap();
    assert_eq!(session.full_matches().len(), 1);
    assert_eq!(
        session.full_matches()[0].rows().unwrap().start.byte_col,
        "chrome ".len()
    );
    app.render_silent();
    let win = app.ui_probe().win(leaf).expect("overlay window");
    assert!(
        !win.range_layer(crate::smelt_edit::RangeLayer::Search)
            .is_empty(),
        "selectable match should paint"
    );
}
