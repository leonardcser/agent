use super::{
    begin_dialog_draw, finish_dialog_frame, render_inline_textarea, wrap_line, DialogResult,
    TextArea,
};
use crate::keymap::{hints, nav_lookup, NavAction};
use crate::render::{draw_bar, RenderOut};
use crate::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use crossterm::terminal;
use crossterm::QueueableCommand;
use std::collections::HashMap;

#[derive(Clone)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Clone)]
pub struct Question {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
}

/// Parse questions from tool call args JSON.
pub fn parse_questions(args: &HashMap<String, serde_json::Value>) -> Vec<Question> {
    let Some(qs) = args.get("questions").and_then(|v| v.as_array()) else {
        return vec![];
    };
    qs.iter()
        .filter_map(|q| {
            let question = q.get("question")?.as_str()?.to_string();
            let header = q.get("header")?.as_str()?.to_string();
            let multi_select = q
                .get("multiSelect")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let options = q
                .get("options")?
                .as_array()?
                .iter()
                .filter_map(|o| {
                    let label = o.get("label")?.as_str()?.to_string();
                    // Strip "Other" option if LLM incorrectly included it
                    if label.eq_ignore_ascii_case("other") {
                        return None;
                    }
                    Some(QuestionOption {
                        label,
                        description: o.get("description")?.as_str()?.to_string(),
                    })
                })
                .collect();
            Some(Question {
                question,
                header,
                options,
                multi_select,
            })
        })
        .collect()
}

/// Non-blocking question dialog state machine.
pub struct QuestionDialog {
    questions: Vec<Question>,
    has_tabs: bool,
    active_tab: usize,
    selections: Vec<usize>,
    multi_toggles: Vec<Vec<bool>>,
    other_areas: Vec<TextArea>,
    kill_ring: String,
    editing_other: Vec<bool>,
    visited: Vec<bool>,
    answered: Vec<bool>,
    dirty: bool,
    request_id: u64,
    /// Cached terminal size, updated on resize.
    term_size: (u16, u16),
}

impl QuestionDialog {
    pub fn new(questions: Vec<Question>, request_id: u64) -> Self {
        let n = questions.len();
        let has_tabs = n > 1;
        Self {
            multi_toggles: questions
                .iter()
                .map(|q| vec![false; q.options.len() + 1])
                .collect(),
            questions,
            has_tabs,
            active_tab: 0,
            selections: vec![0; n],
            other_areas: (0..n).map(|_| TextArea::new()).collect(),
            kill_ring: String::new(),
            editing_other: vec![false; n],
            visited: vec![false; n],
            answered: vec![false; n],
            dirty: true,
            request_id,
            term_size: terminal::size().unwrap_or((80, 24)),
        }
    }

    fn content_rows(&self, width: u16) -> u16 {
        let w = width as usize;
        let ta = &self.other_areas[self.active_tab];
        let ta_visible = self.editing_other[self.active_tab] || !ta.is_empty();
        let q_other_idx = self.questions[self.active_tab].options.len();
        let other_text_col: u16 = if self.questions[self.active_tab].multi_select {
            2 + 2 + 5 + 2
        } else {
            let digits = format!("{}", q_other_idx + 1).len();
            (2 + digits + 2 + 5 + 2) as u16
        };
        let other_wrap_w = width.saturating_sub(other_text_col) as usize;
        let q = &self.questions[self.active_tab];
        let ta_extra: u16 = if ta_visible {
            self.other_areas[self.active_tab]
                .visual_row_count(other_wrap_w)
                .saturating_sub(1)
        } else {
            0
        };
        let q_segments = wrap_line(&q.question, w.saturating_sub(1)).len() as u16;
        // bar(1) + tabs?(1) + blank(1) + question + blank(1) + options + other(1) + ta_extra + blank(1) + footer(1)
        1 + if self.has_tabs { 1 } else { 0 }
            + 1
            + q_segments
            + 1
            + q.options.len() as u16
            + 1
            + ta_extra
            + 1
            + 1
    }

