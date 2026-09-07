use super::*;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use smelt_core::transcript_model::{Block, TranscriptBlockRecord};

use crate::app::search::SearchDirection;
use crate::app::transcript::TranscriptDocument;
use crate::app::transcript_scroll_trace::{
    TranscriptProjectionTargetTrace, TranscriptRecordTraceRange, TranscriptScrollIntent,
    TranscriptScrollTraceFrame, TranscriptTraceAnchor,
};
use crate::smelt_edit::VimMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptScrollProbeEdge {
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptScrollProbeCommand {
    MoveDown,
    MoveUp,
    PageDown,
    PageUp,
    HalfPageDown,
    HalfPageUp,
    JumpTop,
    JumpBottom,
}

static SPARSE_FIXTURE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
struct UserDeltaAnchor {
    sign: i8,
    viewport_top: crate::smelt_edit::RowIndex,
    virtual_row: crate::smelt_edit::RowIndex,
    record_index: usize,
    block_id: smelt_core::transcript_model::BlockId,
    row_offset: crate::smelt_edit::RowIndex,
}

impl UserDeltaAnchor {
    fn same_position(self, other: Self) -> bool {
        self.viewport_top == other.viewport_top
            && self.virtual_row == other.virtual_row
            && self.record_index == other.record_index
            && self.block_id == other.block_id
            && self.row_offset == other.row_offset
    }
}

#[derive(Debug)]
struct PendingWheelMovement {
    rows: isize,
    viewport_before: Vec<String>,
}

#[derive(Default)]
pub(super) struct TranscriptScrollProbeState {
    drag_edge: Option<TranscriptScrollProbeEdge>,
    last_user_delta_anchor: Option<UserDeltaAnchor>,
    pending_wheel_movement: Option<PendingWheelMovement>,
    pending_search_match_check: bool,
    fixture: Option<SparseTranscriptFixture>,
}

impl TranscriptScrollProbeState {
    fn keep_fixture_alive(&self) {
        if let Some(fixture) = &self.fixture {
            let _ = fixture.store();
        }
    }
}

struct SparseTranscriptFixture {
    root: std::path::PathBuf,
    store: smelt_core::session::SessionStoreAddress,
}

impl SparseTranscriptFixture {
    fn new(root: std::path::PathBuf) -> Self {
        let _ = std::fs::remove_dir_all(&root);
        let session_id = "f".repeat(64);
        let mut session = smelt_core::session::Session::new(1, root.clone());
        session.id = session_id.clone();
        let initial = smelt_core::session::initial_store_commit_from_session(&session)
            .expect("build initial transcript fixture");
        let mut writer = smelt_store::OwnedLineageWriter::open(&root, &session_id)
            .expect("open transcript fixture writer");
        writer
            .commit_session(&initial)
            .expect("commit initial transcript fixture");
        let lineage_id = writer.lineage_id().to_string();
        writer.release().expect("release transcript fixture writer");
        let store =
            smelt_core::session::SessionStoreAddress::new(root.clone(), session_id, lineage_id);
        Self { root, store }
    }

    fn store(&self) -> &smelt_core::session::SessionStoreAddress {
        &self.store
    }
}

impl Drop for SparseTranscriptFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn frames_include_search_jump(frames: &[TranscriptScrollTraceFrame]) -> bool {
    frames.iter().any(|frame| {
        matches!(
            frame.scroll_intent,
            TranscriptScrollIntent::SearchJump { .. }
        )
    })
}

impl TestApp {
    pub fn transcript_window(&self) -> TranscriptWindowSnapshot {
        let win = self.app.transcript_win();
        let scroll_top = win.scroll_top();
        TranscriptWindowSnapshot {
            buf: win.buf,
            scroll_top,
            following_tail: win.is_following_tail(),
            viewport: win.viewport,
            document_view: win.document_view_state(),
            row_cursor: win.row_cursor(),
            materialized_rows: win.materialized_rows(),
            vim_mode: win.vim_mode(),
            gutter_pad_left: win.config.gutters.pad_left,
            effective_endpoint: win.effective_endpoint(),
            cursor_absolute_row: win
                .cursor_screen_row(u16::MAX)
                .map(|row| scroll_top.saturating_add(row.into())),
            search_ranges: win
                .range_layer(crate::smelt_edit::RangeLayer::Search)
                .to_vec(),
        }
    }

    pub fn transcript_buffer_text(&self) -> String {
        let win = self.app.transcript_win();
        self.app
            .ui
            .buf(win.buf)
            .map(|buf| buf.source().to_string())
            .unwrap_or_default()
    }

    pub fn transcript_cursor_screen_row(&self, viewport_rows: u16) -> Option<u16> {
        self.app.transcript_win().cursor_screen_row(viewport_rows)
    }

    pub fn configure_transcript_vim(&mut self, enabled: bool, mode: VimMode) {
        self.app.transcript_win_mut().set_vim_enabled(enabled);
        self.app.transcript_win_mut().set_vim_mode(mode);
    }

    pub fn follow_transcript_tail(&mut self) {
        self.app.transcript_win_mut().follow_tail();
    }

    pub(crate) fn pan_transcript_by_lines(&mut self, lines: isize, viewport_rows: u16) {
        let buf_id = self.app.transcript_win().buf;
        let (window, buffer) = self
            .app
            .ui
            .win_and_buf_mut(crate::app::TRANSCRIPT_WIN, buf_id);
        window.expect("transcript window").pan_by_lines(
            buffer.expect("transcript buffer"),
            lines,
            viewport_rows,
        );
    }

    pub(crate) fn jump_transcript_to_bottom(&mut self) {
        let buf_id = self.app.transcript_win().buf;
        let (window, buffer) = self
            .app
            .ui
            .win_and_buf_mut(crate::app::TRANSCRIPT_WIN, buf_id);
        window
            .expect("transcript window")
            .jump_to_bottom(buffer.expect("transcript buffer"));
    }

    pub fn pin_transcript_scroll(&mut self, row: crate::smelt_edit::RowIndex) {
        self.app.transcript_win_mut().pin_scroll(row);
    }

    pub fn set_transcript_document_view(&mut self, state: crate::smelt_edit::DocumentViewState) {
        self.app.transcript_win_mut().set_document_view_state(state);
    }

    #[cfg(feature = "transcript-bench")]
    pub(crate) fn hydrate_transcript_blocks(
        &mut self,
        ids: &[smelt_core::transcript_model::BlockId],
    ) -> bool {
        if ids.is_empty() {
            return true;
        }
        if !self
            .app
            .conversation
            .ensure_transcript_blocks_hydrated_for_harness(ids)
        {
            self.render_silent();
        }
        !self
            .app
            .conversation
            .transcript()
            .deferred_operation_blocks_failed(ids)
    }

    pub(crate) fn reveal_transcript_record_block(
        &mut self,
        record_index: usize,
        top_padding: crate::smelt_edit::RowIndex,
        move_cursor: bool,
    ) -> bool {
        for _ in 0..16 {
            if self
                .app
                .reveal_transcript_record_block(record_index, top_padding, move_cursor)
            {
                return true;
            }
            if !self.app.conversation.transcript_hydration_is_pending() {
                return false;
            }
            self.render_silent();
        }
        false
    }

    pub(crate) fn tick_drag_autoscroll_with_transcript_intent(&mut self) -> bool {
        self.app.tick_drag_autoscroll_with_transcript_intent()
    }

    pub(crate) fn submit_search(
        &mut self,
        target: WinId,
        direction: crate::app::search::SearchDirection,
        query: String,
    ) {
        self.app.submit_search(target, direction, query);
    }

    pub(crate) fn document_action_at(
        &mut self,
        win: WinId,
        pos: crate::smelt_edit::DocPosition,
    ) -> Option<crate::smelt_edit::SpanAction> {
        self.app.document_action_at(win, pos)
    }

    pub(crate) fn document_view_position_at_mouse_for_win(
        &mut self,
        win: WinId,
        event: crossterm::event::MouseEvent,
    ) -> Option<crate::smelt_edit::DocPosition> {
        self.app.document_view_position_at_mouse_for_win(win, event)
    }

    pub(crate) fn transcript_selection_highlights(
        &mut self,
        scroll_top: crate::smelt_edit::RowIndex,
        row_base: crate::smelt_edit::RowIndex,
        viewport_rows: u16,
    ) -> Vec<(usize, u16, u16)> {
        self.app
            .transcript_selection_highlights(scroll_top, row_base, viewport_rows)
    }

    pub(crate) fn transcript_has_row_selection(&self, now: std::time::Instant) -> bool {
        let win = self.app.transcript_win();
        self.app
            .ui
            .buf(win.buf)
            .is_some_and(|buf| win.row_selection_range(buf, now).is_some())
    }

    pub(crate) fn materialize_loaded_transcript_display_rows_expensive(
        &mut self,
    ) -> std::sync::Arc<Vec<String>> {
        self.app
            .materialize_loaded_transcript_display_rows_expensive()
    }

    pub(crate) fn set_transcript_scroll_trace_for_harness(&mut self, enabled: bool) {
        self.app
            .conversation
            .set_transcript_scroll_trace_for_harness(enabled);
    }

    pub(crate) fn set_next_transcript_scroll_trace_input(
        &mut self,
        input: crate::app::transcript_scroll_trace::TranscriptScrollTraceRenderInput,
    ) {
        self.app
            .conversation
            .set_next_transcript_scroll_trace_input(input);
    }

    #[cfg(test)]
    pub(crate) fn set_transcript_scroll_trace_timings_for_harness(&mut self, enabled: bool) {
        self.app
            .conversation
            .set_transcript_scroll_trace_timings_for_harness(enabled);
    }

    pub(crate) fn take_transcript_scroll_trace_frames_for_harness(
        &mut self,
    ) -> Vec<crate::app::transcript_scroll_trace::TranscriptScrollTraceFrame> {
        self.app
            .conversation
            .take_transcript_scroll_trace_frames_for_harness()
    }

    #[cfg(test)]
    pub(crate) fn take_transcript_interaction_trace_events_for_harness(
        &mut self,
    ) -> Vec<crate::app::transcript_scroll_trace::TranscriptInteractionTraceEvent> {
        self.app
            .conversation
            .take_transcript_interaction_trace_events_for_harness()
    }

    #[cfg(test)]
    pub(crate) fn set_transcript_memory_budget_for_harness(
        &mut self,
        budget: crate::app::transcript::TranscriptMemoryBudget,
    ) {
        self.app
            .conversation
            .set_transcript_memory_budget_for_harness(budget);
    }

    #[cfg(test)]
    pub(crate) fn drain_transcript_compaction_for_harness(&mut self) {
        self.app
            .conversation
            .drain_transcript_compaction_for_harness();
    }

    #[cfg(test)]
    pub(crate) fn transcript_tail_state_for_harness(
        &self,
    ) -> Option<(usize, smelt_core::transcript_model::BlockId, bool)> {
        self.app.conversation.transcript_tail_state_for_harness()
    }

    #[cfg(test)]
    pub(crate) fn require_transcript_record_resave_from_for_harness(&mut self, index: usize) {
        self.app
            .conversation
            .require_transcript_record_resave_from_for_harness(index);
    }

    #[cfg(test)]
    pub(crate) fn insert_transcript_checkpoint_for_harness(
        &mut self,
        block_index: usize,
        history_index: usize,
        block: smelt_core::transcript_model::Block,
    ) {
        self.app
            .conversation
            .insert_checkpoint_marker(block_index, history_index, block);
    }

    #[cfg(test)]
    pub(crate) fn replace_loaded_transcript_for_harness(
        &mut self,
        transcript: crate::app::transcript::LoadedTranscript,
    ) {
        self.app
            .conversation
            .replace_loaded_transcript_for_harness(transcript);
    }

    pub(crate) fn replace_transcript_document_for_harness(
        &mut self,
        transcript: crate::app::transcript::TranscriptDocument,
    ) {
        self.app
            .conversation
            .replace_transcript_document_for_harness(transcript);
    }

    pub(crate) fn with_pinned_transcript_blocks<R>(
        &mut self,
        ids: &[smelt_core::transcript_model::BlockId],
        f: impl FnOnce(&smelt_core::transcript_model::BlockHistory) -> R,
    ) -> Option<R> {
        self.app.conversation.with_pinned_transcript_blocks(ids, f)
    }

    #[cfg(test)]
    pub(crate) fn transcript_total_rows(&mut self) -> crate::smelt_edit::RowIndex {
        self.app.transcript_total_rows()
    }

    #[cfg(test)]
    pub(crate) fn transcript_rows_and_breaks_range(
        &mut self,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> crate::smelt_edit::DisplayRows {
        self.app.transcript_rows_and_breaks_range(start, count)
    }

    pub(crate) fn record_transcript_scroll_intent(
        &mut self,
        label: impl Into<String>,
        intent: crate::app::transcript_scroll_trace::TranscriptScrollIntent,
        window_scroll_before: crate::smelt_edit::RowIndex,
    ) {
        self.app
            .record_transcript_scroll_intent(label, intent, window_scroll_before);
    }

    pub(crate) fn scroll_at_with_transcript_intent(
        &mut self,
        row: u16,
        col: u16,
        rows: isize,
        label: &str,
    ) -> bool {
        self.app
            .scroll_at_with_transcript_intent(row, col, rows, label)
    }

    pub(crate) fn execute_document_view_command_for_win(
        &mut self,
        win: WinId,
        command: crate::smelt_edit::DocumentCommand,
        viewport_rows: u16,
        now: std::time::Instant,
    ) -> Option<crate::smelt_edit::DocRange> {
        self.app
            .execute_document_view_command_for_win(win, command, viewport_rows, now)
    }

    pub fn install_sparse_transcript_scroll_fixture(
        &mut self,
        record_count: usize,
        width: u16,
        height: u16,
    ) {
        let record_count = record_count.clamp(96, 900);
        let width = width.clamp(32, 140);
        let height = height.clamp(8, 40);
        let records = heterogeneous_resume_records(record_count);
        let fixture_id = SPARSE_FIXTURE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let fixture = SparseTranscriptFixture::new(
            managed_harness_dir("transcript-scroll").join(format!("fixture-{fixture_id}")),
        );
        crate::persist::write_transcript_record_suffix(fixture.store(), 0, &records)
            .expect("write record suffix");
        let loaded = crate::app::transcript::LoadedTranscript::tail_from_sqlite(
            fixture.store().clone(),
            width,
            height,
        )
        .expect("tail transcript");

        self.set_terminal_size(width, height);
        self.app
            .conversation
            .replace_transcript_document_for_harness(TranscriptDocument::from_loaded_transcript(
                loaded,
            ));
        self.app.app_focus = AppFocus::Content;
        self.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
        self.app.transcript_win_mut().set_vim_enabled(true);
        self.app
            .transcript_win_mut()
            .set_vim_mode(crate::smelt_edit::VimMode::Normal);
        self.app.transcript_win_mut().follow_tail();
        self.app
            .conversation
            .set_transcript_scroll_trace_for_harness(true);
        self.transcript_scroll_probe = TranscriptScrollProbeState {
            fixture: Some(fixture),
            ..TranscriptScrollProbeState::default()
        };
        self.render_silent();
        self.app
            .conversation
            .take_transcript_scroll_trace_frames_for_harness();
    }

    pub fn transcript_scroll_probe_render(&mut self) {
        self.transcript_scroll_probe_render_frames();
    }

    fn transcript_scroll_probe_render_frames(&mut self) -> Vec<TranscriptScrollTraceFrame> {
        self.transcript_scroll_probe.keep_fixture_alive();
        let pending_wheel_movement = self.transcript_scroll_probe.pending_wheel_movement.take();
        let check_search_match =
            std::mem::take(&mut self.transcript_scroll_probe.pending_search_match_check);
        self.render_silent();
        let frames = self
            .app
            .conversation
            .take_transcript_scroll_trace_frames_for_harness();
        assert_transcript_scroll_probe_frames(&mut self.transcript_scroll_probe, &frames);
        if let Some(pending) = pending_wheel_movement {
            let viewport_after = self.transcript_viewport_lines();
            assert_wheel_movement_visible(&pending, &viewport_after, &frames);
        }
        if check_search_match && frames_include_search_jump(&frames) {
            self.assert_current_transcript_search_match();
        }
        self.assert_invariants();
        frames
    }

    pub fn transcript_scroll_probe_no_input_render(&mut self) {
        let before_scroll = self.app.transcript_win().scroll_top();
        let frames = self.transcript_scroll_probe_render_frames();
        let after_scroll = self.app.transcript_win().scroll_top();
        if after_scroll != before_scroll {
            assert!(
                no_input_scroll_top_change_preserved_content(&frames),
                "no-input render changed transcript scroll_top from {before_scroll} to {after_scroll}: {frames:?}"
            );
        }
    }

    pub fn transcript_scroll_probe_wheel(&mut self, down: bool, rel_row: u16) {
        let effective_rows = if down && self.transcript_window().following_tail {
            0
        } else if down {
            3
        } else {
            -3
        };
        if let Some(pending) = &mut self.transcript_scroll_probe.pending_wheel_movement {
            pending.rows = pending.rows.saturating_add(effective_rows);
        } else {
            self.transcript_scroll_probe.pending_wheel_movement = Some(PendingWheelMovement {
                rows: effective_rows,
                viewport_before: self.transcript_viewport_lines(),
            });
        }

        let (row, col) = self.transcript_scroll_probe_content_point(rel_row, 1);
        let kind = if down {
            MouseEventKind::ScrollDown
        } else {
            MouseEventKind::ScrollUp
        };
        self.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            MouseEvent {
                kind,
                row,
                column: col,
                modifiers: KeyModifiers::empty(),
            },
        )));
    }

    pub fn transcript_scroll_probe_content_click(&mut self, rel_row: u16, rel_col: u16) {
        let (row, col) = self.transcript_scroll_probe_content_point(rel_row, rel_col);
        self.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                row,
                column: col,
                modifiers: KeyModifiers::empty(),
            },
        )));
    }

    pub fn transcript_scroll_probe_drag_select(
        &mut self,
        from_row: u16,
        to_row: u16,
        rel_col: u16,
    ) {
        let (start_row, col) = self.transcript_scroll_probe_content_point(from_row, rel_col);
        let (end_row, _) = self.transcript_scroll_probe_content_point(to_row, rel_col);
        self.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                row: start_row,
                column: col,
                modifiers: KeyModifiers::empty(),
            },
        )));
        self.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                row: end_row,
                column: col,
                modifiers: KeyModifiers::empty(),
            },
        )));
    }

    pub fn transcript_scroll_probe_start_edge_drag(&mut self, edge: TranscriptScrollProbeEdge) {
        let vp = self
            .app
            .transcript_win()
            .viewport
            .expect("transcript viewport");
        let col = vp
            .rect
            .left
            .saturating_add(vp.gutter_width)
            .saturating_add(1);
        let down_row = vp.rect.top.saturating_add(vp.rect.height / 2);
        let edge_row = match edge {
            TranscriptScrollProbeEdge::Top => vp.rect.top,
            TranscriptScrollProbeEdge::Bottom => vp.rect.bottom().saturating_sub(1),
        };
        self.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                row: down_row,
                column: col,
                modifiers: KeyModifiers::empty(),
            },
        )));
        self.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                row: edge_row,
                column: col,
                modifiers: KeyModifiers::empty(),
            },
        )));
        self.transcript_scroll_probe.drag_edge = Some(edge);
    }

    pub fn transcript_scroll_probe_drag_autoscroll_tick(&mut self) -> bool {
        self.app.tick_drag_autoscroll_with_transcript_intent()
    }

    pub fn transcript_scroll_probe_finish_drag(&mut self) {
        let (row, col) = self.transcript_scroll_probe_content_point(1, 1);
        self.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                row,
                column: col,
                modifiers: KeyModifiers::empty(),
            },
        )));
        self.transcript_scroll_probe.drag_edge = None;
    }

    pub fn transcript_scroll_probe_start_scrollbar_drag(&mut self, rel_row: u16) {
        self.transcript_scroll_probe_scrollbar_event(
            rel_row,
            MouseEventKind::Down(MouseButton::Left),
        );
    }

    pub fn transcript_scroll_probe_drag_scrollbar(&mut self, rel_row: u16) {
        self.transcript_scroll_probe_scrollbar_event(
            rel_row,
            MouseEventKind::Drag(MouseButton::Left),
        );
    }

    pub fn transcript_scroll_probe_finish_scrollbar_drag(&mut self, rel_row: u16) {
        self.transcript_scroll_probe_scrollbar_event(
            rel_row,
            MouseEventKind::Up(MouseButton::Left),
        );
    }

    pub fn transcript_scroll_probe_scrollbar_click(&mut self, rel_row: u16) {
        self.transcript_scroll_probe_start_scrollbar_drag(rel_row);
        self.transcript_scroll_probe_finish_scrollbar_drag(rel_row);
    }

    fn transcript_scroll_probe_scrollbar_event(&mut self, rel_row: u16, kind: MouseEventKind) {
        let Some(vp) = self.app.transcript_win().viewport else {
            return;
        };
        let Some(scrollbar) = vp.scrollbar else {
            return;
        };
        let row = vp
            .rect
            .top
            .saturating_add(rel_row.min(vp.rect.height.saturating_sub(1)));
        self.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            MouseEvent {
                kind,
                row,
                column: scrollbar.col,
                modifiers: KeyModifiers::empty(),
            },
        )));
    }

    pub fn transcript_scroll_probe_command(&mut self, command: TranscriptScrollProbeCommand) {
        self.app.app_focus = AppFocus::Content;
        self.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
        self.app.transcript_win_mut().set_vim_enabled(true);
        self.app.transcript_win_mut().set_vim_mode(VimMode::Normal);
        match command {
            TranscriptScrollProbeCommand::MoveDown => self.press(KeyCode::Down),
            TranscriptScrollProbeCommand::MoveUp => self.press(KeyCode::Up),
            TranscriptScrollProbeCommand::PageDown => self.press(KeyCode::PageDown),
            TranscriptScrollProbeCommand::PageUp => self.press(KeyCode::PageUp),
            TranscriptScrollProbeCommand::HalfPageDown => {
                self.press_mod(KeyCode::Char('d'), KeyModifiers::CONTROL)
            }
            TranscriptScrollProbeCommand::HalfPageUp => {
                self.press_mod(KeyCode::Char('u'), KeyModifiers::CONTROL)
            }
            TranscriptScrollProbeCommand::JumpTop => {
                self.type_char('g');
                self.type_char('g');
            }
            TranscriptScrollProbeCommand::JumpBottom => self.type_char('G'),
        }
    }

    pub fn transcript_scroll_probe_reveal_record(&mut self, record_index: usize) {
        let total = self
            .app
            .conversation
            .transcript()
            .record_total_count()
            .unwrap_or(1)
            .max(1);
        let record_index = record_index % total;
        let _ = self
            .app
            .reveal_transcript_record_block(record_index, 1, true);
    }

    pub fn transcript_scroll_probe_search_record(&mut self, record_index: usize) {
        let total = self
            .app
            .conversation
            .transcript()
            .record_total_count()
            .unwrap_or(1)
            .max(1);
        let record_index = record_index % total;
        self.app.submit_search(
            crate::app::TRANSCRIPT_WIN,
            SearchDirection::Forward,
            format!("record-{record_index:04}"),
        );
        self.transcript_scroll_probe.pending_search_match_check = self
            .app
            .overlays
            .search_session()
            .and_then(|session| session.current_range())
            .is_some()
            && self.app.pending_transcript_search.is_none();
    }

    pub fn transcript_scroll_probe_search_common_text(&mut self) {
        self.app.submit_search(
            crate::app::TRANSCRIPT_WIN,
            SearchDirection::Forward,
            "markdown".into(),
        );
        self.transcript_scroll_probe.pending_search_match_check = self
            .app
            .overlays
            .search_session()
            .and_then(|session| session.current_range())
            .is_some()
            && self.app.pending_transcript_search.is_none();
    }

    pub fn transcript_scroll_probe_repeat_search(&mut self, reverse: bool) {
        let has_match = self
            .app
            .overlays
            .search_session()
            .and_then(|session| session.current_range())
            .is_some();
        if has_match {
            self.type_char(if reverse { 'N' } else { 'n' });
            self.transcript_scroll_probe.pending_search_match_check =
                self.app.pending_transcript_search.is_none();
        }
    }

    pub fn transcript_scroll_probe_resize(&mut self, width: u16, height: u16) {
        self.transcript_scroll_probe.pending_search_match_check =
            self.current_transcript_search_match_is_cursor();
        self.set_terminal_size(width, height);
    }

    fn current_transcript_search_match_is_cursor(&self) -> bool {
        let Some(range) = self
            .app
            .overlays
            .search_session()
            .and_then(|session| session.current_range())
            .and_then(|range| range.rows())
        else {
            return false;
        };
        self.app
            .transcript_win()
            .row_cursor()
            .is_some_and(|position| position.row == range.start.row)
    }

    fn assert_current_transcript_search_match(&self) {
        let (query, range) = self
            .app
            .overlays
            .search_session()
            .and_then(|session| {
                session
                    .current_range()
                    .and_then(|range| range.rows())
                    .map(|range| (session.query.clone(), range))
            })
            .expect("pending transcript search check has a current match");
        let window = self.app.transcript_win();
        let materialized = window
            .materialized_rows()
            .expect("search result has materialized transcript rows");
        if !materialized.contains_abs_row(range.start.row) {
            return;
        }
        let local_row = crate::smelt_edit::row_to_usize(materialized.local_row(range.start.row));
        let buffer = self
            .app
            .ui
            .buf(window.buf)
            .expect("transcript search buffer");
        let row = buffer.get_line(local_row).unwrap_or_default();
        let matched_text = smelt_buffer::text::slice(row, range.start.byte_col..range.end.byte_col);
        if matched_text != query {
            return;
        }
        assert_eq!(
            window.row_cursor().map(|position| position.row),
            Some(range.start.row),
            "search cursor and current result diverged"
        );
    }

    pub fn transcript_scroll_probe_append(&mut self, variant: u8) {
        let marker = format!("fuzz-append-{variant:03}");
        let content = match variant % 4 {
            0 => format!("{marker} assistant append {}", "tail ".repeat(16)),
            1 => format!("{marker}\n\n```rust\nfn appended() {{}}\n```"),
            2 => format!("{marker} markdown paragraph {}", "wrap ".repeat(32)),
            _ => format!("{marker} compact-ish summary {}", "summary ".repeat(12)),
        };
        self.app.push_block(Block::Text {
            content: content.into(),
        });
    }

    pub fn transcript_scroll_probe_follow_tail(&mut self) {
        self.app.scroll_window(
            crate::app::TRANSCRIPT_WIN,
            crate::app::transcript_scroll::WindowScrollCommand::Tail,
        );
    }

    pub(crate) fn transcript_viewport_lines(&self) -> Vec<String> {
        let window = self.transcript_window();
        let buffer = self.app.ui.buf(window.buf).expect("transcript buffer");
        let viewport_rows = window
            .viewport
            .map(|viewport| usize::from(viewport.rect.height))
            .unwrap_or(0);
        let start = window.local_visual_row(window.scroll_top) as usize;
        (start..start.saturating_add(viewport_rows))
            .map(|row| buffer.get_line(row).unwrap_or_default().to_string())
            .collect()
    }

    fn transcript_scroll_probe_content_point(&self, rel_row: u16, rel_col: u16) -> (u16, u16) {
        let vp = self
            .app
            .transcript_win()
            .viewport
            .expect("transcript viewport");
        let row = vp
            .rect
            .top
            .saturating_add(rel_row.min(vp.rect.height.saturating_sub(1)));
        let col = vp
            .rect
            .left
            .saturating_add(vp.gutter_width)
            .saturating_add(rel_col.min(vp.content_width.saturating_sub(1)));
        (row, col)
    }
}

