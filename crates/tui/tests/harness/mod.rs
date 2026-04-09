//! Test harness for TUI rendering verification (vt100).
// Shared across multiple test binaries; not all items are used in each.
#![allow(dead_code)]

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tui::render::{
    Block, ConfirmDialog, ConfirmRequest, Dialog, RenderOut, Screen, TerminalBackend, ToolOutput,
    ToolState, ToolStatus,
};

// ── TestBackend ──────────────────────────────────────────────────────

/// Shared size state so the harness can resize the backend while the
/// `Screen` still owns it via `Box<dyn TerminalBackend>`.
pub type SharedSize = Rc<Cell<(u16, u16)>>;
pub type SharedCursor = Rc<Cell<u16>>;

pub struct TestBackend {
    size: SharedSize,
    cursor: SharedCursor,
    sink: Arc<Mutex<Vec<u8>>>,
}

impl TestBackend {
    pub fn new(width: u16, height: u16, sink: Arc<Mutex<Vec<u8>>>) -> Self {
        Self {
            size: Rc::new(Cell::new((width, height))),
            cursor: Rc::new(Cell::new(0)),
            sink,
        }
    }

    pub fn new_with_state(
        size: SharedSize,
        cursor: SharedCursor,
        sink: Arc<Mutex<Vec<u8>>>,
    ) -> Self {
        Self { size, cursor, sink }
    }

    pub fn shared_size(&self) -> SharedSize {
        self.size.clone()
    }

    pub fn shared_cursor(&self) -> SharedCursor {
        self.cursor.clone()
    }
}

