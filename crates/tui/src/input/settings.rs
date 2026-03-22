use crate::keymap::{nav_lookup, NavAction};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

/// Generic navigation result from a menu.
pub enum MenuAction {
    /// Item was toggled in-place (settings style), menu stays open.
    Toggle(usize),
    /// Item was selected, menu should close.
    Select(usize),
    /// Tab was pressed (cycle auxiliary state).
    Tab,
    /// Menu was dismissed via Esc/q.
    Dismiss,
    /// Navigation happened, redraw needed.
    Redraw,
    /// Key not consumed.
    Noop,
}

/// Pure navigation state for a list menu.
pub struct Menu {
    pub selected: usize,
    pub len: usize,
    /// true = Enter selects+closes, false = Enter/Space toggles in-place.
    pub select_on_enter: bool,
}

impl Menu {
    pub fn handle_event(&mut self, ev: &Event) -> MenuAction {
        let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = ev
        else {
            return MenuAction::Noop;
        };

        // Menu-specific keys (before shared nav lookup).
        match (*code, *modifiers) {
            (KeyCode::Char('q'), KeyModifiers::NONE) => return MenuAction::Dismiss,
            (KeyCode::Char(' '), _) if !self.select_on_enter => {
                return MenuAction::Toggle(self.selected)
            }
            (KeyCode::Char('t'), m) if m.contains(KeyModifiers::CONTROL) => return MenuAction::Tab,
            _ => {}
        }

        // Shared navigation keys.
        match nav_lookup(*code, *modifiers) {
            Some(NavAction::Dismiss) => MenuAction::Dismiss,
            Some(NavAction::Confirm) => {
                if self.select_on_enter {
                    MenuAction::Select(self.selected)
                } else {
                    MenuAction::Toggle(self.selected)
                }
            }
            Some(NavAction::Edit) => MenuAction::Tab,
            Some(NavAction::Up) => {
                if self.len == 0 {
                    return MenuAction::Noop;
                }
                self.selected = if self.selected > 0 {
                    self.selected - 1
                } else {
                    self.len - 1
                };
                MenuAction::Redraw
            }
            Some(NavAction::Down) => {
                if self.len == 0 {
                    return MenuAction::Noop;
                }
                self.selected = if self.selected + 1 < self.len {
                    self.selected + 1
                } else {
                    0
                };
                MenuAction::Redraw
            }
            _ => MenuAction::Noop,
        }
    }
}

/// Domain-specific data carried alongside the generic Menu navigation.
pub enum MenuKind {
    Settings {
        vim_enabled: bool,
        auto_compact: bool,
        show_speed: bool,
        show_prediction: bool,
        show_slug: bool,
        restrict_to_workspace: bool,
    },
    Model {
        /// (key, model_name, provider_name) for each entry.
        models: Vec<(String, String, String)>,
    },
    Stats {
        left: Vec<crate::metrics::StatsLine>,
        right: Vec<crate::metrics::StatsLine>,
    },
    Theme {
        /// (name, detail, ansi_value)
        presets: Vec<(&'static str, &'static str, u8)>,
        /// Original accent value to restore on dismiss.
        original: u8,
    },
    Color {
        /// (name, detail, ansi_value)
        presets: Vec<(&'static str, &'static str, u8)>,
        /// Original slug color value to restore on dismiss.
        original: u8,
    },
}

pub struct MenuState {
    pub kind: MenuKind,
    pub nav: Menu,
}

/// Domain-specific result returned to the app after a menu closes.
pub enum MenuResult {
    Settings {
        vim: bool,
        auto_compact: bool,
        show_speed: bool,
        show_prediction: bool,
        show_slug: bool,
        restrict_to_workspace: bool,
    },
    ModelSelect(String),
    ThemeSelect(u8),
    ColorSelect(u8),
    Stats,
    Dismissed,
}