fn assert_wheel_movement_visible(
    pending: &PendingWheelMovement,
    viewport_after: &[String],
    frames: &[TranscriptScrollTraceFrame],
) {
    if !has_user_delta_frame(frames) {
        return;
    }

    assert_eq!(
        viewport_after.len(),
        pending.viewport_before.len(),
        "wheel movement changed the visible transcript height: pending={pending:?}, after={viewport_after:?}, frames={frames:?}"
    );
    if viewport_after == pending.viewport_before {
        return;
    }
    if pending.rows == 0 {
        let max_rows = max_leading_rows_before_content(frames)
            .max(1)
            .min(viewport_after.len().saturating_sub(1));
        let stable_overlap = (1..=max_rows).any(|rows| {
            let overlap = viewport_after.len() - rows;
            viewport_after[rows..] == pending.viewport_before[..overlap]
                || viewport_after[..overlap] == pending.viewport_before[rows..]
        });
        assert!(
            stable_overlap || sparse_zero_delta_preserved_anchor(frames),
            "wheel events with no net movement changed visible transcript content: pending={pending:?}, after={viewport_after:?}"
        );
        return;
    }

    let effective_rows = effective_wheel_rows(pending.rows, frames);
    let max_rows = effective_rows
        .unsigned_abs()
        .saturating_add(max_leading_rows_before_content(frames));
    if effective_rows.unsigned_abs() >= viewport_after.len() {
        return;
    }
    let max_rows = max_rows.min(viewport_after.len().saturating_sub(1));
    let movement = (1..=max_rows).find(|&rows| {
        let overlap = viewport_after.len() - rows;
        if pending.rows < 0 {
            viewport_after[rows..] == pending.viewport_before[..overlap]
        } else {
            viewport_after[..overlap] == pending.viewport_before[rows..]
        }
    });
    // Sparse hydration can replace preview rows with exact rows, so textual
    // overlap is not always stable across a valid local movement.
    if movement.is_none()
        && viewport_after.iter().any(|line| !line.is_empty())
        && sparse_user_delta_moved_within_visible_record_span(frames, max_rows)
    {
        return;
    }
    assert!(
        movement.is_some(),
        "wheel movement teleported or reversed visible transcript content: pending={pending:?}, after={viewport_after:?}, frames={frames:?}"
    );
}

