//! Declarative keymap for non-stateful keybindings.
//!
//! All simple key→action mappings live here as data. Stateful handlers (vim
//! normal mode, completer, menu, dialog text editing) remain as dedicated code
//! and are consulted separately by the dispatch loop.

use crossterm::event::{KeyCode, KeyModifiers};

// ── Actions ──────────────────────────────────────────────────────────────────

/// Actions that the keymap can resolve to.
///
/// The dispatch loop matches on these to perform side effects. Ordering here
/// doesn't matter — priority comes from the binding list order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    // App control
    Quit,
    CancelAgent,
    ClearBuffer,
    ToggleMode,
    CycleReasoning,
    ToggleStash,
    OpenHelp,
    Redraw,
    AcceptGhostText,

    // Submit
    Submit,
    InsertNewline,

    // Navigation
    MoveLeft,
    MoveRight,
    MoveWordForward,
    MoveWordBackward,
    MoveStartOfLine,
    MoveEndOfLine,
    MoveUp,
    MoveDown,
    MoveStartOfBuffer,
    MoveEndOfBuffer,
    HistoryPrev,
    HistoryNext,

    // Editing
    Backspace,
    DeleteCharForward,
    DeleteWordBackward,
    DeleteWordForward,
    DeleteToStartOfLine,
    KillToEndOfLine,
    KillToStartOfLine,
    Yank,
    YankPop,
    UppercaseWord,
    LowercaseWord,
    CapitalizeWord,
    Undo,

    // Vim-specific (only active in vim normal mode)
    VimHalfPageUp,
    VimHalfPageDown,

    // Clipboard
    ClipboardImage,

    // Selection (shift+movement extends selection)
    SelectLeft,
    SelectRight,
    SelectUp,
    SelectDown,
    SelectWordForward,
    SelectWordBackward,
    SelectStartOfLine,
    SelectEndOfLine,
    // Clipboard (system)
    CopySelection,
    CutSelection,
}

// ── Context ──────────────────────────────────────────────────────────────────

/// Snapshot of app state used for condition matching.
pub struct KeyContext {
    pub buf_empty: bool,
    pub vim_non_insert: bool,
    pub vim_enabled: bool,
    pub agent_running: bool,
    pub ghost_text_visible: bool,
}

// ── Conditions ───────────────────────────────────────────────────────────────

/// Tri-state condition: must be true, must be false, or don't care.
#[derive(Clone, Copy, Debug)]
enum Cond {
    Any,
    Yes,
    No,
}

impl Cond {
    fn matches(self, value: bool) -> bool {
        match self {
            Cond::Any => true,
            Cond::Yes => value,
            Cond::No => !value,
        }
    }
}

/// Builder for binding conditions.
#[derive(Clone, Copy, Debug)]
pub struct When {
    buf_empty: Cond,
    vim_non_insert: Cond,
    vim_enabled: Cond,
    agent_running: Cond,
    ghost_text: Cond,
}

impl When {
    const fn new() -> Self {
        Self {
            buf_empty: Cond::Any,
            vim_non_insert: Cond::Any,
            vim_enabled: Cond::Any,
            agent_running: Cond::Any,
            ghost_text: Cond::Any,
        }
    }

    const fn buf_empty(mut self) -> Self {
        self.buf_empty = Cond::Yes;
        self
    }

    const fn buf_not_empty(mut self) -> Self {
        self.buf_empty = Cond::No;
        self
    }

    const fn vim_non_insert(mut self) -> Self {
        self.vim_non_insert = Cond::Yes;
        self
    }

    const fn not_vim_non_insert(mut self) -> Self {
        self.vim_non_insert = Cond::No;
        self
    }

    const fn idle(mut self) -> Self {
        self.agent_running = Cond::No;
        self
    }

    const fn running(mut self) -> Self {
        self.agent_running = Cond::Yes;
        self
    }

    const fn ghost_text(mut self) -> Self {
        self.ghost_text = Cond::Yes;
        self
    }

    fn matches(&self, ctx: &KeyContext) -> bool {
        self.buf_empty.matches(ctx.buf_empty)
            && self.vim_non_insert.matches(ctx.vim_non_insert)
            && self.vim_enabled.matches(ctx.vim_enabled)
            && self.agent_running.matches(ctx.agent_running)
            && self.ghost_text.matches(ctx.ghost_text_visible)
    }
}

// ── Binding ──────────────────────────────────────────────────────────────────