impl TerminalBackend for TestBackend {
    fn size(&self) -> (u16, u16) {
        self.size.get()
    }
    fn cursor_y(&self) -> u16 {
        self.cursor.get()
    }
    fn make_output(&self) -> RenderOut {
        RenderOut::shared_sink(self.sink.clone())
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

pub fn extract_full_content(parser: &mut vt100::Parser) -> String {
    let (rows, cols) = parser.screen().size();

    parser.screen_mut().set_scrollback(usize::MAX);
    let max_sb = parser.screen().scrollback();

    if max_sb == 0 {
        return parser.screen().contents();
    }

    let mut all_lines: Vec<String> = parser.screen().rows(0, cols).collect();
    for offset in (0..max_sb).rev() {
        parser.screen_mut().set_scrollback(offset);
        if let Some(line) = parser.screen().rows(0, cols).nth(rows as usize - 1) {
            all_lines.push(line);
        }
    }
    parser.screen_mut().set_scrollback(0);

    while all_lines.last().is_some_and(|l| l.trim().is_empty()) {
        all_lines.pop();
    }

    all_lines.join("\n")
}

pub fn visible_content(parser: &vt100::Parser) -> String {
    let (_rows, cols) = parser.screen().size();
    let lines: Vec<String> = parser.screen().rows(0, cols).collect();
    if lines.iter().all(|l| l.trim().is_empty()) {
        return String::new();
    }
    let start = lines.iter().position(|l| !l.trim().is_empty()).unwrap_or(0);
    let end = lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .unwrap_or(start);
    lines[start..=end].join("\n")
}

fn fresh_render_bytes(
    blocks: &[Block],
    tool_states: &HashMap<String, ToolState>,
    width: u16,
    height: u16,
) -> Vec<u8> {
    let sink = Arc::new(Mutex::new(Vec::new()));
    let size = Rc::new(Cell::new((width, height)));
    let cursor = Rc::new(Cell::new(0));
    let backend = TestBackend::new_with_state(size, cursor, sink.clone());
    let mut screen = Screen::with_backend(Box::new(backend));
    screen.set_anchor_row(0);

    for block in blocks {
        if let Block::ToolCall { call_id, .. } = block {
            if let Some(state) = tool_states.get(call_id) {
                screen.push_tool_call(block.clone(), state.clone());
                continue;
            }
        }
        screen.push(block.clone());
    }
    screen.render_pending_blocks();

    let input = tui::input::InputState::default();
    {
        let mut frame = tui::render::Frame::begin(screen.backend());
        screen.draw_frame(
            &mut frame,
            width as usize,
            Some(tui::render::FramePrompt {
                state: &input,
                mode: protocol::Mode::Normal,
                queued: &[],
                prediction: None,
            }),
            None,
        );
    }

    let bytes = sink.lock().unwrap().clone();
    bytes
}

fn fresh_render(
    blocks: &[Block],
    tool_states: &HashMap<String, ToolState>,
    width: u16,
    height: u16,
) -> String {
    let bytes = fresh_render_bytes(blocks, tool_states, width, height);
    let mut parser = vt100::Parser::new(height, width, 10_000);
    parser.process(&bytes);
    extract_full_content(&mut parser)
}

fn fresh_visible_render(
    blocks: &[Block],
    tool_states: &HashMap<String, ToolState>,
    width: u16,
    height: u16,
) -> String {
    let bytes = fresh_render_bytes(blocks, tool_states, width, height);
    let mut parser = vt100::Parser::new(height, width, 10_000);
    parser.process(&bytes);
    visible_content(&parser)
}

fn build_diff(expected: &str, actual: &str) -> String {
    use similar::TextDiff;
    let diff = TextDiff::from_lines(expected, actual);
    let mut out = String::new();
    out.push_str("--- expected (fresh re-render)\n");
    out.push_str("+++ actual (incremental)\n");
    for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        out.push_str(&format!("{hunk}"));
    }
    out
}

fn block_summary(block: &Block) -> String {
    match block {
        Block::User { text, .. } => format!("User({:?})", truncate(text, 40)),
        Block::Text { content } => format!("Text({:?})", truncate(content, 40)),
        Block::Thinking { content } => format!("Thinking({:?})", truncate(content, 40)),
        Block::ToolCall { name, summary, .. } => format!("ToolCall({name}: {summary})"),
        Block::CodeLine { content, lang } => format!("CodeLine({lang}: {content:?})"),
        Block::Hint { content } => format!("Hint({content:?})"),
        Block::Compacted { summary } => format!("Compacted({summary:?})"),
        Block::Exec { command, .. } => format!("Exec({command:?})"),
        Block::AgentMessage { from_slug, .. } => format!("AgentMessage(from={from_slug:?})"),
        Block::Confirm { tool, .. } => format!("Confirm({tool})"),
        Block::Agent { agent_id, .. } => format!("Agent({agent_id})"),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

// ── TestHarness ─────────────────────────────────────────────────────

pub struct TestHarness {
    pub screen: Screen,
    sink: Arc<Mutex<Vec<u8>>>,
    pub parser: vt100::Parser,
    pub width: u16,
    pub height: u16,
    size: SharedSize,
    cursor: SharedCursor,
    test_name: String,
    actions: Vec<String>,
    assert_count: usize,
    mode: protocol::Mode,
}

impl TestHarness {
    pub fn new(width: u16, height: u16, test_name: &str) -> Self {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let backend = TestBackend::new(width, height, sink.clone());
        let size = backend.shared_size();
        let cursor = backend.shared_cursor();
        let mut screen = Screen::with_backend(Box::new(backend));
        screen.set_anchor_row(0);

        Self {
            screen,
            sink,
            parser: vt100::Parser::new(height, width, 10_000),
            width,
            height,
            size,
            cursor,
            test_name: test_name.to_string(),
            actions: Vec::new(),
            assert_count: 0,
            mode: protocol::Mode::Normal,
        }
    }

    /// Resize the terminal and the vt100 parser simultaneously, then
    /// run the same `redraw` the real event loop uses.
    ///
    /// On a height shrink the harness first emits `CSI N S` (SU, scroll
    /// up) so vt100 pushes the top rows into scrollback before
    /// truncating — matching iTerm/tmux/Ghostty/WezTerm behaviour.
    /// vt100's `set_size` alone would drop the bottom rows, which no
    /// real terminal does.
    pub fn resize(&mut self, width: u16, height: u16) {
        self.actions.push(format!("resize({width}, {height})"));
        let old_h = self.height;
        if width == self.width && height < old_h {
            let delta = old_h - height;
            let seq = format!("\x1b[{delta}S");
            self.parser.process(seq.as_bytes());
            let (row, col) = self.parser.screen().cursor_position();
            let cursor_row = row.saturating_sub(delta).min(height.saturating_sub(1));
            self.parser
                .process(format!("\x1b[{};{}H", cursor_row + 1, col + 1).as_bytes());
        }
        self.width = width;
        self.height = height;
        self.size.set((width, height));
        self.parser.screen_mut().set_size(height, width);
        let (cursor_row, _) = self.parser.screen().cursor_position();
        self.cursor.set(cursor_row.min(height.saturating_sub(1)));
        self.screen.redraw();
        self.drain_sink();
    }

    /// Resize, then if the screen is dirty, draw a prompt frame.
    pub fn resize_then_tick_prompt(&mut self, width: u16, height: u16) {
        self.resize(width, height);
        if self.screen.needs_draw(false) {
            self.draw_prompt();
        }
    }

    /// Simulate a Ctrl+L purge redraw (independent of resize).
    pub fn purge_redraw(&mut self) {
        self.actions.push("purge_redraw".into());
        self.screen.redraw();
        self.drain_sink();
    }

    pub fn push(&mut self, block: Block) {
        self.actions
            .push(format!("push: {}", block_summary(&block)));
        self.screen.push(block);
    }

    pub fn render_pending(&mut self) {
        self.actions.push("render_pending".into());
        self.screen.render_pending_blocks();
        self.drain_sink();
    }

    pub fn push_and_render(&mut self, block: Block) {
        self.push(block);
        self.render_pending();
    }

    pub fn push_tool_call(&mut self, block: Block, state: ToolState) {
        self.actions
            .push(format!("push: {}", block_summary(&block)));
        self.screen.push_tool_call(block, state);
    }

    pub fn push_tool_call_and_render(&mut self, block: Block, state: ToolState) {
        self.push_tool_call(block, state);
        self.render_pending();
    }

    /// Assert the currently visible viewport matches a fresh render.
    pub fn assert_visible_matches_fresh_render(&mut self) {
        self.draw_prompt();
        let incremental = visible_content(&self.parser);
        let blocks = self.screen.blocks();
        let tool_states = self.screen.tool_states_snapshot();
        let fresh = fresh_visible_render(&blocks, &tool_states, self.width, self.height);
        self.compare_and_panic("Visible render mismatch", incremental, fresh);
    }

    /// Assert incremental rendering matches a fresh re-render (viewport + scrollback).
    pub fn assert_scrollback_integrity(&mut self) {
        // Draw a prompt frame so both sides end in the same state.
        let input = tui::input::InputState::default();
        {
            let mut frame = tui::render::Frame::begin(self.screen.backend());
            self.screen.draw_frame(
                &mut frame,
                self.width as usize,
                Some(tui::render::FramePrompt {
                    state: &input,
                    mode: self.mode,
                    queued: &[],
                    prediction: None,
                }),
                None,
            );
        }
        self.drain_sink();

        let incremental = extract_full_content(&mut self.parser);
        let blocks = self.screen.blocks();
        let tool_states = self.screen.tool_states_snapshot();
        let fresh = fresh_render(&blocks, &tool_states, self.width, self.height);
        self.compare_and_panic("Scrollback integrity failed", incremental, fresh);
    }

    fn compare_and_panic(&mut self, label: &str, incremental: String, fresh: String) {
        self.assert_count += 1;
        if incremental == fresh {
            return;
        }
        let diff = build_diff(&fresh, &incremental);
        let dump_dir = format!(
            "target/test-frames/{}/assert_{:03}",
            self.test_name, self.assert_count
        );
        let _ = std::fs::create_dir_all(&dump_dir);
        let _ = std::fs::write(format!("{dump_dir}/expected.txt"), &fresh);
        let _ = std::fs::write(format!("{dump_dir}/actual.txt"), &incremental);
        let _ = std::fs::write(format!("{dump_dir}/diff.txt"), &diff);
        let _ = std::fs::write(format!("{dump_dir}/actions.txt"), self.actions.join("\n"));
        let preview: String = diff.lines().take(40).collect::<Vec<_>>().join("\n");
        panic!(
            "{label} at assertion #{}\nBlocks: {}, Frames: {dump_dir}/\n\n{preview}",
            self.assert_count,
            self.screen.block_count(),
        );
    }

    /// Start a bash tool with a summary string. Logs into `actions`.
    pub fn start_bash_tool(&mut self, call_id: &str, summary: &str) {
        self.actions
            .push(format!("start_bash_tool({call_id}, {summary})"));
        self.screen.start_tool(
            call_id.into(),
            "bash".into(),
            summary.into(),
            HashMap::new(),
        );
    }

    /// Draw a prompt frame and snapshot the current visible viewport.
    pub fn visible(&mut self) -> String {
        self.draw_prompt();
        visible_content(&self.parser)
    }

    // ── Dialog lifecycle helpers ───────────────────────────────────

    /// Open a confirm dialog for `call_id` and draw a single combined
    /// frame (content + overlay + dialog). Leaves the dialog open and
    /// the vt100 parser updated. Returns the `ConfirmDialog` and the
    /// dialog's anchor row, so callers can either dismiss with
    /// `clear_dialog_area` or interact with the dialog further.
    pub fn open_confirm_dialog(
        &mut self,
        call_id: &str,
        name: &str,
        summary: &str,
    ) -> (ConfirmDialog, Option<u16>) {
        self.actions
            .push(format!("open_confirm_dialog({call_id}, {name}, {summary})"));

        let req = ConfirmRequest {
            call_id: call_id.into(),
            tool_name: name.into(),
            desc: summary.into(),
            args: HashMap::new(),
            approval_patterns: vec![],
            outside_dir: None,
            summary: Some(summary.into()),
            request_id: 1,
        };
        let mut dialog = ConfirmDialog::new(&req, false);
        dialog.set_term_size(self.width, self.height);

        self.screen.set_active_status(call_id, ToolStatus::Confirm);
        self.screen.render_pending_blocks();
        self.screen.erase_prompt();
        let dialog_height = dialog.height();

        {
            let mut frame = tui::render::Frame::begin(self.screen.backend());
            self.screen
                .draw_frame(&mut frame, self.width as usize, None, Some(dialog_height));
            let dr = self.screen.dialog_row();
            dialog.draw(&mut frame, dr, self.width, self.height);
        }
        let da = dialog.anchor_row();
        self.screen.sync_dialog_anchor(da);
        self.drain_sink();

        (dialog, da)
    }

    /// Run a full confirm dialog cycle: open, draw, dismiss, finish tool.
    ///
    /// Draws a prompt frame first to establish anchor_row and prompt state,
    /// matching the real event loop where tick() always runs before dialog
    /// handling.
    pub fn confirm_cycle(&mut self, call_id: &str, name: &str, summary: &str, output: &str) {
        self.actions
            .push(format!("confirm_cycle({call_id}, {name}, {summary})"));

        // In the real app, at least one tick() (draw_frame with prompt)
        // runs before a dialog opens. This establishes the prompt anchor.
        self.draw_prompt();

        self.screen
            .start_tool(call_id.into(), name.into(), summary.into(), HashMap::new());
        let (_dialog, da) = self.open_confirm_dialog(call_id, name, summary);

        // Dismiss dialog.
        self.screen.clear_dialog_area(da);
        self.drain_sink();

        // Finish tool.
        self.screen.finish_tool(
            call_id,
            ToolStatus::Ok,
            Some(Box::new(ToolOutput {
                content: output.into(),
                is_error: false,
                metadata: None,
                render_cache: None,
            })),
            Some(Duration::from_millis(100)),
        );
        self.screen.flush_blocks();
        self.drain_sink();

        // flush_blocks may be deferred after dialog dismiss; a tick
        // (draw_frame with prompt) picks up the deferred render — this
        // mirrors the real event loop which calls tick() after every event.
        self.draw_prompt();
    }

    /// Draw a prompt frame (simulates the prompt being visible).
    pub fn draw_prompt(&mut self) {
        self.actions.push("draw_prompt".into());
        let input = tui::input::InputState::default();
        {
            let mut frame = tui::render::Frame::begin(self.screen.backend());
            self.screen.draw_frame(
                &mut frame,
                self.width as usize,
                Some(tui::render::FramePrompt {
                    state: &input,
                    mode: self.mode,
                    queued: &[],
                    prediction: None,
                }),
                None,
            );
        }
        self.drain_sink();
    }

    /// Stream text, flush it, and render.
    pub fn stream_and_flush(&mut self, text: &str) {
        self.actions.push(format!("stream_and_flush({text:?})"));
        self.screen.append_streaming_text(text);
        self.screen.flush_streaming_text();
        self.screen.render_pending_blocks();
        self.drain_sink();
    }

    /// Stream text line by line with a draw_prompt tick after each line.
    pub fn stream_lines_with_ticks(&mut self, text: &str) {
        self.actions
            .push(format!("stream_lines_with_ticks({:?})", truncate(text, 40)));
        for line in text.split_inclusive('\n') {
            self.screen.append_streaming_text(line);
            let input = tui::input::InputState::default();
            {
                let mut frame = tui::render::Frame::begin(self.screen.backend());
                self.screen.draw_frame(
                    &mut frame,
                    self.width as usize,
                    Some(tui::render::FramePrompt {
                        state: &input,
                        mode: self.mode,
                        queued: &[],
                        prediction: None,
                    }),
                    None,
                );
            }
            self.drain_sink();
        }
        self.screen.flush_streaming_text();
        self.screen.render_pending_blocks();
        self.drain_sink();
    }

    /// Extract all visible + scrollback text from the vt100 parser.
    pub fn full_text(&mut self) -> String {
        self.draw_prompt();
        extract_full_content(&mut self.parser)
    }

    /// Assert that all expected strings are present in the captured output.
    pub fn assert_contains_all(&mut self, expected: &[&str]) {
        let text = self.full_text();

        let missing: Vec<&&str> = expected.iter().filter(|s| !text.contains(*s)).collect();
        if missing.is_empty() {
            return;
        }

        let dump_dir = format!("target/test-frames/{}", self.test_name);
        let _ = std::fs::create_dir_all(&dump_dir);
        let _ = std::fs::write(format!("{dump_dir}/captured.txt"), &text);

        panic!(
            "{}: missing content\n\
             Missing: {missing:?}\n\
             Saved to: {dump_dir}/captured.txt\n\n\
             Captured:\n{text}",
            self.test_name,
        );
    }

    // ── Status bar helpers ──────────────────────────────────────────

    /// Extract the last row (status bar) from the vt100 screen.
    /// The status bar is the last non-empty row rendered after draw_prompt.
    pub fn status_line_text(&mut self) -> String {
        self.draw_prompt();
        let text = extract_full_content(&mut self.parser);
        text.lines().last().unwrap_or("").to_string()
    }

    /// Set the mode used for subsequent draw_prompt calls.
    pub fn set_mode(&mut self, mode: protocol::Mode) {
        self.mode = mode;
    }

    // ── Internal ────────────────────────────────────────────────────

    pub fn drain_sink(&mut self) {
        let bytes = {
            let mut buf = self.sink.lock().unwrap();
            let b = buf.clone();
            buf.clear();
            b
        };
        if !bytes.is_empty() {
            // vt100 ignores `ESC[3J` (Clear::Purge). Real terminals
            // wipe scrollback on it. Simulate that: process bytes up
            // to each purge marker, snapshot the visible grid into a
            // fresh parser (dropping scrollback), then continue. Any
            // scroll-mode output after the purge refills the fresh
            // scrollback normally.
            const PURGE: &[u8] = b"\x1b[3J";
            let mut cursor = 0usize;
            while cursor < bytes.len() {
                let rel = bytes[cursor..]
                    .windows(PURGE.len())
                    .position(|w| w == PURGE);
                match rel {
                    Some(rel) => {
                        let end = cursor + rel + PURGE.len();
                        self.parser.process(&bytes[cursor..end]);
                        let (rows, cols) = self.parser.screen().size();
                        let snapshot = self.parser.screen().contents_formatted();
                        self.parser = vt100::Parser::new(rows, cols, 10_000);
                        self.parser.process(&snapshot);
                        cursor = end;
                    }
                    None => {
                        self.parser.process(&bytes[cursor..]);
                        break;
                    }
                }
            }
            let (row, _) = self.parser.screen().cursor_position();
            self.cursor.set(row);
        }
    }
}