fn has_user_delta_frame(frames: &[TranscriptScrollTraceFrame]) -> bool {
    frames.iter().any(|frame| {
        matches!(
            frame.scroll_intent,
            TranscriptScrollIntent::UserDelta { .. }
        )
    })
}

fn effective_wheel_rows(pending_rows: isize, frames: &[TranscriptScrollTraceFrame]) -> isize {
    frames
        .iter()
        .filter_map(|frame| match frame.scroll_intent {
            TranscriptScrollIntent::UserDelta { rows }
                if rows.signum() == pending_rows.signum() =>
            {
                Some(rows)
            }
            _ => None,
        })
        .max_by_key(|rows| rows.unsigned_abs())
        .filter(|rows| rows.unsigned_abs() > pending_rows.unsigned_abs())
        .unwrap_or(pending_rows)
}

fn sparse_zero_delta_preserved_anchor(frames: &[TranscriptScrollTraceFrame]) -> bool {
    frames.iter().any(|frame| {
        matches!(
            frame.scroll_intent,
            TranscriptScrollIntent::UserDelta { rows: 0 }
        ) && viewport_anchor_preserved(frame)
    })
}

fn sparse_user_delta_moved_within_visible_record_span(
    frames: &[TranscriptScrollTraceFrame],
    max_rows: usize,
) -> bool {
    frames.iter().any(|frame| {
        let TranscriptScrollIntent::UserDelta { rows } = frame.scroll_intent else {
            return false;
        };
        if rows == 0 {
            return false;
        }
        let Some(TranscriptTraceAnchor::Content {
            record_index: before_record,
            row_offset: before_row,
            ..
        }) = frame.viewport_anchor_before
        else {
            return false;
        };
        let Some(TranscriptTraceAnchor::Content {
            record_index: after_record,
            row_offset: after_row,
            ..
        }) = frame.viewport_anchor_after
        else {
            return false;
        };
        let movement = (after_record, after_row).cmp(&(before_record, before_row));
        if (rows < 0 && movement.is_gt()) || (rows > 0 && movement.is_lt()) {
            return false;
        }
        let distance = if before_record == after_record {
            usize::try_from(before_row.abs_diff(after_row)).unwrap_or(usize::MAX)
        } else {
            before_record.abs_diff(after_record)
        };
        distance <= max_rows
    })
}