struct Binding {
    code: KeyCode,
    mods: KeyModifiers,
    /// Extra modifier that must NOT be present (used to distinguish e.g.
    /// plain Backspace from Alt+Backspace).
    exclude_mods: KeyModifiers,
    when: When,
    action: KeyAction,
}

// ── Convenience constructors ─────────────────────────────────────────────────

const fn when() -> When {
    When::new()
}

const fn bind(code: KeyCode, mods: KeyModifiers, when: When, action: KeyAction) -> Binding {
    Binding {
        code,
        mods,
        exclude_mods: KeyModifiers::empty(),
        when,
        action,
    }
}

const fn bind_exclude(
    code: KeyCode,
    mods: KeyModifiers,
    exclude_mods: KeyModifiers,
    when: When,
    action: KeyAction,
) -> Binding {
    Binding {
        code,
        mods,
        exclude_mods,
        when,
        action,
    }
}

// Shorthand constants.
const CTRL: KeyModifiers = KeyModifiers::CONTROL;
const ALT: KeyModifiers = KeyModifiers::ALT;
const SUPER: KeyModifiers = KeyModifiers::SUPER;
const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
const NONE: KeyModifiers = KeyModifiers::NONE;

// ── The Keymap ───────────────────────────────────────────────────────────────

/// Central keymap table. Bindings are evaluated top-to-bottom; first match wins.
///
/// This intentionally does NOT include:
/// - Vim normal mode commands (handled by the Vim state machine)
/// - Menu navigation (handled by Menu::handle_event)
/// - Completer navigation (handled by handle_completer_event)
/// - Dialog key handling (handled by each Dialog impl)
/// - Esc / double-Esc logic (inherently stateful, needs timers)
/// - Paste events (not key events)
/// - Char insertion (fallback after keymap miss)
static BINDINGS: &[Binding] = &[
    // ── Ghost text ──────────────────────────────────────────────────────
    bind(
        KeyCode::Tab,
        NONE,
        when().ghost_text().buf_empty(),
        KeyAction::AcceptGhostText,
    ),
    // ── App control ─────────────────────────────────────────────────────
    // Ctrl+C: context-dependent (menu/completer/clear/quit/cancel)
    // Note: menu and completer dismissal happen before keymap lookup.
    bind(
        KeyCode::Char('c'),
        CTRL,
        when().buf_not_empty(),
        KeyAction::ClearBuffer,
    ),
    bind(
        KeyCode::Char('c'),
        CTRL,
        when().buf_empty().running(),
        KeyAction::CancelAgent,
    ),
    bind(
        KeyCode::Char('c'),
        CTRL,
        when().buf_empty().idle(),
        KeyAction::Quit,
    ),
    bind(KeyCode::Char('s'), CTRL, when(), KeyAction::ToggleStash),
    bind(KeyCode::BackTab, NONE, when(), KeyAction::ToggleMode),
    bind(KeyCode::Char('t'), CTRL, when(), KeyAction::CycleReasoning),
    bind(KeyCode::Char('l'), CTRL, when(), KeyAction::Redraw),
    bind(
        KeyCode::Char('?'),
        NONE,
        when().buf_empty(),
        KeyAction::OpenHelp,
    ),
    // ── Submit / newline ────────────────────────────────────────────────
    bind_exclude(KeyCode::Enter, NONE, SHIFT, when(), KeyAction::Submit),
    bind(KeyCode::Enter, SHIFT, when(), KeyAction::InsertNewline),
    // Ctrl+J: navigation in vim normal, newline otherwise.
    // Ctrl+K: navigation in vim normal, kill-to-eol otherwise.
    bind(
        KeyCode::Char('j'),
        CTRL,
        when().vim_non_insert(),
        KeyAction::HistoryNext,
    ),
    bind(
        KeyCode::Char('k'),
        CTRL,
        when().vim_non_insert(),
        KeyAction::HistoryPrev,
    ),
    bind(
        KeyCode::Char('j'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::InsertNewline,
    ),
    // ── Vim half-page scroll (must be before emacs Ctrl+U/D) ────────────
    bind(
        KeyCode::Char('u'),
        CTRL,
        when().vim_non_insert(),
        KeyAction::VimHalfPageUp,
    ),
    bind(
        KeyCode::Char('d'),
        CTRL,
        when().vim_non_insert(),
        KeyAction::VimHalfPageDown,
    ),
    // ── Emacs navigation ────────────────────────────────────────────────
    bind(
        KeyCode::Char('a'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::MoveStartOfLine,
    ),
    bind(
        KeyCode::Char('e'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::MoveEndOfLine,
    ),
    bind(KeyCode::Char('f'), CTRL, when(), KeyAction::MoveRight),
    bind(KeyCode::Char('b'), CTRL, when(), KeyAction::MoveLeft),
    bind(KeyCode::Char('f'), ALT, when(), KeyAction::MoveWordForward),
    bind(KeyCode::Char('b'), ALT, when(), KeyAction::MoveWordBackward),
    // ── Emacs editing ───────────────────────────────────────────────────
    bind(
        KeyCode::Char('d'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::DeleteCharForward,
    ),
    bind(
        KeyCode::Char('d'),
        ALT,
        when(),
        KeyAction::DeleteWordForward,
    ),
    bind(
        KeyCode::Char('w'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::DeleteWordBackward,
    ),
    bind(
        KeyCode::Char('k'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::KillToEndOfLine,
    ),
    bind(
        KeyCode::Char('u'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::KillToStartOfLine,
    ),
    bind(
        KeyCode::Char('y'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::Yank,
    ),
    bind(
        KeyCode::Char('y'),
        ALT,
        when().not_vim_non_insert(),
        KeyAction::YankPop,
    ),
    bind(
        KeyCode::Char('u'),
        ALT,
        when().not_vim_non_insert(),
        KeyAction::UppercaseWord,
    ),
    bind(
        KeyCode::Char('l'),
        ALT,
        when().not_vim_non_insert(),
        KeyAction::LowercaseWord,
    ),
    bind(
        KeyCode::Char('c'),
        ALT,
        when().not_vim_non_insert(),
        KeyAction::CapitalizeWord,
    ),
    bind(
        KeyCode::Char('_'),
        CTRL,
        when().not_vim_non_insert(),
        KeyAction::Undo,
    ),
    // ── Selection (shift+movement, non-vim only) ──────────────────────
    // More-specific combos (shift+alt, shift+ctrl) must come before plain
    // shift so they match first. Note: Cmd (SUPER) combos are not included
    // because macOS terminals intercept them for scrollback/tab management.
    bind(
        KeyCode::Left,
        SHIFT.union(ALT),
        when(),
        KeyAction::SelectWordBackward,
    ),
    bind(
        KeyCode::Right,
        SHIFT.union(ALT),
        when(),
        KeyAction::SelectWordForward,
    ),
    bind(
        KeyCode::Left,
        SHIFT.union(CTRL),
        when(),
        KeyAction::SelectWordBackward,
    ),
    bind(
        KeyCode::Right,
        SHIFT.union(CTRL),
        when(),
        KeyAction::SelectWordForward,
    ),
    bind(KeyCode::Left, SHIFT, when(), KeyAction::SelectLeft),
    bind(KeyCode::Right, SHIFT, when(), KeyAction::SelectRight),
    bind(KeyCode::Up, SHIFT, when(), KeyAction::SelectUp),
    bind(KeyCode::Down, SHIFT, when(), KeyAction::SelectDown),
    bind(KeyCode::Home, SHIFT, when(), KeyAction::SelectStartOfLine),
    bind(KeyCode::End, SHIFT, when(), KeyAction::SelectEndOfLine),
    // ── Arrow / Home / End navigation ───────────────────────────────────
    bind(KeyCode::Left, ALT, when(), KeyAction::MoveWordBackward),
    bind(KeyCode::Left, SUPER, when(), KeyAction::MoveStartOfLine),
    bind(KeyCode::Left, NONE, when(), KeyAction::MoveLeft),
    bind(KeyCode::Right, ALT, when(), KeyAction::MoveWordForward),
    bind(KeyCode::Right, SUPER, when(), KeyAction::MoveEndOfLine),
    bind(KeyCode::Right, NONE, when(), KeyAction::MoveRight),
    bind(KeyCode::Up, SUPER, when(), KeyAction::MoveStartOfBuffer),
    bind(KeyCode::Up, NONE, when(), KeyAction::MoveUp),
    bind(KeyCode::Char('p'), CTRL, when(), KeyAction::HistoryPrev),
    bind(KeyCode::Down, SUPER, when(), KeyAction::MoveEndOfBuffer),
    bind(KeyCode::Down, NONE, when(), KeyAction::MoveDown),
    bind(KeyCode::Char('n'), CTRL, when(), KeyAction::HistoryNext),
    bind(KeyCode::Home, NONE, when(), KeyAction::MoveStartOfLine),
    bind(KeyCode::End, NONE, when(), KeyAction::MoveEndOfLine),
    // ── Delete / Backspace ──────────────────────────────────────────────
    bind(KeyCode::Delete, ALT, when(), KeyAction::DeleteWordForward),
    bind(KeyCode::Delete, NONE, when(), KeyAction::DeleteCharForward),
    bind(
        KeyCode::Backspace,
        ALT,
        when(),
        KeyAction::DeleteWordBackward,
    ),
    bind(
        KeyCode::Backspace,
        CTRL,
        when(),
        KeyAction::DeleteWordBackward,
    ),
    bind(
        KeyCode::Backspace,
        SUPER,
        when(),
        KeyAction::DeleteToStartOfLine,
    ),
    bind(KeyCode::Backspace, NONE, when(), KeyAction::Backspace),
    // ── Clipboard ───────────────────────────────────────────────────────
    bind(KeyCode::Char('c'), SUPER, when(), KeyAction::CopySelection),
    bind(KeyCode::Char('x'), SUPER, when(), KeyAction::CutSelection),
    bind(KeyCode::Char('v'), SUPER, when(), KeyAction::ClipboardImage),
];

// ── Lookup ───────────────────────────────────────────────────────────────────

/// Look up the first matching action for the given key event and context.
/// Returns `None` if no binding matches (caller should try vim / char insert).
pub fn lookup(code: KeyCode, modifiers: KeyModifiers, ctx: &KeyContext) -> Option<KeyAction> {
    for b in BINDINGS {
        if b.code != code {
            continue;
        }
        // Check required modifiers are present.
        if !modifiers.contains(b.mods) {
            continue;
        }
        // Check excluded modifiers are absent.
        if !b.exclude_mods.is_empty() && modifiers.contains(b.exclude_mods) {
            continue;
        }
        // For bindings that require exact modifiers (NONE), don't match if
        // extra modifiers are pressed — unless the binding uses ALT/CTRL/SUPER
        // which are explicitly checked via `contains`.
        if b.mods.is_empty() && !modifiers.is_empty() {
            // Allow SHIFT through — terminals set it for uppercase chars,
            // BackTab, and sometimes arrow keys. Only block if a "real"
            // modifier (CTRL/ALT/SUPER) is present.
            if modifiers.intersects(CTRL.union(ALT).union(SUPER)) {
                continue;
            }
        }
        if b.when.matches(ctx) {
            return Some(b.action);
        }
    }
    None
}

// ── Dialog keymap ────────────────────────────────────────────────────────────

/// Actions shared across dialogs, menus, and list navigation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavAction {
    /// Dismiss / cancel the dialog.
    Dismiss,
    /// Confirm the current selection.
    Confirm,
    /// Move selection up one item.
    Up,
    /// Move selection down one item.
    Down,
    /// Scroll up one page.
    PageUp,
    /// Scroll down one page.
    PageDown,
    /// Switch to text editing mode (e.g. Tab in confirm dialog).
    Edit,
}

/// Shared dialog key bindings. Dialogs call this instead of hand-matching
/// the same keys everywhere. Returns `None` for keys the dialog should
/// handle with its own specific logic.
pub fn nav_lookup(code: KeyCode, modifiers: KeyModifiers) -> Option<NavAction> {
    match (code, modifiers) {
        // Dismiss
        (KeyCode::Esc, _) => Some(NavAction::Dismiss),
        (KeyCode::Char('c'), m) if m.contains(CTRL) => Some(NavAction::Dismiss),
        // Confirm
        (KeyCode::Enter, m) if !m.contains(SHIFT) => Some(NavAction::Confirm),
        // Page scroll
        (KeyCode::Char('u'), m) if m.contains(CTRL) => Some(NavAction::PageUp),
        (KeyCode::Char('d'), m) if m.contains(CTRL) => Some(NavAction::PageDown),
        (KeyCode::Up, m) if m.contains(ALT) => Some(NavAction::PageUp),
        (KeyCode::Down, m) if m.contains(ALT) => Some(NavAction::PageDown),
        (KeyCode::PageUp, _) => Some(NavAction::PageUp),
        (KeyCode::PageDown, _) => Some(NavAction::PageDown),
        // Item navigation
        (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => Some(NavAction::Up),
        (KeyCode::Char('k'), m) if m.contains(CTRL) => Some(NavAction::Up),
        (KeyCode::Char('p'), m) if m.contains(CTRL) => Some(NavAction::Up),
        (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => Some(NavAction::Down),
        (KeyCode::Char('j'), m) if m.contains(CTRL) => Some(NavAction::Down),
        (KeyCode::Char('n'), m) if m.contains(CTRL) => Some(NavAction::Down),
        // Edit mode
        (KeyCode::Tab, _) => Some(NavAction::Edit),
        _ => None,
    }
}

// ── Contextual hints ─────────────────────────────────────────────────────────

/// Shared hint fragments for dialog footers.
pub mod hints {
    // Common nav
    pub fn nav(vim: bool) -> &'static str {
        if vim {
            "j/k: navigate"
        } else {
            "↑/↓: navigate"
        }
    }
    pub fn picker_nav(vim: bool) -> &'static str {
        if vim {
            "ctrl+j/k: navigate"
        } else {
            "↑/↓: navigate"
        }
    }
    pub fn scroll(vim: bool) -> &'static str {
        if vim {
            "ctrl+u/d: scroll"
        } else {
            "pgup/pgdn: scroll"
        }
    }
    pub const CLOSE: &str = "esc: close";
    pub const CANCEL: &str = "esc: cancel";
    pub const CONFIRM: &str = "enter: confirm";
    pub const SELECT: &str = "enter: select";

    // Dialog-specific
    pub const SEND: &str = "enter: send";
    pub const ADD_MSG: &str = "tab: add message";
    pub const EDIT_MSG: &str = "tab: edit";
    pub const CONFIRM_WITH_MSG: &str = "enter: confirm with message";
    pub fn dd_delete(vim: bool) -> &'static str {
        if vim {
            "dd/\u{232b}: delete"
        } else {
            "\u{232b}/dd: delete"
        }
    }
    pub fn del_delete(vim: bool) -> &'static str {
        if vim {
            "dd/del: delete"
        } else {
            "del: delete"
        }
    }
    pub const DD_PENDING: &str = "press d to confirm delete";
    pub const KILL_PROC: &str = "\u{232b}: kill selected";
    pub const BACK: &str = "esc: back";
    pub const NEXT_Q: &str = "tab: next question";

    /// Build a hint line from fragments.
    pub fn join(parts: &[&str]) -> String {
        let mut s = String::from(" ");
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                s.push_str("  ");
            }
            s.push_str(part);
        }
        s
    }

    // ── Help dialog content ─────────────────────────────────────────

    const HELP_PREFIXES: &[(&str, &str)] = &[
        (
            "/command",
            "commands  (try /resume, /compact, /fork, /ps, /vim\u{2026})",
        ),
        ("@<path>", "attach a file or URL"),
        ("!<cmd>", "run a shell command"),
    ];

    const HELP_KEYS: &[(&str, &str)] = &[
        ("enter", "send message"),
        ("ctrl+j  shift+enter", "insert newline"),
        ("ctrl+c", "cancel / interrupt / quit"),
        ("ctrl+r", "search input history"),
        ("ctrl+t", "cycle reasoning effort"),
        ("ctrl+s", "stash / unstash input"),
        (
            "shift+tab",
            "cycle mode  (normal \u{2192} plan \u{2192} apply \u{2192} yolo)",
        ),
        ("\u{2191}/\u{2193}  ctrl+n/p", "navigate history / items"),
        ("ctrl+u / ctrl+d", "scroll up / down  (half page)"),
        ("ctrl+a / ctrl+e", "line start / end"),
        ("ctrl+k / ctrl+w", "kill to end / delete word"),
        ("ctrl+y / alt+y", "yank / yank-pop (cycle kill ring)"),
        (
            "alt+u / alt+l / alt+c",
            "uppercase / lowercase / capitalize word",
        ),
        ("ctrl+_", "undo"),
        ("ctrl+x ctrl+e", "edit in $EDITOR"),
        (
            "shift+\u{2190}/\u{2192}",
            "select text (shift+alt: by word, shift+home/end: line)",
        ),
        ("tab", "autocomplete / accept ghost text"),
        ("esc", "dismiss / unqueue messages"),
        ("esc esc", "cancel agent / compaction / rewind"),
    ];

    const HELP_VIM_OVERRIDES: &[(&str, &str)] = &[
        ("ctrl+j / ctrl+k", "history next / prev  (normal mode)"),
        ("ctrl+u / ctrl+d", "half-page up / down  (normal mode)"),
        ("ctrl+r", "redo  (normal mode)"),
        ("v / V", "visual / visual-line selection  (normal mode)"),
    ];

    /// Build the help sections for the current mode.
    pub fn help_sections(
        vim_enabled: bool,
    ) -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
        let mut sections = vec![
            ("prefixes", HELP_PREFIXES.to_vec()),
            ("keys", HELP_KEYS.to_vec()),
        ];
        if vim_enabled {
            sections.push(("vim normal overrides", HELP_VIM_OVERRIDES.to_vec()));
        }
        sections
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> KeyContext {
        KeyContext {
            buf_empty: true,
            vim_non_insert: false,
            vim_enabled: false,
            agent_running: false,
            ghost_text_visible: false,
        }
    }

    #[test]
    fn ctrl_c_empty_idle_quits() {
        let c = ctx();
        assert_eq!(lookup(KeyCode::Char('c'), CTRL, &c), Some(KeyAction::Quit));
    }

    #[test]
    fn ctrl_c_nonempty_clears() {
        let c = KeyContext {
            buf_empty: false,
            ..ctx()
        };
        assert_eq!(
            lookup(KeyCode::Char('c'), CTRL, &c),
            Some(KeyAction::ClearBuffer)
        );
    }

    #[test]
    fn ctrl_c_running_cancels() {
        let c = KeyContext {
            agent_running: true,
            ..ctx()
        };
        assert_eq!(
            lookup(KeyCode::Char('c'), CTRL, &c),
            Some(KeyAction::CancelAgent)
        );
    }

    #[test]
    fn ctrl_u_vim_non_insert_is_halfpage() {
        let c = KeyContext {
            vim_non_insert: true,
            vim_enabled: true,
            ..ctx()
        };
        assert_eq!(
            lookup(KeyCode::Char('u'), CTRL, &c),
            Some(KeyAction::VimHalfPageUp)
        );
    }

    #[test]
    fn ctrl_u_insert_is_kill() {
        let c = ctx();
        assert_eq!(
            lookup(KeyCode::Char('u'), CTRL, &c),
            Some(KeyAction::KillToStartOfLine)
        );
    }

    #[test]
    fn ctrl_r_vim_non_insert_no_match() {
        let c = KeyContext {
            vim_non_insert: true,
            vim_enabled: true,
            ..ctx()
        };
        // Ctrl+R in vim normal → no keymap match (vim handler does redo)
        assert_eq!(lookup(KeyCode::Char('r'), CTRL, &c), None);
    }

    #[test]
    fn question_mark_nonempty_no_match() {
        let c = KeyContext {
            buf_empty: false,
            ..ctx()
        };
        assert_eq!(lookup(KeyCode::Char('?'), NONE, &c), None);
    }

    #[test]
    fn ghost_text_tab_accepts() {
        let c = KeyContext {
            ghost_text_visible: true,
            ..ctx()
        };
        assert_eq!(
            lookup(KeyCode::Tab, NONE, &c),
            Some(KeyAction::AcceptGhostText)
        );
    }

    #[test]
    fn tab_without_ghost_text_no_match() {
        let c = ctx();
        assert_eq!(lookup(KeyCode::Tab, NONE, &c), None);
    }

    #[test]
    fn enter_submits() {
        let c = ctx();
        assert_eq!(lookup(KeyCode::Enter, NONE, &c), Some(KeyAction::Submit));
    }

    #[test]
    fn shift_enter_inserts_newline() {
        let c = ctx();
        assert_eq!(
            lookup(KeyCode::Enter, SHIFT, &c),
            Some(KeyAction::InsertNewline)
        );
    }

    #[test]
    fn backtab_toggles_mode() {
        let c = ctx();
        // Terminals send BackTab with SHIFT modifier.
        assert_eq!(
            lookup(KeyCode::BackTab, SHIFT, &c),
            Some(KeyAction::ToggleMode)
        );
    }

    #[test]
    fn alt_backspace_deletes_word() {
        let c = ctx();
        assert_eq!(
            lookup(KeyCode::Backspace, ALT, &c),
            Some(KeyAction::DeleteWordBackward)
        );
    }

    #[test]
    fn plain_backspace() {
        let c = ctx();
        assert_eq!(
            lookup(KeyCode::Backspace, NONE, &c),
            Some(KeyAction::Backspace)
        );
    }

    #[test]
    fn shift_left_selects() {
        let c = ctx();
        assert_eq!(
            lookup(KeyCode::Left, SHIFT, &c),
            Some(KeyAction::SelectLeft)
        );
    }

    #[test]
    fn shift_alt_left_selects_word_backward() {
        let c = ctx();
        assert_eq!(
            lookup(KeyCode::Left, SHIFT.union(ALT), &c),
            Some(KeyAction::SelectWordBackward)
        );
    }
}
