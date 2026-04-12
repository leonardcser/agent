use crate::app::AgentToolEntry;
use crate::keymap::{hints, nav_lookup, NavAction};
use crate::render::{crlf, draw_bar, wrap_line};
use crate::utils::format_duration;
use crossterm::event::{KeyCode, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal;
use crossterm::QueueableCommand;
use engine::registry::{AgentStatus, RegistryEntry};
use std::sync::{Arc, Mutex};

use super::{end_dialog_draw, truncate_str, DialogResult, ListState, RenderOut};

/// List-view chrome rows: bar + header + blank + hints. Used as the
/// `overhead` argument to `ListState::new`.
const LIST_OVERHEAD: u16 = 4;

/// Detail-view chrome rows: header(2) + blank + blank + hints. Used
/// when computing how many lines of agent detail content fit.
const DETAIL_CHROME_ROWS: u16 = 5;

/// Minimum number of detail content lines the dialog will show, even
/// on a very short terminal. Below this it would be unreadable.
const MIN_DETAIL_LINES: usize = 3;

/// Maximum detail content lines for an agent at the given terminal
/// height. Caps at half the terminal minus the chrome, with a floor
/// of `MIN_DETAIL_LINES` so the dialog stays usable on short screens.
fn detail_max_lines(term_h: usize) -> usize {
    (term_h / 2)
        .saturating_sub(DETAIL_CHROME_ROWS as usize)
        .max(MIN_DETAIL_LINES)
}

/// Snapshot of a tracked agent's state, passed to the dialog for rendering.
#[derive(Clone)]
pub struct AgentSnapshot {
    pub agent_id: String,
    pub prompt: std::sync::Arc<String>,
    pub tool_calls: Vec<AgentToolEntry>,
    pub cost_usd: f64,
}

/// Shared, live-updating list of agent snapshots.
pub type SharedSnapshots = Arc<Mutex<Vec<AgentSnapshot>>>;

enum View {
    List,
    Detail {
        agent_id: String,
        scroll: usize,
        follow: bool,
    },
}

pub struct AgentsDialog {
    my_pid: u32,
    agents: Vec<RegistryEntry>,
    snapshots: SharedSnapshots,
    list: ListState,
    view: View,
    list_selected: usize,
    vim: bool,
    /// Cached terminal size, updated each draw().
    term_size: (u16, u16),
}

impl AgentsDialog {
    pub fn new(
        my_pid: u32,
        snapshots: SharedSnapshots,
        max_height: Option<u16>,
        vim: bool,
    ) -> Self {
        let agents = Self::fetch(my_pid);
        let list = ListState::new(agents.len().max(1), max_height, LIST_OVERHEAD);
        Self {
            my_pid,
            agents,
            snapshots,
            list,
            view: View::List,
            list_selected: 0,
            vim,
            term_size: terminal::size().unwrap_or((80, 24)),
        }
    }

    fn fetch(my_pid: u32) -> Vec<RegistryEntry> {
        engine::registry::children_of(my_pid)
    }

    fn find_snapshot(&self, agent_id: &str) -> Option<AgentSnapshot> {
        let snaps = self.snapshots.lock().unwrap();
        snaps.iter().find(|s| s.agent_id == agent_id).cloned()
    }

    fn max_detail_lines(&self) -> usize {
        detail_max_lines(self.term_size.1 as usize)
    }

    /// Build the detail view lines for an agent: prompt + tool calls.
    fn detail_lines(snapshot: &AgentSnapshot, width: usize) -> Vec<DetailLine> {
        let mut lines = Vec::new();
        let content_width = width.saturating_sub(2);

        // Prompt section
        lines.push(DetailLine::Label("Prompt:".into()));
        for raw_line in snapshot.prompt.lines() {
            for seg in wrap_line(raw_line, content_width) {
                lines.push(DetailLine::Text(seg));
            }
        }
        lines.push(DetailLine::Blank);

        // Tool calls
        if snapshot.tool_calls.is_empty() {
            lines.push(DetailLine::Text("(no tool calls yet)".into()));
        } else {
            for entry in &snapshot.tool_calls {
                lines.push(DetailLine::ToolCall(entry.clone()));
            }
        }

        lines
    }
}

enum DetailLine {
    Label(String),
    Text(String),
    Blank,
    ToolCall(AgentToolEntry),
}

impl super::Dialog for AgentsDialog {
    fn height(&self) -> u16 {
        match &self.view {
            View::List => self.list.height(self.agents.len().max(1)),
            View::Detail { agent_id, .. } => {
                let w = self.term_size.0 as usize;
                let n = if let Some(ref snap) = self.find_snapshot(agent_id) {
                    Self::detail_lines(snap, w).len()
                } else {
                    1
                };
                let max_content = self.max_detail_lines();
                let content = n.min(max_content) as u16;
                let wanted = content + DETAIL_CHROME_ROWS;
                // Expand to half the terminal when content is scrollable.
                let half = self.term_size.1 / 2;
                if n > max_content {
                    wanted.max(half)
                } else {
                    wanted
                }
            }
        }
    }

    fn mark_dirty(&mut self) {
        self.list.dirty = true;
    }

    fn anchor_row(&self) -> Option<u16> {
        self.list.anchor_row
    }

    fn handle_resize(&mut self) {
        self.list
            .handle_resize(terminal::size().ok().map(|(_, h)| h));
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Option<DialogResult> {
        match &mut self.view {
            View::Detail {
                scroll,
                follow,
                agent_id,
                ..
            } => {
                let (w, h) = (self.term_size.0 as usize, self.term_size.1 as usize);
                let max_vis = detail_max_lines(h);
                let n = {
                    let snaps = self.snapshots.lock().unwrap();
                    if let Some(snap) = snaps.iter().find(|s| s.agent_id == *agent_id) {
                        Self::detail_lines(snap, w).len()
                    } else {
                        0
                    }
                };
                let max_scroll = n.saturating_sub(max_vis);

                match nav_lookup(code, mods) {
                    Some(NavAction::Dismiss) => {
                        self.view = View::List;
                        self.list = ListState::new(self.agents.len().max(1), None, LIST_OVERHEAD);
                        self.list.selected = self.list_selected;
                        self.list.anchor_row = None;
                        None
                    }
                    Some(NavAction::Up) => {
                        *scroll = scroll.saturating_sub(1);
                        *follow = false;
                        self.list.dirty = true;
                        None
                    }
                    Some(NavAction::Down) => {
                        *scroll = (*scroll + 1).min(max_scroll);
                        *follow = *scroll >= max_scroll;
                        self.list.dirty = true;
                        None
                    }
                    Some(NavAction::PageUp) => {
                        *scroll = scroll.saturating_sub(max_vis / 2);
                        *follow = false;
                        self.list.dirty = true;
                        None
                    }
                    Some(NavAction::PageDown) => {
                        *scroll = (*scroll + max_vis / 2).min(max_scroll);
                        *follow = *scroll >= max_scroll;
                        self.list.dirty = true;
                        None
                    }
                    _ => None,
                }
            }
            View::List => {
                if code == KeyCode::Backspace {
                    if let Some(agent) = self.agents.get(self.list.selected) {
                        let pid = agent.pid;
                        if engine::registry::is_in_tree(pid, self.my_pid) {
                            engine::registry::kill_agent(pid);
                            self.agents = Self::fetch(self.my_pid);
                            self.list.set_items(self.agents.len().max(1));
                        }
                    }
                    return None;
                }

                if code == KeyCode::Enter {
                    if let Some(agent) = self.agents.get(self.list.selected) {
                        self.list_selected = self.list.selected;
                        self.view = View::Detail {
                            agent_id: agent.agent_id.clone(),
                            scroll: 0,
                            follow: true,
                        };
                        // Reset anchor so the dialog can reposition at the new size.
                        self.list.anchor_row = None;
                        self.list.dirty = true;
                    }
                    return None;
                }

                let n = self.agents.len();
                match nav_lookup(code, mods) {
                    Some(NavAction::Dismiss) => Some(DialogResult::AgentsClosed),
                    Some(NavAction::Up) => {
                        self.list.select_prev(n);
                        None
                    }
                    Some(NavAction::Down) => {
                        self.list.select_next(n);
                        None
                    }
                    Some(NavAction::PageUp) => {
                        self.list.page_up();
                        None
                    }
                    Some(NavAction::PageDown) => {
                        self.list.page_down(n);
                        None
                    }
                    _ => None,
                }
            }
        }
    }

    fn draw(&mut self, out: &mut RenderOut, start_row: u16, width: u16, height: u16) {
        self.term_size = (width, height);
        match &self.view {
            View::Detail {
                agent_id,
                scroll,
                follow,
            } => {
                let agent_id = agent_id.clone();
                let mut scroll = *scroll;
                let follow = *follow;
                let w = self.term_size.0 as usize;

                let snapshot = self.find_snapshot(&agent_id);
                let lines = if let Some(ref snap) = snapshot {
                    Self::detail_lines(snap, w)
                } else {
                    vec![DetailLine::Text("(agent not tracked)".into())]
                };

                let max_vis = self.max_detail_lines();
                let total = lines.len();
                let max_scroll = total.saturating_sub(max_vis);

                if follow {
                    scroll = max_scroll;
                }
                scroll = scroll.min(max_scroll);

                let visible = total.min(max_vis);

                let Some((w, _)) = self
                    .list
                    .begin_draw(out, start_row, visible + 2, width, height)
                else {
                    return;
                };

                draw_bar(out, w, None, None, crate::theme::AGENT);
                crlf(out);

                // Header: agent name + slug
                let _ = out.queue(Print(" "));
                out.push_style(crate::render::StyleState {
                    fg: Some(crate::theme::AGENT),
                    bold: true,
                    ..Default::default()
                });
                let _ = out.queue(Print(&agent_id));
                out.pop_style();

                // Find status from registry entries
                if let Some(entry) = self.agents.iter().find(|a| a.agent_id == agent_id) {
                    match entry.status {
                        AgentStatus::Working => {}
                        AgentStatus::Idle => {
                            out.push_fg(crate::theme::SUCCESS);
                            let _ = out.queue(Print(" \u{2713}"));
                            out.pop_style();
                        }
                    }
                    if let Some(ref slug) = entry.task_slug {
                        out.push_dim();
                        let _ = out.queue(Print(format!(" \u{00b7} {slug}")));
                        out.pop_style();
                    }
                }
                if let Some(ref snap) = snapshot {
                    if snap.cost_usd > 0.0 {
                        out.push_dim();
                        let _ = out.queue(Print(format!(
                            "  {}",
                            crate::metrics::format_cost(snap.cost_usd)
                        )));
                        out.pop_style();
                    }
                }
                crlf(out);
                crlf(out);

                // Content
                for line in lines.iter().skip(scroll).take(visible) {
                    match line {
                        DetailLine::Label(text) => {
                            out.push_dim();
                            let _ = out.queue(Print(format!("  {text}")));
                            out.pop_style();
                            crlf(out);
                        }
                        DetailLine::Text(text) => {
                            let _ = out.queue(Print(format!(
                                "   {}",
                                truncate_str(text, w.saturating_sub(4))
                            )));
                            crlf(out);
                        }
                        DetailLine::Blank => {
                            crlf(out);
                        }
                        DetailLine::ToolCall(entry) => {
                            let _ = out.queue(Print("  "));
                            out.push_dim();
                            let _ = out.queue(Print(&entry.tool_name));
                            out.pop_style();
                            let max_summary = w.saturating_sub(5 + entry.tool_name.len());
                            let _ = out.queue(Print(format!(
                                " {}",
                                truncate_str(&entry.summary, max_summary)
                            )));
                            if let Some(d) = entry.elapsed {
                                if d.as_secs_f64() >= 0.1 {
                                    out.push_dim();
                                    let _ = out.queue(Print(format!(
                                        "  {}",
                                        format_duration(d.as_secs())
                                    )));
                                    out.pop_style();
                                }
                            }
                            crlf(out);
                        }
                    }
                }

                // Hints
                crlf(out);
                out.push_dim();
                let can_scroll = total > max_vis;
                if can_scroll {
                    let end = (scroll + visible).min(total);
                    let _ = out.queue(Print(format!(
                        " [{end}/{total}]  {}  {}  {}",
                        hints::nav(self.vim),
                        hints::scroll(self.vim),
                        hints::BACK,
                    )));
                } else {
                    let _ = out.queue(Print(&hints::join(&[hints::BACK])));
                }
                out.pop_style();
                end_dialog_draw(out);

                self.view = View::Detail {
                    agent_id,
                    scroll,
                    follow,
                };
            }
            View::List => {
                let fresh = Self::fetch(self.my_pid);
                if fresh.len() != self.agents.len() {
                    self.list.set_items(fresh.len().max(1));
                }
                self.agents = fresh;

                let Some((w, _)) =
                    self.list
                        .begin_draw(out, start_row, self.agents.len().max(1), width, height)
                else {
                    return;
                };

                draw_bar(out, w, None, None, crate::theme::AGENT);
                crlf(out);

                out.push_dim();
                let _ = out.queue(Print(" Agents"));
                out.pop_style();
                crlf(out);

                if self.agents.is_empty() {
                    out.push_dim();
                    let _ = out.queue(Print("  No subagents running"));
                    out.pop_style();
                    crlf(out);
                } else {
                    let name_w = self
                        .agents
                        .iter()
                        .map(|a| a.agent_id.len())
                        .max()
                        .unwrap_or(0);
                    let range = self.list.visible_range(self.agents.len());
                    for (i, agent) in self
                        .agents
                        .iter()
                        .enumerate()
                        .take(range.end)
                        .skip(range.start)
                    {
                        let status_str = match agent.status {
                            AgentStatus::Working => "working",
                            AgentStatus::Idle => "idle   ",
                        };

                        let _ = out.queue(Print("  "));
                        let padded_name = format!("{:<name_w$}", agent.agent_id);
                        if i == self.list.selected {
                            out.push_style(crate::render::StyleState {
                                fg: Some(crate::theme::AGENT),
                                bold: true,
                                ..Default::default()
                            });
                            let _ = out.queue(Print(&padded_name));
                            out.pop_style();
                        } else {
                            let _ = out.queue(Print(&padded_name));
                        }
                        out.push_dim();
                        let _ = out.queue(Print(format!("  {status_str}")));
                        out.pop_style();
                        if let Some(slug) = &agent.task_slug {
                            let max = w.saturating_sub(name_w + 12);
                            let _ = out.queue(Print(format!("  {}", truncate_str(slug, max))));
                        }
                        if let Some(snap) = self.find_snapshot(&agent.agent_id) {
                            if snap.cost_usd > 0.0 {
                                out.push_dim();
                                let _ = out.queue(Print(format!(
                                    "  {}",
                                    crate::metrics::format_cost(snap.cost_usd)
                                )));
                                out.pop_style();
                            }
                        }
                        crlf(out);
                    }
                }

                crlf(out);
                out.push_dim();
                let _ = out.queue(Print(&hints::join(&[
                    "enter: view",
                    hints::KILL_PROC,
                    hints::CLOSE,
                ])));
                out.pop_style();
                end_dialog_draw(out);
            }
        }
    }
}