fn max_leading_rows_before_content(frames: &[TranscriptScrollTraceFrame]) -> usize {
    frames
        .iter()
        .filter(|frame| {
            matches!(
                frame.scroll_intent,
                TranscriptScrollIntent::UserDelta { .. }
            )
        })
        .filter_map(|frame| match frame.viewport_anchor_before {
            Some(TranscriptTraceAnchor::Content { virtual_row, .. }) => virtual_row
                .checked_sub(frame.window_scroll_before)
                .and_then(|rows| usize::try_from(rows).ok()),
            _ => None,
        })
        .max()
        .unwrap_or_default()
}

fn assert_transcript_scroll_probe_frames(
    state: &mut TranscriptScrollProbeState,
    frames: &[TranscriptScrollTraceFrame],
) {
    for frame in frames {
        assert!(
            !frame.placeholder_rows_visible,
            "interactive transcript viewport exposed sparse placeholders: {frame:?}"
        );
        match &frame.scroll_intent {
            TranscriptScrollIntent::UserDelta { rows } => {
                assert_eq!(
                    frame.window_scroll_after_input, frame.window_scroll_before,
                    "local transcript input mutated Window::scroll_top before projection: {frame:?}"
                );
                assert!(
                    frame.projection_target.exact_target_row().is_some(),
                    "local transcript movement projected through a non-exact target: {frame:?}"
                );
                assert!(
                    !frame.placeholder_rows_visible,
                    "local transcript movement exposed sparse placeholders: {frame:?}"
                );
                assert!(
                    frame.first_visible_content_anchor.is_some(),
                    "local transcript movement lost its visible content anchor: {frame:?}"
                );
                assert_record_ranges_overlap(state, frame);
                assert_user_delta_from_frame_start(*rows, frame);
                assert_user_delta_direction(state, *rows, frame);
            }
            TranscriptScrollIntent::SearchJump { .. }
            | TranscriptScrollIntent::RevealPosition { .. }
            | TranscriptScrollIntent::RevealBlock { .. }
            | TranscriptScrollIntent::RevealFirstRecord { .. } => {
                assert!(
                    !frame.placeholder_rows_visible,
                    "semantic transcript reveal exposed sparse placeholders: {frame:?}"
                );
                assert!(
                    frame.first_visible_content_anchor.is_some(),
                    "semantic transcript reveal did not resolve visible content: {frame:?}"
                );
                state.last_user_delta_anchor = None;
            }
            TranscriptScrollIntent::ExactContentAnchor(anchor) => {
                assert!(
                    !matches!(anchor, TranscriptTraceAnchor::EstimatedRow(_)),
                    "exact transcript operation fell back to an estimated row anchor: {frame:?}"
                );
                state.last_user_delta_anchor = None;
            }
            TranscriptScrollIntent::PreserveViewport
            | TranscriptScrollIntent::ResizeReflow { .. } => {
                assert_preserve_frame_keeps_anchor(frame);
                state.last_user_delta_anchor = None;
            }
            TranscriptScrollIntent::ScrollbarFraction { .. }
            | TranscriptScrollIntent::ApproximateRowSeek(_)
            | TranscriptScrollIntent::Tail
            | TranscriptScrollIntent::PageDelta { .. } => {
                assert!(
                    frame.first_visible_content_anchor.is_some(),
                    "resolved transcript frame has no visible content anchor: {frame:?}"
                );
                state.last_user_delta_anchor = None;
            }
        }
    }
}

