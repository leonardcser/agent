//! Top-level chat screen: block history, streaming state, prompt composition.
//!
//! `Screen` is the render module's main state object — it owns the
//! block history, active streaming overlays (thinking / text / tools /
//! agents / exec), and all the flags that feed the status line and
//! prompt rendering. `draw_frame` is the single entry point called
//! from the main loop; it renders blocks (scroll mode), the ephemeral
//! overlay, and the prompt (or dialog placement) atomically.

use super::blocks;
use super::blocks::{
    collect_trailing_thinking, gap_between, render_thinking_summary, thinking_summary, Element,
};
use super::cache::{PersistedLayoutCache, RenderCache};
use super::completions::{completion_reserved_rows, draw_completions, draw_menu};
use super::history::{
    AgentBlockStatus, Block, BlockId, Status, Throbber, ToolOutputRef, ToolState, ToolStatus,
    ViewState,
};
use super::transcript::Transcript;

/// Visual selection in the content pane, captured from vim state.
/// Line indices are 0-based from the top of the full transcript; cols
/// count chars on that line.
#[derive(Clone, Copy, Debug)]
pub struct ContentVisualRange {
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
    pub kind: ContentVisualKind,
}

#[derive(Clone, Copy, Debug)]
pub enum ContentVisualKind {
    Char,
    Line,
}
use super::layout_out::{LayoutSink, SpanCollector};
use super::prompt::PromptState;
use super::selection::{
    build_char_kinds, build_display_spans, compute_visual_line_offsets, map_cursor,
    render_styled_chars, spans_to_string, wrap_and_locate_cursor, wrap_line, SpanKind,
};
use super::status::{
    draw_bar, render_status_spans, vim_mode_label, BarSpan, StatusPosition, StatusSpan,
};
use super::working::WorkingState;
use super::{
    cursor_colors, draw_soft_cursor, emit_newlines, format_tokens, reasoning_color,
    DialogPlacement, Frame, FramePrompt, RenderOut, StdioBackend, StyleState, TerminalBackend,
    SPINNER_FRAMES,
};
use crate::input::{InputSnapshot, InputState};
use crate::keymap::hints;
use crate::theme;
use crossterm::{
    cursor,
    style::{Color, Print, ResetColor},
    terminal, QueueableCommand,
};
use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

pub struct Screen {
    pub(crate) transcript: Transcript,
    prompt: PromptState,
    working: WorkingState,
    context_tokens: Option<u32>,
    context_window: Option<u32>,
    session_cost_usd: f64,
    model_label: Option<String>,
    reasoning_effort: protocol::ReasoningEffort,
    /// True once terminal auto-scrolling has pushed content into scrollback.
    pub has_scrollback: bool,
    /// Terminal row where block content starts (top of conversation).
    /// Set once when the first block is rendered; reset on purge/clear.
    content_start_row: Option<u16>,
    /// Skip the next `mark_blocks_dirty` call so the next `draw_frame`
    /// picks up the blocks instead of a stale mid-dialog repaint.
    defer_pending_render: bool,
    /// A permission dialog is waiting for the user to stop typing.
    pending_dialog: bool,
    /// A dialog is currently open (confirm, rewind, etc.).
    dialog_open: bool,
    /// Whether the active dialog's height should be constrained to
    /// `max(h/2, natural_space)` to limit scroll-up.
    constrain_dialog: bool,
    running_procs: usize,
    running_agents: usize,
    show_tps: bool,
    show_tokens: bool,
    show_cost: bool,
    show_slug: bool,
    show_thinking: bool,
    /// Cached state for rendering the status line during dialogs.
    last_vim_enabled: bool,
    last_vim_mode: Option<crate::vim::ViMode>,
    last_mode: protocol::Mode,
    /// App-level focus (Prompt / History). Driven by App::app_focus.
    last_app_focus: crate::app::AppFocus,
    /// Buffer-agnostic cursor/scroll snapshot for the focused window
    /// (prompt or transcript). Pushed by `App::tick_prompt` each frame
    /// so the status line reads a single, already-computed record
    /// instead of recomputing from stale view fields.
    last_status_position: Option<StatusPosition>,
    /// Plain-text snapshot of each visible row (top to bottom) captured
    /// during `draw_viewport_frame`. Used by the content pane's motion
    /// handlers and yank to reason over what the user actually sees.
    last_viewport_text: Vec<String>,
    /// Gutter reservations for the transcript window (padding +
    /// scrollbar column). Pushed from `App` so the paint path picks up
    /// the authoritative source without a reverse dependency.
    transcript_gutters: crate::window::WindowGutters,
    /// Transcript viewport region recorded during the last paint. Used
    /// by mouse hit-testing and scroll math. `None` before the first
    /// viewport frame.
    last_transcript_region: Option<super::region::TranscriptRegion>,
    /// Centralized layout state computed at the start of each frame.
    pub(crate) layout: super::layout::LayoutState,
    /// Ephemeral btw side-question state, rendered above the prompt.
    btw: Option<BtwBlock>,
    /// Ephemeral notification shown above the prompt, dismissed on any key.
    notification: Option<Notification>,
    /// Short task label (slug) shown on the status bar after the throbber.
    task_label: Option<String>,

    /// Terminal I/O backend (real terminal or test buffer).
    backend: Box<dyn TerminalBackend>,
    focused: bool,
}

/// A short ephemeral notification rendered above the prompt bar.
pub struct Notification {
    pub message: String,
    pub is_error: bool,
}

