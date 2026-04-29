use crate::grid::{GridSlice, Style};
use crate::layout::Rect;
use crossterm::event::{KeyCode, KeyModifiers, MouseEvent};

/// Semantic events emitted by widgets when a key resolves into a
/// high-level action. Replaces the old stringly-typed
/// `KeyResult::Action(String)` dispatch. The enum lives in `component`
/// so every widget and the dispatcher (`Dialog`, `FloatDialog`,
/// `Ui::handle_key_with_actions`) can match on it without string
/// parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WidgetEvent {
    /// User confirmed with no text payload (Enter on a list, etc.).
    Submit,
    /// User confirmed from a text widget; carries the final text.
    SubmitText(String),
    /// User cancelled (Esc on a TextInput). Distinct from `Dismiss`
    /// so callers can choose whether to treat a text-widget Esc as
    /// "close the dialog" or "clear the field".
    Cancel,
    /// User dismissed the surrounding container (Esc on a Dialog).
    Dismiss,
    /// User selected a specific row by index.
    Select(usize),
    /// User selected without an explicit index (cursor position wins).
    SelectDefault,
    /// Text content changed (per keystroke on TextInput).
    TextChanged,
    /// Component wants its text payload copied to the system clipboard
    /// (`TextInput` drag-select on release). Not auto-classified to a
    /// `WinEvent` — App matches on it directly.
    Yank(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyResult {
    Consumed,
    Ignored,
    Action(WidgetEvent),
    /// Mouse-only: the component handled a `Down` event AND wants drag
    /// capture — App should route subsequent `Drag` and `Up` events to
    /// this layer regardless of pointer position until release. Used by
    /// `TextInput` for click-drag text selection. Returned only from
    /// `handle_mouse`; treated as `Consumed` in key paths.
    Capture,
}

#[derive(Default)]
pub struct DrawContext {
    pub terminal_width: u16,
    pub terminal_height: u16,
    pub focused: bool,
    /// Selection overlay style for `Window`-driven surfaces and inline
    /// drag-select widgets (TextInput, Notification). Populated by the
    /// compositor from the host's `Ui::set_selection_bg` slot, so every
    /// widget paints the same color without carrying its own field.
    pub selection_style: Style,
}

#[derive(Debug, Clone)]
pub struct CursorInfo {
    pub col: u16,
    pub row: u16,
    pub style: Option<CursorStyle>,
}

#[derive(Debug, Clone)]
pub struct CursorStyle {
    pub glyph: char,
    pub style: Style,
}

impl CursorInfo {
    pub fn hardware(col: u16, row: u16) -> Self {
        Self {
            col,
            row,
            style: None,
        }
    }

    pub fn block(col: u16, row: u16, glyph: char, style: Style) -> Self {
        Self {
            col,
            row,
            style: Some(CursorStyle { glyph, style }),
        }
    }
}

pub trait Component: 'static {
    /// Resolve any internal layout that depends on the frame's
    /// allocated rect. Called once per frame before `draw`. Default
    /// no-op; components with sub-layout (e.g. Dialog's panel stack)
    /// override this.
    fn prepare(&mut self, _area: Rect, _ctx: &DrawContext) {}
    fn draw(&self, area: Rect, grid: &mut GridSlice<'_>, ctx: &DrawContext);
    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult;
    /// Handle a mouse event. Coordinates in `event` are absolute
    /// (terminal-relative). Components that care about clicks/drags
    /// override this; default returns `Ignored`. Wheel scroll is
    /// routed by App through dedicated scroll methods, not here, so
    /// implementations can ignore `ScrollUp/Down` variants.
    fn handle_mouse(&mut self, _event: MouseEvent) -> KeyResult {
        KeyResult::Ignored
    }
    fn cursor(&self) -> Option<CursorInfo> {
        None
    }
    fn as_any(&self) -> &dyn std::any::Any {
        panic!("as_any not implemented")
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        panic!("as_any_mut not implemented")
    }
}