fn assert_record_ranges_overlap(
    state: &TranscriptScrollProbeState,
    frame: &TranscriptScrollTraceFrame,
) {
    let Some(edge) = state.drag_edge else {
        return;
    };
    if viewport_anchor_preserved(frame) || sparse_user_delta_settled_stale_active_range(frame) {
        return;
    }
    let Some(before) = frame.active_record_range_before else {
        return;
    };
    let Some(after) = frame.active_record_range_after else {
        return;
    };
    assert!(
        ranges_overlap(before, after),
        "{edge:?} drag/autoscroll jumped to disjoint record coverage: before={before:?}, after={after:?}, frame={frame:?}"
    );
}

fn sparse_user_delta_settled_stale_active_range(frame: &TranscriptScrollTraceFrame) -> bool {
    let Some(before) = frame.active_record_range_before else {
        return false;
    };
    let Some(after) = frame.active_record_range_after else {
        return false;
    };
    let Some(TranscriptTraceAnchor::Content {
        record_index: before_record,
        ..
    }) = frame.viewport_anchor_before
    else {
        return false;
    };
    let Some(TranscriptTraceAnchor::Content {
        record_index: after_record,
        ..
    }) = frame.viewport_anchor_after
    else {
        return false;
    };
    !frame.placeholder_rows_visible
        && !record_range_contains(before, before_record)
        && record_range_contains(after, after_record)
}

fn viewport_anchor_preserved(frame: &TranscriptScrollTraceFrame) -> bool {
    matches!(
        (frame.viewport_anchor_before, frame.viewport_anchor_after),
        (
            Some(TranscriptTraceAnchor::Content {
                record_index: before_record,
                block_id: before_block,
                row_offset: before_row,
                ..
            }),
            Some(TranscriptTraceAnchor::Content {
                record_index: after_record,
                block_id: after_block,
                row_offset: after_row,
                ..
            })
        ) if before_record == after_record && before_block == after_block && before_row == after_row
    )
}

fn no_input_scroll_top_change_preserved_content(frames: &[TranscriptScrollTraceFrame]) -> bool {
    !frames.is_empty()
        && frames.iter().all(|frame| {
            matches!(
                frame.scroll_intent,
                TranscriptScrollIntent::PreserveViewport
                    | TranscriptScrollIntent::ResizeReflow { .. }
            ) && viewport_anchor_preserved(frame)
                && !frame.placeholder_rows_visible
        })
}

fn ranges_overlap(a: TranscriptRecordTraceRange, b: TranscriptRecordTraceRange) -> bool {
    a.start <= b.end && b.start <= a.end
}

fn assert_user_delta_from_frame_start(rows: isize, frame: &TranscriptScrollTraceFrame) {
    let Some(TranscriptTraceAnchor::Content {
        virtual_row: before_virtual_row,
        record_index: before_record,
        block_id: before_block,
        row_offset: before_row,
        ..
    }) = frame.viewport_anchor_before
    else {
        return;
    };
    let Some(TranscriptTraceAnchor::Content {
        virtual_row: after_virtual_row,
        record_index: after_record,
        block_id: after_block,
        row_offset: after_row,
        ..
    }) = frame.viewport_anchor_after
    else {
        panic!("local movement lost its semantic viewport anchor: {frame:?}");
    };
    if sparse_user_delta_settled_stale_frame_start(frame) {
        return;
    }
    let semantic_movement =
        (after_record, after_block, after_row).cmp(&(before_record, before_block, before_row));
    let virtual_movement = after_virtual_row.cmp(&before_virtual_row);
    let viewport_movement = frame.resolved_scroll_top.cmp(&frame.window_scroll_before);
    if rows < 0 {
        assert!(
            !local_delta_moved_against_direction(
                rows,
                semantic_movement,
                virtual_movement,
                viewport_movement,
            ),
            "first upward local movement moved the semantic viewport anchor downward: {frame:?}"
        );
    } else if rows > 0 {
        assert!(
            !local_delta_moved_against_direction(
                rows,
                semantic_movement,
                virtual_movement,
                viewport_movement,
            ),
            "first downward local movement moved the semantic viewport anchor upward: {frame:?}"
        );
    }
}

fn sparse_user_delta_settled_stale_frame_start(frame: &TranscriptScrollTraceFrame) -> bool {
    let Some(TranscriptTraceAnchor::Content {
        virtual_row: before_row,
        ..
    }) = frame.viewport_anchor_before
    else {
        return false;
    };
    let Some(TranscriptTraceAnchor::Content {
        virtual_row: after_row,
        ..
    }) = frame.viewport_anchor_after
    else {
        return false;
    };
    matches!(
        frame.projection_target,
        TranscriptProjectionTargetTrace::StableRowDelta { row, .. }
            if row == frame.window_scroll_before
                && row == frame.resolved_scroll_top
                && after_row == row
                && before_row != row
    ) && !frame.placeholder_rows_visible
}

fn local_delta_moved_against_direction(
    rows: isize,
    semantic_movement: std::cmp::Ordering,
    virtual_movement: std::cmp::Ordering,
    viewport_movement: std::cmp::Ordering,
) -> bool {
    if rows < 0 {
        semantic_movement.is_gt() && !virtual_movement.is_lt() && !viewport_movement.is_lt()
    } else if rows > 0 {
        semantic_movement.is_lt() && !virtual_movement.is_gt() && !viewport_movement.is_gt()
    } else {
        false
    }
}

fn assert_user_delta_direction(
    state: &mut TranscriptScrollProbeState,
    rows: isize,
    frame: &TranscriptScrollTraceFrame,
) {
    let sign = rows.signum() as i8;
    let frame_start = match frame.viewport_anchor_before {
        Some(TranscriptTraceAnchor::Content {
            virtual_row,
            record_index,
            block_id,
            row_offset,
            ..
        }) => Some(UserDeltaAnchor {
            sign,
            viewport_top: frame.window_scroll_before,
            virtual_row,
            record_index,
            block_id,
            row_offset,
        }),
        _ => None,
    };
    let Some(TranscriptTraceAnchor::Content {
        virtual_row,
        record_index,
        block_id,
        row_offset,
        ..
    }) = frame.viewport_anchor_after
    else {
        state.last_user_delta_anchor = None;
        return;
    };
    let current = UserDeltaAnchor {
        sign,
        viewport_top: frame.resolved_scroll_top,
        virtual_row,
        record_index,
        block_id,
        row_offset,
    };
    let Some(previous) = state.last_user_delta_anchor else {
        state.last_user_delta_anchor = Some(current);
        return;
    };
    if previous.sign == sign {
        let Some(frame_start) = frame_start else {
            state.last_user_delta_anchor = Some(current);
            return;
        };
        if !previous.same_position(frame_start) {
            state.last_user_delta_anchor = Some(current);
            return;
        }
    }
    let semantic_movement = (current.record_index, current.block_id, current.row_offset).cmp(&(
        previous.record_index,
        previous.block_id,
        previous.row_offset,
    ));
    let virtual_movement = current.virtual_row.cmp(&previous.virtual_row);
    let viewport_movement = current.viewport_top.cmp(&previous.viewport_top);
    if previous.sign == sign && sign < 0 {
        assert!(
            !local_delta_moved_against_direction(
                rows,
                semantic_movement,
                virtual_movement,
                viewport_movement,
            ),
            "upward local movement moved the semantic viewport anchor downward: previous={previous:?}, current={current:?}, frame={frame:?}"
        );
    } else if previous.sign == sign && sign > 0 {
        assert!(
            !local_delta_moved_against_direction(
                rows,
                semantic_movement,
                virtual_movement,
                viewport_movement,
            ),
            "downward local movement moved the semantic viewport anchor upward: previous={previous:?}, current={current:?}, frame={frame:?}"
        );
    }
    state.last_user_delta_anchor = Some(current);
}