/// State for an in-flight `/btw` side question.
pub struct BtwBlock {
    pub question: String,
    pub image_labels: Vec<String>,
    pub response: Option<String>,
    /// Cached wrapped lines for scrolling.
    wrapped: Vec<String>,
    scroll_offset: usize,
    /// Terminal width when lines were last wrapped.
    wrap_width: usize,
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen {
    pub fn new() -> Self {
        Self::with_backend(Box::new(StdioBackend))
    }

    pub fn with_backend(backend: Box<dyn TerminalBackend>) -> Self {
        Self {
            transcript: Transcript::new(),
            prompt: PromptState::new(),
            working: WorkingState::new(),
            context_tokens: None,
            context_window: None,
            session_cost_usd: 0.0,
            model_label: None,
            reasoning_effort: Default::default(),
            has_scrollback: false,
            content_start_row: None,
            defer_pending_render: false,
            pending_dialog: false,
            dialog_open: false,
            constrain_dialog: false,
            running_procs: 0,
            running_agents: 0,
            show_tps: true,
            show_tokens: true,
            show_cost: true,
            show_slug: true,
            show_thinking: true,
            last_vim_enabled: false,
            last_vim_mode: None,
            last_mode: protocol::Mode::Normal,
            last_app_focus: crate::app::AppFocus::Prompt,
            last_status_position: None,
            last_viewport_text: Vec::new(),
            transcript_gutters: crate::window::WindowGutters {
                pad_left: 1,
                pad_right: 1,
                scrollbar: Some(crate::window::GutterSide::Right),
            },
            last_transcript_region: None,
            layout: super::layout::LayoutState::compute(&super::layout::LayoutInput {
                term_width: 80,
                term_height: 24,
                prompt_height: 3,
                dialog_height: None,
                constrain_dialog: false,
            }),
            btw: None,
            notification: None,
            task_label: None,
            backend,
            focused: true,
        }
    }

    pub fn size(&self) -> (u16, u16) {
        self.backend.size()
    }

    /// Width available for transcript content. Reserves the rightmost
    /// column for the scrollbar track so the scrollbar never overpaints
    /// rendered content and mouse hit-testing has a stable target.
    pub fn transcript_width(&self) -> usize {
        let (w, _) = self.backend.size();
        (w as usize).saturating_sub(1).max(1)
    }

    pub fn show_thinking(&self) -> bool {
        self.show_thinking
    }

    /// Expose the backend for dialogs that need output + size.
    pub fn backend(&self) -> &dyn TerminalBackend {
        &*self.backend
    }

    pub fn set_focused(&mut self, focused: bool) {
        if self.focused == focused {
            return;
        }
        self.focused = focused;
        // Focus only affects the prompt's soft cursor visual.  When a
        // dialog is open, the prompt isn't drawn — marking it dirty
        // would force a full dialog-mode repaint whose bottom-gap
        // cleanup (`queue_dialog_gap`) clears the screen below the
        // dialog's anchor row, flashing the dialog off and on.
        if !self.dialog_open {
            self.prompt.dirty = true;
        }
    }

    /// Set the prompt anchor row explicitly (used by test harness).
    pub fn set_anchor_row(&mut self, row: u16) {
        self.prompt.anchor_row = Some(row);
    }

    pub fn block_count(&self) -> usize {
        self.transcript.block_count()
    }

    pub fn blocks(&self) -> Vec<Block> {
        self.transcript.blocks()
    }

    pub fn tool_states_snapshot(&self) -> HashMap<String, ToolState> {
        self.transcript.tool_states_snapshot()
    }

    pub fn set_btw(&mut self, question: String, image_labels: Vec<String>) {
        self.btw = Some(BtwBlock {
            question,
            image_labels,
            response: None,
            wrapped: Vec::new(),
            scroll_offset: 0,
            wrap_width: 0,
        });
        self.prompt.dirty = true;
    }

    pub fn set_btw_response(&mut self, content: String) {
        if let Some(ref mut btw) = self.btw {
            btw.response = Some(content);
            btw.wrapped.clear();
            btw.scroll_offset = 0;
            btw.wrap_width = 0;
            self.prompt.dirty = true;
        }
    }

    pub fn dismiss_btw(&mut self) {
        if self.btw.is_some() {
            self.btw = None;
            self.prompt.dirty = true;
        }
    }

    pub fn has_btw(&self) -> bool {
        self.btw.is_some()
    }

    /// Scroll the btw block. Returns true if state changed.
    pub fn btw_scroll(&mut self, delta: isize) -> bool {
        let term_h = self.size().1 as usize;
        let Some(ref mut btw) = self.btw else {
            return false;
        };
        if btw.wrapped.is_empty() {
            return false;
        }
        let max_lines = btw_max_body_rows(term_h);
        let max = btw.wrapped.len().saturating_sub(max_lines);
        let old = btw.scroll_offset;
        if delta < 0 {
            btw.scroll_offset = btw.scroll_offset.saturating_sub((-delta) as usize);
        } else {
            btw.scroll_offset = (btw.scroll_offset + delta as usize).min(max);
        }
        if btw.scroll_offset != old {
            self.prompt.dirty = true;
            true
        } else {
            false
        }
    }

    pub fn notify(&mut self, message: String) {
        self.notification = Some(Notification {
            message,
            is_error: false,
        });
        self.prompt.dirty = true;
    }

    pub fn notify_error(&mut self, message: String) {
        self.notification = Some(Notification {
            message,
            is_error: true,
        });
        self.prompt.dirty = true;
    }

    pub fn dismiss_notification(&mut self) {
        if self.notification.is_some() {
            self.notification = None;
            self.prompt.dirty = true;
        }
    }

    pub fn has_notification(&self) -> bool {
        self.notification.is_some()
    }

    /// Apply all toggle settings from a resolved settings snapshot.
    pub fn apply_settings(&mut self, s: &crate::state::ResolvedSettings) {
        self.show_tps = s.show_tps;
        self.show_tokens = s.show_tokens;
        self.show_cost = s.show_cost;
        self.show_slug = s.show_slug;
        self.show_thinking = s.show_thinking;
        self.prompt.dirty = true;
    }

    pub fn set_running_procs(&mut self, count: usize) {
        if count != self.running_procs {
            self.running_procs = count;
            self.prompt.dirty = true;
        }
    }

    pub fn set_agent_count(&mut self, count: usize) {
        if count != self.running_agents {
            self.running_agents = count;
            self.prompt.dirty = true;
        }
    }

    pub fn start_active_agent(&mut self, agent_id: String) {
        self.transcript.start_active_agent(agent_id);
        self.prompt.dirty = true;
    }

    pub fn update_active_agent(
        &mut self,
        agent_id: &str,
        slug: Option<&str>,
        tool_calls: &[crate::app::AgentToolEntry],
        status: AgentBlockStatus,
    ) {
        self.transcript
            .update_active_agent(agent_id, slug, tool_calls, status);
        self.prompt.dirty = true;
    }

    pub fn cancel_active_agents(&mut self) {
        self.transcript.cancel_active_agents();
    }

    pub fn finish_active_agent(&mut self, agent_id: &str) {
        self.transcript.finish_active_agent(agent_id);
        self.prompt.dirty = true;
    }

    pub fn finish_all_active_agents(&mut self) {
        self.transcript.finish_all_active_agents();
        self.prompt.dirty = true;
    }

    /// Row where a dialog should start rendering (lines up with the prompt bar).
    pub fn dialog_row(&self) -> u16 {
        self.prompt.prev_dialog_row.unwrap_or(0)
    }

    /// Emit a blank gap line after the dialog, then clear any stale
    /// rows between the dialog and the status bar.  Called globally
    /// from `render_frame` so every dialog gets the same gap without
    /// each one needing to emit it.
    ///
    /// Only emits when `out` is in overlay mode (`out.row` is `Some`).
    /// On early-exit frames where the dialog didn't redraw, `out.row`
    /// stays `None` (scroll mode) and a newline would push a `\r\n`
    /// into scrollback, polluting the scroll buffer and shifting the
    /// visible dialog.
    pub fn queue_dialog_gap(&self, out: &mut RenderOut) {
        if out.row.is_none() {
            return;
        }
        out.newline();
        let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
    }

    /// Render the status line at the very last row of the terminal into
    /// the given output buffer.  Used as a callback inside dialog sync frames
    /// so the status bar is painted atomically with dialog content.
    pub fn queue_status_line(&self, out: &mut RenderOut) {
        let (_, h) = self.size();
        if h == 0 {
            return;
        }
        let _ = out.queue(cursor::SavePosition);
        let _ = out.queue(cursor::MoveTo(0, h - 1));
        out.reset_style();
        self.render_status_line(out);
        let _ = out.queue(cursor::RestorePosition);
    }

    /// Render the status line content at the current cursor position.
    /// Responsively drops/truncates elements when the terminal is too narrow.
    fn render_status_line(&self, out: &mut RenderOut) {
        let (w, _) = self.size();
        let width = w as usize;
        let status_bg = Color::AnsiValue(233);

        // ── Build all status spans ──
        let mut spans: Vec<StatusSpan> = Vec::with_capacity(16);

        // Slug pill: spinner (always visible) + label (truncatable).
        let is_compacting = self.working.throbber == Some(Throbber::Compacting);
        let spinner = self.working.spinner_char();
        let pill_bg = if is_compacting {
            Color::White
        } else {
            theme::slug_color()
        };
        let pill_style = StyleState {
            fg: Some(Color::Black),
            bg: Some(pill_bg),
            ..StyleState::default()
        };

        if let Some(sp) = spinner {
            spans.push(StatusSpan {
                text: format!(" {sp} "),
                style: pill_style.clone(),
                priority: 0,
                ..StatusSpan::default()
            });
            let label = if is_compacting {
                "compacting ".into()
            } else if self.show_slug {
                self.task_label
                    .as_ref()
                    .map(|l| format!("{l} "))
                    .unwrap_or_else(|| "working ".into())
            } else {
                "working ".into()
            };
            spans.push(StatusSpan {
                text: label,
                style: pill_style,
                priority: 5,
                truncatable: true,
                ..StatusSpan::default()
            });
        } else if self.show_slug {
            if let Some(ref label) = self.task_label {
                spans.push(StatusSpan {
                    text: format!(" {label} "),
                    style: pill_style,
                    priority: 5,
                    truncatable: true,
                    ..StatusSpan::default()
                });
            }
        }

        if self.last_vim_enabled {
            let vim_label = vim_mode_label(self.last_vim_mode).unwrap_or("NORMAL");
            let vim_fg = match self.last_vim_mode {
                Some(crate::vim::ViMode::Insert) => Color::AnsiValue(78),
                Some(crate::vim::ViMode::Visual) | Some(crate::vim::ViMode::VisualLine) => {
                    Color::AnsiValue(176)
                }
                _ => Color::AnsiValue(74),
            };
            spans.push(StatusSpan {
                text: format!(" {vim_label} "),
                style: StyleState {
                    fg: Some(vim_fg),
                    bg: Some(Color::AnsiValue(236)),
                    ..StyleState::default()
                },
                priority: 3,
                ..StatusSpan::default()
            });
        }

        // Mode indicator.
        let (mode_icon, mode_name, mode_fg) = match self.last_mode {
            protocol::Mode::Plan => ("◇ ", "plan", theme::PLAN),
            protocol::Mode::Apply => ("→ ", "apply", theme::APPLY),
            protocol::Mode::Yolo => ("⚡", "yolo", theme::YOLO),
            protocol::Mode::Normal => ("○ ", "normal", theme::muted()),
        };
        spans.push(StatusSpan {
            text: format!(" {mode_icon}{mode_name} "),
            style: StyleState {
                fg: Some(mode_fg),
                bg: Some(Color::AnsiValue(234)),
                ..StyleState::default()
            },
            priority: 1,
            ..StatusSpan::default()
        });

        // Throbber status (timer, tok/s, retry, done, interrupted).
        // Skip the first span for active states — it duplicates the pill.
        let throbber_spans = self.working.throbber_spans(self.show_tps);
        let is_active = matches!(
            self.working.throbber,
            Some(Throbber::Working) | Some(Throbber::Compacting) | Some(Throbber::Retrying { .. })
        );
        let status_bar_spans: &[BarSpan] = if is_active && !throbber_spans.is_empty() {
            &throbber_spans[1..]
        } else {
            &throbber_spans
        };
        for bar_span in status_bar_spans {
            // Map BarSpan priorities: timer (0) → 4, tok/s (3) → 6.
            let priority = match bar_span.priority {
                0 => 4,
                3 => 6,
                p => p,
            };
            spans.push(StatusSpan {
                text: bar_span.text.clone(),
                style: StyleState {
                    fg: Some(bar_span.color),
                    bg: Some(status_bg),
                    bold: bar_span.bold,
                    dim: bar_span.dim,
                    ..StyleState::default()
                },
                priority,
                ..StatusSpan::default()
            });
        }

        // Permission pending.
        if self.pending_dialog && !self.dialog_open {
            spans.push(StatusSpan {
                text: "permission pending".into(),
                style: StyleState {
                    fg: Some(theme::accent()),
                    bg: Some(status_bg),
                    bold: true,
                    ..StyleState::default()
                },
                priority: 2,
                group: true,
                ..StatusSpan::default()
            });
        }

        // Running procs.
        if self.running_procs > 0 {
            let label = if self.running_procs == 1 {
                "1 proc".into()
            } else {
                format!("{} procs", self.running_procs)
            };
            spans.push(StatusSpan {
                text: label,
                style: StyleState {
                    fg: Some(theme::accent()),
                    bg: Some(status_bg),
                    ..StyleState::default()
                },
                priority: 2,
                group: true,
                ..StatusSpan::default()
            });
        }

        // Running agents.
        if self.running_agents > 0 {
            let label = if self.running_agents == 1 {
                "1 agent".into()
            } else {
                format!("{} agents", self.running_agents)
            };
            spans.push(StatusSpan {
                text: label,
                style: StyleState {
                    fg: Some(theme::AGENT),
                    bg: Some(status_bg),
                    ..StyleState::default()
                },
                priority: 2,
                group: true,
                ..StatusSpan::default()
            });
        }

        // Right-aligned cursor / scroll position — buffer-agnostic.
        // The app pushes a `StatusPosition` each tick derived from the
        // focused window; missing = no focused buffer yet (dialogs,
        // early frames) so we simply omit the span.
        if let Some(p) = self.last_status_position {
            spans.push(StatusSpan {
                text: p.render(),
                style: StyleState {
                    fg: Some(theme::muted()),
                    bg: Some(status_bg),
                    ..StyleState::default()
                },
                priority: 3,
                truncatable: true,
                align_right: true,
                ..StatusSpan::default()
            });
        }

        // ── Responsive layout ──
        render_status_spans(out, &mut spans, width, status_bg);
    }

    /// Dismiss a dialog overlay.
    ///
    /// Clears from the screen anchor (or the dialog row, whichever is
    /// higher) down, so any ephemeral overlay shifted upward by
    /// `ScrollUp` is wiped along with the dialog bar itself.
    pub fn clear_dialog_area(&mut self) {
        let dialog_row = self.prompt.prev_dialog_row.unwrap_or(0);
        let screen_anchor = self.prompt.anchor_row.unwrap_or(dialog_row);
        let clear_from = screen_anchor.min(dialog_row);

        let height = self.size().1;
        let mut frame = Frame::begin(&*self.backend);
        for row in clear_from..height {
            let _ = frame.queue(cursor::MoveTo(0, row));
            let _ = frame.queue(terminal::Clear(terminal::ClearType::CurrentLine));
        }
        // When the dialog scrolled enough to push the anchor to row 0,
        // the prompt gap that was between the last block and the dialog
        // got pushed into scrollback.  The next block render would emit
        // gap_between() again, creating a double blank line.  Suppress
        // the leading gap so only the scrollback copy remains.
        // Fullscreen dialogs omit the gap (see the `gap` calc in
        // `draw_frame` dialog mode) — nothing was scrolled into
        // scrollback to duplicate, so don't suppress.
        let scrolled_by_dialog = screen_anchor == 0 && self.has_scrollback;
        self.defer_pending_render = true;
        // Only reset anchor/prev_rows when the dialog caused ScrollUp
        // (prompt was physically moved). For non-scrolled dialogs the
        // prompt is still in its original position — just mark dirty so
        // it redraws in place.
        if scrolled_by_dialog || self.prompt.anchor_row.is_none() {
            self.prompt.anchor_row = Some(clear_from);
            self.prompt.prev_rows = 0;
        }
        self.prompt.drawn = false;
        self.prompt.dirty = true;
        self.prompt.prev_dialog_row = None;
    }

    /// Move the cursor to the line after the prompt so the shell resumes cleanly.
    /// When `clear_below` is true, clears remaining rows (completions).
    pub fn move_cursor_past_prompt(&self, clear_below: bool) {
        if !self.prompt.drawn {
            return;
        }
        let anchor = self.prompt.anchor_row.unwrap_or(0);
        let last_row = anchor + self.prompt.prev_rows.saturating_sub(1);
        let height = self.size().1;
        let mut out = self.backend.make_output();
        // Erase the software block cursor so it doesn't linger on exit.
        if let Some((col, row)) = self.prompt.soft_cursor {
            let _ = out.queue(cursor::MoveTo(col, row));
            let _ = out.queue(ResetColor);
            out.print(" ");
        }
        let _ = out.queue(cursor::MoveTo(0, last_row.min(height.saturating_sub(1))));
        let _ = out.queue(Print("\r\n\r\n"));
        out.line_cols = 0;
        if clear_below {
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
        }
        let _ = out.flush();
    }

    pub fn begin_turn(&mut self) {
        self.transcript.begin_turn();
    }

    pub fn push_tool_call(&mut self, block: Block, state: ToolState) {
        self.transcript.push_tool_call(block, state);
        self.prompt.dirty = true;
    }

    pub fn push(&mut self, block: Block) {
        self.transcript.push(block);
        self.prompt.dirty = true;
    }

    pub fn append_streaming_thinking(&mut self, delta: &str) {
        self.transcript.append_streaming_thinking(delta);
        self.prompt.dirty = true;
    }

    pub fn flush_streaming_thinking(&mut self) {
        self.transcript.flush_streaming_thinking();
        self.prompt.dirty = true;
    }

    /// Gap before a thinking summary overlay, skipping over hidden thinking blocks.
    fn thinking_summary_gap(&self) -> u16 {
        if let Some(last) = self
            .transcript
            .history
            .order
            .iter()
            .rev()
            .filter_map(|id| self.transcript.history.blocks.get(id))
            .find(|b| !matches!(b, Block::Thinking { .. }))
        {
            gap_between(
                &Element::Block(last),
                &Element::Block(&Block::Thinking {
                    content: String::new(),
                }),
            )
        } else if self.transcript.history.is_empty() {
            0
        } else {
            1
        }
    }

    pub fn append_streaming_text(&mut self, delta: &str) {
        self.transcript.append_streaming_text(delta);
        self.prompt.dirty = true;
    }

    pub fn flush_streaming_text(&mut self) {
        self.transcript.flush_streaming_text();
        self.prompt.dirty = true;
    }

    pub fn start_tool(
        &mut self,
        call_id: String,
        name: String,
        summary: String,
        args: HashMap<String, serde_json::Value>,
    ) {
        self.transcript.start_tool(call_id, name, summary, args);
        self.prompt.dirty = true;
    }

    pub fn start_exec(&mut self, command: String) {
        self.transcript.start_exec(command);
        self.prompt.dirty = true;
    }

    pub fn append_exec_output(&mut self, chunk: &str) {
        self.transcript.append_exec_output(chunk);
        self.prompt.dirty = true;
    }

    pub fn finish_exec(&mut self, exit_code: Option<i32>) {
        self.transcript.finish_exec(exit_code);
        self.prompt.dirty = true;
    }

    pub fn finalize_exec(&mut self) {
        self.transcript.finalize_exec();
        self.prompt.dirty = true;
    }

    pub fn has_active_exec(&self) -> bool {
        self.transcript.has_active_exec()
    }

    pub fn append_active_output(&mut self, call_id: &str, chunk: &str) {
        self.transcript.append_active_output(call_id, chunk);
        self.prompt.dirty = true;
    }

    pub fn set_active_status(&mut self, call_id: &str, status: ToolStatus) {
        self.transcript.set_active_status(call_id, status);
        self.prompt.dirty = true;
    }

    pub fn set_active_user_message(&mut self, call_id: &str, msg: String) {
        self.transcript.set_active_user_message(call_id, msg);
        self.prompt.dirty = true;
    }

    pub fn finish_tool(
        &mut self,
        call_id: &str,
        status: ToolStatus,
        output: Option<ToolOutputRef>,
        engine_elapsed: Option<Duration>,
    ) {
        self.transcript
            .finish_tool(call_id, status, output, engine_elapsed);
        self.prompt.dirty = true;
    }

    pub fn set_context_tokens(&mut self, tokens: u32) {
        self.context_tokens = Some(tokens);
        self.prompt.dirty = true;
    }

    pub fn set_context_window(&mut self, window: u32) {
        self.context_window = Some(window);
        self.prompt.dirty = true;
    }

    pub fn clear_context_tokens(&mut self) {
        self.context_tokens = None;
        self.prompt.dirty = true;
    }

    pub fn context_tokens(&self) -> Option<u32> {
        self.context_tokens
    }

    pub fn set_session_cost(&mut self, usd: f64) {
        self.session_cost_usd = usd;
        self.prompt.dirty = true;
    }

    pub fn set_model_label(&mut self, label: String) {
        self.model_label = Some(label);
        self.prompt.dirty = true;
    }

    pub fn set_task_label(&mut self, label: String) {
        if label.trim().is_empty() {
            self.task_label = None;
        } else {
            self.task_label = Some(label);
        }
        self.prompt.dirty = true;
    }

    pub fn clear_task_label(&mut self) {
        self.task_label = None;
        self.prompt.dirty = true;
    }

    pub fn set_reasoning_effort(&mut self, effort: protocol::ReasoningEffort) {
        self.reasoning_effort = effort;
        self.prompt.dirty = true;
    }

    pub fn set_app_focus(&mut self, focus: crate::app::AppFocus) {
        if self.last_app_focus != focus {
            self.last_app_focus = focus;
            self.prompt.dirty = true;
        }
    }

    /// Number of rows the prompt pane occupied in the last draw. Used by
    /// mouse hit-testing to route clicks to the right pane.
    pub fn prev_prompt_rows(&self) -> u16 {
        self.prompt.prev_rows
    }

    /// Screen region `(top_row, rows, scroll_offset, gutter, usable_width)`
    /// occupied by the input text area in the last frame. Used by mouse
    /// hit-testing for click-to-position on the prompt.
    /// Transcript viewport region recorded during the last paint. Used
    /// by mouse hit-testing and scroll math.
    pub(crate) fn transcript_region(&self) -> Option<super::region::TranscriptRegion> {
        self.last_transcript_region
    }

    pub fn input_region(&self) -> Option<(u16, u16, usize, u16, u16)> {
        self.prompt
            .input_region
            .map(|r| (r.top_row, r.rows, r.scroll_offset, r.gutter, r.usable_width))
    }

    /// Scrollbar geometry for the prompt input area recorded during the
    /// last paint. `None` when the input fits in its viewport.
    pub(crate) fn input_scrollbar(&self) -> Option<super::region::ScrollbarGeom> {
        self.prompt.input_region.and_then(|r| r.scrollbar)
    }

    /// Overwrite the prompt's top-relative input scroll offset. Used by
    /// scrollbar click/drag to jump the input viewport.
    pub(crate) fn set_input_scroll(&mut self, offset: usize) {
        self.prompt.input_scroll = offset;
        self.prompt.dirty = true;
    }

    /// Plain-text rendering of the last-painted viewport rows (top to
    /// bottom). Used by the content pane's vim-style motions and yank.
    pub fn viewport_text_rows(&self) -> &[String] {
        &self.last_viewport_text
    }

    /// Overlay a reverse-video highlight for the given visual selection
    /// on top of the already-painted transcript. Ranges are expressed
    /// in absolute buffer (line_from_top, col) coordinates; the viewport
    /// draws lines top-to-bottom so the mapping is direct.
    fn paint_visual_range(
        &self,
        out: &mut RenderOut,
        viewport_rows: u16,
        width: u16,
        range: &ContentVisualRange,
    ) {
        let rows = &self.last_viewport_text;
        if rows.is_empty() || viewport_rows == 0 {
            return;
        }
        // `range` is viewport-relative: line 0 = top of viewport.
        // `last_viewport_text` indexes top-down.
        use crate::text_utils::cell_to_byte;
        use unicode_width::UnicodeWidthStr;
        for line_idx in range.start_line..=range.end_line.min(rows.len().saturating_sub(1)) {
            if line_idx >= rows.len() || (line_idx as u16) >= viewport_rows {
                break;
            }
            let line = &rows[line_idx];
            // Columns are in display cells (set by `content_visual_range`).
            let line_cells = UnicodeWidthStr::width(line.as_str());
            let (sel_start, sel_end) = match range.kind {
                ContentVisualKind::Char => {
                    let start = if line_idx == range.start_line {
                        range.start_col
                    } else {
                        0
                    };
                    let end = if line_idx == range.end_line {
                        range.end_col.min(line_cells)
                    } else {
                        line_cells
                    };
                    (start, end)
                }
                ContentVisualKind::Line => (0, line_cells),
            };
            if sel_end <= sel_start {
                continue;
            }
            let byte_start = cell_to_byte(line, sel_start);
            let byte_end = cell_to_byte(line, sel_end);
            let sub = &line[byte_start..byte_end];
            // Offset by the transcript window's left gutter so the
            // highlight lands on the content cells, not on the gutter.
            let col0 = (sel_start as u16) + self.transcript_gutters.pad_left;
            out.move_to(col0, line_idx as u16);
            out.push_style(StyleState {
                bg: Some(theme::selection_bg()),
                ..StyleState::default()
            });
            out.print(sub);
            if matches!(range.kind, ContentVisualKind::Line) {
                let used = UnicodeWidthStr::width(sub) as u16;
                let remaining = width.saturating_sub(sel_start as u16 + used);
                for _ in 0..remaining {
                    out.print(" ");
                }
            }
            out.pop_style();
        }
    }

    /// Plain-text rendering of the full transcript (including any
    /// ephemeral streaming content). Used by the content pane as the
    /// vim buffer so motions span the entire conversation, not just the
    /// current viewport slice.
    pub fn full_transcript_text(&mut self, width: usize) -> Vec<String> {
        let _ = width;
        let tw = self.transcript_width() as u16;
        let snap = self.transcript.snapshot(tw, self.show_thinking);
        let mut rows = snap.rows.clone();
        if self.has_ephemeral() {
            let mut col = SpanCollector::new(tw);
            self.render_ephemeral_into(&mut col, tw as usize);
            for line in col.finish().lines {
                let mut s = String::new();
                for span in &line.spans {
                    s.push_str(&span.text);
                }
                rows.push(s);
            }
        }
        rows
    }

    pub fn working_throbber(&self) -> Option<Throbber> {
        self.working.throbber
    }

    pub fn set_throbber(&mut self, state: Throbber) {
        self.working.set_throbber(state);
        self.prompt.dirty = true;
    }

    pub fn record_tokens_per_sec(&mut self, tps: f64) {
        self.working.record_tokens_per_sec(tps);
        self.prompt.dirty = true;
    }

    pub fn turn_meta(&self) -> Option<protocol::TurnMeta> {
        self.working.turn_meta()
    }

    pub fn restore_from_turn_meta(&mut self, meta: &protocol::TurnMeta) {
        self.working.restore_from_turn_meta(meta);
        self.prompt.dirty = true;
    }

    pub fn clear_throbber(&mut self) {
        self.working.clear();
        self.prompt.dirty = true;
    }

    pub fn set_pending_dialog(&mut self, pending: bool) {
        self.pending_dialog = pending;
        self.prompt.dirty = true;
    }

    pub fn set_dialog_open(&mut self, open: bool) {
        if open == self.dialog_open {
            return;
        }
        self.dialog_open = open;
        // Spinner pause/resume is handled by the caller based on
        // whether the dialog blocks the agent — non-blocking dialogs
        // keep the spinner animating.
        self.prompt.dirty = true;
    }

    pub fn set_constrain_dialog(&mut self, constrain: bool) {
        self.constrain_dialog = constrain;
    }

    /// Pause the working spinner. Used when a blocking dialog (confirm,
    /// question) opens and the agent is suspended.
    pub fn pause_spinner(&mut self) {
        self.working.pause();
    }

    /// Resume the working spinner after a blocking dialog closes.
    pub fn resume_spinner(&mut self) {
        self.working.resume();
    }

    pub fn mark_dirty(&mut self) {
        self.prompt.dirty = true;
    }

    /// Override the vim mode shown in the status bar. Called by the
    /// app each frame with the **focused window's** vim mode so the
    /// status bar is contextual — prompt mode when the prompt is
    /// focused, transcript mode when the transcript window is
    /// focused. Without this, the cached value from the last prompt
    /// render is stale relative to the focused window.
    /// Push the focused window's cursor position + scroll into the
    /// status bar. One setter for every buffer — the app computes a
    /// buffer-agnostic `StatusPosition` from the focused window and
    /// the status line renders it right-aligned, so the format is
    /// identical regardless of which window has focus.
    pub fn set_status_position(&mut self, p: Option<StatusPosition>) {
        if self.last_status_position != p {
            self.last_status_position = p;
            self.prompt.dirty = true;
        }
    }

    pub fn set_status_vim(&mut self, enabled: bool, mode: Option<crate::vim::ViMode>) {
        if self.last_vim_enabled != enabled || self.last_vim_mode != mode {
            self.last_vim_enabled = enabled;
            self.last_vim_mode = mode;
            self.prompt.dirty = true;
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.prompt.dirty || self.transcript.history.has_unflushed()
    }

    /// Center the input viewport on the cursor (vim `zz`).
    pub fn center_input_scroll(&mut self) {
        // The actual centering happens in draw_prompt_sections using a
        // sentinel value. We set input_scroll to usize::MAX so the
        // scroll logic knows to center instead of preserving position.
        self.prompt.input_scroll = usize::MAX;
        self.prompt.dirty = true;
    }

    pub fn finish_turn(&mut self) {
        let _perf = crate::perf::begin("render:finish_turn");
        self.transcript.finalize_active_tools();
        self.mark_blocks_dirty();
    }

    pub fn finalize_active_tools(&mut self) {
        self.transcript.finalize_active_tools();
        self.prompt.dirty = true;
    }

    pub fn finalize_active_tools_as(&mut self, status: ToolStatus) {
        self.transcript.finalize_active_tools_as(status);
        self.prompt.dirty = true;
    }

    pub fn tool_state(&self, call_id: &str) -> Option<&ToolState> {
        self.transcript.tool_state(call_id)
    }

    pub fn block_view_state(&self, id: BlockId) -> ViewState {
        self.transcript.block_view_state(id)
    }

    pub fn set_block_view_state(&mut self, id: BlockId, state: ViewState) {
        self.transcript.set_block_view_state(id, state);
        self.prompt.dirty = true;
    }

    pub fn block_status(&self, id: BlockId) -> Status {
        self.transcript.block_status(id)
    }

    pub fn set_block_status(&mut self, id: BlockId, status: Status) {
        self.transcript.set_block_status(id, status);
    }

    pub fn drain_finished_blocks(&mut self) -> Vec<BlockId> {
        self.transcript.drain_finished_blocks()
    }

    /// Push the transcript window's gutter reservation into the paint
    /// path. Called by `App` whenever the transcript window mutates its
    /// gutters so the scrollbar column and content-width math stay
    /// consistent.
    pub fn set_transcript_gutters(&mut self, gutters: crate::window::WindowGutters) {
        if self.transcript_gutters != gutters {
            self.transcript_gutters = gutters;
            self.prompt.dirty = true;
        }
    }

    pub fn rewrite_block(&mut self, id: BlockId, block: Block) {
        self.transcript.rewrite_block(id, block);
        self.prompt.dirty = true;
    }

    pub fn push_streaming(&mut self, block: Block) -> BlockId {
        let id = self.transcript.push_streaming(block);
        self.prompt.dirty = true;
        id
    }

    pub fn streaming_block_ids(&self) -> Vec<BlockId> {
        self.transcript.streaming_block_ids()
    }

    pub fn update_tool_state(
        &mut self,
        call_id: &str,
        mutator: impl FnOnce(&mut ToolState),
    ) -> bool {
        let result = self.transcript.update_tool_state(call_id, mutator);
        if result {
            self.prompt.dirty = true;
        }
        result
    }

    pub fn set_tool_state(&mut self, call_id: String, state: ToolState) {
        self.transcript.set_tool_state(call_id, state);
    }

    /// Whether any content (blocks, active tool, active exec) exists above
    pub fn mark_blocks_dirty(&mut self) {
        // Under the flat-line viewport model, blocks are never flushed
        // to scrollback — the next frame repaints the transcript from
        // scratch. Just mark the screen dirty so the tick loop picks
        // up any newly-pushed blocks.
        if self.defer_pending_render {
            self.defer_pending_render = false;
            return;
        }
        self.prompt.dirty = true;
    }

    /// Mark the prompt as needing a full redraw.  Does NOT perform any
    /// terminal I/O — the next `draw_frame` will clear stale rows and
    /// repaint atomically within a single synchronized-update frame,
    /// preventing the flash that occurred when erasure was flushed as a
    /// separate frame.
    pub fn erase_prompt(&mut self) {
        if self.prompt.drawn {
            self.prompt.drawn = false;
            self.prompt.dirty = true;
        }
    }

    /// Force a full repaint on the next tick. Under the flat-line
    /// viewport model this just clears the current screen and marks
    /// the prompt dirty — the next `draw_viewport_frame` will rebuild
    /// everything from scratch.
    pub fn redraw(&mut self) {
        let _perf = crate::perf::begin("redraw");
        let (w, _) = self.size();
        if w as usize != self.transcript.history.cache_width {
            self.transcript.history.invalidate_for_width(w as usize);
        }
        self.prompt.drawn = false;
        self.prompt.dirty = true;
        self.prompt.prev_rows = 0;
    }

    pub fn clear(&mut self) {
        self.transcript.history.clear();
        self.transcript.clear_active_state();
        self.prompt = PromptState::new();
        self.prompt.anchor_row = Some(0);
        self.working.clear();
        self.context_tokens = None;
        self.session_cost_usd = 0.0;
        self.task_label = None;
        self.has_scrollback = false;
        self.content_start_row = None;
        let mut frame = Frame::begin(&*self.backend);
        let _ = frame.queue(cursor::MoveTo(0, 0));
        let _ = frame.queue(terminal::Clear(terminal::ClearType::All));
        let _ = frame.queue(terminal::Clear(terminal::ClearType::Purge));
    }

    pub fn has_history(&self) -> bool {
        self.transcript.has_history()
    }

    /// Snapshot the per-tool intermediate representations stored on
    /// committed `Block::ToolCall` blocks. The IR is width-independent and
    /// expensive to rebuild (it contains the LCS diff and syntect tokens),
    /// so we persist it alongside the session and reattach on resume.
    /// Returns `None` if no IR has been built yet.
    pub fn export_render_cache(&self) -> Option<RenderCache> {
        let mut cache = RenderCache::new(String::new());
        for id in &self.transcript.history.order {
            if let Some(Block::ToolCall { call_id, .. }) = self.transcript.history.blocks.get(id) {
                if let Some(state) = self.transcript.history.tool_states.get(call_id) {
                    if let Some(out) = state.output.as_deref() {
                        if let Some(ir) = &out.render_cache {
                            cache.insert_tool_output(call_id.clone(), ir.clone());
                        }
                    }
                }
            }
        }
        if cache.tool_outputs.is_empty() {
            None
        } else {
            Some(cache)
        }
    }

    /// Whether the layout cache has changed since the last
    /// `export_layout_cache`. Used by `save_session` to skip writing the
    /// cache file when nothing would change on disk.
    pub fn layout_cache_dirty(&self) -> bool {
        self.transcript.history.cache_dirty
    }

    /// Export a content-addressed snapshot of every cached block artifact
    /// that is safe to persist. Tool blocks whose `ToolState` is not yet
    /// terminal are skipped — their layout captures transient state.
    pub fn export_layout_cache(&mut self) -> Option<PersistedLayoutCache> {
        if self.transcript.history.is_empty() {
            return None;
        }
        let mut cache = PersistedLayoutCache::new(crate::theme::is_light());
        // Walk `order`, re-keying artifacts by content hash so another
        // session (with different monotonic `BlockId`s) can install the
        // same cache.
        for id in &self.transcript.history.order {
            let Some(block) = self.transcript.history.blocks.get(id) else {
                continue;
            };
            let persist = match block {
                Block::ToolCall { call_id, .. } => self
                    .transcript
                    .history
                    .tool_states
                    .get(call_id)
                    .map(|s| s.is_terminal())
                    .unwrap_or(false),
                _ => true,
            };
            if !persist {
                continue;
            }
            let hash = self.transcript.history.content_hash(*id);
            if cache.blocks.contains_key(&hash) {
                continue;
            }
            if let Some(artifact) = self.transcript.history.artifacts.get(id) {
                if !artifact.is_empty() {
                    cache.blocks.insert(hash, artifact.clone());
                }
            }
        }
        self.transcript.history.cache_dirty = false;
        if cache.blocks.is_empty() {
            return None;
        }
        crate::perf::record_value("layout_cache:artifacts", cache.blocks.len() as u64);
        let total_layouts: usize = cache.blocks.values().map(|a| a.layouts.len()).sum();
        crate::perf::record_value("layout_cache:layouts", total_layouts as u64);
        Some(cache)
    }

    /// Install a previously persisted layout cache. Entries for block ids
    /// not currently in history are ignored; missing ids just become cache
    /// misses on the next render. Tool blocks in a non-terminal state
    /// still skip cache adoption so the next render rebuilds their layout.
    pub fn import_layout_cache(&mut self, cache: PersistedLayoutCache) {
        if !cache.is_compatible(crate::theme::is_light()) {
            return;
        }
        let nw = self.size().0;
        // Map cached content hashes onto the first live `BlockId` with
        // matching content. Each hash installs once — duplicates in the
        // current history all reuse the same cached artifact via the
        // shared `content_hash` field in `LayoutKey`.
        let mut by_hash: HashMap<u64, BlockId> = HashMap::new();
        for id in &self.transcript.history.order {
            let hash = self.transcript.history.content_hash(*id);
            by_hash.entry(hash).or_insert(*id);
        }
        for (hash, mut artifact) in cache.blocks {
            let Some(id) = by_hash.get(&hash).copied() else {
                continue;
            };
            let Some(block) = self.transcript.history.blocks.get(&id) else {
                continue;
            };
            let allow = match block {
                Block::ToolCall { call_id, .. } => self
                    .transcript
                    .history
                    .tool_states
                    .get(call_id)
                    .map(|s| s.is_terminal())
                    .unwrap_or(false),
                _ => true,
            };
            if !allow {
                continue;
            }
            artifact
                .layouts
                .retain(|(k, b)| k.width == nw || b.is_valid_at(nw));
            if artifact.is_empty() {
                continue;
            }
            self.transcript
                .history
                .artifacts
                .entry(id)
                .and_modify(|a| {
                    for (k, b) in &artifact.layouts {
                        a.insert(*k, b.clone());
                    }
                })
                .or_insert(artifact);
        }
        self.transcript.history.cache_width = nw as usize;
        self.transcript.history.cache_dirty = false;
    }

    pub fn user_turns(&self) -> Vec<(usize, String)> {
        self.transcript.user_turns()
    }

    pub fn truncate_to(&mut self, block_idx: usize) {
        self.transcript.truncate_to(block_idx);
        self.redraw();
    }

    pub fn draw_prompt(&mut self, state: &InputState, mode: protocol::Mode, width: usize) {
        let mut frame = Frame::begin(&*self.backend);
        self.draw_viewport_frame(
            &mut frame,
            width,
            FramePrompt {
                state,
                mode,
                queued: &[],
                prediction: None,
            },
            0,
            0,
            0,
            None,
        );
    }

    /// Update spinner animation state. Call before rendering.
    pub fn update_spinner(&mut self) {
        if let Some(elapsed) = self.working.elapsed() {
            let frame = (elapsed.as_millis() / 150) as usize % SPINNER_FRAMES.len();
            if frame != self.working.last_spinner_frame {
                self.working.last_spinner_frame = frame;
                self.prompt.dirty = true;
            }
        }
        // Refresh live elapsed on any streaming agent blocks so their
        // duration ticks up without needing an explicit engine event.
        self.transcript.tick_active_agents();
    }

    /// Returns true when there is content or prompt work to render.
    pub fn needs_draw(&self, is_dialog: bool) -> bool {
        let has_new_blocks = self.transcript.history.has_unflushed();
        if is_dialog {
            has_new_blocks || (self.has_ephemeral() && self.prompt.dirty)
        } else {
            has_new_blocks || self.prompt.dirty
        }
    }

    /// Whether the animated thinking-summary overlay is active. All
    /// other streams (text, tables, code lines, tools, agents, exec)
    /// flow through streaming blocks in the main paint path; only the
    /// aggregate thinking summary (shown when `show_thinking == false`)
    /// remains as an overlay because it's a synthesized summary, not a
    /// stream.
    fn has_ephemeral(&self) -> bool {
        self.transcript.active_thinking.is_some() && !self.show_thinking
    }

    /// Paint the animated thinking-summary above the prompt when
    /// thinking is hidden. Every other live element renders as a
    /// streaming block in the main transcript.
    fn render_ephemeral_into<S: LayoutSink>(&self, out: &mut S, width: usize) {
        let Some(ref at) = self.transcript.active_thinking else {
            return;
        };
        if self.show_thinking {
            return;
        }
        let mut combined = collect_trailing_thinking(
            self.transcript
                .history
                .order
                .iter()
                .map(|id| &self.transcript.history.blocks[id]),
        );
        if !at.paragraph.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&at.paragraph);
        }
        if !at.current_line.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&at.current_line);
        }
        if !combined.is_empty() {
            let (label, line_count) = thinking_summary(&combined);
            emit_newlines(out, self.thinking_summary_gap());
            render_thinking_summary(out, width, &label, line_count, true);
        }
    }

    /// Flat-line viewport draw path.
    ///
    /// Repaints the entire screen every frame: a top region that holds
    /// the transcript (history + any active streaming content) and the
    /// prompt stack at the bottom. `scroll_offset` (in rows) shifts the
    /// transcript slice upward (0 = stuck to bottom).
    ///
    /// Returns the clamped scroll offset (so the caller can normalize
    /// its `history_scroll_offset` back to a valid range).
    #[allow(clippy::too_many_arguments)]
    pub fn draw_viewport_frame(
        &mut self,
        out: &mut RenderOut,
        width: usize,
        prompt: FramePrompt<'_>,
        scroll_offset: u16,
        history_cursor_line: u16,
        history_cursor_col: u16,
        visual_range: Option<ContentVisualRange>,
    ) -> (u16, u16, u16) {
        let _perf = crate::perf::begin("render:viewport_frame");
        self.update_spinner();

        let (term_w, term_h) = self.size();
        out.init_cursor(0, term_w, term_h);

        out.row = Some(0);
        out.move_to(0, 0);

        let natural_prompt_height =
            self.measure_prompt_height(prompt.state, width, prompt.queued, prompt.prediction);
        self.layout = super::layout::LayoutState::compute(&super::layout::LayoutInput {
            term_width: term_w,
            term_height: term_h,
            prompt_height: natural_prompt_height,
            dialog_height: None,
            constrain_dialog: false,
        });
        let viewport_rows = self.layout.viewport_rows();
        let prompt_height = self.layout.prompt.height;
        let gap_rows = self.layout.gap;

        // Reserve gutter columns (padding + scrollbar track) so the bar
        // never overpaints transcript content. Layout width must match
        // across total_rows / paint / viewport_text or mouse hit-testing
        // drifts relative to what's actually drawn. The transcript
        // window's `gutters` carry the authoritative reservation.
        let gutters = self.transcript_gutters;
        let tw = (gutters.content_width(width as u16) as usize).max(1);

        // Build ephemeral tail (streaming overlays) as a flat DisplayBlock.
        let ephemeral_lines: Vec<crate::render::display::DisplayLine> = if self.has_ephemeral() {
            let mut col = SpanCollector::new(tw as u16);
            self.render_ephemeral_into(&mut col, tw);
            col.finish().lines
        } else {
            Vec::new()
        };

        // Compute total transcript rows for scrollbar + viewport geometry.
        // The snapshot is cached; accessing it here ensures layout is warm
        // before paint_viewport runs.
        let snapshot_total = self
            .transcript
            .snapshot(tw as u16, self.show_thinking)
            .rows
            .len() as u16;
        let total_transcript_rows = snapshot_total.saturating_add(ephemeral_lines.len() as u16);

        // Paint transcript slice (history + ephemeral tail) into the
        // viewport. We always repaint — keeping the viewport visually
        // stable during selection is handled by the caller pinning
        // `scroll_offset`, not by skipping the paint.
        let clamped = self.transcript.history.paint_viewport(
            out,
            tw,
            self.show_thinking,
            0,
            viewport_rows,
            scroll_offset,
            &ephemeral_lines,
            gutters.pad_left,
        );

        // Scrollbar on the rightmost column, matching the prompt.
        // Visible whenever content overflows the viewport so the user
        // has a predictable target to click / drag.
        let geom =
            super::viewport::ViewportGeom::new(total_transcript_rows, viewport_rows, clamped);
        let scrollbar_col = match self.transcript_gutters.scrollbar {
            Some(crate::window::GutterSide::Left) => 0,
            _ => (width as u16).saturating_sub(1),
        };
        let scrollbar_geom: Option<super::region::ScrollbarGeom> = if geom.max_scroll() > 0 {
            let max_scroll = geom.max_scroll() as usize;
            let inverted = max_scroll.saturating_sub(clamped as usize);
            let scrollbar = super::scrollbar::Scrollbar::new(
                total_transcript_rows as usize,
                viewport_rows as usize,
                inverted,
            );
            super::scrollbar::paint_column(out, scrollbar_col, 0, viewport_rows, &scrollbar);
            Some(super::region::ScrollbarGeom {
                col: scrollbar_col,
                top_row: 0,
                rows: viewport_rows,
                total_rows: total_transcript_rows,
            })
        } else {
            None
        };
        self.last_transcript_region = Some(super::region::TranscriptRegion {
            top_row: 0,
            rows: viewport_rows,
            content_width: tw as u16,
            scrollbar: scrollbar_geom,
            total_rows: total_transcript_rows,
            scroll_offset: clamped,
        });
        // Record plain text for the content pane's motion handlers.
        {
            let snap = self.transcript.snapshot(tw as u16, self.show_thinking);
            let mut vt = snap.viewport_rows(viewport_rows, clamped);
            // Append ephemeral lines (hidden-thinking summary) — they
            // participate in the viewport text for vim motions but are
            // not part of the cached snapshot.
            if !ephemeral_lines.is_empty() {
                for line in &ephemeral_lines {
                    let mut s = String::new();
                    for span in &line.spans {
                        s.push_str(&span.text);
                    }
                    vt.push(s);
                }
                vt.truncate(viewport_rows as usize);
            }
            self.last_viewport_text = vt;
        }

        // Overlay visual selection highlighting before drawing the cursor.
        if let Some(range) = visual_range {
            self.paint_visual_range(out, viewport_rows, tw as u16, &range);
        }

        // When the content pane has focus, paint a block cursor at
        // (col, row) within the viewport using the same colors as the
        // prompt's soft cursor. `history_cursor_line` is the
        // bottom-relative row (0 = bottom row of the viewport); the
        // painter writes in top-down coords.
        let (clamped_cursor_line, clamped_cursor_col) =
            if self.last_app_focus == crate::app::AppFocus::Content && viewport_rows > 0 {
                let max_line = viewport_rows.saturating_sub(1);
                let line = history_cursor_line.min(max_line);
                let max_col = (tw as u16).saturating_sub(1);
                let col = history_cursor_col.min(max_col);
                let cursor_row = viewport_rows.saturating_sub(1 + line);
                let under: String = self
                    .last_viewport_text
                    .get(cursor_row as usize)
                    .map(|row| {
                        let byte = crate::text_utils::cell_to_byte(row, col as usize);
                        row[byte..].chars().next()
                    })
                    .and_then(|c| c)
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| " ".to_string());
                // Offset by the left gutter so the cursor lands on the
                // glyph inside the content rect (viewport_text holds pure
                // content; the gutter is unstyled padding painted at
                // col 0..pad_left by `paint_viewport`).
                draw_soft_cursor(out, col + gutters.pad_left, cursor_row, &under);
                (line, col)
            } else {
                (history_cursor_line, history_cursor_col)
            };

        // Paint prompt stack at the bottom, leaving the gap row blank.
        // Reset any lingering styling from the transcript paint above so
        // the prompt starts from a clean default state.
        out.reset_style();
        // Explicitly blank the gap row so stale residue from previous
        // frames doesn't leak through.
        if gap_rows > 0 {
            out.move_to(0, viewport_rows);
            let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
        }
        let prompt_top = viewport_rows + gap_rows;
        out.row = Some(prompt_top);
        out.move_to(0, prompt_top);
        self.draw_prompt_sections(
            out,
            prompt.state,
            prompt.mode,
            width,
            prompt.queued,
            prompt.prediction,
            prompt_height as usize,
        );

        // State so other paths don't think they need to repaint.
        self.prompt.drawn = true;
        self.prompt.dirty = false;
        self.prompt.prev_rows = prompt_height;
        self.prompt.anchor_row = Some(prompt_top);
        self.prompt.prev_dialog_row = Some(prompt_top);
        self.prompt.prev_prompt_ui_rows = prompt_height;
        self.content_start_row = Some(0);
        self.has_scrollback = false;
        // Fully flushed — every frame re-renders everything.
        self.transcript.history.flushed = self.transcript.history.order.len();

        (clamped, clamped_cursor_line, clamped_cursor_col)
    }

    /// Alt-buffer dialog frame: paints transcript into the top region
    /// and returns a `DialogPlacement` for where the caller should draw
    /// the dialog. Reserves `dialog_height + 1 gap + 1 status` rows at
    /// the bottom. No scrollback commits; full repaint every frame.
    pub fn draw_viewport_dialog_frame(
        &mut self,
        out: &mut RenderOut,
        width: usize,
        dialog_height: u16,
    ) -> (bool, Option<DialogPlacement>) {
        let _perf = crate::perf::begin("render:viewport_dialog_frame");
        self.update_spinner();

        let raw_dialog_height = dialog_height;
        let (term_w, term_h) = self.size();
        out.init_cursor(0, term_w, term_h);

        let has_new_blocks = self.transcript.history.has_unflushed();
        let has_ephemeral = self.has_ephemeral();
        let dialog_height_changed = dialog_height != self.prompt.prev_dialog_height;

        // Fast path: nothing changed, return cached placement.
        if self.prompt.drawn
            && !has_new_blocks
            && !dialog_height_changed
            && !self.prompt.dirty
            && !has_ephemeral
        {
            let placement = self.prompt.prev_dialog_row.map(|row| {
                let max_avail = term_h.saturating_sub(2 + row);
                DialogPlacement {
                    row,
                    granted_rows: dialog_height.min(max_avail),
                }
            });
            return (false, placement);
        }

        self.layout = super::layout::LayoutState::compute(&super::layout::LayoutInput {
            term_width: term_w,
            term_height: term_h,
            prompt_height: 0,
            dialog_height: Some(dialog_height),
            constrain_dialog: self.constrain_dialog,
        });
        let viewport_rows = self.layout.viewport_rows();

        out.row = Some(0);
        out.move_to(0, 0);

        let gutters = self.transcript_gutters;
        let tw = (gutters.content_width(width as u16) as usize).max(1);

        // Build ephemeral tail.
        let ephemeral_lines: Vec<crate::render::display::DisplayLine> = if has_ephemeral {
            let mut col = SpanCollector::new(tw as u16);
            self.render_ephemeral_into(&mut col, tw);
            col.finish().lines
        } else {
            Vec::new()
        };

        let snapshot_total = self
            .transcript
            .snapshot(tw as u16, self.show_thinking)
            .rows
            .len() as u16;
        let total_transcript_rows = snapshot_total.saturating_add(ephemeral_lines.len() as u16);

        let clamped = self.transcript.history.paint_viewport(
            out,
            tw,
            self.show_thinking,
            0,
            viewport_rows,
            0,
            &ephemeral_lines,
            gutters.pad_left,
        );

        // Scrollbar on the rightmost column over the transcript region.
        let scrollbar_col = match self.transcript_gutters.scrollbar {
            Some(crate::window::GutterSide::Left) => 0,
            _ => (width as u16).saturating_sub(1),
        };
        let dialog_geom =
            super::viewport::ViewportGeom::new(total_transcript_rows, viewport_rows, clamped);
        let scrollbar_geom: Option<super::region::ScrollbarGeom> = if dialog_geom.max_scroll() > 0 {
            let max_scroll = dialog_geom.max_scroll() as usize;
            let inverted = max_scroll.saturating_sub(clamped as usize);
            let scrollbar = super::scrollbar::Scrollbar::new(
                total_transcript_rows as usize,
                viewport_rows as usize,
                inverted,
            );
            super::scrollbar::paint_column(out, scrollbar_col, 0, viewport_rows, &scrollbar);
            Some(super::region::ScrollbarGeom {
                col: scrollbar_col,
                top_row: 0,
                rows: viewport_rows,
                total_rows: total_transcript_rows,
            })
        } else {
            None
        };
        self.last_transcript_region = Some(super::region::TranscriptRegion {
            top_row: 0,
            rows: viewport_rows,
            content_width: tw as u16,
            scrollbar: scrollbar_geom,
            total_rows: total_transcript_rows,
            scroll_offset: clamped,
        });
        {
            let snap = self.transcript.snapshot(tw as u16, self.show_thinking);
            let mut vt = snap.viewport_rows(viewport_rows, clamped);
            if !ephemeral_lines.is_empty() {
                for line in &ephemeral_lines {
                    let mut s = String::new();
                    for span in &line.spans {
                        s.push_str(&span.text);
                    }
                    vt.push(s);
                }
                vt.truncate(viewport_rows as usize);
            }
            self.last_viewport_text = vt;
        }

        // Clear the band below the transcript down to (but excluding)
        // the status bar so stale prompt/dialog residue from prior
        // frames can't leak through. Status bar is painted by the
        // caller via `queue_status_line` at `term_h - 1`.
        out.reset_style();
        if viewport_rows < term_h {
            out.move_to(0, viewport_rows);
            for row in viewport_rows..term_h {
                out.move_to(0, row);
                let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
            }
        }

        let dialog_row = viewport_rows;
        self.prompt.drawn = true;
        self.prompt.dirty = false;
        self.prompt.anchor_row = Some(0);
        self.prompt.prev_dialog_row = Some(dialog_row);
        self.prompt.prev_dialog_height = raw_dialog_height;
        self.prompt.prev_dialog_gap = 0;
        self.prompt.prev_rows = 0;
        self.prompt.prev_prompt_ui_rows = 0;
        self.content_start_row = Some(0);
        self.has_scrollback = false;
        self.transcript.history.flushed = self.transcript.history.order.len();

        let placement = self.layout.dialog.as_ref().map(|d| DialogPlacement {
            row: d.rect.top,
            granted_rows: d.rect.height,
        });

        (true, placement)
    }

    /// Measure prompt height without painting. Used by `draw_frame` to
    /// compute ScrollUp before entering overlay mode.
    fn measure_prompt_height(
        &self,
        state: &InputState,
        width: usize,
        queued: &[String],
        prediction: Option<&str>,
    ) -> u16 {
        let usable = width.saturating_sub(2);
        let text_w = usable.saturating_sub(2).max(1);

        // Extra rows: notification + queued + stash + btw.
        let notification: u16 = if self.notification.is_some() { 1 } else { 0 };
        let stash: u16 = if state.stash.is_some() { 1 } else { 0 };

        let mut queued_rows = 0u16;
        for msg in queued {
            for line in queued_logical_lines(msg) {
                let chars = line.chars().count();
                queued_rows += if chars == 0 {
                    1
                } else {
                    chars.div_ceil(text_w) as u16
                };
            }
        }

        let btw_rows: u16 = if let Some(ref btw) = self.btw {
            let term_h = self.size().1 as usize;
            let max_lines = btw_max_body_rows(term_h).max(1);
            let body = match btw.response {
                Some(_) => {
                    let visible = btw.wrapped.len().min(max_lines) as u16;
                    visible + 2 // body lines + blank + hint
                }
                None => 1, // spinner
            };
            1 + body + 1 // header + body + separator
        } else {
            0
        };

        // Input rows.
        let show_prediction = prediction.is_some() && state.buf.is_empty();
        let input_rows: u16 = if show_prediction {
            1
        } else {
            let (visual_lines, _, _, _) = wrap_and_locate_cursor(&state.buf, &[], 0, usable);
            visual_lines.len() as u16
        };

        // Completions / status.
        let menu_rows = state.menu_rows();
        let comp_rows: u16 = if menu_rows > 0 {
            menu_rows as u16
        } else {
            completion_reserved_rows(state.completer.as_ref()) as u16
        };
        let status_rows: u16 = if comp_rows == 0 { 1 } else { 0 };

        notification
            + queued_rows
            + stash
            + btw_rows
            + 1 // top bar
            + input_rows
            + 1 // bottom bar
            + status_rows
            + comp_rows
    }

    /// Render the prompt section. `out.row` MUST be set (overlay mode)
    /// so all line advances use MoveTo. Returns the total rows painted.
    #[allow(clippy::too_many_arguments)]
    fn draw_prompt_sections(
        &mut self,
        out: &mut RenderOut,
        state: &InputState,
        mode: protocol::Mode,
        width: usize,
        queued: &[String],
        prediction: Option<&str>,
        height: usize,
    ) -> u16 {
        let _perf = crate::perf::begin("render:prompt");
        // Note: `last_vim_enabled` and `last_vim_mode` are now set by
        // `App::tick_prompt` via `set_status_vim(...)` so the status
        // bar reflects the *focused* window's vim mode, not whatever
        // the prompt happened to be in. Only `last_mode` is cached
        // here since it's the agent mode, not the vim mode.
        self.last_mode = mode;
        self.prompt.soft_cursor = None;
        let usable = width.saturating_sub(2);
        // Neutralize any styling carried over from the preceding
        // history/overlay paint in this same frame before the prompt
        // sections start printing plain text.
        out.reset_style();
        let notification_rows = render_notification(out, self.notification.as_ref(), usable);
        let queued_visual = render_queued(out, queued, usable);
        let queued_rows = queued_visual as usize;
        let stash_rows = render_stash(out, &state.stash, usable);
        let term_h = self.size().1 as usize;
        let btw_visual = if let Some(ref mut btw) = self.btw {
            let max_btw = btw_max_body_rows(term_h);
            render_btw(out, btw, usable, max_btw, state.vim_enabled()) as usize
        } else {
            0
        };
        let bar_color = theme::bar();

        // Build all bar spans with priorities. draw_bar drops highest
        // priority first until everything fits.
        // Priorities: 0 = always, 1 = context tokens, 2 = model, 3 = tok/s
        let mut right_spans = Vec::new();
        if let Some(ref model) = self.model_label {
            right_spans.push(BarSpan {
                text: format!(" {}", model),
                color: theme::muted(),
                bg: None,
                bold: false,
                dim: false,
                priority: 2,
            });
            if self.reasoning_effort != protocol::ReasoningEffort::Off {
                let effort = self.reasoning_effort;
                right_spans.push(BarSpan {
                    text: format!(" {}", effort.label()),
                    color: reasoning_color(effort),
                    bg: None,
                    bold: false,
                    dim: false,
                    priority: 2,
                });
            }
        }
        if self.show_tokens {
            if let Some(tokens) = self.context_tokens {
                if !right_spans.is_empty() {
                    right_spans.push(BarSpan {
                        text: " ·".into(),
                        color: bar_color,
                        bg: None,
                        bold: false,
                        dim: false,
                        priority: 2,
                    });
                }
                let token_text = if let Some(window) = self.context_window {
                    if window > 0 {
                        let pct = (tokens as f64 / window as f64 * 100.0) as u32;
                        format!(" {} ({}%)", format_tokens(tokens), pct)
                    } else {
                        format!(" {}", format_tokens(tokens))
                    }
                } else {
                    format!(" {}", format_tokens(tokens))
                };
                right_spans.push(BarSpan {
                    text: token_text,
                    color: theme::muted(),
                    bg: None,
                    bold: false,
                    dim: false,
                    priority: 1,
                });
            }
        }
        if self.show_cost && self.session_cost_usd > 0.0 {
            if !right_spans.is_empty() {
                right_spans.push(BarSpan {
                    text: " ·".into(),
                    color: bar_color,
                    bg: None,
                    bold: false,
                    dim: false,
                    priority: 2,
                });
            }
            right_spans.push(BarSpan {
                text: format!(" {}", crate::metrics::format_cost(self.session_cost_usd)),
                color: theme::muted(),
                bg: None,
                bold: false,
                dim: false,
                priority: 1,
            });
        }
        draw_bar(
            out,
            width,
            None,
            if right_spans.is_empty() {
                None
            } else {
                Some(&right_spans)
            },
            bar_color,
        );
        out.newline();

        let spans = build_display_spans(&state.buf, &state.attachment_ids, &state.store);
        let display_buf = spans_to_string(&spans);
        let char_kinds = build_char_kinds(&spans);
        let display_cursor = map_cursor(state.cursor_char(), &state.buf, &spans);
        // Map selection range from raw byte offsets to display character offsets.
        let display_selection = state.selection_range().map(|(start, end)| {
            let raw_start_char = crate::input::char_pos(&state.buf, start);
            let raw_end_char = crate::input::char_pos(&state.buf, end);
            let ds = map_cursor(raw_start_char, &state.buf, &spans);
            let de = map_cursor(raw_end_char, &state.buf, &spans);
            (ds, de)
        });
        let (visual_lines, cursor_line, _, cursor_char_in_line) =
            wrap_and_locate_cursor(&display_buf, &char_kinds, display_cursor, usable);
        let cmd_hint =
            crate::completer::Completer::command_hint(&state.buf, &state.command_arg_sources);
        let has_arg_space = cmd_hint.is_some()
            && state.buf.len() > cmd_hint.as_ref().unwrap().0.len()
            && state.buf.as_bytes()[cmd_hint.as_ref().unwrap().0.len()] == b' ';
        let is_command =
            cmd_hint.is_some() || crate::completer::Completer::is_command(state.buf.trim());
        let is_exec = matches!(state.buf.as_bytes(), [b'!', c, ..] if !c.is_ascii_whitespace());
        let is_exec_invalid = state.buf == "!";
        let total_content_rows = visual_lines.len();
        let menu_rows = state.menu_rows();
        let comp_total = if menu_rows > 0 {
            menu_rows
        } else {
            completion_reserved_rows(state.completer.as_ref())
        };
        let mut comp_rows = comp_total;

        // Reserve space for the status line (always shown when no completions/menus).
        let status_line_reserve: usize = if comp_total == 0 { 1 } else { 0 };

        // 2 = top bar (above input) + bottom bar (below input).
        const PROMPT_BARS: usize = 2;
        let fixed_base = notification_rows as usize
            + stash_rows as usize
            + queued_rows
            + btw_visual
            + PROMPT_BARS
            + status_line_reserve;
        let mut fixed = fixed_base + comp_rows;
        let mut max_content_rows = height.saturating_sub(fixed);
        if max_content_rows == 0 {
            let available_for_comp = height.saturating_sub(fixed_base + 1);
            if available_for_comp == 0 {
                comp_rows = 0;
            } else {
                comp_rows = comp_rows.min(available_for_comp);
            }
            fixed = fixed_base + comp_rows;
            max_content_rows = height.saturating_sub(fixed);
            if max_content_rows == 0 {
                max_content_rows = 1;
            }
        }

        let content_rows = total_content_rows.min(max_content_rows);
        let scroll_offset = if total_content_rows > content_rows {
            // Vim-style viewport: persist scroll across frames, only adjust
            // when the cursor moves outside the visible range.
            let mut off = self.prompt.input_scroll;
            // Sentinel: center viewport on cursor (zz).
            if off == usize::MAX {
                off = cursor_line.saturating_sub(content_rows / 2);
            }
            // Cursor below viewport → scroll down just enough.
            if cursor_line >= off + content_rows {
                off = cursor_line + 1 - content_rows;
            }
            // Cursor above viewport → scroll up just enough.
            if cursor_line < off {
                off = cursor_line;
            }
            // Clamp to valid range.
            let max_off = total_content_rows.saturating_sub(content_rows);
            off = off.min(max_off);
            self.prompt.input_scroll = off;
            off
        } else {
            self.prompt.input_scroll = 0;
            0
        };
        let show_prediction = prediction.is_some() && state.buf.is_empty();
        if show_prediction {
            let pred = prediction.unwrap();
            let first_line = pred.lines().next().unwrap_or(pred);
            let cursor_row = out.row.unwrap_or(0);
            let prompt_focused =
                self.focused && self.last_app_focus == crate::app::AppFocus::Prompt;
            // Match the one-space gutter used for normal input lines so the
            // prediction aligns with where real characters would be typed.
            out.print(" ");
            let max_chars = usable.saturating_sub(1);
            let mut chars = first_line.chars().take(max_chars);
            if let Some(first) = chars.next() {
                if prompt_focused {
                    // Only the focused window draws a block cursor —
                    // otherwise the transcript window's cursor + this
                    // one render as two simultaneous cursors.
                    let (fg, bg) = cursor_colors();
                    out.set_fg(fg);
                    out.set_bg(bg);
                    out.print(&first.to_string());
                    out.reset_style();
                } else {
                    out.push_dim();
                    out.print(&first.to_string());
                    out.pop_style();
                }
                out.push_dim();
                let rest: String = chars.collect();
                out.print(&rest);
                out.pop_style();
            } else if prompt_focused {
                draw_soft_cursor(out, 1, cursor_row, " ");
            }
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            out.newline();
        }

        // Compute cumulative display-char offset for each visual line.
        // Must match the counting logic in wrap_and_locate_cursor: each
        // visual line contributes its char count, and each '\n' in the
        // display buffer contributes 1 additional char between logical lines.
        let line_char_offsets = compute_visual_line_offsets(&display_buf, &visual_lines);

        let scrollbar =
            super::scrollbar::Scrollbar::new(total_content_rows, content_rows, scroll_offset);

        let input_top_row = out.cursor_row;
        let painted_input_rows = if show_prediction {
            0
        } else {
            visual_lines
                .iter()
                .skip(scroll_offset)
                .take(content_rows)
                .count() as u16
        };
        // Mirror the transcript scrollbar geometry so prompt scrollbar
        // clicks/drags use the same `ScrollbarGeom::scroll_offset_for_row`
        // implementation. The prompt stores `scroll_offset` top-relative
        // (unlike the transcript's bottom-relative offset); the geometry
        // record captures only paint coordinates, leaving the interpretation
        // to the event handler.
        let bar_col = (width as u16).saturating_sub(1);
        let input_scrollbar = if total_content_rows > content_rows && painted_input_rows > 0 {
            Some(super::region::ScrollbarGeom {
                col: bar_col,
                top_row: input_top_row,
                rows: painted_input_rows,
                total_rows: total_content_rows as u16,
            })
        } else {
            None
        };
        self.prompt.input_region = Some(super::prompt::InputRegion {
            top_row: input_top_row,
            rows: painted_input_rows,
            scroll_offset,
            gutter: 1,
            usable_width: usable as u16,
            scrollbar: input_scrollbar,
        });
        for (li, (line, kinds)) in visual_lines
            .iter()
            .skip(scroll_offset)
            .take(if show_prediction { 0 } else { content_rows })
            .enumerate()
        {
            let abs_idx = scroll_offset + li;
            // Compute per-line selection range (in char offsets within this line).
            let line_sel = display_selection.and_then(|(sel_start, sel_end)| {
                let line_start = line_char_offsets[abs_idx];
                let line_len = line.chars().count();
                let line_end = line_start + line_len;
                if line_len == 0 && sel_start <= line_start && sel_end > line_start {
                    // Empty line within selection — highlight a phantom space.
                    Some((0, 1))
                } else if sel_end <= line_start || sel_start >= line_end {
                    None
                } else {
                    let s = sel_start.saturating_sub(line_start);
                    let e = sel_end.min(line_end) - line_start;
                    Some((s, e))
                }
            });
            let line_cursor = if abs_idx == cursor_line
                && self.focused
                && self.last_app_focus == crate::app::AppFocus::Prompt
            {
                Some(cursor_char_in_line)
            } else {
                None
            };
            out.print(" ");
            if has_arg_space && abs_idx == 0 {
                // Command prefix in accent, argument text in normal style.
                let (prefix, hint) = cmd_hint.as_ref().unwrap();
                let prefix_len = prefix.chars().count();
                let line_chars = line.chars().count();
                // Build kinds: accent for the prefix chars, plain for the rest.
                let mut cmd_kinds = vec![SpanKind::AtRef; prefix_len.min(line_chars)];
                cmd_kinds.resize(line_chars, SpanKind::Plain);
                render_styled_chars(out, line, &cmd_kinds, line_sel, line_cursor);
                // Show hint only when exactly one trailing space follows /cmd —
                // additional spaces mean the user started typing, so the hint
                // should disappear rather than shift right.
                if line_chars >= prefix_len && state.buf == format!("{prefix} ") {
                    let max = usable.saturating_sub(prefix_len + 2);
                    let truncated: String = if hint.chars().count() > max {
                        let mut s: String = hint.chars().take(max.saturating_sub(1)).collect();
                        s.push('…');
                        s
                    } else {
                        hint.clone()
                    };
                    out.push_dim();
                    out.print(&truncated);
                    out.pop_style();
                }
            } else if has_arg_space {
                render_styled_chars(out, line, kinds, line_sel, line_cursor);
            } else if is_command {
                // All chars are accent-colored; reuse AtRef kind for accent rendering.
                let accent_kinds = vec![SpanKind::AtRef; line.chars().count()];
                render_styled_chars(out, line, &accent_kinds, line_sel, line_cursor);
            } else if (is_exec || is_exec_invalid) && abs_idx == 0 && line.starts_with('!') {
                // Render the `!` prefix with its own style (possibly selected).
                let bang_cursor = line_cursor == Some(0);
                let bang_selected = line_sel.is_some_and(|(s, _)| s == 0);
                if bang_cursor {
                    let (fg, bg) = cursor_colors();
                    out.set_fg(fg);
                    out.set_bg(bg);
                    out.print("!");
                    out.reset_style();
                } else {
                    out.push_style(StyleState {
                        fg: Some(Color::Red),
                        bg: if bang_selected {
                            Some(theme::selection_bg())
                        } else {
                            None
                        },
                        bold: true,
                        ..StyleState::default()
                    });
                    out.print("!");
                    out.pop_style();
                }
                // Shift selection range by 1 for the remaining text.
                let rest_sel = line_sel.and_then(|(s, e)| {
                    let s2 = if s == 0 { 0 } else { s - 1 };
                    let e2 = e.saturating_sub(1);
                    if s2 < e2 {
                        Some((s2, e2))
                    } else {
                        None
                    }
                });
                let rest_cursor = line_cursor.and_then(|c| c.checked_sub(1));
                render_styled_chars(out, &line[1..], &kinds[1..], rest_sel, rest_cursor);
            } else {
                render_styled_chars(out, line, kinds, line_sel, line_cursor);
            }
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            if line_cursor.is_some() {
                self.prompt.soft_cursor = Some((1 + cursor_char_in_line as u16, out.cursor_row));
            }
            if scrollbar.visible {
                let bg = if scrollbar.is_thumb(li) {
                    theme::scrollbar_thumb()
                } else {
                    theme::scrollbar_track()
                };
                let _ = out.queue(cursor::MoveToColumn((width as u16).saturating_sub(1)));
                out.push_bg(bg);
                out.print(" ");
                out.pop_style();
            }
            out.newline();
        }

        draw_bar(out, width, None, None, bar_color);

        // Status line below the prompt:
        // pill(spinner+slug) mode vim_mode · status time · speed · procs · agents
        let status_line_rows = if comp_rows == 0 {
            out.newline();
            self.render_status_line(out);
            1
        } else {
            0
        };

        if comp_rows > 0 {
            out.newline();
        }
        let comp_rows = if let Some(ref ms) = state.menu {
            draw_menu(out, ms, comp_rows)
        } else {
            draw_completions(
                out,
                state.completer.as_ref(),
                comp_rows,
                state.vim_enabled(),
            )
        };

        // Mirror of `fixed_base`'s structure: extras, top bar, input
        // content, bottom bar, status line / completions.
        (notification_rows as usize
            + stash_rows as usize
            + queued_rows
            + btw_visual
            + 1 // top bar (above input)
            + content_rows
            + 1 // bottom bar (below input)
            + status_line_rows
            + comp_rows) as u16
    }
}

