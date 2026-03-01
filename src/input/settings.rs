use crossterm::event::{Event, KeyCode, KeyEvent};

/// Generic navigation result from a menu.
pub enum MenuAction {
    /// Item was toggled in-place (settings style), menu stays open.
    Toggle(usize),
    /// Item was selected, menu should close.
    Select(usize),
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
        match ev {
            Event::Key(KeyEvent {
                code: KeyCode::Esc, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('q'),
                ..
            }) => MenuAction::Dismiss,
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                ..
            }) => {
                if self.select_on_enter {
                    MenuAction::Select(self.selected)
                } else {
                    MenuAction::Toggle(self.selected)
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char(' '),
                ..
            }) if !self.select_on_enter => MenuAction::Toggle(self.selected),
            Event::Key(KeyEvent {
                code: KeyCode::Up, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('k'),
                ..
            }) => {
                if self.selected > 0 {
                    self.selected -= 1;
                    MenuAction::Redraw
                } else {
                    MenuAction::Noop
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Down,
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('j'),
                ..
            }) => {
                if self.selected + 1 < self.len {
                    self.selected += 1;
                    MenuAction::Redraw
                } else {
                    MenuAction::Noop
                }
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
    },
    Model {
        /// (key, model_name, provider_name) for each entry.
        models: Vec<(String, String, String)>,
    },
}

pub struct MenuState {
    pub kind: MenuKind,
    pub nav: Menu,
}

/// Domain-specific result returned to the app after a menu closes.
pub enum MenuResult {
    Settings { vim: bool, auto_compact: bool },
    ModelSelect(String),
    Dismissed,
}