fn assert_preserve_frame_keeps_anchor(frame: &TranscriptScrollTraceFrame) {
    let Some(TranscriptTraceAnchor::Content {
        record_index: before_record,
        ..
    }) = frame.viewport_anchor_before
    else {
        return;
    };
    let Some(TranscriptTraceAnchor::Content {
        record_index: after_record,
        ..
    }) = frame.viewport_anchor_after
    else {
        panic!("preserve/resize frame lost content anchor: {frame:?}");
    };
    if sparse_preserve_frame_settled_pinned_projection(frame, before_record, after_record)
        || sparse_preserve_frame_settled_stale_resolved_anchor(frame)
    {
        return;
    }
    assert_eq!(
        after_record, before_record,
        "preserve/resize frame moved to different transcript record: {frame:?}"
    );
}

fn sparse_preserve_frame_settled_pinned_projection(
    frame: &TranscriptScrollTraceFrame,
    before_record: usize,
    after_record: usize,
) -> bool {
    let Some(before) = frame.active_record_range_before else {
        return false;
    };
    let Some(after) = frame.active_record_range_after else {
        return false;
    };
    let target_outside_materialized = matches!(
        frame.projection_target,
        TranscriptProjectionTargetTrace::ExactRow(row)
            if row < frame.materialized_range.start || row >= frame.materialized_range.end
    );
    if !target_outside_materialized || frame.placeholder_rows_visible {
        return false;
    }
    (before == after && !record_range_contains(after, after_record))
        || (!record_range_contains(before, before_record)
            && record_range_contains(after, after_record))
}

fn sparse_preserve_frame_settled_stale_resolved_anchor(frame: &TranscriptScrollTraceFrame) -> bool {
    let Some(before) = frame.active_record_range_before else {
        return false;
    };
    let Some(after) = frame.active_record_range_after else {
        return false;
    };
    let Some(TranscriptTraceAnchor::Content {
        virtual_row: before_row,
        record_index: before_record,
        ..
    }) = frame.viewport_anchor_before
    else {
        return false;
    };
    let Some(TranscriptTraceAnchor::Content {
        virtual_row: after_row,
        record_index: after_record,
        ..
    }) = frame.viewport_anchor_after
    else {
        return false;
    };
    matches!(
        frame.projection_target,
        TranscriptProjectionTargetTrace::StableRowDelta { row, delta: 0 }
            if row == frame.window_scroll_before
                && row == frame.resolved_scroll_top
                && after_row == row
                && before_row != row
    ) && !frame.placeholder_rows_visible
        && before == after
        && record_range_contains(before, before_record)
        && record_range_contains(after, after_record)
}

fn record_range_contains(range: TranscriptRecordTraceRange, record: usize) -> bool {
    range.start <= record && record < range.end
}

