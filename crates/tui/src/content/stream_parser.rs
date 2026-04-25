//! Streaming input adapter.
//!
//! `StreamParser` accumulates character-level deltas from the engine,
//! detects structural boundaries (paragraphs, code blocks, tables),
//! and writes finished blocks into a `BlockHistory`. It is transient
//! input state — only alive while the engine is streaming.
//!
//! `App` owns a `StreamParser` alongside the `Transcript` (block
//! store + snapshot cache). The dependency is one-way: parser writes
//! into `BlockHistory`, never reads the snapshot.

use super::is_table_separator;
use crate::app::transcript_model::{
    ActiveAgent, ActiveText, ActiveThinking, ActiveTool, AgentBlockStatus, Block, BlockHistory,
    BlockId, Status, ToolOutput, ToolOutputRef, ToolState, ToolStatus,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub(crate) struct StreamParser {
    active_thinking: Option<ActiveThinking>,
    active_text: Option<ActiveText>,
    stream_exec_id: Option<BlockId>,
    active_tools: Vec<ActiveTool>,
    active_agents: Vec<ActiveAgent>,
}

impl StreamParser {
    pub fn new() -> Self {
        Self {
            active_thinking: None,
            active_text: None,
            stream_exec_id: None,
            active_tools: Vec::new(),
            active_agents: Vec::new(),
        }
    }

    pub fn clear(&mut self) {
        self.active_thinking = None;
        self.active_text = None;
        self.active_tools.clear();
        self.active_agents.clear();
        self.stream_exec_id = None;
    }

    pub fn begin_turn(&mut self) {
        self.active_tools.clear();
    }

    pub fn has_active_exec(&self) -> bool {
        self.stream_exec_id.is_some()
    }

    pub fn has_active_thinking(&self) -> bool {
        self.active_thinking.is_some()
    }

    pub fn active_thinking(&self) -> Option<&ActiveThinking> {
        self.active_thinking.as_ref()
    }

    pub fn clear_tools_and_agents(&mut self) {
        self.active_tools.clear();
        self.active_agents.clear();
    }

    // ── Streaming thinking ──────────────────────────────────────────

    pub fn append_streaming_thinking(&mut self, history: &mut BlockHistory, delta: &str) {
        let at = self.active_thinking.get_or_insert_with(|| ActiveThinking {
            current_line: String::new(),
            paragraph: String::new(),
            streaming_id: None,
        });

        for ch in delta.chars() {
            if ch == '\r' {
                continue;
            }
            if ch == '\n' {
                let line = std::mem::take(&mut at.current_line);
                if !at.paragraph.is_empty() {
                    at.paragraph.push('\n');
                }
                at.paragraph.push_str(&line);
            } else {
                at.current_line.push(ch);
            }
        }
        let preview = match (at.paragraph.is_empty(), at.current_line.is_empty()) {
            (true, true) => None,
            (true, false) => Some(at.current_line.clone()),
            (false, true) => Some(at.paragraph.clone()),
            (false, false) => Some(format!("{}\n{}", at.paragraph, at.current_line)),
        };
        if let Some(content) = preview.filter(|t| !t.trim().is_empty()) {
            let block = Block::Thinking { content };
            if let Some(id) = at.streaming_id {
                history.rewrite(id, block);
            } else {
                let id = history.push(block);
                history.set_status(id, Status::Streaming);
                at.streaming_id = Some(id);
            }
        }
    }

    pub fn flush_streaming_thinking(&mut self, history: &mut BlockHistory) {
        if let Some(mut at) = self.active_thinking.take() {
            if !at.current_line.is_empty() {
                if !at.paragraph.is_empty() {
                    at.paragraph.push('\n');
                }
                at.paragraph.push_str(&at.current_line);
            }
            let trimmed = at.paragraph.trim().to_string();
            if let Some(id) = at.streaming_id {
                if trimmed.is_empty() {
                    history.rewrite(
                        id,
                        Block::Thinking {
                            content: String::new(),
                        },
                    );
                } else {
                    history.rewrite(id, Block::Thinking { content: trimmed });
                }
                history.set_status(id, Status::Done);
            } else if !trimmed.is_empty() {
                history.push(Block::Thinking { content: trimmed });
            }
        }
    }

    // ── Streaming text ──────────────────────────────────────────────

    pub fn append_streaming_text(&mut self, history: &mut BlockHistory, delta: &str) {
        if self.active_thinking.is_some() {
            self.flush_streaming_thinking(history);
        }

        let at = self.active_text.get_or_insert_with(|| ActiveText {
            current_line: String::new(),
            paragraph: String::new(),
            in_code_block: None,
            table_rows: Vec::new(),
            table_data_rows: 0,
            streaming_id: None,
            table_streaming_id: None,
            code_line_streaming_id: None,
        });

        for ch in delta.chars() {
            if ch == '\r' {
                continue;
            }
            if ch == '\n' {
                let line = std::mem::take(&mut at.current_line);
                Self::process_text_line(history, at, &line);
            } else {
                at.current_line.push(ch);
            }
        }
        Self::sync_streaming_text(history, at);
    }

    fn sync_streaming_text(history: &mut BlockHistory, at: &mut ActiveText) {
        if let Some(ref lang) = at.in_code_block {
            if !at.current_line.is_empty() {
                let block = Block::CodeLine {
                    content: at.current_line.clone(),
                    lang: lang.clone(),
                };
                if let Some(id) = at.code_line_streaming_id {
                    history.rewrite(id, block);
                } else {
                    let id = history.push(block);
                    history.set_status(id, Status::Streaming);
                    at.code_line_streaming_id = Some(id);
                }
            }
            return;
        }
        let in_table = !at.table_rows.is_empty() || at.current_line.trim_start().starts_with('|');
        if in_table {
            let mut content = String::new();
            for row in &at.table_rows {
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str(row);
            }
            if at.current_line.trim_start().starts_with('|') {
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str(&at.current_line);
            }
            if content.is_empty() {
                return;
            }
            let block = Block::Text { content };
            if let Some(id) = at.table_streaming_id {
                history.rewrite(id, block);
            } else {
                let id = history.push(block);
                history.set_status(id, Status::Streaming);
                at.table_streaming_id = Some(id);
            }
            return;
        }
        let preview = match (at.paragraph.is_empty(), at.current_line.is_empty()) {
            (true, true) => None,
            (true, false) => Some(at.current_line.clone()),
            (false, true) => Some(at.paragraph.clone()),
            (false, false) => Some(format!("{}\n{}", at.paragraph, at.current_line)),
        };
        let Some(content) = preview.filter(|t| !t.trim().is_empty()) else {
            return;
        };
        let block = Block::Text { content };
        if let Some(id) = at.streaming_id {
            history.rewrite(id, block);
        } else {
            let id = history.push(block);
            history.set_status(id, Status::Streaming);
            at.streaming_id = Some(id);
        }
    }

    fn process_text_line(history: &mut BlockHistory, at: &mut ActiveText, line: &str) {
        let trimmed = line.trim_start();

        if trimmed.starts_with("```") {
            if at.in_code_block.is_some() {
                if let Some(id) = at.code_line_streaming_id.take() {
                    history.set_status(id, Status::Done);
                }
                at.in_code_block = None;
                return;
            } else {
                Self::flush_paragraph(history, at);
                Self::flush_table(history, at);
                let lang = trimmed.trim_start_matches('`').trim().to_string();
                at.in_code_block = Some(lang);
                return;
            }
        }

        if let Some(ref lang) = at.in_code_block {
            let block = Block::CodeLine {
                content: line.to_string(),
                lang: lang.clone(),
            };
            if let Some(id) = at.code_line_streaming_id.take() {
                history.rewrite(id, block);
                history.set_status(id, Status::Done);
            } else {
                history.push(block);
            }
            return;
        }

        if trimmed.starts_with('|') {
            Self::flush_paragraph(history, at);
            if !is_table_separator(line) {
                at.table_data_rows += 1;
            }
            at.table_rows.push(line.to_string());
            return;
        }

        if line.trim().is_empty() {
            if !at.table_rows.is_empty() {
                return;
            }
            if !at.paragraph.is_empty() {
                Self::flush_paragraph(history, at);
            }
            return;
        }

        Self::flush_table(history, at);

        if !at.paragraph.is_empty() {
            at.paragraph.push('\n');
        }
        at.paragraph.push_str(line);
    }

    fn flush_table(history: &mut BlockHistory, at: &mut ActiveText) {
        if !at.table_rows.is_empty() {
            let content = std::mem::take(&mut at.table_rows).join("\n");
            if let Some(id) = at.table_streaming_id.take() {
                history.rewrite(id, Block::Text { content });
                history.set_status(id, Status::Done);
            } else {
                history.push(Block::Text { content });
            }
            at.table_data_rows = 0;
        } else if let Some(id) = at.table_streaming_id.take() {
            history.set_status(id, Status::Done);
        }
    }

    fn flush_paragraph(history: &mut BlockHistory, at: &mut ActiveText) {
        let para = std::mem::take(&mut at.paragraph);
        let trimmed = para.trim().to_string();
        if let Some(id) = at.streaming_id.take() {
            if trimmed.is_empty() {
                history.rewrite(
                    id,
                    Block::Text {
                        content: String::new(),
                    },
                );
            } else {
                history.rewrite(id, Block::Text { content: trimmed });
            }
            history.set_status(id, Status::Done);
        } else if !trimmed.is_empty() {
            history.push(Block::Text { content: trimmed });
        }
    }

    pub fn flush_streaming_text(&mut self, history: &mut BlockHistory) {
        self.flush_streaming_thinking(history);
        if let Some(mut at) = self.active_text.take() {
            if at.in_code_block.is_some() {
                if at.current_line.trim_start().starts_with("```") {
                    at.current_line.clear();
                    if let Some(id) = at.code_line_streaming_id.take() {
                        history.set_status(id, Status::Done);
                    }
                } else if !at.current_line.is_empty() {
                    let lang = at.in_code_block.as_ref().unwrap().clone();
                    let block = Block::CodeLine {
                        content: std::mem::take(&mut at.current_line),
                        lang,
                    };
                    if let Some(id) = at.code_line_streaming_id.take() {
                        history.rewrite(id, block);
                        history.set_status(id, Status::Done);
                    } else {
                        history.push(block);
                    }
                } else if let Some(id) = at.code_line_streaming_id.take() {
                    history.set_status(id, Status::Done);
                }
                at.in_code_block = None;
            }
            if !at.current_line.is_empty() && at.current_line.trim_start().starts_with('|') {
                at.table_rows.push(std::mem::take(&mut at.current_line));
            }
            Self::flush_table(history, &mut at);
            if !at.current_line.is_empty() {
                if !at.paragraph.is_empty() {
                    at.paragraph.push('\n');
                }
                at.paragraph.push_str(&at.current_line);
            }
            Self::flush_paragraph(history, &mut at);
        }
    }

    // ── Tool lifecycle ──────────────────────────────────────────────

    pub fn start_tool(
        &mut self,
        history: &mut BlockHistory,
        call_id: String,
        name: String,
        summary: String,
        args: HashMap<String, serde_json::Value>,
    ) {
        let start_time = Instant::now();
        let block = Block::ToolCall {
            call_id: call_id.clone(),
            name: name.clone(),
            summary,
            args,
        };
        let state = ToolState {
            status: ToolStatus::Pending,
            elapsed: None,
            output: None,
            user_message: None,
        };
        let block_id = history.push_with_state(block, call_id.clone(), state);
        history.set_status(block_id, Status::Streaming);
        self.active_tools.push(ActiveTool {
            call_id,
            name,
            block_id,
            start_time,
        });
    }

    fn resolve_active_call_id(&self, history: &BlockHistory, call_id: &str) -> Option<String> {
        if !call_id.is_empty() {
            return Some(call_id.to_string());
        }
        self.active_tools
            .last()
            .map(|t| t.call_id.clone())
            .or_else(|| Self::last_tool_call_id(history))
    }

    fn last_tool_call_id(history: &BlockHistory) -> Option<String> {
        history
            .order
            .iter()
            .rev()
            .find_map(|id| match history.blocks.get(id) {
                Some(Block::ToolCall { call_id, .. }) => Some(call_id.clone()),
                _ => None,
            })
    }

    pub fn append_active_output(&mut self, history: &mut BlockHistory, call_id: &str, chunk: &str) {
        let Some(cid) = self.resolve_active_call_id(history, call_id) else {
            return;
        };
        let chunk = chunk.to_string();
        Self::update_tool_state(history, &cid, move |state| match state.output {
            Some(ref mut out) => {
                if !out.content.is_empty() {
                    out.content.push('\n');
                }
                out.content.push_str(&chunk);
            }
            None => {
                state.output = Some(Box::new(ToolOutput {
                    content: chunk,
                    is_error: false,
                    metadata: None,
                    render_cache: None,
                }));
            }
        });
    }

    pub fn set_active_status(
        &mut self,
        history: &mut BlockHistory,
        call_id: &str,
        status: ToolStatus,
    ) {
        let Some(cid) = self.resolve_active_call_id(history, call_id) else {
            return;
        };
        if let Some(active) = self.active_tools.iter_mut().find(|t| t.call_id == cid) {
            if matches!(
                history.tool_states.get(&cid).map(|s| s.status),
                Some(ToolStatus::Confirm)
            ) && status == ToolStatus::Pending
            {
                active.start_time = Instant::now();
            }
        }
        Self::update_tool_state(history, &cid, |state| state.status = status);
    }

    pub fn set_active_user_message(
        &mut self,
        history: &mut BlockHistory,
        call_id: &str,
        msg: String,
    ) {
        let Some(cid) = self.resolve_active_call_id(history, call_id) else {
            return;
        };
        Self::update_tool_state(history, &cid, |state| state.user_message = Some(msg));
    }

    pub fn finish_tool(
        &mut self,
        history: &mut BlockHistory,
        call_id: &str,
        status: ToolStatus,
        output: Option<ToolOutputRef>,
        engine_elapsed: Option<Duration>,
    ) {
        let Some(cid) = self.resolve_active_call_id(history, call_id) else {
            return;
        };
        let active_idx = self.active_tools.iter().position(|t| t.call_id == cid);
        let elapsed = if status == ToolStatus::Denied {
            None
        } else if let Some(idx) = active_idx {
            let tool = &self.active_tools[idx];
            engine_elapsed.or_else(|| tool.elapsed())
        } else {
            engine_elapsed
        };
        Self::update_tool_state(history, &cid, |state| {
            state.status = status;
            if let Some(out) = output {
                state.output = Some(out);
            }
            state.elapsed = elapsed;
        });
        if let Some(idx) = active_idx {
            let block_id = self.active_tools[idx].block_id;
            self.active_tools.remove(idx);
            history.set_status(block_id, Status::Done);
        }
    }

    pub fn finalize_active_tools(&mut self, history: &mut BlockHistory) {
        self.finalize_active_tools_as(history, ToolStatus::Err);
    }

    pub fn finalize_active_tools_as(&mut self, history: &mut BlockHistory, status: ToolStatus) {
        self.finish_all_active_agents(history);
        let tools: Vec<ActiveTool> = self.active_tools.drain(..).collect();
        for tool in tools {
            let elapsed = if status == ToolStatus::Denied {
                None
            } else {
                tool.elapsed()
            };
            history.set_status(tool.block_id, Status::Done);
            let cid = tool.call_id.clone();
            Self::update_tool_state(history, &cid, |state| {
                state.status = status;
                state.elapsed = elapsed;
            });
        }
    }

    fn update_tool_state(
        history: &mut BlockHistory,
        call_id: &str,
        mutator: impl FnOnce(&mut ToolState),
    ) {
        let Some(state) = history.tool_states.get_mut(call_id) else {
            return;
        };
        mutator(state);
        if let Some(id) = history.tool_block_id(call_id) {
            history.invalidate_block_layout(id);
        }
    }

    // ── Exec lifecycle ──────────────────────────────────────────────

    pub fn start_exec(&mut self, history: &mut BlockHistory, command: String) {
        let id = history.push(Block::Exec {
            command,
            output: String::new(),
        });
        history.set_status(id, Status::Streaming);
        self.stream_exec_id = Some(id);
    }

    pub fn append_exec_output(&mut self, history: &mut BlockHistory, chunk: &str) {
        let Some(id) = self.stream_exec_id else {
            return;
        };
        let Some(Block::Exec { command, output }) = history.blocks.get(&id).cloned() else {
            return;
        };
        let mut new_output = output;
        if !new_output.is_empty() && !new_output.ends_with('\n') {
            new_output.push('\n');
        }
        new_output.push_str(chunk);
        history.rewrite(
            id,
            Block::Exec {
                command,
                output: new_output,
            },
        );
    }

    pub fn finish_exec(&mut self, _exit_code: Option<i32>) {}

    pub fn finalize_exec(&mut self, history: &mut BlockHistory) {
        let Some(id) = self.stream_exec_id.take() else {
            return;
        };
        if let Some(Block::Exec { command, output }) = history.blocks.get(&id).cloned() {
            let mut trimmed = output;
            trimmed.truncate(trimmed.trim_end().len());
            history.rewrite(
                id,
                Block::Exec {
                    command,
                    output: trimmed,
                },
            );
        }
        history.set_status(id, Status::Done);
    }

    // ── Agent lifecycle ─────────────────────────────────────────────

    pub fn start_active_agent(&mut self, history: &mut BlockHistory, agent_id: String) {
        let start_time = Instant::now();
        let block = Block::Agent {
            agent_id: agent_id.clone(),
            slug: None,
            blocking: true,
            tool_calls: Vec::new(),
            status: AgentBlockStatus::Running,
            elapsed: Some(Duration::from_secs(0)),
        };
        let block_id = history.push(block);
        history.set_status(block_id, Status::Streaming);
        self.active_agents.push(ActiveAgent {
            agent_id,
            block_id,
            start_time,
            final_elapsed: None,
        });
    }

    pub fn update_active_agent(
        &mut self,
        history: &mut BlockHistory,
        agent_id: &str,
        slug: Option<&str>,
        tool_calls: &[crate::app::AgentToolEntry],
        status: AgentBlockStatus,
    ) {
        let (block_id, elapsed) = {
            let Some(active) = self
                .active_agents
                .iter_mut()
                .find(|a| a.agent_id == agent_id)
            else {
                return;
            };
            if status != AgentBlockStatus::Running && active.final_elapsed.is_none() {
                active.final_elapsed = Some(active.start_time.elapsed());
            }
            let elapsed = active
                .final_elapsed
                .unwrap_or_else(|| active.start_time.elapsed());
            (active.block_id, elapsed)
        };
        history.rewrite(
            block_id,
            Block::Agent {
                agent_id: agent_id.to_string(),
                slug: slug.map(str::to_string),
                blocking: true,
                tool_calls: tool_calls.to_vec(),
                status,
                elapsed: Some(elapsed),
            },
        );
    }

    pub fn cancel_active_agents(&mut self, history: &mut BlockHistory) {
        type AgentCancel = (
            BlockId,
            String,
            Duration,
            Vec<crate::app::AgentToolEntry>,
            Option<String>,
        );
        let updates: Vec<AgentCancel> = self
            .active_agents
            .iter_mut()
            .map(|a| {
                if a.final_elapsed.is_none() {
                    a.final_elapsed = Some(a.start_time.elapsed());
                }
                let elapsed = a.final_elapsed.unwrap_or_else(|| a.start_time.elapsed());
                let (slug, tool_calls) = match history.blocks.get(&a.block_id) {
                    Some(Block::Agent {
                        slug, tool_calls, ..
                    }) => (slug.clone(), tool_calls.clone()),
                    _ => (None, Vec::new()),
                };
                (a.block_id, a.agent_id.clone(), elapsed, tool_calls, slug)
            })
            .collect();
        for (block_id, agent_id, elapsed, tool_calls, slug) in updates {
            history.rewrite(
                block_id,
                Block::Agent {
                    agent_id,
                    slug,
                    blocking: true,
                    tool_calls,
                    status: AgentBlockStatus::Error,
                    elapsed: Some(elapsed),
                },
            );
        }
    }

    pub fn finish_active_agent(&mut self, history: &mut BlockHistory, agent_id: &str) {
        let Some(idx) = self
            .active_agents
            .iter()
            .position(|a| a.agent_id == agent_id)
        else {
            return;
        };
        let mut active = self.active_agents.remove(idx);
        if active.final_elapsed.is_none() {
            active.final_elapsed = Some(active.start_time.elapsed());
        }
        let elapsed = active
            .final_elapsed
            .unwrap_or_else(|| active.start_time.elapsed());
        let (slug, tool_calls, status) = match history.blocks.get(&active.block_id) {
            Some(Block::Agent {
                slug,
                tool_calls,
                status,
                ..
            }) => {
                let next = if *status == AgentBlockStatus::Running {
                    AgentBlockStatus::Done
                } else {
                    *status
                };
                (slug.clone(), tool_calls.clone(), next)
            }
            _ => (None, Vec::new(), AgentBlockStatus::Done),
        };
        history.rewrite(
            active.block_id,
            Block::Agent {
                agent_id: active.agent_id,
                slug,
                blocking: true,
                tool_calls,
                status,
                elapsed: Some(elapsed),
            },
        );
        history.set_status(active.block_id, Status::Done);
    }

    pub fn finish_all_active_agents(&mut self, history: &mut BlockHistory) {
        let ids: Vec<String> = self
            .active_agents
            .iter()
            .map(|a| a.agent_id.clone())
            .collect();
        for id in ids {
            self.finish_active_agent(history, &id);
        }
    }

    pub fn tick_active_agents(&mut self, history: &mut BlockHistory) {
        let ticks: Vec<(BlockId, Duration)> = self
            .active_agents
            .iter()
            .filter(|a| a.final_elapsed.is_none())
            .map(|a| (a.block_id, a.start_time.elapsed()))
            .collect();
        for (block_id, elapsed) in ticks {
            let Some(Block::Agent {
                agent_id,
                slug,
                tool_calls,
                status,
                ..
            }) = history.blocks.get(&block_id).cloned()
            else {
                continue;
            };
            history.rewrite(
                block_id,
                Block::Agent {
                    agent_id,
                    slug,
                    blocking: true,
                    tool_calls,
                    status,
                    elapsed: Some(elapsed),
                },
            );
        }
    }
}