    fn build_answer(&self) -> String {
        let mut answers = serde_json::Map::new();
        for (i, q) in self.questions.iter().enumerate() {
            let other_idx = q.options.len();
            let other_text = self.other_areas[i].text();
            let answer = if q.multi_select {
                let mut selected: Vec<String> = Vec::new();
                for (j, toggled) in self.multi_toggles[i].iter().enumerate() {
                    if *toggled {
                        if j == other_idx {
                            selected.push(format!("Other: {other_text}"));
                        } else {
                            selected.push(q.options[j].label.clone());
                        }
                    }
                }
                if selected.is_empty() {
                    if self.selections[i] == other_idx {
                        serde_json::Value::String(format!("Other: {other_text}"))
                    } else {
                        serde_json::Value::String(q.options[self.selections[i]].label.clone())
                    }
                } else {
                    serde_json::Value::Array(
                        selected
                            .into_iter()
                            .map(serde_json::Value::String)
                            .collect(),
                    )
                }
            } else if self.selections[i] == other_idx {
                serde_json::Value::String(format!("Other: {other_text}"))
            } else {
                serde_json::Value::String(q.options[self.selections[i]].label.clone())
            };
            answers.insert(q.question.clone(), answer);
        }
        serde_json::Value::Object(answers).to_string()
    }
}

impl super::Dialog for QuestionDialog {
    fn blocks_agent(&self) -> bool {
        true
    }

