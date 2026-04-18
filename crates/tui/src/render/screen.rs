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
    collect_trailing_thinking, gap_between, render_active_exec, render_block,
    render_thinking_summary, render_tool, thinking_summary, Element,
};
use super::cache::{PersistedLayoutCache, RenderCache};
use super::completions::{completion_reserved_rows, draw_completions, draw_menu};
use super::context::PaintContext;
use super::history::{
    ActiveAgent, ActiveExec, ActiveText, ActiveThinking, ActiveTool, AgentBlockStatus, Block,
    BlockHistory, BlockId, Throbber, ToolOutput, ToolOutputRef, ToolState, ToolStatus,
};
use super::layout_out::{LayoutSink, SpanCollector};
use super::paint::paint_line;
use super::prompt::PromptState;
use super::selection::{
    build_char_kinds, build_display_spans, compute_visual_line_offsets, map_cursor,
    render_styled_chars, spans_to_string, wrap_and_locate_cursor, wrap_line, SpanKind,
};
use super::status::{draw_bar, render_status_spans, vim_mode_label, BarSpan, StatusSpan};
use super::working::WorkingState;
use super::{
    cursor_colors, draw_soft_cursor, emit_newlines, format_tokens, is_table_separator,
    reasoning_color, DialogPlacement, Frame, FramePrompt, RenderOut, StdioBackend, StyleState,
    TerminalBackend, SPINNER_FRAMES,
};
use crate::input::{InputSnapshot, InputState};
use crate::keymap::hints;
use crate::theme;
use crossterm::{
    cursor,
    style::{Color, Print, ResetColor},
    terminal, QueueableCommand,
};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::time::{Duration, Instant};

pub struct Screen {
    history: BlockHistory,
    active_thinking: Option<ActiveThinking>,
    active_text: Option<ActiveText>,
    active_tools: Vec<ActiveTool>,
    active_agents: Vec<ActiveAgent>,
    active_exec: Option<ActiveExec>,
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
    /// Skip the next `render_pending_blocks` call.  Set by
    /// `clear_dialog_area` so that `finish_turn` → `flush_blocks` doesn't
    /// render blocks in scroll mode right after a dialog is dismissed (which
    /// causes scrollback pollution on some terminals).  The blocks are
    /// rendered by the next `draw_frame` instead.
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
            history: BlockHistory::new(),
            active_thinking: None,
            active_text: None,
            active_tools: Vec::new(),
            active_agents: Vec::new(),
            active_exec: None,
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

    fn cursor_y(&self) -> u16 {
        self.prompt
            .anchor_row
            .unwrap_or_else(|| self.backend.cursor_y())
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

    /// Number of committed blocks in history.
    pub fn block_count(&self) -> usize {
        self.history.len()
    }

    /// Cloned snapshot of all blocks in history, in order.
    pub fn blocks(&self) -> Vec<Block> {
        self.history
            .order
            .iter()
            .map(|id| self.history.blocks[id].clone())
            .collect()
    }

    /// Cloned snapshot of every committed tool's `ToolState`. Pairs with
    /// `blocks()` to fully reconstruct history (used by the test harness).
    pub fn tool_states_snapshot(&self) -> HashMap<String, ToolState> {
        self.history.tool_states.clone()
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

    /// Start tracking a blocking agent in the dynamic section.
    pub fn start_active_agent(&mut self, agent_id: String) {
        self.active_agents.push(ActiveAgent {
            agent_id,
            slug: None,
            tool_calls: Vec::new(),
            status: AgentBlockStatus::Running,
            start_time: Instant::now(),
            final_elapsed: None,
        });
        self.prompt.dirty = true;
    }

    /// Update a specific active blocking agent's state.
    pub fn update_active_agent(
        &mut self,
        agent_id: &str,
        slug: Option<&str>,
        tool_calls: &[crate::app::AgentToolEntry],
        status: AgentBlockStatus,
    ) {
        if let Some(agent) = self
            .active_agents
            .iter_mut()
            .find(|a| a.agent_id == agent_id)
        {
            agent.slug = slug.map(str::to_string);
            agent.tool_calls = tool_calls.to_vec();
            if status != AgentBlockStatus::Running && agent.status == AgentBlockStatus::Running {
                // Freeze the timer on completion.
                agent.final_elapsed = Some(agent.start_time.elapsed());
            }
            agent.status = status;
            self.prompt.dirty = true;
        }
    }

    /// Mark all active agents as cancelled/error (before flush commits them).
    pub fn cancel_active_agents(&mut self) {
        for agent in &mut self.active_agents {
            agent.status = AgentBlockStatus::Error;
            agent.final_elapsed = Some(agent.start_time.elapsed());
        }
    }

    /// Commit a specific active agent to history and remove it from the live set.
    pub fn finish_active_agent(&mut self, agent_id: &str) {
        if let Some(idx) = self
            .active_agents
            .iter()
            .position(|a| a.agent_id == agent_id)
        {
            let mut agent = self.active_agents.remove(idx);
            // If still marked Running, the tool returned successfully —
            // the subagent's TurnComplete may not have been drained yet.
            if agent.status == AgentBlockStatus::Running {
                agent.status = AgentBlockStatus::Done;
                agent.final_elapsed = Some(agent.start_time.elapsed());
            }
            let elapsed = agent
                .final_elapsed
                .unwrap_or_else(|| agent.start_time.elapsed());
            self.history.push(Block::Agent {
                agent_id: agent.agent_id,
                slug: agent.slug,
                blocking: true,
                tool_calls: agent.tool_calls,
                status: agent.status,
                elapsed: Some(elapsed),
            });
            self.prompt.dirty = true;
        }
    }

    /// Commit all active agents to history and clear the live set.
    pub fn finish_all_active_agents(&mut self) {
        let agents: Vec<ActiveAgent> = self.active_agents.drain(..).collect();
        for mut agent in agents {
            if agent.status == AgentBlockStatus::Running {
                agent.status = AgentBlockStatus::Done;
                agent.final_elapsed = Some(agent.start_time.elapsed());
            }
            let elapsed = agent
                .final_elapsed
                .unwrap_or_else(|| agent.start_time.elapsed());
            self.history.push(Block::Agent {
                agent_id: agent.agent_id,
                slug: agent.slug,
                blocking: true,
                tool_calls: agent.tool_calls,
                status: agent.status,
                elapsed: Some(elapsed),
            });
        }
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
        out.overlay_newline();
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
                group: false,
                truncatable: false,
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
                group: false,
                truncatable: true,
            });
        } else if self.show_slug {
            if let Some(ref label) = self.task_label {
                spans.push(StatusSpan {
                    text: format!(" {label} "),
                    style: pill_style,
                    priority: 5,
                    group: false,
                    truncatable: true,
                });
            }
        }