fn heterogeneous_resume_records(count: usize) -> Vec<TranscriptBlockRecord> {
    let mut source = smelt_core::content::transcript::Transcript::new();
    for idx in 0..count {
        let marker = format!("record-{idx:04}");
        match idx % 10 {
            0 => source.push(Block::User {
                text: format!(
                    "{marker} user prompt with image labels and wrapped text {}",
                    "u ".repeat(12)
                ),
                image_labels: vec![format!("image-{idx}")],
                command: false,
                sent_at_ms: None,
            }),
            1 => source.push(Block::Text {
                content: format!(
                    "{marker} assistant paragraph\n\n```diff\n- old {idx}\n+ new {idx}\n```\n{}",
                    "markdown wrap ".repeat(20)
                )
                .into(),
            }),
            2 => source.push(Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                content: format!("{marker} thinking trace {}", "reasoning ".repeat(28)).into(),
                kind: protocol::ReasoningKind::Raw,
            }),
            3 => source.push(Block::CodeLine {
                content: format!("{marker} let value_{idx} = compute({idx});"),
                lang: "rust".into(),
            }),
            4 => source.push(Block::Exec {
                command: format!("echo {marker}"),
                output: format!("{marker} stdout line\n{}", "exec output ".repeat(18)).into(),
            }),
            5 => source.push(Block::Compacted {
                summary: format!("{marker} compacted summary {}", "summary ".repeat(10)),
            }),
            6 => source.push(Block::CompactionPreview {
                summary: format!("{marker} streaming preview {}", "preview ".repeat(15)),
            }),
            7 => source.push(Block::ToolCall {
                call_id: format!("read-file-{idx}"),
                name: "read_file".into(),
                summary: protocol::StyledLines::from_plain(format!(
                    "{marker} read_file src/{idx}.rs"
                )),
                args: std::collections::HashMap::from([(
                    "file_path".to_string(),
                    serde_json::json!(format!("src/{idx}.rs")),
                )])
                .into(),
            }),
            8 => source.push(Block::ToolCall {
                call_id: format!("grep-{idx}"),
                name: "grep".into(),
                summary: protocol::StyledLines::from_plain(format!("{marker} grep needle")),
                args: std::collections::HashMap::from([(
                    "pattern".to_string(),
                    serde_json::json!(marker),
                )])
                .into(),
            }),
            _ => source.push(Block::ProcessStatus {
                text: format!("{marker} background process finished"),
                event: None,
            }),
        };
    }
    source.history.block_records()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_search_resize_reflow_preserves_viewport_anchor() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(128, 129, 27);
        app.transcript_scroll_probe_search_record(37779);
        app.transcript_scroll_probe_render();
        app.set_terminal_size(32, 8);
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_search_candidate_resize_retains_exact_viewport_during_hydration() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(150, 40, 10);
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(false, 0);
            app.transcript_scroll_probe_render();
        }
        for record in [31097, 31097, 43385, 31097] {
            app.transcript_scroll_probe_reveal_record(record);
            app.transcript_scroll_probe_render();
        }

        let retained_height = app
            .transcript_window()
            .viewport
            .expect("settled transcript viewport")
            .rect
            .height;
        app.transcript_scroll_probe_resize(57, 23);
        app.render_unsettled_silent();
        assert_eq!(
            app.transcript_window()
                .viewport
                .expect("retained transcript viewport")
                .rect
                .height,
            retained_height
        );

        app.render_silent();
        assert!(
            app.transcript_window()
                .viewport
                .expect("expanded transcript viewport")
                .rect
                .height
                > retained_height
        );
    }

    #[test]
    fn sparse_search_follow_tail_after_deferred_match_settles() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(380, 40, 10);
        app.transcript_scroll_probe_search_record(65476);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_follow_tail();
        app.transcript_scroll_probe_render();
        assert!(app.transcript_window().following_tail);
    }

    #[test]
    fn sparse_search_repeats_land_on_the_matching_row() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(128, 80, 16);
        app.transcript_scroll_probe_search_common_text();
        app.transcript_scroll_probe_render();

        for step in 0..32 {
            if step % 7 == 0 {
                let width = if step % 14 == 0 { 40 } else { 129 };
                app.transcript_scroll_probe_resize(width, 16);
                app.transcript_scroll_probe_render();
            }
            app.transcript_scroll_probe_repeat_search(step % 5 == 0);
            app.transcript_scroll_probe_render();
        }
    }

    #[test]
    fn sparse_search_repeat_after_reflow_keeps_cursor_on_match() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(173, 55, 21);
        for _ in 0..5 {
            app.transcript_scroll_probe_resize(103, 15);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_resize(96, 15);
        app.transcript_scroll_probe_render();
        for _ in 0..4 {
            app.transcript_scroll_probe_resize(103, 15);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..18 {
            app.transcript_scroll_probe_search_common_text();
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_resize(71, 15);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_resize(103, 15);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_resize(103, 23);
        app.transcript_scroll_probe_render();
        for _ in 0..2 {
            app.transcript_scroll_probe_no_input_render();
        }
        for _ in 0..5 {
            app.transcript_scroll_probe_search_common_text();
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_repeat_search(true);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_drag_select(0, 0, 0);
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_coalesced_wheel_trace_sets_visible_movement_bound() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(256, 77, 15);
        app.transcript_scroll_probe_reveal_record(65535);
        app.transcript_scroll_probe_render();
        for _ in 0..2 {
            app.transcript_scroll_probe_search_common_text();
            app.transcript_scroll_probe_render();
        }
        for _ in 0..2 {
            app.transcript_scroll_probe_repeat_search(true);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..5 {
            app.transcript_scroll_probe_search_common_text();
            app.transcript_scroll_probe_render();
        }
        for _ in 0..3 {
            app.transcript_scroll_probe_repeat_search(true);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_search_common_text();
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_follow_tail();
        app.transcript_scroll_probe_render();
        for _ in 0..2 {
            app.transcript_scroll_probe_repeat_search(true);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..2 {
            app.transcript_scroll_probe_search_common_text();
            app.transcript_scroll_probe_render();
        }
        for _ in 0..12 {
            for _ in 0..8 {
                app.transcript_scroll_probe_wheel(true, 11);
            }
            app.transcript_scroll_probe_render();
        }
        for _ in 0..4 {
            app.transcript_scroll_probe_search_common_text();
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_no_input_render();
        app.transcript_scroll_probe_repeat_search(true);
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_page_down_allows_stale_frame_start_settling() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(133, 45, 15);
        for _ in 0..10 {
            app.transcript_scroll_probe_wheel(true, 5);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..2 {
            app.transcript_scroll_probe_start_edge_drag(TranscriptScrollProbeEdge::Bottom);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..8 {
            app.transcript_scroll_probe_search_common_text();
            app.transcript_scroll_probe_render();
        }
        for _ in 0..5 {
            app.transcript_scroll_probe_resize(101, 13);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_resize(57, 33);
        app.transcript_scroll_probe_render();
        for _ in 0..12 {
            app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::PageDown);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..2 {
            app.transcript_scroll_probe_no_input_render();
        }
        for _ in 0..3 {
            app.transcript_scroll_probe_repeat_search(true);
            app.transcript_scroll_probe_render();
        }
    }

    #[test]
    fn sparse_preserve_allows_stale_resolved_anchor_settling() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(502, 48, 10);
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(true, 121);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..2 {
            app.transcript_scroll_probe_search_record(31097);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..12 {
            app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::MoveUp);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..3 {
            app.transcript_scroll_probe_no_input_render();
        }
        app.transcript_scroll_probe_drag_select(65, 65, 65);
        app.transcript_scroll_probe_render();
        for _ in 0..12 {
            for _ in 0..12 {
                app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::HalfPageUp);
                app.transcript_scroll_probe_render();
            }
        }
        for _ in 0..3 {
            for _ in 0..12 {
                app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::MoveUp);
                app.transcript_scroll_probe_render();
            }
        }
        for _ in 0..12 {
            app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::HalfPageUp);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_drag_select(65, 65, 145);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_reveal_record(37265);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_reveal_record(53199);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_no_input_render();
        app.transcript_scroll_probe_reveal_record(31121);
        app.transcript_scroll_probe_render();
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(false, 0);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..12 {
            app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::MoveUp);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_search_record(31097);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_search_record(29049);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_search_record(31097);
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_bottom_drag_wheel_allows_stale_active_range_reanchor() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(486, 40, 10);
        for _ in 0..4 {
            app.transcript_scroll_probe_repeat_search(true);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..12 {
            app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::MoveUp);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_search_record(31097);
        app.transcript_scroll_probe_render();
        for _ in 0..11 {
            app.transcript_scroll_probe_start_edge_drag(TranscriptScrollProbeEdge::Bottom);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..2 {
            app.transcript_scroll_probe_wheel(true, 77);
        }
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_search_no_input_render_preserves_visible_content() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(149, 103, 10);
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(true, 121);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..2 {
            app.transcript_scroll_probe_search_record(31097);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_finish_drag();
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_finish_drag();
        app.transcript_scroll_probe_render();
        for _ in 0..2 {
            app.transcript_scroll_probe_no_input_render();
        }
        app.transcript_scroll_probe_drag_select(65, 65, 65);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_drag_select(65, 117, 117);
        app.transcript_scroll_probe_render();
        for _ in 0..11 {
            for _ in 0..12 {
                app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::HalfPageUp);
                app.transcript_scroll_probe_render();
            }
        }
        for _ in 0..12 {
            app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::HalfPageUp);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..2 {
            for _ in 0..12 {
                app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::MoveUp);
                app.transcript_scroll_probe_render();
            }
        }
        for _ in 0..12 {
            app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::HalfPageUp);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_drag_select(65, 65, 65);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_reveal_record(37265);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_reveal_record(57233);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_no_input_render();
    }

    #[test]
    fn sparse_search_preserve_allows_stale_projection_reanchor() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(150, 40, 10);
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(true, 121);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..2 {
            app.transcript_scroll_probe_search_record(31097);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..16 {
            let _ = app.transcript_scroll_probe_drag_autoscroll_tick();
            app.transcript_scroll_probe_render();
        }
        for _ in 0..6 {
            app.transcript_scroll_probe_search_record(31097);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_content_click(121, 121);
        app.transcript_scroll_probe_render();
        for _ in 0..5 {
            app.transcript_scroll_probe_search_record(31097);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_search_record(31071);
        app.transcript_scroll_probe_render();
        for _ in 0..3 {
            app.transcript_scroll_probe_search_record(31097);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..3 {
            app.transcript_scroll_probe_search_common_text();
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_repeat_search(true);
        app.transcript_scroll_probe_render();
        for _ in 0..7 {
            app.transcript_scroll_probe_search_record(31097);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_search_record(30976);
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_search_preserve_ignores_stale_estimated_anchor() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(150, 40, 10);
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(true, 121);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..3 {
            app.transcript_scroll_probe_search_record(31097);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_search_common_text();
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_no_input_render();
        app.transcript_scroll_probe_repeat_search(true);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_search_record(31097);
        app.transcript_scroll_probe_render();
        for _ in 0..3 {
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_repeat_search(true);
        app.transcript_scroll_probe_render();
        for _ in 0..4 {
            app.transcript_scroll_probe_render();
        }
        for _ in 0..6 {
            app.transcript_scroll_probe_search_record(31097);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_search_record(65401);
        app.transcript_scroll_probe_render();
        for _ in 0..6 {
            app.transcript_scroll_probe_repeat_search(true);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..5 {
            app.transcript_scroll_probe_search_record(31097);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_search_record(56697);
        app.transcript_scroll_probe_render();
        for _ in 0..2 {
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_search_record(31097);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_search_record(0);
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_search_resize_clamps_stale_cursor_screen_row() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(96, 61, 31);
        for _ in 0..2 {
            app.transcript_scroll_probe_wheel(true, 21);
        }
        app.transcript_scroll_probe_render();
        for _ in 0..2 {
            app.transcript_scroll_probe_wheel(true, 121);
        }
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_search_record(31097);
        app.transcript_scroll_probe_render();
        for _ in 0..15 {
            app.transcript_scroll_probe_repeat_search(true);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..3 {
            app.transcript_scroll_probe_search_record(31097);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_resize(32, 8);
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_bottom_edge_drag_resize_wheel_moves_downward() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(96, 129, 15);
        app.transcript_scroll_probe_start_edge_drag(TranscriptScrollProbeEdge::Bottom);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_render();
        app.set_terminal_size(95, 39);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_wheel(true, 255);
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_top_edge_drag_half_page_up_then_wheel_settles() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(150, 65, 19);
        app.transcript_scroll_probe_start_edge_drag(TranscriptScrollProbeEdge::Top);
        app.transcript_scroll_probe_render();
        for _ in 0..12 {
            app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::HalfPageUp);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_wheel(false, 0);
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_search_click_then_wheel_settles() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(96, 103, 13);
        app.transcript_scroll_probe_search_record(31097);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_content_click(45, 45);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_wheel(false, 0);
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_search_resize_twice_settles_sparse_match() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(502, 40, 10);
        app.transcript_scroll_probe_search_record(31097);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_resize(101, 13);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_resize(101, 8);
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_search_wheel_resize_allows_unresolved_match() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(502, 135, 13);
        app.transcript_scroll_probe_search_common_text();
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_wheel(true, 165);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_resize(101, 33);
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_bottom_drag_autoscroll_allows_preserved_anchor_rehydration() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(141, 85, 23);
        for _ in 0..3 {
            app.transcript_scroll_probe_search_common_text();
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_start_edge_drag(TranscriptScrollProbeEdge::Bottom);
        app.transcript_scroll_probe_render();
        for _ in 0..64 {
            app.transcript_scroll_probe_drag_autoscroll_tick();
            app.transcript_scroll_probe_render();
        }
    }

    #[test]
    fn sparse_resize_wheel_reverse_search_hydration_settles() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(256, 51, 21);
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(true, 11);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..4 {
            app.transcript_scroll_probe_resize(101, 13);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_resize(101, 33);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_search_common_text();
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_scrollbar_click(21);
        app.transcript_scroll_probe_render();
        for _ in 0..2 {
            app.transcript_scroll_probe_wheel(true, 21);
        }
        app.transcript_scroll_probe_render();
        for _ in 0..88 {
            app.transcript_scroll_probe_wheel(true, 11);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..4 {
            app.transcript_scroll_probe_resize(101, 13);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_resize(57, 33);
        app.transcript_scroll_probe_render();
        for _ in 0..4 {
            app.transcript_scroll_probe_repeat_search(true);
            app.transcript_scroll_probe_render();
        }
    }

    #[test]
    fn sparse_search_reveal_common_hydration_settles() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(150, 40, 10);
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(false, 0);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_search_record(31097);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_search_record(31097);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_search_record(43385);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_search_record(31097);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_reveal_record(36729);
        app.transcript_scroll_probe_render();
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(true, 9);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(true, 143);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_reveal_record(36751);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_reveal_record(36751);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_search_common_text();
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_zero_net_mixed_wheel_allows_sparse_reanchor() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(502, 40, 10);
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(true, 121);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_search_record(31097);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_search_record(31097);
        app.transcript_scroll_probe_render();
        for _ in 0..12 {
            app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::MoveUp);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..3 {
            app.transcript_scroll_probe_no_input_render();
        }
        app.transcript_scroll_probe_drag_select(65, 65, 65);
        app.transcript_scroll_probe_render();
        for _ in 0..6 {
            for _ in 0..12 {
                app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::HalfPageUp);
                app.transcript_scroll_probe_render();
            }
        }
        for _ in 0..12 {
            app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::JumpBottom);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..3 {
            for _ in 0..12 {
                app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::HalfPageUp);
                app.transcript_scroll_probe_render();
            }
        }
        for _ in 0..4 {
            app.transcript_scroll_probe_wheel(true, 117);
            app.transcript_scroll_probe_wheel(false, 117);
        }
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_no_input_render_settles_after_search_projection() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(128, 55, 21);
        for _ in 0..3 {
            app.transcript_scroll_probe_no_input_render();
        }
        app.transcript_scroll_probe_search_record(34181);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_search_record(11141);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_search_record(53125);
        app.transcript_scroll_probe_render();
        for _ in 0..4 {
            app.transcript_scroll_probe_no_input_render();
        }
        app.transcript_scroll_probe_scrollbar_click(207);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_repeat_search(true);
        app.transcript_scroll_probe_render();
        for _ in 0..2 {
            app.transcript_scroll_probe_wheel(false, 207);
        }
        app.transcript_scroll_probe_render();
        for _ in 0..2 {
            app.transcript_scroll_probe_no_input_render();
        }
        app.transcript_scroll_probe_reveal_record(53199);
        app.transcript_scroll_probe_render();
        for _ in 0..3 {
            app.transcript_scroll_probe_no_input_render();
        }
        for _ in 0..3 {
            app.transcript_scroll_probe_search_record(34181);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_search_record(31354);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_search_record(34181);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_no_input_render();
    }

    #[test]
    fn sparse_command_scroll_keeps_upward_anchor_direction() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(502, 40, 13);
        for _ in 0..12 {
            app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::MoveUp);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(false, 0);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_wheel(true, 45);
        app.transcript_scroll_probe_wheel(true, 45);
        app.transcript_scroll_probe_render();
        let mut down = true;
        for _ in 0..4 {
            app.transcript_scroll_probe_wheel(down, 121);
            down = !down;
        }
        app.transcript_scroll_probe_render();
        for _ in 0..12 {
            app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::MoveUp);
            app.transcript_scroll_probe_render();
        }
    }

    #[test]
    fn sparse_preserve_reanchors_after_record_window_rotation() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(113, 89, 15);
        app.transcript_scroll_probe_start_edge_drag(TranscriptScrollProbeEdge::Bottom);
        app.transcript_scroll_probe_render();
        for _ in 0..12 {
            app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::MoveUp);
            app.transcript_scroll_probe_render();
        }
        let viewport_rows = app
            .app
            .transcript_win()
            .viewport
            .map(|viewport| viewport.rect.height)
            .unwrap_or(1);
        assert!(!app
            .app
            .conversation
            .activate_transcript_search_record_window(
                app.app.transcript_width() as u16,
                32,
                viewport_rows,
            ));
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_tail_repin_keeps_wheel_direction_stable() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(502, 40, 10);
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(false, 11);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(true, 11);
            app.transcript_scroll_probe_render();
        }
    }

    #[test]
    fn sparse_reveal_search_tail_wheel_hydration_settles() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(502, 40, 10);
        app.transcript_scroll_probe_repeat_search(true);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_follow_tail();
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_reveal_record(52685);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_search_record(65476);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_follow_tail();
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_wheel(false, 0);
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_search_resize_after_reveal_preserves_viewport_anchor() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(502, 40, 10);
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(true, 121);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..3 {
            app.transcript_scroll_probe_search_record(31097);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(false, 0);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(false, 36);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..2 {
            app.transcript_scroll_probe_search_record(31097);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..5 {
            app.transcript_scroll_probe_resize(101, 13);
            app.transcript_scroll_probe_render();
        }
        for record in [37779, 37779, 42387] {
            app.transcript_scroll_probe_reveal_record(record);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..6 {
            app.transcript_scroll_probe_resize(101, 13);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_drag_select(121, 121, 121);
        app.transcript_scroll_probe_render();
        for _ in 0..4 {
            app.transcript_scroll_probe_search_record(31097);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_search_record(34438);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_wheel(false, 0);
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_zero_net_wheel_preserves_search_cursor_after_edge_hydration() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(502, 40, 10);
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(true, 121);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..3 {
            app.transcript_scroll_probe_search_record(31097);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(false, 0);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..2 {
            app.transcript_scroll_probe_wheel(false, 0);
        }
        app.transcript_scroll_probe_render();
        for _ in 0..2 {
            app.transcript_scroll_probe_search_record(31097);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_search_record(42405);
        app.transcript_scroll_probe_render();
        for _ in 0..4 {
            app.transcript_scroll_probe_resize(101, 13);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_resize(101, 27);
        app.transcript_scroll_probe_render();
        for record in [37779, 37779] {
            app.transcript_scroll_probe_reveal_record(record);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..3 {
            app.transcript_scroll_probe_resize(101, 13);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_resize(101, 39);
        app.transcript_scroll_probe_render();
        for down in [true, false, true, false] {
            app.transcript_scroll_probe_wheel(down, 31);
        }
        app.transcript_scroll_probe_render();
        for _ in 0..5 {
            app.transcript_scroll_probe_resize(101, 13);
            app.transcript_scroll_probe_render();
        }
        for _ in 0..5 {
            app.transcript_scroll_probe_search_record(31097);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_search_record(31110);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_wheel(false, 0);
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_zero_net_mixed_wheel_uses_coalesced_intent() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(96, 55, 21);
        for _ in 0..8 {
            app.transcript_scroll_probe_wheel(false, 0);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_wheel(false, 0);
        app.transcript_scroll_probe_wheel(true, 0);
        app.transcript_scroll_probe_render();
    }

    #[test]
    fn sparse_append_away_from_tail_preserves_persisted_anchor() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.install_sparse_transcript_scroll_fixture(502, 40, 10);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_append(196);
        app.transcript_scroll_probe_render();
        app.transcript_scroll_probe_search_record(52685);
        app.transcript_scroll_probe_render();
        for _ in 0..12 {
            app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::HalfPageDown);
            app.transcript_scroll_probe_render();
        }
        app.transcript_scroll_probe_append(121);
        app.transcript_scroll_probe_render();
    }
}