    fn height(&self) -> u16 {
        self.content_rows(self.term_size.0)
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn handle_resize(&mut self) {
        self.term_size = terminal::size().unwrap_or(self.term_size);
        self.dirty = true;
    }

    fn set_kill_ring(&mut self, contents: String) {
        self.kill_ring = contents;
    }

    fn kill_ring(&self) -> Option<&str> {
        Some(&self.kill_ring)
    }

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<DialogResult> {
        self.dirty = true;
        let q = &self.questions[self.active_tab];
        let other_idx = q.options.len();

        // ── Editing "other" text area ───────────────────────────────────
        if self.editing_other[self.active_tab] {
            match nav_lookup(code, modifiers) {
                Some(NavAction::Confirm) => {
                    self.editing_other[self.active_tab] = false;
                    self.answered[self.active_tab] = true;
                    if let Some(next) = (0..self.questions.len()).find(|&i| !self.answered[i]) {
                        self.active_tab = next;
                    } else {
                        return Some(DialogResult::Question {
                            answer: Some(self.build_answer()),
                            request_id: self.request_id,
                        });
                    }
                }
                Some(NavAction::Dismiss) => {
                    if code == KeyCode::Esc {
                        self.editing_other[self.active_tab] = false;
                    } else if self.other_areas[self.active_tab].is_empty() {
                        return Some(DialogResult::Question {
                            answer: None,
                            request_id: self.request_id,
                        });
                    } else {
                        self.other_areas[self.active_tab].clear();
                        self.editing_other[self.active_tab] = false;
                        if q.multi_select {
                            self.multi_toggles[self.active_tab][other_idx] = false;
                        }
                    }
                }
                _ => {
                    self.other_areas[self.active_tab].handle_key_with_kill_ring(
                        code,
                        modifiers,
                        &mut self.kill_ring,
                    );
                }
            }
            return None;
        }

        // ── Question-specific keys (before shared dialog lookup) ────────
        // Tab navigation between questions.
        match (code, modifiers) {
            (KeyCode::Right, _) | (KeyCode::Char('l'), KeyModifiers::NONE) => {
                if self.has_tabs {
                    self.visited[self.active_tab] = true;
                    self.active_tab = (self.active_tab + 1) % self.questions.len();
                }
                return None;
            }
            (KeyCode::BackTab, _)
            | (KeyCode::Left, _)
            | (KeyCode::Char('h'), KeyModifiers::NONE) => {
                if self.has_tabs {
                    self.visited[self.active_tab] = true;
                    self.active_tab = if self.active_tab == 0 {
                        self.questions.len() - 1
                    } else {
                        self.active_tab - 1
                    };
                }
                return None;
            }
            (KeyCode::Char(' '), _) if q.multi_select => {
                let idx = self.selections[self.active_tab];
                if idx == other_idx && self.other_areas[self.active_tab].is_empty() {
                    self.editing_other[self.active_tab] = true;
                } else {
                    self.multi_toggles[self.active_tab][idx] =
                        !self.multi_toggles[self.active_tab][idx];
                }
                return None;
            }
            (KeyCode::Char(c), _) if c.is_ascii_digit() => {
                let num = c.to_digit(10).unwrap_or(0) as usize;
                if num >= 1 && num <= other_idx + 1 {
                    if q.multi_select {
                        self.multi_toggles[self.active_tab][num - 1] =
                            !self.multi_toggles[self.active_tab][num - 1];
                    } else {
                        self.selections[self.active_tab] = num - 1;
                    }
                }
                return None;
            }
            _ => {}
        }

        // ── Shared dialog keys ──────────────────────────────────────────
        match nav_lookup(code, modifiers) {
            Some(NavAction::Dismiss) => Some(DialogResult::Question {
                answer: None,
                request_id: self.request_id,
            }),
            Some(NavAction::Confirm) => {
                self.answered[self.active_tab] = true;
                if let Some(next) = (0..self.questions.len()).find(|&i| !self.answered[i]) {
                    self.active_tab = next;
                    None
                } else {
                    Some(DialogResult::Question {
                        answer: Some(self.build_answer()),
                        request_id: self.request_id,
                    })
                }
            }
            Some(NavAction::Edit) => {
                if self.selections[self.active_tab] == other_idx {
                    self.editing_other[self.active_tab] = true;
                    if q.multi_select {
                        self.multi_toggles[self.active_tab][other_idx] = true;
                    }
                }
                None
            }
            Some(NavAction::Up) => {
                self.selections[self.active_tab] = if self.selections[self.active_tab] == 0 {
                    other_idx
                } else {
                    self.selections[self.active_tab] - 1
                };
                None
            }
            Some(NavAction::Down) => {
                self.selections[self.active_tab] =
                    (self.selections[self.active_tab] + 1) % (other_idx + 1);
                None
            }
            _ => None,
        }
    }

    fn draw(&mut self, out: &mut RenderOut, start_row: u16, width: u16, granted_rows: u16) {
        if !self.dirty {
            return;
        }
        self.dirty = false;

        let w = width as usize;

        let ta = &self.other_areas[self.active_tab];
        let ta_visible = self.editing_other[self.active_tab] || !ta.is_empty();
        let q_other_idx = self.questions[self.active_tab].options.len();
        let other_text_col: u16 = if self.questions[self.active_tab].multi_select {
            2 + 2 + 5 + 2
        } else {
            let digits = format!("{}", q_other_idx + 1).len();
            (2 + digits + 2 + 5 + 2) as u16
        };
        let other_wrap_w = width.saturating_sub(other_text_col) as usize;

        let q = &self.questions[self.active_tab];

        begin_dialog_draw(out, start_row);
        let bar_row = start_row;
        let mut row = bar_row;

        draw_bar(out, w, None, None, theme::accent());
        out.newline();
        row += 1;

        if self.has_tabs {
            out.print(" ");
            for (i, q) in self.questions.iter().enumerate() {
                let bullet = if self.answered[i] || self.visited[i] {
                    "\u{25a0}"
                } else {
                    "\u{25a1}"
                };
                if i == self.active_tab {
                    out.push_style(crate::render::StyleState {
                        fg: Some(theme::accent()),
                        bold: true,
                        ..Default::default()
                    });
                    out.print(&format!(" {} {} ", bullet, q.header));
                    out.pop_style();
                } else if self.answered[i] {
                    out.push_fg(theme::SUCCESS);
                    out.print(&format!(" {}", bullet));
                    out.pop_style();
                    out.push_dim();
                    out.print(&format!(" {} ", q.header));
                    out.pop_style();
                } else {
                    out.push_dim();
                    out.print(&format!(" {} {} ", bullet, q.header));
                    out.pop_style();
                }
            }
            out.newline();
            row += 1;
        }

        let sel = self.selections[self.active_tab];
        let is_multi = q.multi_select;
        let other_idx = q.options.len();

        let suffix = if is_multi { " (space to toggle)" } else { "" };
        let q_max = w.saturating_sub(1 + suffix.len());
        let segments = wrap_line(&q.question, q_max);
        for (i, seg) in segments.iter().enumerate() {
            out.print(" ");
            out.push_bold();
            out.print(seg);
            out.pop_style();
            if i == 0 && !suffix.is_empty() {
                out.push_dim();
                out.print(suffix);
                out.pop_style();
            }
            out.newline();
            row += 1;
        }

        out.newline();
        row += 1;

        for (i, opt) in q.options.iter().enumerate() {
            out.print("  ");
            let is_current = sel == i;
            let is_toggled = is_multi && self.multi_toggles[self.active_tab][i];

            if is_multi {
                let check = if is_toggled { "\u{25c9}" } else { "\u{25cb}" };
                if is_current {
                    out.push_fg(theme::accent());
                    out.print(&format!("{} ", check));
                    out.print(&opt.label);
                    out.pop_style();
                } else {
                    out.push_dim();
                    out.print(&format!("{} ", check));
                    out.pop_style();
                    out.print(&opt.label);
                }
            } else {
                out.push_dim();
                out.print(&format!("{}.", i + 1));
                out.pop_style();
                out.print(" ");
                if is_current {
                    out.push_fg(theme::accent());
                    out.print(&opt.label);
                    out.pop_style();
                } else {
                    out.print(&opt.label);
                }
            }

            if is_current && !opt.description.is_empty() {
                let prefix_len = if is_multi {
                    2 + 2 // "  \u{25c9} "
                } else {
                    2 + format!("{}.", i + 1).len() + 1 // "  N. "
                };
                let used = prefix_len + opt.label.chars().count() + 2; // "  " gap
                let remaining = w.saturating_sub(used);
                if remaining > 3 {
                    let desc: String = opt.description.chars().take(remaining).collect();
                    out.push_dim();
                    out.print(&format!("  {desc}"));
                    out.pop_style();
                }
            }
            out.newline();
            row += 1;
        }

        // "Other" option with inline textarea
        out.print("  ");
        let is_other_current = sel == other_idx;
        let is_other_toggled = is_multi && self.multi_toggles[self.active_tab][other_idx];

        if is_multi {
            let check = if is_other_toggled {
                "\u{25c9}"
            } else {
                "\u{25cb}"
            };
            if is_other_current {
                out.push_fg(theme::accent());
                out.print(&format!("{} Other", check));
                out.pop_style();
            } else {
                out.push_dim();
                out.print(&format!("{} ", check));
                out.pop_style();
                out.print("Other");
            }
        } else {
            out.push_dim();
            out.print(&format!("{}.", other_idx + 1));
            out.pop_style();
            out.print(" ");
            if is_other_current {
                out.push_fg(theme::accent());
                out.print("Other");
                out.pop_style();
            } else {
                out.print("Other");
            }
        }

        let editing = self.editing_other[self.active_tab];
        let mut cursor_pos = None;
        if ta_visible {
            let (new_row, cpos) =
                render_inline_textarea(out, ta, editing, other_text_col, other_wrap_w, row);
            row = new_row;
            cursor_pos = cpos;
        } else {
            out.newline();
        }
        let _ = row;

        // Footer
        out.newline();
        out.push_dim();
        let hint = if editing {
            hints::join(&[hints::CANCEL, hints::CONFIRM])
        } else if self.has_tabs {
            hints::join(&[hints::NEXT_Q, hints::CONFIRM, hints::CANCEL])
        } else {
            hints::join(&[hints::CONFIRM, hints::CANCEL])
        };
        out.print(&hint);
        out.pop_style();
        // Only clear below the dialog if there's viewport space left.
        // When the dialog fills the full terminal, clearing here wipes
        // the last visible line.
        if out.row.is_some_and(|r| r < start_row + granted_rows) {
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
        }

        finish_dialog_frame(out, cursor_pos, editing);
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