        // App focus pill (always shown; two-level mode on top of vim).
        let (focus_label, focus_fg) = match self.last_app_focus {
            crate::app::AppFocus::Prompt => ("PROMPT", Color::AnsiValue(74)),
            crate::app::AppFocus::History => ("HISTORY", Color::AnsiValue(214)),
        };
        spans.push(StatusSpan {
            text: format!(" {focus_label} "),
            style: StyleState {
                fg: Some(focus_fg),
                bg: Some(Color::AnsiValue(236)),
                bold: true,
                ..StyleState::default()
            },
            priority: 2,
            group: false,
            truncatable: false,
        });

        // Vim mode.
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
                group: false,
                truncatable: false,
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
            group: false,
            truncatable: false,
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
                group: false,
                truncatable: false,
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
                truncatable: false,
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
                truncatable: false,
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
                truncatable: false,
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
        if scrolled_by_dialog && self.prompt.prev_dialog_gap > 0 {
            self.history.suppress_leading_gap = true;
        }
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
        self.history.last_block_rows = 0;
        self.active_tools.clear();
    }

    /// Push a `Block::ToolCall` along with its `ToolState`. Use this on
    /// the resume path where the protocol message already carries a
    /// finished tool result.
    pub fn push_tool_call(&mut self, block: Block, state: ToolState) {
        debug_assert!(matches!(block, Block::ToolCall { .. }));
        let call_id = match &block {
            Block::ToolCall { call_id, .. } => call_id.clone(),
            _ => return,
        };
        self.history.push_with_state(block, call_id, state);
        self.prompt.dirty = true;
    }

    pub fn push(&mut self, block: Block) {
        let block = match block {
            Block::Text { content } => {
                let t = content.trim();
                if t.is_empty() {
                    return;
                }
                Block::Text {
                    content: t.to_string(),
                }
            }
            Block::AgentMessage {
                from_id,
                from_slug,
                content,
            } => {
                let t = content.trim();
                if t.is_empty() {
                    return;
                }
                Block::AgentMessage {
                    from_id,
                    from_slug,
                    content: t.to_string(),
                }
            }
            Block::Thinking { content } => {
                let t = content.trim();
                if t.is_empty() {
                    return;
                }
                Block::Thinking {
                    content: t.to_string(),
                }
            }
            Block::Compacted { summary } => {
                let t = summary.trim();
                if t.is_empty() {
                    return;
                }
                Block::Compacted {
                    summary: t.to_string(),
                }
            }
            other => other,
        };
        self.history.push(block);
        self.prompt.dirty = true;
    }

    // ── Streaming thinking ────────────────────────────────────────────

    pub fn append_streaming_thinking(&mut self, delta: &str) {
        let at = self.active_thinking.get_or_insert_with(|| ActiveThinking {
            current_line: String::new(),
            paragraph: String::new(),
        });

        for ch in delta.chars() {
            if ch == '\r' {
                continue;
            }
            if ch == '\n' {
                let line = std::mem::take(&mut at.current_line);
                if line.trim().is_empty() && !at.paragraph.is_empty() {
                    // Blank line — commit the paragraph.
                    // Include the trailing newline so it renders as visual spacing.
                    at.paragraph.push('\n');
                    let para = std::mem::take(&mut at.paragraph);
                    self.history.push(Block::Thinking { content: para });
                } else {
                    if !at.paragraph.is_empty() {
                        at.paragraph.push('\n');
                    }
                    at.paragraph.push_str(&line);
                }
            } else {
                at.current_line.push(ch);
            }
        }
        self.prompt.dirty = true;
    }

    /// Flush remaining thinking content.
    pub fn flush_streaming_thinking(&mut self) {
        if let Some(mut at) = self.active_thinking.take() {
            // Commit any remaining content (paragraph + current line).
            if !at.current_line.is_empty() {
                if !at.paragraph.is_empty() {
                    at.paragraph.push('\n');
                }
                at.paragraph.push_str(&at.current_line);
            }
            let trimmed = at.paragraph.trim();
            if !trimmed.is_empty() {
                self.history.push(Block::Thinking {
                    content: trimmed.to_string(),
                });
            }
            self.prompt.dirty = true;
        }
    }

    /// Gap before a thinking summary overlay, skipping over hidden thinking blocks.
    fn thinking_summary_gap(&self) -> u16 {
        if let Some(last) = self
            .history
            .order
            .iter()
            .rev()
            .filter_map(|id| self.history.blocks.get(id))
            .find(|b| !matches!(b, Block::Thinking { .. }))
        {
            gap_between(
                &Element::Block(last),
                &Element::Block(&Block::Thinking {
                    content: String::new(),
                }),
            )
        } else if self.history.is_empty() {
            0
        } else {
            1
        }
    }

    // ── Streaming text ─────────────────────────────────────────────────

    pub fn append_streaming_text(&mut self, delta: &str) {
        // Text starting means thinking is done — commit remaining thinking.
        if self.active_thinking.is_some() {
            self.flush_streaming_thinking();
        }

        let at = self.active_text.get_or_insert_with(|| ActiveText {
            current_line: String::new(),
            paragraph: String::new(),
            in_code_block: None,
            table_rows: Vec::new(),
            table_data_rows: 0,
        });

        for ch in delta.chars() {
            if ch == '\r' {
                continue; // Strip \r (CRLF → LF)
            }
            if ch == '\n' {
                let line = std::mem::take(&mut at.current_line);
                Self::process_text_line(&mut self.history, at, &line);
            } else {
                at.current_line.push(ch);
            }
        }
        self.prompt.dirty = true;
    }

    /// Process a completed line of streaming text.
    fn process_text_line(history: &mut BlockHistory, at: &mut ActiveText, line: &str) {
        let trimmed = line.trim_start();

        // ── Code fence detection ────────────────────────────────────────
        if trimmed.starts_with("```") {
            if at.in_code_block.is_some() {
                // Closing fence — individual code lines were already committed.
                at.in_code_block = None;
                return;
            } else {
                // Opening fence — commit pending text/table.
                Self::commit_paragraph(history, at);
                Self::commit_table(history, at);
                let lang = trimmed.trim_start_matches('`').trim().to_string();
                at.in_code_block = Some(lang);
                return;
            }
        }

        // ── Inside a code block ─────────────────────────────────────────
        if let Some(ref lang) = at.in_code_block {
            history.push(Block::CodeLine {
                content: line.to_string(),
                lang: lang.clone(),
            });
            return;
        }

        // ── Table row — accumulate silently ────────────────────────────
        if trimmed.starts_with('|') {
            Self::commit_paragraph(history, at);
            if !is_table_separator(line) {
                at.table_data_rows += 1;
            }
            at.table_rows.push(line.to_string());
            return;
        }

        // ── Blank line ───────────────────────────────────────────────────
        if line.trim().is_empty() {
            if !at.table_rows.is_empty() {
                return; // Skip blank lines inside tables.
            }
            if !at.paragraph.is_empty() {
                Self::commit_paragraph(history, at);
            }
            return;
        }

        // ── Non-table line after table — commit the table ────────────────
        Self::commit_table(history, at);

        // ── Regular text line ───────────────────────────────────────────
        if !at.paragraph.is_empty() {
            at.paragraph.push('\n');
        }
        at.paragraph.push_str(line);
    }

    fn commit_table(history: &mut BlockHistory, at: &mut ActiveText) {
        if !at.table_rows.is_empty() {
            let content = std::mem::take(&mut at.table_rows).join("\n");
            history.push(Block::Text { content });
            at.table_data_rows = 0;
        }
    }

    fn commit_paragraph(history: &mut BlockHistory, at: &mut ActiveText) {
        let para = std::mem::take(&mut at.paragraph);
        let trimmed = para.trim();
        if !trimmed.is_empty() {
            history.push(Block::Text {
                content: trimmed.to_string(),
            });
        }
    }

    /// Flush remaining streaming text.
    pub fn flush_streaming_text(&mut self) {
        self.flush_streaming_thinking();
        if let Some(mut at) = self.active_text.take() {
            // If inside an unclosed code block, check whether current_line
            // is the closing fence before committing it as a code line.
            if at.in_code_block.is_some() {
                if at.current_line.trim_start().starts_with("```") {
                    // Closing fence — just close the block, don't render it.
                    at.current_line.clear();
                } else if !at.current_line.is_empty() {
                    let lang = at.in_code_block.as_ref().unwrap().clone();
                    self.history.push(Block::CodeLine {
                        content: std::mem::take(&mut at.current_line),
                        lang,
                    });
                }
                at.in_code_block = None;
            }
            // If current_line is a table row, add it to the table.
            if !at.current_line.is_empty() && at.current_line.trim_start().starts_with('|') {
                at.table_rows.push(std::mem::take(&mut at.current_line));
            }
            Self::commit_table(&mut self.history, &mut at);
            // Commit remaining paragraph + current line.
            if !at.current_line.is_empty() {
                if !at.paragraph.is_empty() {
                    at.paragraph.push('\n');
                }
                at.paragraph.push_str(&at.current_line);
            }
            Self::commit_paragraph(&mut self.history, &mut at);
            self.prompt.dirty = true;
        }
    }

    pub fn start_tool(
        &mut self,
        call_id: String,
        name: String,
        summary: String,
        args: HashMap<String, serde_json::Value>,
    ) {
        self.active_tools.push(ActiveTool {
            call_id,
            name,
            summary,
            args,
            status: ToolStatus::Pending,
            output: None,
            user_message: None,
            start_time: Instant::now(),
        });
        self.prompt.dirty = true;
    }

    pub fn start_exec(&mut self, command: String) {
        self.active_exec = Some(ActiveExec {
            command,
            output: String::new(),
            start_time: Instant::now(),
            finished: false,
            exit_code: None,
        });
        self.prompt.dirty = true;
    }

    pub fn append_exec_output(&mut self, chunk: &str) {
        if let Some(ref mut exec) = self.active_exec {
            if !exec.output.is_empty() && !exec.output.ends_with('\n') {
                exec.output.push('\n');
            }
            exec.output.push_str(chunk);
            self.prompt.dirty = true;
        }
    }

    pub fn finish_exec(&mut self, exit_code: Option<i32>) {
        if let Some(ref mut exec) = self.active_exec {
            exec.finished = true;
            exec.exit_code = exit_code;
            self.prompt.dirty = true;
        }
    }

    /// Commit the active exec to block history.
    pub fn commit_exec(&mut self) {
        if let Some(exec) = self.active_exec.take() {
            let mut output = exec.output;
            output.truncate(output.trim_end().len());
            self.history.push(Block::Exec {
                command: exec.command,
                output,
            });
            self.prompt.dirty = true;
        }
    }

    pub fn has_active_exec(&self) -> bool {
        self.active_exec.is_some()
    }

    /// Index of an active tool by call_id. Empty call_id (e.g.
    /// ask_user_question) falls back to the last active tool.
    fn active_tool_index(&self, call_id: &str) -> Option<usize> {
        if call_id.is_empty() {
            self.active_tools.len().checked_sub(1)
        } else {
            self.active_tools.iter().position(|t| t.call_id == call_id)
        }
    }

    fn active_tool_mut(&mut self, call_id: &str) -> Option<&mut ActiveTool> {
        let idx = self.active_tool_index(call_id)?;
        Some(&mut self.active_tools[idx])
    }

    pub fn append_active_output(&mut self, call_id: &str, chunk: &str) {
        if let Some(tool) = self.active_tool_mut(call_id) {
            match tool.output {
                Some(ref mut out) => {
                    if !out.content.is_empty() {
                        out.content.push('\n');
                    }
                    out.content.push_str(chunk);
                }
                None => {
                    tool.output = Some(Box::new(ToolOutput {
                        content: chunk.to_string(),
                        is_error: false,
                        metadata: None,
                        render_cache: None,
                    }));
                }
            }
            self.prompt.dirty = true;
        } else if let Some(cid) = self.last_tool_call_id() {
            self.update_tool_state(&cid, |state| match state.output {
                Some(ref mut out) => {
                    if !out.content.is_empty() {
                        out.content.push('\n');
                    }
                    out.content.push_str(chunk);
                }
                None => {
                    state.output = Some(Box::new(ToolOutput {
                        content: chunk.to_string(),
                        is_error: false,
                        metadata: None,
                        render_cache: None,
                    }));
                }
            });
        }
    }

    pub fn set_active_status(&mut self, call_id: &str, status: ToolStatus) {
        if let Some(tool) = self.active_tool_mut(call_id) {
            // Reset timer when transitioning from confirm → pending (user approved)
            if tool.status == ToolStatus::Confirm && status == ToolStatus::Pending {
                tool.start_time = Instant::now();
            }
            tool.status = status;
            self.prompt.dirty = true;
        } else if let Some(cid) = self.last_tool_call_id() {
            self.update_tool_state(&cid, |state| state.status = status);
        }
    }

    pub fn set_active_user_message(&mut self, call_id: &str, msg: String) {
        if let Some(tool) = self.active_tool_mut(call_id) {
            tool.user_message = Some(msg);
            self.prompt.dirty = true;
        } else if let Some(cid) = self.last_tool_call_id() {
            self.update_tool_state(&cid, |state| state.user_message = Some(msg));
        }
    }

    pub fn finish_tool(
        &mut self,
        call_id: &str,
        status: ToolStatus,
        output: Option<ToolOutputRef>,
        engine_elapsed: Option<Duration>,
    ) {
        if let Some(idx) = self.active_tool_index(call_id) {
            let tool = self.active_tools.remove(idx);
            let elapsed = if status == ToolStatus::Denied {
                None
            } else {
                engine_elapsed.or_else(|| tool.elapsed())
            };
            let cid = if call_id.is_empty() {
                tool.call_id.clone()
            } else {
                call_id.to_string()
            };
            let state = ToolState {
                status,
                elapsed,
                output,
                user_message: tool.user_message,
            };
            self.history.push_with_state(
                Block::ToolCall {
                    call_id: cid.clone(),
                    name: tool.name,
                    summary: tool.summary,
                    args: tool.args,
                },
                cid,
                state,
            );
            self.prompt.dirty = true;
        } else if let Some(cid) = self.last_tool_call_id() {
            self.update_tool_state(&cid, |state| {
                state.status = status;
                state.output = output;
            });
        }
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

    pub fn is_dirty(&self) -> bool {
        self.prompt.dirty || self.history.has_unflushed()
    }

    /// Center the input viewport on the cursor (vim `zz`).
    pub fn center_input_scroll(&mut self) {
        // The actual centering happens in draw_prompt_sections using a
        // sentinel value. We set input_scroll to usize::MAX so the
        // scroll logic knows to center instead of preserving position.
        self.prompt.input_scroll = usize::MAX;
        self.prompt.dirty = true;
    }

    /// Convert active tools to history blocks and render any pending blocks.
    pub fn flush_blocks(&mut self) {
        let _perf = crate::perf::begin("render:flush_blocks");
        self.commit_active_tools();
        self.render_pending_blocks();
    }

    /// Convert all active tools to history blocks without rendering.
    /// The blocks remain unflushed so that `draw_frame(None)` will render
    /// them (along with any preceding reasoning blocks) before the dialog
    /// paints on top.
    pub fn commit_active_tools(&mut self) {
        self.commit_active_tools_as(ToolStatus::Err);
    }

    pub fn commit_active_tools_as(&mut self, status: ToolStatus) {
        self.finish_all_active_agents();
        for tool in self.active_tools.drain(..) {
            let elapsed = if status == ToolStatus::Denied {
                None
            } else {
                tool.elapsed()
            };
            let state = ToolState {
                status,
                elapsed,
                output: tool.output,
                user_message: tool.user_message,
            };
            self.history.push_with_state(
                Block::ToolCall {
                    call_id: tool.call_id.clone(),
                    name: tool.name,
                    summary: tool.summary,
                    args: tool.args,
                },
                tool.call_id,
                state,
            );
        }
    }

    /// `call_id` of the most recent committed `Block::ToolCall`, if any.
    fn last_tool_call_id(&self) -> Option<String> {
        self.history
            .order
            .iter()
            .rev()
            .find_map(|id| match self.history.blocks.get(id) {
                Some(Block::ToolCall { call_id, .. }) => Some(call_id.clone()),
                _ => None,
            })
    }

    /// Read-only view of a committed tool's mutable state.
    pub fn tool_state(&self, call_id: &str) -> Option<&ToolState> {
        self.history.tool_states.get(call_id)
    }

    /// Mutate a committed tool's state and invalidate its layout cache so
    /// the next paint reflects the change. Returns true if `call_id` was
    /// found in history.
    pub fn update_tool_state(
        &mut self,
        call_id: &str,
        mutator: impl FnOnce(&mut ToolState),
    ) -> bool {
        let Some(state) = self.history.tool_states.get_mut(call_id) else {
            return false;
        };
        mutator(state);
        if let Some(id) = self.history.tool_block_id(call_id) {
            self.history.invalidate_block_layout(id);
        }
        self.prompt.dirty = true;
        true
    }

    /// Insert or replace tool state for a call_id without touching blocks.
    /// Used by resume to attach state to freshly reconstructed blocks.
    pub fn set_tool_state(&mut self, call_id: String, state: ToolState) {
        self.history.tool_states.insert(call_id, state);
    }

    /// Whether any content (blocks, active tool, active exec) exists above
    /// the prompt.  Used to decide whether to emit a 1-line gap before the
    /// prompt bar.
    fn has_content(&self) -> bool {
        !self.history.is_empty() || self.has_ephemeral()
    }

    pub fn render_pending_blocks(&mut self) {
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
        if w as usize != self.history.cache_width {
            self.history.invalidate_for_width(w as usize);
        }
        let mut frame = Frame::begin(&*self.backend);
        let _ = frame.queue(cursor::MoveTo(0, 0));
        let _ = frame.queue(terminal::Clear(terminal::ClearType::All));
        self.prompt.drawn = false;
        self.prompt.dirty = true;
        self.prompt.prev_rows = 0;
    }

    pub fn clear(&mut self) {
        self.history.clear();
        self.active_thinking = None;
        self.active_text = None;
        self.active_tools.clear();
        self.active_agents.clear();
        self.active_exec = None;
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
        !self.history.is_empty()
    }

    /// Snapshot the per-tool intermediate representations stored on
    /// committed `Block::ToolCall` blocks. The IR is width-independent and
    /// expensive to rebuild (it contains the LCS diff and syntect tokens),
    /// so we persist it alongside the session and reattach on resume.
    /// Returns `None` if no IR has been built yet.
    pub fn export_render_cache(&self) -> Option<RenderCache> {
        let mut cache = RenderCache::new(String::new());
        for id in &self.history.order {
            if let Some(Block::ToolCall { call_id, .. }) = self.history.blocks.get(id) {
                if let Some(state) = self.history.tool_states.get(call_id) {
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
        self.history.cache_dirty
    }

    /// Export a content-addressed snapshot of every cached block artifact
    /// that is safe to persist. Tool blocks whose `ToolState` is not yet
    /// terminal are skipped — their layout captures transient state.
    pub fn export_layout_cache(&mut self) -> Option<PersistedLayoutCache> {
        if self.history.is_empty() {
            return None;
        }
        let mut cache = PersistedLayoutCache::new(crate::theme::is_light());
        // Walk `order` so we only export artifacts for blocks currently in
        // history (and so we can inspect the `ToolState` of each tool block
        // exactly once — duplicates in `order` resolve to the same entry).
        for id in &self.history.order {
            if cache.blocks.contains_key(id) {
                continue;
            }
            let Some(block) = self.history.blocks.get(id) else {
                continue;
            };
            let persist = match block {
                Block::ToolCall { call_id, .. } => self
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
            if let Some(artifact) = self.history.artifacts.get(id) {
                if !artifact.is_empty() {
                    cache.blocks.insert(*id, artifact.clone());
                }
            }
        }
        self.history.cache_dirty = false;
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
        let live: HashSet<BlockId> = self.history.order.iter().copied().collect();
        for (id, mut artifact) in cache.blocks {
            if !live.contains(&id) {
                continue;
            }
            let Some(block) = self.history.blocks.get(&id) else {
                continue;
            };
            let allow = match block {
                Block::ToolCall { call_id, .. } => self
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
            // Drop any layouts that would not paint correctly at the
            // current terminal width.
            artifact
                .layouts
                .retain(|(k, b)| k.width == nw || b.is_valid_at(nw));
            if artifact.is_empty() {
                continue;
            }
            self.history
                .artifacts
                .entry(id)
                .and_modify(|a| {
                    for (k, b) in &artifact.layouts {
                        a.insert(*k, b.clone());
                    }
                })
                .or_insert(artifact);
        }
        self.history.cache_width = nw as usize;
        self.history.cache_dirty = false;
    }

    /// Returns (block_index, full_text) for each User block. The index is
    /// the position in the ordered history and is the value expected by
    /// `truncate_to`.
    pub fn user_turns(&self) -> Vec<(usize, String)> {
        self.history
            .order
            .iter()
            .enumerate()
            .filter_map(|(i, id)| match self.history.blocks.get(id) {
                Some(Block::User { text, .. }) => Some((i, text.clone())),
                _ => None,
            })
            .collect()
    }

    /// Truncate blocks so that only blocks before `block_idx` remain.
    pub fn truncate_to(&mut self, block_idx: usize) {
        self.history.truncate(block_idx);
        self.active_tools.clear();
        self.active_agents.clear();
        self.redraw();
    }

    pub fn draw_prompt(&mut self, state: &InputState, mode: protocol::Mode, width: usize) {
        let mut frame = Frame::begin(&*self.backend);
        self.draw_frame(
            &mut frame,
            width,
            Some(FramePrompt {
                state,
                mode,
                queued: &[],
                prediction: None,
            }),
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
    }

    /// Returns true when there is content or prompt work to render.
    pub fn needs_draw(&self, is_dialog: bool) -> bool {
        let has_new_blocks = self.history.has_unflushed();
        if is_dialog {
            has_new_blocks || (self.has_ephemeral() && self.prompt.dirty)
        } else {
            has_new_blocks || self.prompt.dirty
        }
    }

    /// Whether any streaming overlay element is active.
    fn has_ephemeral(&self) -> bool {
        self.active_thinking.is_some()
            || self.active_text.is_some()
            || !self.active_tools.is_empty()
            || !self.active_agents.is_empty()
            || self.active_exec.is_some()
    }

    /// Write every ephemeral element into `out` with `newline()` for
    /// inter-item gaps. The caller feeds a `SpanCollector` and then
    /// tail-crops the flat line stream — content-agnostic cropping,
    /// so streaming bash output, a partial markdown table, and a
    /// multi-line tool command are all dropped oldest-first together.
    fn render_ephemeral_into<S: LayoutSink>(&self, out: &mut S, width: usize) {
        // `last_committed` is borrowed — no clone. As streaming items
        // are emitted they are also stored in `prev_synth` so the next
        // item's gap is computed against the most recently emitted
        // element rather than the last committed one.
        let last_committed: Option<&Block> = self.history.last_block();
        let mut prev_synth: Option<Block> = None;
        let mut had_streaming = false;

        // ── Active thinking ─────────────────────────────────────────
        if let Some(ref at) = self.active_thinking {
            if self.show_thinking {
                let content = match (at.paragraph.is_empty(), at.current_line.is_empty()) {
                    (true, true) => None,
                    (true, false) => Some(at.current_line.clone()),
                    (false, true) => Some(at.paragraph.clone()),
                    (false, false) => Some(format!("{}\n{}", at.paragraph, at.current_line)),
                };
                if let Some(content) = content.filter(|t| !t.trim().is_empty()) {
                    let block = Block::Thinking { content };
                    let gap = prev_synth
                        .as_ref()
                        .or(last_committed)
                        .map(|p| gap_between(&Element::Block(p), &Element::Block(&block)))
                        .unwrap_or(0);
                    emit_newlines(out, gap);
                    render_block(out, &block, None, width, self.show_thinking);
                    prev_synth = Some(block);
                    had_streaming = true;
                }
            } else {
                // Animated summary: aggregate committed Thinking blocks
                // with the in-flight text so the count keeps ticking
                // even right after a paragraph commit.
                let mut combined = collect_trailing_thinking(
                    self.history.order.iter().map(|id| &self.history.blocks[id]),
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
                    had_streaming = true;
                }
            }
        }

        // ── Active text (paragraph, code line, or partial table) ───
        if let Some(ref at) = self.active_text {
            let in_table =
                !at.table_rows.is_empty() || at.current_line.trim_start().starts_with('|');

            let block_opt: Option<Block> = if in_table {
                // Partial markdown table rendered live. Paint cropping
                // handles overflow; commits normally when complete.
                let mut total = at.table_rows.iter().map(|r| r.len() + 1).sum::<usize>();
                let cur_trim = at.current_line.trim_start();
                let append_cur = cur_trim.starts_with('|');
                if append_cur {
                    total += at.current_line.len() + 1;
                }
                if total == 0 {
                    None
                } else {
                    let mut content = String::with_capacity(total);
                    for row in &at.table_rows {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(row);
                    }
                    if append_cur {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(&at.current_line);
                    }
                    Some(Block::Text { content })
                }
            } else if at.in_code_block.is_some() && !at.current_line.is_empty() {
                Some(Block::CodeLine {
                    content: at.current_line.clone(),
                    lang: at.in_code_block.clone().unwrap_or_default(),
                })
            } else if !at.paragraph.is_empty() || !at.current_line.trim().is_empty() {
                let mut content =
                    String::with_capacity(at.paragraph.len() + at.current_line.len() + 1);
                content.push_str(&at.paragraph);
                if !at.current_line.is_empty() {
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    content.push_str(&at.current_line);
                }
                (!content.trim().is_empty()).then_some(Block::Text { content })
            } else {
                None
            };

            if let Some(block) = block_opt {
                let gap = prev_synth
                    .as_ref()
                    .or(last_committed)
                    .map(|p| gap_between(&Element::Block(p), &Element::Block(&block)))
                    .unwrap_or(0);
                emit_newlines(out, gap);
                render_block(out, &block, None, width, self.show_thinking);
                prev_synth = Some(block);
                had_streaming = true;
            }
        }

        // ── Active tools ───────────────────────────────────────────
        let mut tool_count = 0usize;
        for tool in self.active_tools.iter() {
            let tool_gap = if tool_count == 0 {
                if had_streaming {
                    1
                } else if let Some(p) = prev_synth.as_ref().or(last_committed) {
                    gap_between(&Element::Block(p), &Element::ActiveTool)
                } else {
                    0
                }
            } else {
                gap_between(&Element::ActiveTool, &Element::ActiveTool)
            };
            emit_newlines(out, tool_gap);
            render_tool(
                out,
                &tool.call_id,
                &tool.name,
                &tool.summary,
                &tool.args,
                tool.status,
                Some(tool.start_time.elapsed()),
                tool.output.as_deref(),
                tool.user_message.as_deref(),
                width,
            );
            tool_count += 1;
        }

        // ── Active blocking agents ─────────────────────────────────
        for (i, agent) in self.active_agents.iter().enumerate() {
            let agent_gap = if i > 0 || tool_count > 0 {
                1
            } else if let Some(p) = prev_synth.as_ref().or(last_committed) {
                gap_between(&Element::Block(p), &Element::ActiveTool)
            } else {
                0
            };
            emit_newlines(out, agent_gap);
            let elapsed = agent
                .final_elapsed
                .unwrap_or_else(|| agent.start_time.elapsed());
            let agent_block = Block::Agent {
                agent_id: agent.agent_id.clone(),
                slug: agent.slug.clone(),
                blocking: true,
                tool_calls: agent.tool_calls.clone(),
                status: agent.status,
                elapsed: Some(elapsed),
            };
            render_block(out, &agent_block, None, width, self.show_thinking);
        }

        // ── Active exec ────────────────────────────────────────────
        if let Some(ref exec) = self.active_exec {
            let exec_gap = if !self.active_agents.is_empty() || tool_count > 0 {
                1
            } else if let Some(p) = prev_synth.as_ref().or(last_committed) {
                gap_between(&Element::Block(p), &Element::ActiveExec)
            } else {
                0
            };
            emit_newlines(out, exec_gap);
            render_active_exec(out, exec, width);
        }
    }

    /// Unified rendering entry point. Renders pending blocks + active
    /// overlay, then either the prompt (`Some`) or nothing (`None` =
    /// dialog covers it). `dialog_height` is the height of the active
    /// dialog in dialog mode — used to reserve space so the overlay
    /// tail-crops above it instead of fighting the dialog's own layout.
    ///
    /// The caller owns the `Frame` (sync lifecycle). This method only
    /// queues draw commands into the provided output buffer.
    ///
    /// Returns `true` when content-only mode drew something (caller
    /// should re-dirty any overlay dialog so it repaints on top).
    pub fn draw_frame(
        &mut self,
        out: &mut RenderOut,
        width: usize,
        prompt: Option<FramePrompt>,
        dialog_height: Option<u16>,
    ) -> (bool, Option<DialogPlacement>) {
        let _perf = crate::perf::begin("render:frame");

        self.update_spinner();

        let has_new_blocks = self.history.has_unflushed();
        let is_dialog = prompt.is_none();
        let has_ephemeral = self.has_ephemeral();

        // Seed `term_width` before the dialog-only fast path below,
        // which returns without reaching the main `init_cursor` call.
        let (seed_term_w, seed_term_h) = self.size();
        out.init_cursor(out.cursor_row, seed_term_w, seed_term_h);

        // Dialog mode: only repaint the content region when new blocks
        // land, the overlay has changed, or the dialog height changed
        // (needs full layout recomputation).  But ALWAYS return a
        // valid placement so the dialog itself can redraw (e.g. after
        // the user navigated within it).
        let dialog_height_changed =
            is_dialog && dialog_height.unwrap_or(0) != self.prompt.prev_dialog_height;
        if is_dialog
            && self.prompt.drawn
            && !has_new_blocks
            && !dialog_height_changed
            && !(has_ephemeral && self.prompt.dirty)
        {
            // Content hasn't changed — skip the expensive repaint but
            // return the last placement so the dialog can still draw.
            let placement = self.prompt.prev_dialog_row.map(|row| {
                let mut dh = dialog_height.unwrap_or(0);
                if self.constrain_dialog {
                    let (_, th) = self.size();
                    let half_h = th / 2;
                    // Use the stored anchor and prev_rows (overlay+gap)
                    // from the previous frame as approximation.
                    let anchor = self.prompt.anchor_row.unwrap_or(0);
                    let overhead = self.prompt.prev_rows + 2;
                    let natural = th.saturating_sub(anchor + overhead);
                    dh = dh.min(half_h.max(natural));
                }
                let max_avail = self.size().1.saturating_sub(2 + row);
                DialogPlacement {
                    row,
                    granted_rows: dh.min(max_avail),
                }
            });
            return (false, placement);
        }
        // Full mode: skip if nothing changed.
        if !is_dialog && !has_new_blocks && !self.prompt.dirty {
            return (false, None);
        }

        // ── Position cursor ─────────────────────────────────────────────
        let (term_w, term_h) = self.size();
        let explicit_anchor = self.prompt.anchor_row.take();
        let draw_start_row = explicit_anchor.unwrap_or_else(|| self.cursor_y());

        // Initialize cursor tracking for this frame.
        out.init_cursor(draw_start_row, term_w, term_h);
        // Reposition when the prompt was previously drawn (incremental
        // update) OR when an explicit anchor was set (e.g. after
        // redraw/clear/rewind where the cursor may not match the anchor).
        if self.prompt.drawn || explicit_anchor.is_some() {
            out.move_to(0, draw_start_row);
        }
        // NOTE: out.row stays None during history.render so blocks use
        // scroll-mode newline (\r\n → scrollback), not overlay-mode
        // newline (MoveTo).  Set it to Some only after blocks are done.

        // ── Render blocks (scroll mode — commits to scrollback) ─────
        let block_rows = self.history.render(out, width, self.show_thinking);

        // `cursor_row` is ground truth after scroll-mode rendering.
        let base_anchor = out.cursor_row;

        // Switch to overlay positioning for the ephemeral content and
        // dialog that follow.
        if is_dialog {
            out.row = Some(base_anchor);
        }

        // ── Lay out ephemeral overlay (measure only) ────────────────
        let (overlay_flat, overlay_rows) = if has_ephemeral {
            let mut col = SpanCollector::new(width as u16);
            self.render_ephemeral_into(&mut col, width);
            let flat = col.finish();
            let rows = flat.lines.len() as u16;
            (Some(flat), rows)
        } else {
            (None, 0)
        };

        // ── Measure total mutable region ────────────────────────────
        // For constrained dialogs, cap the effective height to
        // max(h/2, natural_space) so the dialog doesn't scroll the
        // viewport more than half the terminal.  Unconstrained dialogs
        // (confirm, question) use their full requested height.
        let raw_dialog_height = dialog_height;
        let unconstrained_prompt_gap: u16 = if self.has_content() { 1 } else { 0 };
        let dialog_height = if is_dialog && self.constrain_dialog {
            dialog_height.map(|dh| {
                let half_h = term_h / 2;
                let overhead = overlay_rows + unconstrained_prompt_gap + 2;
                let natural = term_h.saturating_sub(base_anchor + overhead);
                dh.min(half_h.max(natural))
            })
        } else {
            dialog_height
        };
        // In dialog mode, the gap between content and dialog is only
        // rendered when `dh < term_h - 1 - overlay_end` (see the
        // `gap` computation below).  A fullscreen dialog (one whose
        // requested height + status bar already exceeds the viewport)
        // omits the gap.  The measurement must match, otherwise every
        // redraw of a fullscreen dialog emits a spurious scroll_up(1)
        // and leaks blank rows into scrollback — notably on focus
        // change, which marks the prompt dirty and forces a re-render.
        let prompt_gap: u16 = if is_dialog {
            let dh = dialog_height.unwrap_or(0);
            // Matches `gap` below with the worst-case overlay_end = 0.
            if self.has_content() && dh + 1 < term_h {
                1
            } else {
                0
            }
        } else {
            unconstrained_prompt_gap
        };

        let prompt_height: u16 = if let Some(ref p) = prompt {
            self.measure_prompt_height(p.state, width, p.queued, p.prediction)
        } else {
            // Reserve dialog + bottom gap + status bar.
            let dh = dialog_height.unwrap_or(self.prompt.prev_prompt_ui_rows.max(1));
            // Constrained dialogs leave room for the ephemeral overlay
            // so it doesn't get tail-cropped.  Unconstrained dialogs
            // (confirm, question) take priority over the overlay.
            let cap = if self.constrain_dialog {
                term_h.saturating_sub(overlay_rows)
            } else {
                term_h
            };
            (dh + 2).min(cap)
        };
        // Only count overlay rows that will actually be visible. When
        // an unconstrained dialog fills the terminal, the overlay is
        // fully cropped — including it in total_mutable would scroll
        // extra blank lines into scrollback.
        let viewport_for_overlay = term_h.saturating_sub(prompt_gap + prompt_height);
        let effective_overlay = overlay_rows.min(viewport_for_overlay);
        let total_mutable = effective_overlay + prompt_gap + prompt_height;

        // ── ScrollUp if mutable region overflows viewport ────────
        let _ = out.queue(cursor::MoveTo(0, base_anchor));
        let available = term_h.saturating_sub(base_anchor);
        let scroll_amount = total_mutable.saturating_sub(available);

        // Clear upfront only when scrolling. Without scrolling, each
        // painted line already clears its own trailing residue via
        // newline, and the dialog/prompt cleans up below itself — so
        // skipping the bulk clear avoids a visible blank→repaint flash
        // on terminals with imperfect synchronized-update support.
        let needs_scroll = scroll_amount > 0;
        if needs_scroll {
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
            out.scroll_up(scroll_amount);
            self.has_scrollback = true;
        }
        let final_anchor = base_anchor.saturating_sub(scroll_amount);

        // Switch to absolute positioning. From here on, everything
        // uses MoveTo via `out.row` — position is always exact.
        out.row = Some(final_anchor);
        let _ = out.queue(cursor::MoveTo(0, final_anchor));

        // ── Paint ephemeral overlay ─────────────────────────────────
        let ephemeral_rows: u16 = if let Some(flat) = overlay_flat {
            let theme = crate::theme::snapshot();
            let pctx = PaintContext {
                theme: &theme,
                term_width: width as u16,
            };
            // Tail-crop: if overlay itself exceeds viewport above
            // prompt, drop lines from the head.
            let viewport_for_overlay = term_h.saturating_sub(prompt_gap + prompt_height);
            let crop_head =
                overlay_rows.saturating_sub(viewport_for_overlay.saturating_sub(final_anchor));
            for line in &flat.lines[crop_head as usize..] {
                paint_line(out, line, &pctx);
            }
            overlay_rows.saturating_sub(crop_head)
        } else {
            0
        };

        // ── Render prompt or dialog ─────────────────────────────────
        if let Some(p) = prompt {
            // Gap between content and prompt.
            for _ in 0..prompt_gap {
                out.overlay_newline();
            }

            // `out.row` is set, so all newline calls inside
            // draw_prompt_sections use MoveTo — position is exact.
            let prompt_start_row = out.row.unwrap();
            let available_height = term_h.saturating_sub(prompt_start_row) as usize;
            let new_rows = self.draw_prompt_sections(
                out,
                p.state,
                p.mode,
                width,
                p.queued,
                p.prediction,
                available_height,
            );

            self.prompt.prev_rows = new_rows;
            self.prompt.prev_prompt_ui_rows = new_rows;

            self.prompt.anchor_row = Some(final_anchor);
            self.prompt.prev_dialog_row = Some(final_anchor + ephemeral_rows + prompt_gap);
            self.prompt.drawn = true;
            self.prompt.dirty = false;
            if scroll_amount > 0 {
                self.content_start_row = Some(
                    term_h.saturating_sub(ephemeral_rows + prompt_gap + new_rows + block_rows),
                );
            } else if self.content_start_row.is_none() {
                self.content_start_row = Some(draw_start_row);
            }

            // When the upfront Clear::FromCursorDown was skipped
            // (deferred_clear), erase stale rows that linger below the
            // freshly painted content. SavePosition / RestorePosition
            // preserve the input cursor that draw_prompt_sections placed.
            if !needs_scroll {
                let cleanup = prompt_start_row + new_rows;
                if cleanup < term_h {
                    let _ = out.queue(cursor::SavePosition);
                    let _ = out.queue(cursor::MoveTo(0, cleanup));
                    let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
                    let _ = out.queue(cursor::RestorePosition);
                }
            }

            (false, None)
        } else {
            // ── Dialog mode ─────────────────────────────────────────
            // Gap between chat content and dialog top — mirrors the
            // prompt_gap above, but only when there IS content above
            // AND the dialog doesn't fill the full available space
            // (otherwise the gap wastes a valuable row).
            let has_content_above = block_rows > 0 || ephemeral_rows > 0 || self.has_content();
            let overlay_end = final_anchor + ephemeral_rows;
            let dh = dialog_height.unwrap_or(0);
            let max_no_gap = term_h.saturating_sub(1 + overlay_end);
            let gap: u16 = if has_content_above && dh < max_no_gap {
                out.overlay_newline();
                1
            } else {
                0
            };

            let content_rows = block_rows + ephemeral_rows + gap;
            let dialog_row = overlay_end + gap;
            // Reserve 1 row for the status bar and 1 row for the gap
            // between dialog and status bar (always present).
            let max_available = term_h.saturating_sub(2 + dialog_row);
            let granted_rows = dh.min(max_available);

            self.prompt.anchor_row = Some(final_anchor);
            self.prompt.prev_dialog_row = Some(dialog_row);
            self.prompt.prev_dialog_height = raw_dialog_height.unwrap_or(0);
            self.prompt.prev_dialog_gap = gap;
            self.prompt.prev_rows = ephemeral_rows + gap;
            self.prompt.drawn = true;
            self.prompt.dirty = false;

            let placement = if granted_rows > 0 {
                Some(DialogPlacement {
                    row: dialog_row,
                    granted_rows,
                })
            } else {
                None
            };
            (content_rows > 0, placement)
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
    pub fn draw_viewport_frame(
        &mut self,
        out: &mut RenderOut,
        width: usize,
        prompt: FramePrompt<'_>,
        scroll_offset: u16,
        history_cursor_line: u16,
    ) -> (u16, u16) {
        let _perf = crate::perf::begin("render:viewport_frame");
        self.update_spinner();

        let (term_w, term_h) = self.size();
        out.init_cursor(0, term_w, term_h);

        // Clear full screen.
        out.row = Some(0);
        out.move_to(0, 0);
        let _ = out.queue(terminal::Clear(terminal::ClearType::All));

        // Measure prompt so we know how many rows to reserve at the bottom.
        let prompt_height =
            self.measure_prompt_height(prompt.state, width, prompt.queued, prompt.prediction);
        // One-row gap between transcript and prompt.
        let gap_rows: u16 = 1;
        let viewport_rows = term_h.saturating_sub(prompt_height + gap_rows);

        // Build ephemeral tail (streaming overlays) as a flat DisplayBlock.
        let ephemeral_lines: Vec<crate::render::display::DisplayLine> = if self.has_ephemeral() {
            let mut col = SpanCollector::new(width as u16);
            self.render_ephemeral_into(&mut col, width);
            col.finish().lines
        } else {
            Vec::new()
        };

        // Paint transcript slice (history + ephemeral tail) into the viewport.
        let clamped = self.history.paint_viewport(
            out,
            width,
            self.show_thinking,
            0,
            viewport_rows,
            scroll_offset,
            &ephemeral_lines,
        );

        // Draw the history cursor (tmux-style) when in History focus.
        let clamped_cursor =
            if self.last_app_focus == crate::app::AppFocus::History && viewport_rows > 0 {
                let max_cur = viewport_rows.saturating_sub(1);
                let cur = history_cursor_line.min(max_cur);
                let cursor_row = viewport_rows.saturating_sub(1 + cur);
                out.move_to(0, cursor_row);
                out.push_style(StyleState {
                    fg: Some(Color::Black),
                    bg: Some(theme::accent()),
                    bold: true,
                    ..StyleState::default()
                });
                out.print("\u{258B}"); // ▋
                out.pop_style();
                cur
            } else {
                history_cursor_line
            };

        // Paint prompt stack at the bottom, leaving the gap row blank.
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
        self.history.flushed = self.history.order.len();

        (clamped, clamped_cursor)
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
        // Cache state for dialog-mode status line rendering.
        self.last_vim_enabled = state.vim_enabled();
        self.last_vim_mode = state.vim_mode();
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
            // Match the one-space gutter used for normal input lines so the
            // prediction aligns with where real characters would be typed.
            out.print(" ");
            let max_chars = usable.saturating_sub(1);
            let mut chars = first_line.chars().take(max_chars);
            if let Some(first) = chars.next() {
                let (fg, bg) = cursor_colors();
                out.set_fg(fg);
                out.set_bg(bg);
                out.print(&first.to_string());
                out.reset_style();
                out.push_dim();
                let rest: String = chars.collect();
                out.print(&rest);
                out.pop_style();
            } else {
                draw_soft_cursor(out, 1, cursor_row);
            }
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            out.newline();
        }

        // Compute cumulative display-char offset for each visual line.
        // Must match the counting logic in wrap_and_locate_cursor: each
        // visual line contributes its char count, and each '\n' in the
        // display buffer contributes 1 additional char between logical lines.
        let line_char_offsets = compute_visual_line_offsets(&display_buf, &visual_lines);

        let has_scrollbar = total_content_rows > content_rows && content_rows > 0;
        let (thumb_start, thumb_end) = if has_scrollbar {
            let thumb_size = (content_rows * content_rows / total_content_rows).max(1);
            let max_scroll = total_content_rows - content_rows;
            let thumb_pos = scroll_offset
                .checked_mul(content_rows.saturating_sub(thumb_size))
                .and_then(|v| v.checked_div(max_scroll))
                .unwrap_or(0);
            (thumb_pos, thumb_pos + thumb_size)
        } else {
            (0, 0)
        };

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
            if has_scrollbar {
                let bg = if li >= thumb_start && li < thumb_end {
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
    out.overlay_newline();
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
    out.overlay_newline();
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
                out.overlay_newline();
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
                out.overlay_newline();
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
    out.overlay_newline();
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
                out.overlay_newline();
                rows += 1;
            }

            // Blank line before hint.
            out.overlay_newline();
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
            out.overlay_newline();
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
            out.overlay_newline();
            rows += 1;
        }
    }

    // Blank separator line before the bar.
    out.overlay_newline();
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
        screen.draw_frame(
            &mut out,
            40,
            Some(FramePrompt {
                state: &input,
                mode: protocol::Mode::Normal,
                queued: &[],
                prediction: None,
            }),
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
