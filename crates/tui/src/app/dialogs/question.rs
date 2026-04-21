//! `ask_user_question` dialog — migrated to the new `ui::Dialog` panel
//! framework. A single `QuestionWidget` panels the tabs, question
//! prompt, option list (single- or multi-select), and inline "Other"
//! text area into one self-contained widget so state stays in one
//! place. `Question` is the `DialogState` that resolves the request
//! when the widget emits `submit` / `dismiss`.

use super::super::{App, TurnState};
use super::{ActionResult, DialogState};
use crate::keymap::hints;
use crate::render::{wrap_line, Question as QuestionDef};
use crate::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use ui::component::{Component, CursorInfo, DrawContext, KeyResult};
use ui::dialog::PanelWidget;
use ui::grid::{GridSlice, Style};
use ui::layout::Rect;
use ui::text_input::TextInput;

pub fn open(app: &mut App, questions: Vec<QuestionDef>, request_id: u64) {
    if questions.is_empty() {
        return;
    }
    let dialog_config = app.builtin_dialog_config(None, vec![]);
    let widget = Box::new(QuestionWidget::new(questions));
    let win_id = app.ui.dialog_open(
        ui::FloatConfig {
            title: None,
            border: ui::Border::None,
            placement: ui::Placement::dock_bottom_full_width(ui::Constraint::Pct(60)),
            ..Default::default()
        },
        dialog_config,
        vec![ui::PanelSpec::widget(widget, ui::PanelHeight::Fill)],
    );
    if let Some(win_id) = win_id {
        app.float_states
            .insert(win_id, Box::new(Question { request_id }));
    }
}

pub struct Question {
    request_id: u64,
}

impl DialogState for Question {
    fn blocks_agent(&self) -> bool {
        true
    }

    fn on_action(
        &mut self,
        app: &mut App,
        win: ui::WinId,
        action: &str,
        agent: &mut Option<TurnState>,
    ) -> ActionResult {
        match action {
            "submit" => {
                let answer = app
                    .ui
                    .dialog_mut(win)
                    .and_then(|d| d.panel_widget_mut::<QuestionWidget>(0))
                    .map(|w| w.build_answer());
                app.resolve_question(answer, self.request_id, agent);
                ActionResult::Close
            }
            "dismiss" => {
                app.resolve_question(None, self.request_id, agent);
                ActionResult::Close
            }
            _ => ActionResult::Pass,
        }
    }
}

// ── Widget ─────────────────────────────────────────────────────────────

pub(crate) struct QuestionWidget {
    questions: Vec<QuestionDef>,
    has_tabs: bool,
    active_tab: usize,
    selections: Vec<usize>,
    multi_toggles: Vec<Vec<bool>>,
    other_inputs: Vec<TextInput>,
    editing_other: Vec<bool>,
    visited: Vec<bool>,
    answered: Vec<bool>,
    /// Row/col (relative to widget's draw area) where the current
    /// tab's "Other" text value starts. Written by `prepare`, read by
    /// `draw` / `cursor`. Only meaningful when
    /// `editing_other[active_tab]` is true.
    other_row: u16,
    other_col: u16,
    /// Draw width captured at `prepare` time; used by `draw` / cursor
    /// layout.
    width: u16,
}

impl QuestionWidget {
    pub fn new(questions: Vec<QuestionDef>) -> Self {
        let n = questions.len();
        Self {
            multi_toggles: questions
                .iter()
                .map(|q| vec![false; q.options.len() + 1])
                .collect(),
            has_tabs: n > 1,
            active_tab: 0,
            selections: vec![0; n],
            other_inputs: (0..n).map(|_| TextInput::new()).collect(),
            editing_other: vec![false; n],
            visited: vec![false; n],
            answered: vec![false; n],
            other_row: 0,
            other_col: 0,
            width: 0,
            questions,
        }
    }