fn render_notification(
    out: &mut RenderOut,
    notification: Option<&Notification>,
    usable: usize,
) -> u16 {
    let Some(notification) = notification else {
        return 0;
    };

    let label = if notification.is_error {
        "error"
    } else {
        "info"
    };
    let max_msg = usable.saturating_sub(label.len() + 3);

    out.print(" ");
    out.push_style(StyleState {
        fg: if notification.is_error {
            Some(theme::ERROR)
        } else {
            None
        },
        bold: true,
        ..StyleState::default()
    });
    out.print(label);
    out.pop_style();
    out.print("  ");

    let msg: String = notification.message.chars().take(max_msg).collect();
    out.push_dim();
    out.print(&msg);
    out.pop_style();
    out.newline();
    1
}

fn render_stash(out: &mut RenderOut, stash: &Option<InputSnapshot>, usable: usize) -> u16 {
    let Some(_) = stash else {
        return 0;
    };
    let text = "› Stashed (ctrl+s to unstash)";
    let display: String = text.chars().take(usable).collect();
    out.print("  ");
    out.push_style(StyleState {
        fg: Some(theme::muted()),
        dim: true,
        ..StyleState::default()
    });
    out.print(&display);
    out.pop_style();
    out.newline();
    1
}

/// Mirror Block::User's line preprocessing for a queued message:
/// expand tabs, strip leading/trailing blank lines, trim trailing
/// whitespace on each remaining line.
fn queued_logical_lines(msg: &str) -> Vec<String> {
    let all_lines: Vec<String> = msg.lines().map(|l| l.replace('\t', "    ")).collect();
    let start = all_lines.iter().position(|l| !l.is_empty()).unwrap_or(0);
    let end = all_lines
        .iter()
        .rposition(|l| !l.is_empty())
        .map_or(0, |i| i + 1);
    all_lines[start..end]
        .iter()
        .map(|l| l.trim_end().to_string())
        .collect()
}

