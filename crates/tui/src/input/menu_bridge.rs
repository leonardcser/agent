//! Wiring between the input buffer and the modal `MenuState`.
//!
//! Menus (Stats, Cost, and friends) are full-width popovers that grab all
//! keystrokes while open. They are separate from the completer popups: menus
//! use `crate::keymap::nav_lookup` for navigation, while completers handle
//! their own keys. This file owns:
//!   * dispatch into the menu (`handle_menu_event`)
//!   * dismissal + row-count queries
//!   * the `open_*` helpers for settings/stats/cost/picker menus

use super::{
    Action, History, InputState, Menu, MenuAction, MenuKind, MenuResult, MenuState, SettingsState,
};
use crate::completer::Completer;
use crossterm::event::Event;

impl InputState {
    pub(super) fn handle_menu_event(&mut self, ev: &Event) -> Action {
        let ms = self.menu.as_mut().unwrap();
        match ms.nav.handle_event(ev) {
            MenuAction::Toggle(_) => Action::Redraw,
            MenuAction::Tab => Action::Redraw,
            MenuAction::Select(_) => Action::Redraw,
            MenuAction::Dismiss => Action::MenuResult(self.dismiss_menu().unwrap()),
            MenuAction::Redraw => Action::Redraw,
            MenuAction::Noop => Action::Noop,
        }
    }

    /// Dismiss the current menu, returning the appropriate result.
    pub fn dismiss_menu(&mut self) -> Option<MenuResult> {
        let ms = self.menu.take()?;
        Some(match ms.kind {
            MenuKind::Stats { .. } => MenuResult::Stats,
            MenuKind::Cost { .. } => MenuResult::Cost,
        })
    }

    /// Number of rows the current menu needs (0 if no menu).
    pub fn menu_rows(&self) -> usize {
        match &self.menu {
            Some(ms) => match &ms.kind {
                MenuKind::Stats { left, right } => crate::metrics::stats_row_count(left, right),
                MenuKind::Cost { lines } => lines.len(),
            },
            None => 0,
        }
    }

    pub fn open_settings(&mut self, state: &SettingsState) {
        self.menu = None;
        if self.history_saved_buf.is_none() {
            self.history_saved_buf = Some((self.buffer.buf.clone(), self.buffer.cpos));
        }
        let mut comp = Completer::settings(state);
        comp.update_query(self.buffer.buf.clone());
        self.completer = Some(comp);
    }

    pub fn open_stats(&mut self, stats: crate::metrics::StatsOutput) {
        self.completer = None;
        self.menu = Some(MenuState {
            nav: Menu {
                selected: 0,
                len: 0,
                select_on_enter: false,
            },
            kind: MenuKind::Stats {
                left: stats.left,
                right: stats.right,
            },
        });
    }

    pub fn open_cost(&mut self, lines: Vec<crate::metrics::StatsLine>) {
        self.completer = None;
        self.menu = Some(MenuState {
            nav: Menu {
                selected: 0,
                len: 0,
                select_on_enter: false,
            },
            kind: MenuKind::Cost { lines },
        });
    }

    pub fn open_model_completer(&mut self, models: &[(String, String, String)]) {
        self.menu = None;
        self.history_saved_buf = Some((self.buffer.buf.clone(), self.buffer.cpos));
        let mut comp = Completer::models(models);
        comp.update_query(self.buffer.buf.clone());
        self.completer = Some(comp);
    }

    pub fn open_theme_completer(&mut self) {
        self.menu = None;
        self.history_saved_buf = Some((self.buffer.buf.clone(), self.buffer.cpos));
        let mut comp = Completer::themes(crate::theme::accent_value());
        comp.update_query(self.buffer.buf.clone());
        self.completer = Some(comp);
    }

    pub fn open_color_completer(&mut self) {
        self.menu = None;
        self.history_saved_buf = Some((self.buffer.buf.clone(), self.buffer.cpos));
        let mut comp = Completer::colors(crate::theme::slug_color_value());
        comp.update_query(self.buffer.buf.clone());
        self.completer = Some(comp);
    }

    /// Open history fuzzy search using the completer component.
    pub fn open_history_search(&mut self, history: &History) {
        self.history_saved_buf = Some((self.buffer.buf.clone(), self.buffer.cpos));
        // Keep buf as-is so the current content becomes the initial search query.
        let mut comp = Completer::history(history.entries());
        comp.update_query(self.buffer.buf.clone());
        self.completer = Some(comp);
    }
}