    /// Recompute the "Other" row/col based on the current tab's state
    /// and the widget's draw width. Walks the same layout logic as
    /// `draw` without painting.
    fn recompute_other_layout(&mut self) {
        let w = self.width;
        if w == 0 {
            self.other_row = 0;
            self.other_col = 0;
            return;
        }
        let mut row: u16 = 0;
        row += 1; // accent bar
        if self.has_tabs {
            row += 1;
        }
        row += 1; // blank
        let q = &self.questions[self.active_tab];
        let is_multi = q.multi_select;
        let other_idx = q.options.len();
        let suffix = if is_multi { " (space to toggle)" } else { "" };
        let q_max = (w as usize).saturating_sub(1 + suffix.len());
        let segments = wrap_line(&q.question, q_max);
        row += segments.len() as u16;
        row += 1; // blank
        row += q.options.len() as u16;
        // "Other" row is here; text column depends on the multi /
        // single prefix.
        self.other_row = row;
        let col_prefix: u16 = if is_multi {
            2 + 2 /* "X " */ + 5 /* "Other" */ + 2 /* "  " gap */
        } else {
            let digits = format!("{}", other_idx + 1).len() as u16;
            2 + digits + 1 /* "N. " */ + 5 + 2
        };
        self.other_col = col_prefix;
    }