fn render_queued(out: &mut RenderOut, queued: &[String], usable: usize) -> u16 {
    // Mirrors Block::User rendering (blocks.rs) with a 1-char indent.
    let indent = 1usize;
    let text_w = usable.saturating_sub(indent + 1).max(1);
    let mut rows = 0u16;
    for msg in queued {
        let is_command = crate::completer::Completer::is_command(msg.trim());
        let logical_lines = queued_logical_lines(msg);
        let wraps = logical_lines.iter().any(|l| l.chars().count() > text_w);
        let multiline = logical_lines.len() > 1 || wraps;
        let block_w = if multiline {
            if wraps {
                text_w + 1
            } else {
                logical_lines
                    .iter()
                    .map(|l| l.chars().count())
                    .max()
                    .unwrap_or(0)
                    + 1
            }
        } else {
            0
        };
        for line in &logical_lines {
            if line.is_empty() {
                let fill = if block_w > 0 { block_w + 1 } else { 2 };
                out.print(&" ".repeat(indent));
                out.push_bg(theme::user_bg());
                out.print(&" ".repeat(fill));
                out.pop_style();
                out.newline();
                rows += 1;
                continue;
            }
            let chunks = wrap_line(line, text_w);
            for chunk in &chunks {
                let chunk_len = chunk.chars().count();
                let trailing = if block_w > 0 {
                    block_w.saturating_sub(chunk_len)
                } else {
                    1
                };
                out.print(&" ".repeat(indent));
                out.push_style(StyleState {
                    bg: Some(theme::user_bg()),
                    bold: true,
                    ..StyleState::default()
                });
                out.print(" ");
                blocks::print_user_highlights(out, chunk, &[], is_command);
                out.print(&" ".repeat(trailing));
                out.pop_style();
                out.newline();
                rows += 1;
            }
        }
    }
    rows
}

