//! Test harness for TUI rendering verification (vt100).
// Shared across multiple test binaries; not all items are used in each.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tui::render::{
    Block, ConfirmDialog, ConfirmRequest, Dialog, RenderOut, Screen, TerminalBackend, ToolOutput,
    ToolStatus,
};

// ── TestBackend ──────────────────────────────────────────────────────

pub struct TestBackend {
    width: u16,
    height: u16,
    sink: Arc<Mutex<Vec<u8>>>,
}

impl TestBackend {
    pub fn new(width: u16, height: u16, sink: Arc<Mutex<Vec<u8>>>) -> Self {
        Self {
            width,
            height,
            sink,
        }
    }
}

impl TerminalBackend for TestBackend {
    fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }
    fn cursor_y(&self) -> u16 {
        0
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

fn fresh_render(blocks: &[Block], width: u16, height: u16) -> String {
    let sink = Arc::new(Mutex::new(Vec::new()));
    let backend = TestBackend::new(width, height, sink.clone());
    let mut screen = Screen::with_backend(Box::new(backend));
    screen.set_anchor_row(0);

    for block in blocks {
        screen.push(block.clone());
    }
    screen.render_pending_blocks();

    let input = tui::input::InputState::default();
    screen.draw_frame(
        width as usize,
        Some(tui::render::FramePrompt {
            state: &input,
            mode: protocol::Mode::Normal,
            queued: &[],
            prediction: None,
        }),
    );

    let bytes = sink.lock().unwrap().clone();
    let mut parser = vt100::Parser::new(height, width, 10_000);
    parser.process(&bytes);
    extract_full_content(&mut parser)
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
    test_name: String,
    actions: Vec<String>,
    assert_count: usize,
}

impl TestHarness {
    pub fn new(width: u16, height: u16, test_name: &str) -> Self {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let backend = TestBackend::new(width, height, sink.clone());
        let mut screen = Screen::with_backend(Box::new(backend));
        screen.set_anchor_row(0);

        Self {
            screen,
            sink,
            parser: vt100::Parser::new(height, width, 10_000),
            width,
            height,
            test_name: test_name.to_string(),
            actions: Vec::new(),
            assert_count: 0,
        }
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

    /// Assert incremental rendering matches a fresh re-render.
    pub fn assert_scrollback_integrity(&mut self) {
        self.assert_count += 1;

        // Draw a prompt frame so both sides end in the same state.
        let input = tui::input::InputState::default();
        self.screen.draw_frame(
            self.width as usize,
            Some(tui::render::FramePrompt {
                state: &input,
                mode: protocol::Mode::Normal,
                queued: &[],
                prediction: None,
            }),
        );
        self.drain_sink();

        let incremental = extract_full_content(&mut self.parser);
        let blocks = self.screen.blocks();
        let fresh = fresh_render(&blocks, self.width, self.height);

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
            "Scrollback integrity failed at assertion #{}\n\
             Blocks: {}, Frames: {dump_dir}/\n\n\
             {preview}",
            self.assert_count,
            self.screen.block_count(),
        );
    }

    // ── Dialog lifecycle helpers ───────────────────────────────────

    /// Run a full confirm dialog cycle: open, draw, dismiss, finish tool.
    pub fn confirm_cycle(&mut self, call_id: &str, name: &str, summary: &str, output: &str) {
        self.actions
            .push(format!("confirm_cycle({call_id}, {name}, {summary})"));

        self.screen
            .start_tool(call_id.into(), name.into(), summary.into(), HashMap::new());
        self.screen.set_active_status(call_id, ToolStatus::Confirm);
        self.screen.render_pending_blocks();
        self.drain_sink();

        // Open dialog.
        self.screen.render_pending_blocks_for_dialog();
        self.screen.erase_prompt_nosync();
        self.screen.set_show_tool_in_dialog(true);

        // Draw content-only frame (no prompt).
        self.screen.draw_frame(self.width as usize, None);
        self.drain_sink();

        // Draw the actual ConfirmDialog.
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
        let sync = self.screen.take_sync_started();
        let dr = self.screen.dialog_row();
        dialog.draw(dr, sync, self.screen.backend());
        self.drain_sink();
        let da = dialog.anchor_row();
        self.screen.sync_dialog_anchor(da);
        self.drain_sink();

        // Dismiss dialog.
        self.screen.clear_dialog_area(da);
        self.drain_sink();

        // Finish tool.
        self.screen.finish_tool(
            call_id,
            ToolStatus::Ok,
            Some(ToolOutput {
                content: output.into(),
                is_error: false,
                metadata: None,
            }),
            Some(Duration::from_millis(100)),
        );
        self.screen.flush_blocks();
        self.drain_sink();

        // flush_blocks may be deferred after dialog dismiss; a tick
        // (draw_frame with prompt) picks up the deferred render — this
        // mirrors the real event loop which calls tick() after every event.
        let input = tui::input::InputState::default();
        self.screen.draw_frame(
            self.width as usize,
            Some(tui::render::FramePrompt {
                state: &input,
                mode: protocol::Mode::Normal,
                queued: &[],
                prediction: None,
            }),
        );
        self.drain_sink();
    }

    /// Draw a prompt frame (simulates the prompt being visible).
    pub fn draw_prompt(&mut self) {
        self.actions.push("draw_prompt".into());
        let input = tui::input::InputState::default();
        self.screen.draw_frame(
            self.width as usize,
            Some(tui::render::FramePrompt {
                state: &input,
                mode: protocol::Mode::Normal,
                queued: &[],
                prediction: None,
            }),
        );
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
            self.screen.draw_frame(
                self.width as usize,
                Some(tui::render::FramePrompt {
                    state: &input,
                    mode: protocol::Mode::Normal,
                    queued: &[],
                    prediction: None,
                }),
            );
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

    // ── Internal ────────────────────────────────────────────────────

    pub fn drain_sink(&mut self) {
        let bytes = {
            let mut buf = self.sink.lock().unwrap();
            let b = buf.clone();
            buf.clear();
            b
        };
        if !bytes.is_empty() {
            self.parser.process(&bytes);
        }
    }
}