    pub(crate) fn build_answer(&self) -> String {
        let mut answers = serde_json::Map::new();
        for (i, q) in self.questions.iter().enumerate() {
            let other_idx = q.options.len();
            let other_text = self.other_inputs[i].text();
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

    fn accent_style() -> Style {
        Style {
            fg: Some(theme::accent()),
            ..Default::default()
        }
    }

    fn dim_style() -> Style {
        Style {
            dim: true,
            ..Default::default()
        }
    }
}

// ── Rendering helper ───────────────────────────────────────────────────

struct RowPainter<'a, 'b> {
    slice: &'a mut GridSlice<'b>,
    row: u16,
    col: u16,
    width: u16,
}

impl<'a, 'b> RowPainter<'a, 'b> {
    fn new(slice: &'a mut GridSlice<'b>, row: u16) -> Self {
        let width = slice.width();
        Self {
            slice,
            row,
            col: 0,
            width,
        }
    }

    fn write(&mut self, text: &str, style: Style) {
        for ch in text.chars() {
            if self.col >= self.width {
                break;
            }
            self.slice.set(self.col, self.row, ch, style);
            self.col += 1;
        }
    }

    fn pad(&mut self, n: u16) {
        for _ in 0..n {
            if self.col >= self.width {
                break;
            }
            self.slice.set(self.col, self.row, ' ', Style::default());
            self.col += 1;
        }
    }
}

impl PanelWidget for QuestionWidget {
    fn prepare(&mut self, area: Rect, _ctx: &DrawContext) {
        self.width = area.width;
        self.recompute_other_layout();
    }

    fn draw(&self, _area: Rect, slice: &mut GridSlice<'_>, _ctx: &DrawContext) {
        let w = slice.width();
        let h = slice.height();
        if w == 0 || h == 0 {
            return;
        }
        let mut row: u16 = 0;

        // Accent top bar row.
        let accent_bg = Style {
            bg: Some(theme::accent()),
            ..Default::default()
        };
        for x in 0..w {
            slice.set(x, row, ' ', accent_bg);
        }
        row += 1;

        // Tab row (if multi-question).
        if self.has_tabs && row < h {
            let mut rp = RowPainter::new(slice, row);
            rp.pad(1);
            for (i, q) in self.questions.iter().enumerate() {
                let bullet = if self.answered[i] || self.visited[i] {
                    '\u{25a0}'
                } else {
                    '\u{25a1}'
                };
                if i == self.active_tab {
                    let style = Style {
                        fg: Some(theme::accent()),
                        bold: true,
                        ..Default::default()
                    };
                    rp.write(&format!(" {bullet} {} ", q.header), style);
                } else if self.answered[i] {
                    rp.write(
                        &format!(" {bullet}"),
                        Style {
                            fg: Some(theme::SUCCESS),
                            ..Default::default()
                        },
                    );
                    rp.write(&format!(" {} ", q.header), Self::dim_style());
                } else {
                    rp.write(&format!(" {bullet} {} ", q.header), Self::dim_style());
                }
            }
            row += 1;
        }

        let q = &self.questions[self.active_tab];
        let sel = self.selections[self.active_tab];
        let is_multi = q.multi_select;
        let other_idx = q.options.len();

        // Blank row.
        row = row.saturating_add(1);

        // Question text with optional multi-select suffix.
        let suffix = if is_multi { " (space to toggle)" } else { "" };
        let q_max = (w as usize).saturating_sub(1 + suffix.len());
        let segments = wrap_line(&q.question, q_max);
        for (i, seg) in segments.iter().enumerate() {
            if row >= h {
                break;
            }
            let mut rp = RowPainter::new(slice, row);
            rp.pad(1);
            rp.write(
                seg,
                Style {
                    bold: true,
                    ..Default::default()
                },
            );
            if i == 0 && !suffix.is_empty() {
                rp.write(suffix, Self::dim_style());
            }
            row += 1;
        }

        // Blank row.
        row = row.saturating_add(1);

        // Options list.
        for (i, opt) in q.options.iter().enumerate() {
            if row >= h {
                break;
            }
            let is_current = sel == i;
            let is_toggled = is_multi && self.multi_toggles[self.active_tab][i];
            let mut rp = RowPainter::new(slice, row);
            rp.pad(2);
            if is_multi {
                let check = if is_toggled { '\u{25c9}' } else { '\u{25cb}' };
                if is_current {
                    rp.write(&format!("{check} "), Self::accent_style());
                    rp.write(&opt.label, Self::accent_style());
                } else {
                    rp.write(&format!("{check} "), Self::dim_style());
                    rp.write(&opt.label, Style::default());
                }
            } else {
                rp.write(&format!("{}.", i + 1), Self::dim_style());
                rp.pad(1);
                let label_style = if is_current {
                    Self::accent_style()
                } else {
                    Style::default()
                };
                rp.write(&opt.label, label_style);
            }
            // Inline description for the current item.
            if is_current && !opt.description.is_empty() {
                let prefix_len = if is_multi {
                    2 + 2
                } else {
                    2 + format!("{}.", i + 1).len() + 1
                };
                let used = prefix_len + opt.label.chars().count() + 2;
                let remaining = (w as usize).saturating_sub(used);
                if remaining > 3 {
                    let desc: String = opt.description.chars().take(remaining).collect();
                    rp.write("  ", Self::dim_style());
                    rp.write(&desc, Self::dim_style());
                }
            }
            row += 1;
        }

        // "Other" row with inline text value.
        if row < h {
            let is_other_current = sel == other_idx;
            let is_other_toggled = is_multi && self.multi_toggles[self.active_tab][other_idx];
            let mut rp = RowPainter::new(slice, row);
            rp.pad(2);
            if is_multi {
                let check = if is_other_toggled {
                    '\u{25c9}'
                } else {
                    '\u{25cb}'
                };
                if is_other_current {
                    rp.write(&format!("{check} Other"), Self::accent_style());
                } else {
                    rp.write(&format!("{check} "), Self::dim_style());
                    rp.write("Other", Style::default());
                }
            } else {
                rp.write(&format!("{}.", other_idx + 1), Self::dim_style());
                rp.pad(1);
                if is_other_current {
                    rp.write("Other", Self::accent_style());
                } else {
                    rp.write("Other", Style::default());
                }
            }

            let editing = self.editing_other[self.active_tab];
            let text = self.other_inputs[self.active_tab].text();
            if editing || !text.is_empty() {
                rp.pad(2);
                // Layout of other_row/other_col was precomputed in
                // `prepare`; just paint the text.
                rp.write(text, Style::default());
            }
            row = row.saturating_add(1);
        }

        // Footer hints on the last row.
        if h > 0 {
            let footer_row = h - 1;
            if footer_row > row {
                let editing = self.editing_other[self.active_tab];
                let hint = if editing {
                    hints::join(&[hints::CANCEL, hints::CONFIRM])
                } else if self.has_tabs {
                    hints::join(&[hints::NEXT_Q, hints::CONFIRM, hints::CANCEL])
                } else {
                    hints::join(&[hints::CONFIRM, hints::CANCEL])
                };
                let mut rp = RowPainter::new(slice, footer_row);
                rp.write(&hint, Self::dim_style());
            }
        }
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult {
        let q = &self.questions[self.active_tab];
        let other_idx = q.options.len();
        let is_multi = q.multi_select;

        // ── Editing "Other" ─────────────────────────────────────────
        if self.editing_other[self.active_tab] {
            match (code, mods) {
                (KeyCode::Enter, _) => {
                    self.editing_other[self.active_tab] = false;
                    self.answered[self.active_tab] = true;
                    if let Some(next) = (0..self.questions.len()).find(|&i| !self.answered[i]) {
                        self.active_tab = next;
                        return KeyResult::Consumed;
                    }
                    return KeyResult::Action("submit".into());
                }
                (KeyCode::Esc, _) => {
                    if self.other_inputs[self.active_tab].text().is_empty() {
                        return KeyResult::Action("dismiss".into());
                    }
                    self.other_inputs[self.active_tab].clear();
                    self.editing_other[self.active_tab] = false;
                    if is_multi {
                        self.multi_toggles[self.active_tab][other_idx] = false;
                    }
                    return KeyResult::Consumed;
                }
                _ => {
                    // Delegate to TextInput's edit keys; ignore its
                    // own submit/cancel actions since Enter/Esc were
                    // handled above.
                    let r = Component::handle_key(
                        &mut self.other_inputs[self.active_tab],
                        code,
                        mods,
                    );
                    return match r {
                        KeyResult::Action(_) => KeyResult::Consumed,
                        other => other,
                    };
                }
            }
        }

        // ── Tab navigation & Question-specific keys ────────────────
        match (code, mods) {
            (KeyCode::Right, _) | (KeyCode::Char('l'), KeyModifiers::NONE) if self.has_tabs => {
                self.visited[self.active_tab] = true;
                self.active_tab = (self.active_tab + 1) % self.questions.len();
                return KeyResult::Consumed;
            }
            (KeyCode::BackTab, _)
            | (KeyCode::Left, _)
            | (KeyCode::Char('h'), KeyModifiers::NONE)
                if self.has_tabs =>
            {
                self.visited[self.active_tab] = true;
                self.active_tab = if self.active_tab == 0 {
                    self.questions.len() - 1
                } else {
                    self.active_tab - 1
                };
                return KeyResult::Consumed;
            }
            (KeyCode::Char(' '), _) if is_multi => {
                let idx = self.selections[self.active_tab];
                if idx == other_idx && self.other_inputs[self.active_tab].text().is_empty() {
                    self.editing_other[self.active_tab] = true;
                } else {
                    self.multi_toggles[self.active_tab][idx] =
                        !self.multi_toggles[self.active_tab][idx];
                }
                return KeyResult::Consumed;
            }
            (KeyCode::Char(c), _) if c.is_ascii_digit() => {
                let num = c.to_digit(10).unwrap_or(0) as usize;
                if num >= 1 && num <= other_idx + 1 {
                    if is_multi {
                        self.multi_toggles[self.active_tab][num - 1] =
                            !self.multi_toggles[self.active_tab][num - 1];
                    } else {
                        self.selections[self.active_tab] = num - 1;
                    }
                }
                return KeyResult::Consumed;
            }
            _ => {}
        }

        // ── Shared nav ─────────────────────────────────────────────
        match (code, mods) {
            (KeyCode::Esc, _) => KeyResult::Action("dismiss".into()),
            (KeyCode::Enter, _) => {
                self.answered[self.active_tab] = true;
                if let Some(next) = (0..self.questions.len()).find(|&i| !self.answered[i]) {
                    self.active_tab = next;
                    KeyResult::Consumed
                } else {
                    KeyResult::Action("submit".into())
                }
            }
            (KeyCode::Char('e'), KeyModifiers::NONE) => {
                if self.selections[self.active_tab] == other_idx {
                    self.editing_other[self.active_tab] = true;
                    if is_multi {
                        self.multi_toggles[self.active_tab][other_idx] = true;
                    }
                }
                KeyResult::Consumed
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                self.selections[self.active_tab] = if self.selections[self.active_tab] == 0 {
                    other_idx
                } else {
                    self.selections[self.active_tab] - 1
                };
                KeyResult::Consumed
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                self.selections[self.active_tab] =
                    (self.selections[self.active_tab] + 1) % (other_idx + 1);
                KeyResult::Consumed
            }
            _ => KeyResult::Ignored,
        }
    }

    fn cursor(&self) -> Option<CursorInfo> {
        if !self.editing_other[self.active_tab] {
            return None;
        }
        let ti = &self.other_inputs[self.active_tab];
        let col_offset = ti.cursor_col() as u16;
        Some(CursorInfo {
            col: self.other_col.saturating_add(col_offset),
            row: self.other_row,
            style: None,
        })
    }

    fn content_rows(&self) -> usize {
        let q = &self.questions[self.active_tab];
        let tab_rows = if self.has_tabs { 1 } else { 0 };
        let q_rows = q.question.lines().count().max(1);
        let opt_rows = q.options.len() + 1;
        1 + tab_rows + 1 + q_rows + 1 + opt_rows + 1 + 1
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