/// Chrome rows the BTW block reserves around its body content (header
/// row + bar row + input rows etc., before the body fills the rest).
const BTW_CHROME_ROWS: usize = 4;

/// Maximum body lines the BTW block displays at the given terminal
/// height. Capped at half the terminal so the BTW never dominates the
/// screen, with `BTW_CHROME_ROWS` taken out for header/input chrome.
fn btw_max_body_rows(term_h: usize) -> usize {
    (term_h / 2).saturating_sub(BTW_CHROME_ROWS).max(1)
}

fn render_btw(
    out: &mut RenderOut,
    btw: &mut BtwBlock,
    usable: usize,
    max_content_lines: usize,
    vim_enabled: bool,
) -> u16 {
    let max_lines = max_content_lines.max(1);
    let mut rows = 0u16;

    // Header: "/btw" in accent, question with @path and image highlighting.
    out.print(" ");
    out.push_fg(theme::accent());
    out.print("/btw");
    out.pop_style();
    out.print(" ");
    let max_q = usable.saturating_sub(6); // " /btw " = 6 chars
    let q: String = btw.question.chars().take(max_q).collect();
    blocks::print_user_highlights(out, &q, &btw.image_labels, false);
    out.newline();
    rows += 1;

    // Body: response or spinner.
    match btw.response {
        Some(ref text) => {
            let render_w = usable;

            // Rebuild rendered line cache on width change or first render.
            if btw.wrapped.is_empty() || btw.wrap_width != render_w {
                btw.wrapped.clear();
                let mut buf = RenderOut::buffer();
                blocks::render_markdown_inner(&mut buf, text, render_w, "   ", false, None);
                let _ = std::io::Write::flush(&mut buf);
                let bytes = buf.into_bytes();
                let rendered = String::from_utf8_lossy(&bytes);
                for line in rendered.split("\r\n") {
                    btw.wrapped.push(line.to_string());
                }
                // Remove trailing empty from split.
                if btw.wrapped.last().is_some_and(|l| l.is_empty()) {
                    btw.wrapped.pop();
                }
                if btw.wrapped.is_empty() {
                    btw.wrapped.push(String::new());
                }
                btw.wrap_width = render_w;
                // Clamp scroll.
                let max = btw.wrapped.len().saturating_sub(max_lines);
                btw.scroll_offset = btw.scroll_offset.min(max);
            }

            let total = btw.wrapped.len();
            let visible = total.min(max_lines);
            let can_scroll = total > max_lines;

            for line in btw.wrapped.iter().skip(btw.scroll_offset).take(visible) {
                out.print(line);
                out.newline();
                rows += 1;
            }

            // Blank line before hint.
            out.newline();
            rows += 1;

            // Scroll hint or dismiss hint.
            out.push_fg(theme::muted());
            if can_scroll {
                let end = (btw.scroll_offset + visible).min(total);
                out.print(&format!(
                    "   [{end}/{total}]  {}  {}  esc: close",
                    hints::nav(vim_enabled),
                    hints::scroll(vim_enabled),
                ));
            } else {
                out.print("   esc: close");
            }
            out.pop_style();
            out.newline();
            rows += 1;
        }
        None => {
            let frame = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                / 150) as usize
                % SPINNER_FRAMES.len();
            out.push_fg(theme::muted());
            out.print(&format!("   {} thinking", SPINNER_FRAMES[frame]));
            out.pop_style();
            out.newline();
            rows += 1;
        }
    }

    // Blank separator line before the bar.
    out.newline();
    rows += 1;

    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{FramePrompt, StdioBackend};

    #[test]
    fn status_line_renders_content() {
        let mut screen = Screen::with_backend(Box::new(StdioBackend));
        screen.set_running_procs(1);
        let mut out = RenderOut::buffer();
        screen.queue_status_line(&mut out);
        let rendered = String::from_utf8(out.into_bytes()).unwrap();
        // Status line should contain the "proc" indicator.
        assert!(
            rendered.contains("proc"),
            "status line missing proc indicator: {rendered:?}"
        );
    }

    #[test]
    fn prompt_sections_reset_terminal_style_before_rendering() {
        let mut screen = Screen::with_backend(Box::new(StdioBackend));
        screen.set_anchor_row(0);
        let input = crate::input::InputState::default();
        let mut out = RenderOut::buffer();
        screen.draw_viewport_frame(
            &mut out,
            40,
            FramePrompt {
                state: &input,
                mode: protocol::Mode::Normal,
                queued: &[],
                prediction: None,
            },
            0,
            0,
            0,
            None,
        );
        let rendered = String::from_utf8(out.into_bytes()).unwrap();
        assert!(
            rendered.contains("\u{1b}[0m\u{1b}[0m"),
            "rendered: {rendered:?}"
        );
    }

    #[test]
    fn export_render_cache_skips_blocks_without_ir() {
        let mut screen = Screen::new();
        screen.push(Block::Thinking {
            content: "alpha\nbeta".into(),
        });
        // Thinking blocks don't carry tool-output IR, so the cache is empty.
        assert!(screen.export_render_cache().is_none());
    }
}
