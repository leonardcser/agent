mod motions;
mod text_objects;

pub(crate) use motions::{move_down, move_down_col, move_up, move_up_col};

use crate::kill_ring::KillRing;
use crate::text::{
    char_class, line_end, line_start, word_backward_pos, word_forward_pos, CharClass,
};
use crate::undo::{UndoEntry, UndoHistory};
use crate::AttachmentId;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use motions::{
    advance_chars, clamp_normal, current_line_content_range, current_line_range, find_char,
    find_matching_bracket, first_non_blank, first_non_blank_at, goto_line, line_end_normal,
    move_left, move_right_inclusive, move_right_normal, next_char_boundary, prev_char_boundary,
    repeat_find, retreat_chars, word_end_pos,
};
use text_objects::text_object;

// ── Public types ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ViMode {
    Insert,
    Normal,
    Visual,
    VisualLine,
}

/// What the caller should do after a key is processed.
#[derive(Debug, PartialEq)]
pub enum Action {
    /// Key consumed, buf/cpos may have changed.
    Consumed,
    /// Submit the input (Enter).
    Submit,
    /// Navigate history up.
    HistoryPrev,
    /// Navigate history down.
    HistoryNext,
    /// Open buffer in $EDITOR.
    EditInEditor,
    /// Center the input viewport on the cursor (zz).
    CenterScroll,
    /// Key not handled — caller should use its own logic.
    Passthrough,
}

/// Shared mutable state that vim needs to operate on.
///
/// Vim no longer owns a private register or undo history — those live on
/// `InputState` (as the kill ring and the single `UndoHistory`). The caller
/// bundles them here along with the live buffer so vim can read and mutate
/// them without any post-keystroke synchronization.
///
/// `clipboard` bridges yank / paste with the system clipboard: yanks
/// mirror into it, pastes pull from it when it was updated externally
/// (see `KillRing::last_clipboard_write`).
pub struct VimContext<'a> {
    pub buf: &'a mut String,
    pub cpos: &'a mut usize,
    pub attachments: &'a mut Vec<AttachmentId>,
    pub kill_ring: &'a mut KillRing,
    pub history: &'a mut UndoHistory,
    pub clipboard: &'a mut dyn crate::clipboard::Clipboard,
}

impl VimContext<'_> {
    /// Snapshot buffer state into undo history before mutating.
    fn save_undo(&mut self) {
        self.history
            .save(UndoEntry::snapshot(self.buf, *self.cpos, self.attachments));
    }

    /// Undo: pop the most recent snapshot, stashing the current state on redo.
    fn undo(&mut self) {
        let current = UndoEntry::snapshot(self.buf, *self.cpos, self.attachments);
        if let Some(entry) = self.history.undo(current) {
            *self.buf = entry.buf;
            *self.cpos = entry.cpos;
            *self.attachments = entry.attachments;
            clamp_normal(self.buf, self.cpos);
        }
    }

    /// Redo: pop the most recent redo, stashing the current state on undo.
    fn redo(&mut self) {
        let current = UndoEntry::snapshot(self.buf, *self.cpos, self.attachments);
        if let Some(entry) = self.history.redo(current) {
            *self.buf = entry.buf;
            *self.cpos = entry.cpos;
            *self.attachments = entry.attachments;
            clamp_normal(self.buf, self.cpos);
        }
    }

    /// Copy `buf[start..end]` into the kill ring with the given linewise flag.
    /// Also mirrors the text to the system clipboard so a `y` in smelt is
    /// immediately pasteable in other apps, matching nvim's
    /// `clipboard=unnamedplus`.
    fn yank_range(&mut self, start: usize, end: usize, linewise: bool) {
        let text = self.buf[start..end].to_string();
        self.kill_ring
            .set_with_source(text.clone(), linewise, start, end);
        if self.clipboard.write(&text).is_ok() {
            self.kill_ring.record_clipboard_write(text);
        }
    }

    fn register(&self) -> &str {
        self.kill_ring.current()
    }

    fn register_linewise(&self) -> bool {
        self.kill_ring.is_linewise()
    }

    /// Before reading the register for a paste, reconcile with the
    /// system clipboard. If the clipboard holds text different from
    /// what we last pushed, an external source updated it — overwrite
    /// the kill ring with that text (charwise, since external sources
    /// don't carry vim's linewise flag). When they match, the kill
    /// ring stays authoritative so `p` vs `P` + linewise still work.
    fn sync_paste_from_clipboard(&mut self) {
        let current = self.clipboard.read();
        let Some(text) = current else { return };
        let prev = self.kill_ring.last_clipboard_write().map(str::to_owned);
        if prev.as_deref() == Some(text.as_str()) {
            return;
        }
        self.kill_ring.set_with_linewise(text.clone(), false);
        self.kill_ring.record_clipboard_write(text);
    }
}

// ── Internal types ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Delete,
    Change,
    Yank,
}

impl Op {
    fn char(self) -> char {
        match self {
            Op::Delete => 'd',
            Op::Change => 'c',
            Op::Yank => 'y',
        }
    }
}

#[derive(Clone, Copy)]
enum FindKind {
    Forward,
    ForwardTill,
    Backward,
    BackwardTill,
}

impl FindKind {
    fn reversed(self) -> Self {
        match self {
            FindKind::Forward => FindKind::Backward,
            FindKind::ForwardTill => FindKind::BackwardTill,
            FindKind::Backward => FindKind::Forward,
            FindKind::BackwardTill => FindKind::ForwardTill,
        }
    }
}

#[derive(Clone, Copy)]
enum SubState {
    Ready,
    WaitingOp(Op),
    WaitingG,
    WaitingZ,
    /// Operator pending + `g` pressed, waiting for `g` to complete `gg` motion.
    WaitingOpG(Op),
    WaitingR,
    WaitingFind(FindKind),
    /// Operator pending + find motion (e.g. `df`, `dt`), waiting for the target char.
    WaitingOpFind(Op, FindKind),
    /// Operator + `i`/`a` pressed, waiting for object type char.
    WaitingTextObj(Op, bool),
    /// Visual mode `i`/`a` pressed, waiting for object type char.
    WaitingVisualTextObj(bool),
}

// ── Vim state ───────────────────────────────────────────────────────────────

pub struct Vim {
    mode: ViMode,
    sub: SubState,
    /// Count accumulated before the operator (or before a standalone motion).
    count1: Option<usize>,
    /// Count accumulated after the operator, before the motion.
    count2: Option<usize>,
    last_find: Option<(FindKind, char)>,
    /// Byte position of the visual mode anchor (where 'v'/'V' was pressed).
    visual_anchor: usize,
    /// Desired column for vertical motions (j/k). Synchronised with
    /// the owning window's `WindowCursor.curswant` at every entry to
    /// `handle_key` so the keymap's shift+arrow path and vim's j/k
    /// share one source of truth — vertical motion behaves identically
    /// regardless of which key produced it.
    curswant: Option<usize>,
}

impl Default for Vim {
    fn default() -> Self {
        Self::new()
    }
}

impl Vim {
    pub fn new() -> Self {
        Self {
            mode: ViMode::Insert,
            sub: SubState::Ready,
            count1: None,
            count2: None,
            last_find: None,
            visual_anchor: 0,
            curswant: None,
        }
    }

    pub fn mode(&self) -> ViMode {
        self.mode
    }

    /// Returns the visual selection range (start, end) as byte offsets when
    /// in visual or visual-line mode. The range is always ordered (start <= end).
    pub fn visual_range(&self, buf: &str, cpos: usize) -> Option<(usize, usize)> {
        match self.mode {
            ViMode::Visual => {
                let anchor = self.visual_anchor.min(buf.len());
                let cursor = cpos.min(buf.len());
                let (a, b) = if anchor <= cursor {
                    (anchor, next_char_boundary(buf, cursor).min(buf.len()))
                } else {
                    (cursor, next_char_boundary(buf, anchor).min(buf.len()))
                };
                Some((a, b))
            }
            ViMode::VisualLine => {
                let anchor = self.visual_anchor.min(buf.len());
                let cursor = cpos.min(buf.len());
                let start = line_start(buf, anchor).min(line_start(buf, cursor));
                let end = line_end(buf, anchor).max(line_end(buf, cursor));
                Some((start, end))
            }
            _ => None,
        }
    }

    pub fn set_mode(&mut self, mode: ViMode) {
        self.mode = mode;
        self.sub = SubState::Ready;
        self.reset_counts();
    }

    /// Read the current preferred vertical-motion column. Callers sync
    /// this back to the owning window's `WindowCursor` after a dispatch
    /// so shift+arrow and vim j/k share one curswant.
    pub fn curswant(&self) -> Option<usize> {
        self.curswant
    }

    /// Seed `curswant` before a dispatch (from the window's
    /// `WindowCursor.curswant`). Vim's j / k / visual-j / visual-k then
    /// read and update this value; the caller writes it back out.
    pub fn set_curswant(&mut self, c: Option<usize>) {
        self.curswant = c;
    }

    /// Anchor visual selection at `cpos` and enter the requested visual
    /// mode (`Visual` or `VisualLine`). Used by mouse drag-select so the
    /// selection originates at the click rather than the previous
    /// cursor position.
    /// Read the Visual-mode anchor byte. Returns `Some(byte)` only
    /// while in `Visual`/`VisualLine`; `None` in Normal/Insert. Used by
    /// the prompt mouse adapter to translate between source-byte and
    /// wrapped-byte spaces across `Window::handle_mouse` calls.
    pub fn visual_anchor(&self) -> Option<usize> {
        match self.mode {
            ViMode::Visual | ViMode::VisualLine => Some(self.visual_anchor),
            _ => None,
        }
    }

    pub fn begin_visual(&mut self, mode: ViMode, cpos: usize) {
        self.mode = mode;
        self.sub = SubState::Ready;
        self.reset_counts();
        self.visual_anchor = cpos;
    }

    /// Process a key event. Reads and mutates `ctx` (buffer, cursor,
    /// attachments, kill ring, undo history) as needed.
    pub fn handle_key(&mut self, key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
        match self.mode {
            ViMode::Insert => self.handle_insert(key, ctx),
            ViMode::Normal => self.handle_normal(key, ctx),
            ViMode::Visual | ViMode::VisualLine => self.handle_visual(key, ctx),
        }
    }

    // ── Insert mode ─────────────────────────────────────────────────────

    fn handle_insert(&mut self, key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
        match key {
            // Esc or Ctrl+[ → normal mode
            KeyEvent {
                code: KeyCode::Esc, ..
            }
            | KeyEvent {
                code: KeyCode::Char('['),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.enter_normal(ctx.buf, ctx.cpos);
                Action::Consumed
            }
            // Ctrl+W / Ctrl+U → pass through to main handler (kill ring support).
            KeyEvent {
                code: KeyCode::Char('w' | 'u'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => Action::Passthrough,
            // Ctrl+H → backspace (same as Backspace, but let caller handle)
            KeyEvent {
                code: KeyCode::Char('h'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => Action::Passthrough,
            // Everything else → let caller handle normal insert editing
            _ => Action::Passthrough,
        }
    }

    // ── Normal mode ─────────────────────────────────────────────────────

    fn handle_normal(&mut self, key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
        // Ctrl+key handling in normal mode.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('r') => {
                    ctx.redo();
                    return Action::Consumed;
                }
                // Pass through keys that the main handler needs.
                KeyCode::Char(
                    'c' | 'd' | 'u' | 't' | 'k' | 'l' | 'f' | 'b' | 'j' | 'n' | 'p' | 's',
                ) => return Action::Passthrough,
                _ => return Action::Consumed,
            }
        }

        // BackTab passes through for mode toggle.
        if key.code == KeyCode::BackTab {
            return Action::Passthrough;
        }

        // Shift+arrow / Shift+Home/End pass through so the keymap's
        // shared shift-selection actions (`SelectLeft`, …) run —
        // selection extension is the same operation whether vim is on
        // or off, so it lives in one place.
        if key.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(
                key.code,
                KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Home
                    | KeyCode::End
            )
        {
            return Action::Passthrough;
        }

        // Handle sub-states first.
        match self.sub {
            SubState::WaitingR => return self.handle_waiting_r(key, ctx),
            SubState::WaitingZ => {
                self.sub = SubState::Ready;
                return if matches!(key.code, KeyCode::Char('z')) {
                    Action::CenterScroll
                } else {
                    Action::Consumed
                };
            }
            SubState::WaitingFind(kind) => return self.handle_waiting_find(key, kind, ctx),
            SubState::WaitingOpFind(op, kind) => {
                return self.handle_waiting_op_find(key, op, kind, ctx)
            }
            SubState::WaitingG => return self.handle_waiting_g(key, ctx),
            SubState::WaitingOpG(op) => return self.handle_waiting_op_g(key, op, ctx),
            SubState::WaitingTextObj(op, inner) => {
                return self.handle_waiting_textobj(key, op, inner, ctx)
            }
            SubState::WaitingOp(op) => {
                // Could be digit, motion, text object prefix (i/a), or same-key (dd/cc/yy).
                if let KeyCode::Char(c) = key.code {
                    // Digit accumulation for count2.
                    if c.is_ascii_digit() && (c != '0' || self.count2.is_some()) {
                        self.count2 =
                            Some(self.count2.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize);
                        return Action::Consumed;
                    }
                    // Same operator key → linewise (dd, cc, yy).
                    if c == op.char() {
                        return self.execute_linewise_op(op, ctx);
                    }
                    // Text object prefix.
                    if c == 'i' || c == 'a' {
                        self.sub = SubState::WaitingTextObj(op, c == 'i');
                        return Action::Consumed;
                    }
                }
                // Otherwise try as a motion.
                let result = self.execute_op_motion(key, op, ctx);
                // Don't reset if execute_op_motion transitioned to a new substate
                // (e.g. WaitingOpFind for df/dt combos).
                if matches!(self.sub, SubState::WaitingOp(_)) {
                    self.reset_pending();
                }
                return result;
            }
            SubState::WaitingVisualTextObj(_) | SubState::Ready => {}
        }

        // Ready state — handle count digits, commands, motions.
        if let KeyCode::Char(c) = key.code {
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                return self.handle_normal_char(c, ctx);
            }
        }

        // Non-char keys in normal mode.
        match key.code {
            KeyCode::Esc => {
                self.reset_pending();
                Action::Consumed
            }
            KeyCode::Enter => Action::Submit,
            KeyCode::Left => {
                *ctx.cpos = move_left(ctx.buf, *ctx.cpos);
                Action::Consumed
            }
            KeyCode::Right => {
                *ctx.cpos = move_right_normal(ctx.buf, *ctx.cpos);
                Action::Consumed
            }
            KeyCode::Up => Action::HistoryPrev,
            KeyCode::Down => Action::HistoryNext,
            KeyCode::Home => {
                *ctx.cpos = line_start(ctx.buf, *ctx.cpos);
                Action::Consumed
            }
            KeyCode::End => {
                *ctx.cpos = line_end_normal(ctx.buf, *ctx.cpos);
                Action::Consumed
            }
            KeyCode::Backspace => {
                *ctx.cpos = move_left(ctx.buf, *ctx.cpos);
                Action::Consumed
            }
            _ => Action::Consumed,
        }
    }

    fn handle_normal_char(&mut self, c: char, ctx: &mut VimContext<'_>) -> Action {
        // Clear desired column for any non-vertical motion.
        if c != 'j' && c != 'k' && !c.is_ascii_digit() {
            self.curswant = None;
        }

        // Count digit accumulation.
        if c.is_ascii_digit() && (c != '0' || self.count1.is_some()) {
            self.count1 = Some(self.count1.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize);
            return Action::Consumed;
        }

        match c {
            // ── Operators ───────────────────────────────────────────────
            'd' => {
                self.sub = SubState::WaitingOp(Op::Delete);
                Action::Consumed
            }
            'c' => {
                self.sub = SubState::WaitingOp(Op::Change);
                Action::Consumed
            }
            'y' => {
                self.sub = SubState::WaitingOp(Op::Yank);
                Action::Consumed
            }

            // ── Operator shortcuts ──────────────────────────────────────
            'D' => {
                ctx.save_undo();
                let end = line_end(ctx.buf, *ctx.cpos);
                ctx.yank_range(*ctx.cpos, end, false);
                ctx.buf.drain(*ctx.cpos..end);
                clamp_normal(ctx.buf, ctx.cpos);
                self.reset_pending();
                Action::Consumed
            }
            'C' => {
                ctx.save_undo();
                let end = line_end(ctx.buf, *ctx.cpos);
                ctx.yank_range(*ctx.cpos, end, false);
                ctx.buf.drain(*ctx.cpos..end);
                self.enter_insert_mode();
                Action::Consumed
            }
            'Y' => {
                let (start, end) = current_line_range(ctx.buf, *ctx.cpos);
                ctx.yank_range(start, end, true);
                ctx.kill_ring.mark_yanked();
                self.reset_pending();
                Action::Consumed
            }

            // ── Direct edits ────────────────────────────────────────────
            'x' => {
                let n = self.take_count();
                if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
                    ctx.save_undo();
                    let end = advance_chars(ctx.buf, *ctx.cpos, n);
                    ctx.yank_range(*ctx.cpos, end, false);
                    ctx.buf.drain(*ctx.cpos..end);
                    clamp_normal(ctx.buf, ctx.cpos);
                }
                self.reset_pending();
                Action::Consumed
            }
            'X' => {
                let n = self.take_count();
                if *ctx.cpos > 0 {
                    ctx.save_undo();
                    let start = retreat_chars(ctx.buf, *ctx.cpos, n);
                    ctx.yank_range(start, *ctx.cpos, false);
                    ctx.buf.drain(start..*ctx.cpos);
                    *ctx.cpos = start;
                    clamp_normal(ctx.buf, ctx.cpos);
                }
                self.reset_pending();
                Action::Consumed
            }
            's' => {
                let n = self.take_count();
                ctx.save_undo();
                if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
                    let end = advance_chars(ctx.buf, *ctx.cpos, n);
                    ctx.yank_range(*ctx.cpos, end, false);
                    ctx.buf.drain(*ctx.cpos..end);
                }
                self.enter_insert_mode();
                Action::Consumed
            }
            'S' => {
                ctx.save_undo();
                let (start, end) = current_line_content_range(ctx.buf, *ctx.cpos);
                ctx.yank_range(start, end, false);
                ctx.buf.drain(start..end);
                *ctx.cpos = start;
                self.enter_insert_mode();
                Action::Consumed
            }
            'r' => {
                self.sub = SubState::WaitingR;
                Action::Consumed
            }
            '~' => {
                let n = self.take_count();
                if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
                    ctx.save_undo();
                    for _ in 0..n {
                        if *ctx.cpos >= ctx.buf.len() {
                            break;
                        }
                        let ch = ctx.buf[*ctx.cpos..].chars().next().unwrap();
                        let end = *ctx.cpos + ch.len_utf8();
                        let toggled: String = if ch.is_uppercase() {
                            ch.to_lowercase().collect()
                        } else {
                            ch.to_uppercase().collect()
                        };
                        ctx.buf.replace_range(*ctx.cpos..end, &toggled);
                        *ctx.cpos += toggled.len();
                    }
                    clamp_normal(ctx.buf, ctx.cpos);
                }
                self.reset_pending();
                Action::Consumed
            }

            // ── Paste ───────────────────────────────────────────────────
            'p' => {
                ctx.sync_paste_from_clipboard();
                if !ctx.register().is_empty() {
                    ctx.save_undo();
                    if ctx.register_linewise() {
                        let eol = line_end(ctx.buf, *ctx.cpos);
                        let text = ctx.register().to_string();
                        let insert = format!("\n{}", text);
                        ctx.buf.insert_str(eol, &insert);
                        *ctx.cpos = eol + 1;
                        // Move to first non-blank.
                        *ctx.cpos += ctx.buf[*ctx.cpos..]
                            .bytes()
                            .take_while(|b| *b == b' ' || *b == b'\t')
                            .count();
                    } else {
                        let after = advance_chars(ctx.buf, *ctx.cpos, 1).min(ctx.buf.len());
                        let text = ctx.register().to_string();
                        ctx.buf.insert_str(after, &text);
                        let paste_end = after + text.len();
                        *ctx.cpos = prev_char_boundary(ctx.buf, paste_end).max(after);
                        clamp_normal(ctx.buf, ctx.cpos);
                    }
                }
                Action::Consumed
            }
            'P' => {
                ctx.sync_paste_from_clipboard();
                if !ctx.register().is_empty() {
                    ctx.save_undo();
                    if ctx.register_linewise() {
                        let sol = line_start(ctx.buf, *ctx.cpos);
                        let text = ctx.register().to_string();
                        let insert = format!("{}\n", text);
                        ctx.buf.insert_str(sol, &insert);
                        *ctx.cpos = sol;
                        *ctx.cpos += ctx.buf[*ctx.cpos..]
                            .bytes()
                            .take_while(|b| *b == b' ' || *b == b'\t')
                            .count();
                    } else {
                        let text = ctx.register().to_string();
                        ctx.buf.insert_str(*ctx.cpos, &text);
                        let plen = text.len();
                        if plen > 0 {
                            let paste_end = *ctx.cpos + plen;
                            *ctx.cpos = prev_char_boundary(ctx.buf, paste_end).max(*ctx.cpos);
                            clamp_normal(ctx.buf, ctx.cpos);
                        }
                    }
                }
                Action::Consumed
            }

            // ── Undo / Redo ─────────────────────────────────────────────
            'u' => {
                ctx.undo();
                Action::Consumed
            }

            // ── Visual mode ─────────────────────────────────────────────
            'v' => {
                self.visual_anchor = *ctx.cpos;
                self.mode = ViMode::Visual;
                self.reset_pending();
                Action::Consumed
            }
            'V' => {
                self.visual_anchor = *ctx.cpos;
                self.mode = ViMode::VisualLine;
                self.reset_pending();
                Action::Consumed
            }

            // ── Enter insert mode ───────────────────────────────────────
            'i' => {
                self.take_count();
                ctx.save_undo();
                self.enter_insert_mode();
                Action::Consumed
            }
            'I' => {
                self.take_count();
                ctx.save_undo();
                *ctx.cpos = first_non_blank(ctx.buf, *ctx.cpos);
                self.enter_insert_mode();
                Action::Consumed
            }
            'a' => {
                self.take_count();
                ctx.save_undo();
                if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
                    *ctx.cpos = advance_chars(ctx.buf, *ctx.cpos, 1);
                }
                self.enter_insert_mode();
                Action::Consumed
            }
            'A' => {
                self.take_count();
                ctx.save_undo();
                *ctx.cpos = line_end(ctx.buf, *ctx.cpos);
                self.enter_insert_mode();
                Action::Consumed
            }
            'o' => {
                ctx.save_undo();
                let eol = line_end(ctx.buf, *ctx.cpos);
                ctx.buf.insert(eol, '\n');
                *ctx.cpos = eol + 1;
                self.enter_insert_mode();
                Action::Consumed
            }
            'O' => {
                ctx.save_undo();
                let sol = line_start(ctx.buf, *ctx.cpos);
                ctx.buf.insert(sol, '\n');
                *ctx.cpos = sol;
                self.enter_insert_mode();
                Action::Consumed
            }

            // ── Find ────────────────────────────────────────────────────
            'f' => {
                self.sub = SubState::WaitingFind(FindKind::Forward);
                Action::Consumed
            }
            'F' => {
                self.sub = SubState::WaitingFind(FindKind::Backward);
                Action::Consumed
            }
            't' => {
                self.sub = SubState::WaitingFind(FindKind::ForwardTill);
                Action::Consumed
            }
            'T' => {
                self.sub = SubState::WaitingFind(FindKind::BackwardTill);
                Action::Consumed
            }
            ';' => {
                if let Some((kind, ch)) = self.last_find {
                    let n = self.take_count();
                    *ctx.cpos = repeat_find(ctx.buf, *ctx.cpos, kind, ch, n);
                }
                self.reset_pending();
                Action::Consumed
            }
            ',' => {
                if let Some((kind, ch)) = self.last_find {
                    let n = self.take_count();
                    *ctx.cpos = repeat_find(ctx.buf, *ctx.cpos, kind.reversed(), ch, n);
                }
                self.reset_pending();
                Action::Consumed
            }

            // ── Wait-for-second-char ────────────────────────────────────
            'g' => {
                self.sub = SubState::WaitingG;
                Action::Consumed
            }
            'z' => {
                self.sub = SubState::WaitingZ;
                Action::Consumed
            }

            // ── Motions ─────────────────────────────────────────────────
            'h' => {
                let n = self.take_count();
                for _ in 0..n {
                    *ctx.cpos = move_left(ctx.buf, *ctx.cpos);
                }
                Action::Consumed
            }
            'l' => {
                let n = self.take_count();
                for _ in 0..n {
                    *ctx.cpos = move_right_normal(ctx.buf, *ctx.cpos);
                }
                Action::Consumed
            }
            'j' => {
                let n = self.take_count();
                if ctx.buf.contains('\n') {
                    let (new_pos, col) = move_down_col(ctx.buf, *ctx.cpos, self.curswant);
                    if new_pos == *ctx.cpos && n <= 1 {
                        self.reset_pending();
                        return Action::HistoryNext;
                    }
                    self.curswant = Some(col);
                    *ctx.cpos = new_pos;
                    for _ in 1..n {
                        (*ctx.cpos, _) = move_down_col(ctx.buf, *ctx.cpos, self.curswant);
                    }
                    clamp_normal(ctx.buf, ctx.cpos);
                    return Action::Consumed;
                }
                self.reset_pending();
                if n <= 1 {
                    Action::HistoryNext
                } else {
                    Action::Consumed
                }
            }
            'k' => {
                let n = self.take_count();
                if ctx.buf.contains('\n') {
                    let (new_pos, col) = move_up_col(ctx.buf, *ctx.cpos, self.curswant);
                    if new_pos == *ctx.cpos && n <= 1 {
                        self.reset_pending();
                        return Action::HistoryPrev;
                    }
                    self.curswant = Some(col);
                    *ctx.cpos = new_pos;
                    for _ in 1..n {
                        (*ctx.cpos, _) = move_up_col(ctx.buf, *ctx.cpos, self.curswant);
                    }
                    clamp_normal(ctx.buf, ctx.cpos);
                    return Action::Consumed;
                }
                self.reset_pending();
                if n <= 1 {
                    Action::HistoryPrev
                } else {
                    Action::Consumed
                }
            }
            'w' => {
                let n = self.take_count();
                for _ in 0..n {
                    *ctx.cpos = word_forward_pos(ctx.buf, *ctx.cpos, CharClass::Word);
                }
                clamp_normal(ctx.buf, ctx.cpos);
                Action::Consumed
            }
            'W' => {
                let n = self.take_count();
                for _ in 0..n {
                    *ctx.cpos = word_forward_pos(ctx.buf, *ctx.cpos, CharClass::WORD);
                }
                clamp_normal(ctx.buf, ctx.cpos);
                Action::Consumed
            }
            'b' => {
                let n = self.take_count();
                for _ in 0..n {
                    *ctx.cpos = word_backward_pos(ctx.buf, *ctx.cpos, CharClass::Word);
                }
                Action::Consumed
            }
            'B' => {
                let n = self.take_count();
                for _ in 0..n {
                    *ctx.cpos = word_backward_pos(ctx.buf, *ctx.cpos, CharClass::WORD);
                }
                Action::Consumed
            }
            'e' => {
                let n = self.take_count();
                for _ in 0..n {
                    *ctx.cpos = word_end_pos(ctx.buf, *ctx.cpos, CharClass::Word);
                }
                clamp_normal(ctx.buf, ctx.cpos);
                Action::Consumed
            }
            'E' => {
                let n = self.take_count();
                for _ in 0..n {
                    *ctx.cpos = word_end_pos(ctx.buf, *ctx.cpos, CharClass::WORD);
                }
                clamp_normal(ctx.buf, ctx.cpos);
                Action::Consumed
            }
            '0' => {
                *ctx.cpos = line_start(ctx.buf, *ctx.cpos);
                self.curswant = None;
                self.reset_pending();
                Action::Consumed
            }
            '^' | '_' => {
                *ctx.cpos = first_non_blank(ctx.buf, *ctx.cpos);
                self.reset_pending();
                Action::Consumed
            }
            '$' => {
                let n = self.take_count();
                // n$ moves down n-1 lines then to end.
                for _ in 1..n {
                    *ctx.cpos = move_down(ctx.buf, *ctx.cpos);
                }
                *ctx.cpos = line_end_normal(ctx.buf, *ctx.cpos);
                Action::Consumed
            }
            'G' => {
                let had_count = self.count1.is_some();
                let n = self.take_count();
                *ctx.cpos = if had_count {
                    goto_line(ctx.buf, n.saturating_sub(1))
                } else {
                    ctx.buf.len()
                };
                clamp_normal(ctx.buf, ctx.cpos);
                Action::Consumed
            }

            // ── Match bracket ────────────────────────────────────────────
            '%' => {
                self.reset_counts();
                if let Some(p) = find_matching_bracket(ctx.buf, *ctx.cpos) {
                    *ctx.cpos = p;
                }
                Action::Consumed
            }

            'J' => {
                let count = self.take_count().max(2);
                let eol = line_end(ctx.buf, *ctx.cpos);
                if eol < ctx.buf.len() {
                    ctx.save_undo();
                    let mut join_pos = *ctx.cpos;
                    for _ in 1..count {
                        let after = &ctx.buf[join_pos..];
                        if let Some(nl) = after.find('\n') {
                            let abs = join_pos + nl;
                            let mut end = abs + 1;
                            while end < ctx.buf.len() && ctx.buf.as_bytes()[end] == b' ' {
                                end += 1;
                            }
                            ctx.buf.replace_range(abs..end, " ");
                            join_pos = abs;
                        } else {
                            break;
                        }
                    }
                    *ctx.cpos = join_pos;
                }
                Action::Consumed
            }

            // Unknown — swallow it.
            _ => {
                self.reset_pending();
                Action::Consumed
            }
        }
    }

    // ── Visual mode ──────────────────────────────────────────────────────

    fn handle_visual(&mut self, key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
        // Handle sub-states.
        if let SubState::WaitingVisualTextObj(inner) = self.sub {
            self.sub = SubState::Ready;
            if let KeyCode::Char(c) = key.code {
                if let Some((start, end)) = text_object(ctx.buf, *ctx.cpos, inner, c) {
                    self.visual_anchor = start;
                    *ctx.cpos = end.saturating_sub(1);
                }
            }
            return Action::Consumed;
        }
        if let SubState::WaitingFind(kind) = self.sub {
            return self.handle_waiting_find(key, kind, ctx);
        }
        if let SubState::WaitingG = self.sub {
            return self.handle_waiting_g(key, ctx);
        }
        if let SubState::WaitingZ = self.sub {
            self.sub = SubState::Ready;
            return if matches!(key.code, KeyCode::Char('z')) {
                Action::CenterScroll
            } else {
                Action::Consumed
            };
        }

        // Pass through Ctrl keys.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Passthrough;
        }

        // Count digit accumulation.
        if let KeyCode::Char(c) = key.code {
            if c.is_ascii_digit() && (c != '0' || self.count1.is_some()) {
                self.count1 =
                    Some(self.count1.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize);
                return Action::Consumed;
            }
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                return self.handle_visual_char(c, ctx);
            }
        }

        // Non-char keys.
        match key.code {
            KeyCode::Esc => {
                self.exit_visual();
                Action::Consumed
            }
            KeyCode::Left => {
                *ctx.cpos = move_left(ctx.buf, *ctx.cpos);
                Action::Consumed
            }
            KeyCode::Right => {
                *ctx.cpos = move_right_normal(ctx.buf, *ctx.cpos);
                Action::Consumed
            }
            KeyCode::Up => {
                *ctx.cpos = move_up(ctx.buf, *ctx.cpos);
                Action::Consumed
            }
            KeyCode::Down => {
                *ctx.cpos = move_down(ctx.buf, *ctx.cpos);
                Action::Consumed
            }
            KeyCode::Home => {
                *ctx.cpos = line_start(ctx.buf, *ctx.cpos);
                Action::Consumed
            }
            KeyCode::End => {
                *ctx.cpos = line_end_normal(ctx.buf, *ctx.cpos);
                Action::Consumed
            }
            _ => Action::Consumed,
        }
    }

    fn handle_visual_char(&mut self, c: char, ctx: &mut VimContext<'_>) -> Action {
        if c != 'j' && c != 'k' && !c.is_ascii_digit() {
            self.curswant = None;
        }
        match c {
            // ── Escape visual mode ─────────────────────────────────────
            'v' if self.mode == ViMode::Visual => {
                self.exit_visual();
                Action::Consumed
            }
            'V' if self.mode == ViMode::VisualLine => {
                self.exit_visual();
                Action::Consumed
            }
            // Switch between visual modes
            'v' if self.mode == ViMode::VisualLine => {
                self.mode = ViMode::Visual;
                Action::Consumed
            }
            'V' if self.mode == ViMode::Visual => {
                self.mode = ViMode::VisualLine;
                Action::Consumed
            }

            // ── Substitute (s → change, S → linewise change) ────────
            's' => {
                // Visual s is the same as c.
                self.handle_visual_char('c', ctx)
            }
            'S' => {
                // Visual S forces linewise, then changes.
                self.mode = ViMode::VisualLine;
                self.handle_visual_char('c', ctx)
            }

            // ── Operators on selection ──────────────────────────────────
            'd' | 'x' => {
                if let Some((start, end)) = self.visual_range(ctx.buf, *ctx.cpos) {
                    let linewise = self.mode == ViMode::VisualLine;
                    ctx.save_undo();
                    ctx.yank_range(start, end, linewise);
                    if linewise {
                        // Include trailing newline if present.
                        let drain_end = if end < ctx.buf.len() && ctx.buf.as_bytes()[end] == b'\n' {
                            end + 1
                        } else if start > 0 && ctx.buf.as_bytes()[start - 1] == b'\n' {
                            // Last line(s) — remove preceding newline.
                            let s = start - 1;
                            ctx.buf.drain(s..end);
                            *ctx.cpos = s.min(ctx.buf.len());
                            clamp_normal(ctx.buf, ctx.cpos);
                            if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
                                *ctx.cpos =
                                    first_non_blank_at(ctx.buf, line_start(ctx.buf, *ctx.cpos));
                            }
                            self.exit_visual();
                            return Action::Consumed;
                        } else {
                            end
                        };
                        ctx.buf.drain(start..drain_end);
                    } else {
                        ctx.buf.drain(start..end);
                    }
                    *ctx.cpos = start.min(ctx.buf.len());
                    clamp_normal(ctx.buf, ctx.cpos);
                }
                self.exit_visual();
                Action::Consumed
            }
            'c' => {
                if let Some((start, end)) = self.visual_range(ctx.buf, *ctx.cpos) {
                    let linewise = self.mode == ViMode::VisualLine;
                    ctx.save_undo();
                    ctx.yank_range(start, end, linewise);
                    if linewise {
                        // Like cc: clear line content but keep the line structure.
                        // Find the content range (excluding leading/trailing newlines).
                        let content_start = first_non_blank_at(ctx.buf, start);
                        ctx.buf.drain(content_start..end);
                        *ctx.cpos = content_start;
                    } else {
                        ctx.buf.drain(start..end);
                        *ctx.cpos = start;
                    }
                    self.mode = ViMode::Insert;
                    self.sub = SubState::Ready;
                    self.reset_counts();
                    return Action::Consumed;
                }
                self.exit_visual();
                Action::Consumed
            }
            'y' => {
                if let Some((start, end)) = self.visual_range(ctx.buf, *ctx.cpos) {
                    let linewise = self.mode == ViMode::VisualLine;
                    ctx.yank_range(start, end, linewise);
                    ctx.kill_ring.mark_yanked();
                    *ctx.cpos = start;
                }
                self.exit_visual();
                Action::Consumed
            }

            // ── Case toggling on selection ─────────────────────────────
            '~' => {
                if let Some((start, end)) = self.visual_range(ctx.buf, *ctx.cpos) {
                    ctx.save_undo();
                    let toggled: String = ctx.buf[start..end]
                        .chars()
                        .map(|ch| {
                            if ch.is_uppercase() {
                                ch.to_lowercase().next().unwrap_or(ch)
                            } else {
                                ch.to_uppercase().next().unwrap_or(ch)
                            }
                        })
                        .collect();
                    ctx.buf.replace_range(start..end, &toggled);
                }
                self.exit_visual();
                Action::Consumed
            }
            'U' => {
                if let Some((start, end)) = self.visual_range(ctx.buf, *ctx.cpos) {
                    ctx.save_undo();
                    let upper = ctx.buf[start..end].to_uppercase();
                    ctx.buf.replace_range(start..end, &upper);
                }
                self.exit_visual();
                Action::Consumed
            }
            'u' => {
                if let Some((start, end)) = self.visual_range(ctx.buf, *ctx.cpos) {
                    ctx.save_undo();
                    let lower = ctx.buf[start..end].to_lowercase();
                    ctx.buf.replace_range(start..end, &lower);
                }
                self.exit_visual();
                Action::Consumed
            }

            // ── Join lines ─────────────────────────────────────────────
            'J' => {
                if let Some((start, end)) = self.visual_range(ctx.buf, *ctx.cpos) {
                    ctx.save_undo();
                    let mut pos = start;
                    let mut remaining = end;
                    while pos < remaining.min(ctx.buf.len()) {
                        if let Some(nl) = ctx.buf[pos..remaining.min(ctx.buf.len())].find('\n') {
                            let abs = pos + nl;
                            let mut ws_end = abs + 1;
                            while ws_end < ctx.buf.len() && ctx.buf.as_bytes()[ws_end] == b' ' {
                                ws_end += 1;
                            }
                            let removed = ws_end - abs;
                            ctx.buf.replace_range(abs..ws_end, " ");
                            remaining -= removed - 1; // replaced N chars with 1
                            pos = abs + 1;
                        } else {
                            break;
                        }
                    }
                    *ctx.cpos = start;
                }
                self.exit_visual();
                Action::Consumed
            }

            // ── Paste over selection ───────────────────────────────────
            'p' | 'P' => {
                ctx.sync_paste_from_clipboard();
                if !ctx.register().is_empty() {
                    if let Some((start, end)) = self.visual_range(ctx.buf, *ctx.cpos) {
                        ctx.save_undo();
                        let old = ctx.buf[start..end].to_string();
                        let text = ctx.register().to_string();
                        ctx.buf.replace_range(start..end, &text);
                        *ctx.cpos = start;
                        clamp_normal(ctx.buf, ctx.cpos);
                        // The replaced text goes into register (like vim).
                        // Also mirror to clipboard so subsequent external
                        // pastes pick it up.
                        ctx.kill_ring.set_with_linewise(old.clone(), false);
                        if ctx.clipboard.write(&old).is_ok() {
                            ctx.kill_ring.record_clipboard_write(old);
                        }
                    }
                }
                self.exit_visual();
                Action::Consumed
            }

            // ── Motions (move cursor, anchor stays) ────────────────────
            'h' => {
                let n = self.take_count();
                for _ in 0..n {
                    *ctx.cpos = move_left(ctx.buf, *ctx.cpos);
                }
                Action::Consumed
            }
            'l' => {
                let n = self.take_count();
                for _ in 0..n {
                    *ctx.cpos = move_right_normal(ctx.buf, *ctx.cpos);
                }
                Action::Consumed
            }
            'j' => {
                let n = self.take_count();
                for _ in 0..n {
                    let col;
                    (*ctx.cpos, col) = move_down_col(ctx.buf, *ctx.cpos, self.curswant);
                    self.curswant = Some(col);
                }
                clamp_normal(ctx.buf, ctx.cpos);
                Action::Consumed
            }
            'k' => {
                let n = self.take_count();
                for _ in 0..n {
                    let col;
                    (*ctx.cpos, col) = move_up_col(ctx.buf, *ctx.cpos, self.curswant);
                    self.curswant = Some(col);
                }
                clamp_normal(ctx.buf, ctx.cpos);
                Action::Consumed
            }
            'w' => {
                let n = self.take_count();
                for _ in 0..n {
                    *ctx.cpos = word_forward_pos(ctx.buf, *ctx.cpos, CharClass::Word);
                }
                clamp_normal(ctx.buf, ctx.cpos);
                Action::Consumed
            }
            'W' => {
                let n = self.take_count();
                for _ in 0..n {
                    *ctx.cpos = word_forward_pos(ctx.buf, *ctx.cpos, CharClass::WORD);
                }
                clamp_normal(ctx.buf, ctx.cpos);
                Action::Consumed
            }
            'b' => {
                let n = self.take_count();
                for _ in 0..n {
                    *ctx.cpos = word_backward_pos(ctx.buf, *ctx.cpos, CharClass::Word);
                }
                Action::Consumed
            }
            'B' => {
                let n = self.take_count();
                for _ in 0..n {
                    *ctx.cpos = word_backward_pos(ctx.buf, *ctx.cpos, CharClass::WORD);
                }
                Action::Consumed
            }
            'e' => {
                let n = self.take_count();
                for _ in 0..n {
                    *ctx.cpos = word_end_pos(ctx.buf, *ctx.cpos, CharClass::Word);
                }
                clamp_normal(ctx.buf, ctx.cpos);
                Action::Consumed
            }
            'E' => {
                let n = self.take_count();
                for _ in 0..n {
                    *ctx.cpos = word_end_pos(ctx.buf, *ctx.cpos, CharClass::WORD);
                }
                clamp_normal(ctx.buf, ctx.cpos);
                Action::Consumed
            }
            '0' => {
                *ctx.cpos = line_start(ctx.buf, *ctx.cpos);
                Action::Consumed
            }
            '^' | '_' => {
                *ctx.cpos = first_non_blank(ctx.buf, *ctx.cpos);
                Action::Consumed
            }
            '$' => {
                *ctx.cpos = line_end_normal(ctx.buf, *ctx.cpos);
                Action::Consumed
            }
            'G' => {
                let had_count = self.count1.is_some();
                let n = self.take_count();
                *ctx.cpos = if had_count {
                    goto_line(ctx.buf, n.saturating_sub(1))
                } else {
                    ctx.buf.len()
                };
                clamp_normal(ctx.buf, ctx.cpos);
                Action::Consumed
            }
            '%' => {
                self.reset_counts();
                if let Some(p) = find_matching_bracket(ctx.buf, *ctx.cpos) {
                    *ctx.cpos = p;
                }
                Action::Consumed
            }
            'g' => {
                self.sub = SubState::WaitingG;
                Action::Consumed
            }
            'f' => {
                self.sub = SubState::WaitingFind(FindKind::Forward);
                Action::Consumed
            }
            'F' => {
                self.sub = SubState::WaitingFind(FindKind::Backward);
                Action::Consumed
            }
            't' => {
                self.sub = SubState::WaitingFind(FindKind::ForwardTill);
                Action::Consumed
            }
            'T' => {
                self.sub = SubState::WaitingFind(FindKind::BackwardTill);
                Action::Consumed
            }
            ';' => {
                if let Some((kind, ch)) = self.last_find {
                    let n = self.take_count();
                    *ctx.cpos = repeat_find(ctx.buf, *ctx.cpos, kind, ch, n);
                }
                Action::Consumed
            }
            ',' => {
                if let Some((kind, ch)) = self.last_find {
                    let n = self.take_count();
                    *ctx.cpos = repeat_find(ctx.buf, *ctx.cpos, kind.reversed(), ch, n);
                }
                Action::Consumed
            }

            // ── Count digits ───────────────────────────────────────────
            c if c.is_ascii_digit() && (c != '0' || self.count1.is_some()) => {
                self.count1 =
                    Some(self.count1.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize);
                Action::Consumed
            }

            // ── Swap anchor and cursor ─────────────────────────────────
            'o' => {
                std::mem::swap(&mut self.visual_anchor, ctx.cpos);
                Action::Consumed
            }

            // ── Text objects (iw, aw, i", a( etc.) ────────────────────
            'i' => {
                self.sub = SubState::WaitingVisualTextObj(true);
                Action::Consumed
            }
            'a' => {
                self.sub = SubState::WaitingVisualTextObj(false);
                Action::Consumed
            }

            // Unknown — swallow.
            _ => Action::Consumed,
        }
    }

    // ── Sub-state handlers ──────────────────────────────────────────────

    fn handle_waiting_r(&mut self, key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
        self.sub = SubState::Ready;
        let replacement_char = match key.code {
            KeyCode::Char(c) => Some(c),
            KeyCode::Enter => Some('\n'),
            _ => None,
        };
        if let Some(c) = replacement_char {
            if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
                let n = self.take_count();
                ctx.save_undo();
                let mut pos = *ctx.cpos;
                for _ in 0..n {
                    if pos >= ctx.buf.len() {
                        break;
                    }
                    let old = ctx.buf[pos..].chars().next().unwrap();
                    let end = pos + old.len_utf8();
                    let replacement = c.to_string();
                    ctx.buf.replace_range(pos..end, &replacement);
                    pos += replacement.len();
                }
                *ctx.cpos = prev_char_boundary(ctx.buf, pos).max(*ctx.cpos);
                clamp_normal(ctx.buf, ctx.cpos);
            }
        }
        self.reset_pending();
        Action::Consumed
    }

    fn handle_waiting_find(
        &mut self,
        key: KeyEvent,
        kind: FindKind,
        ctx: &mut VimContext<'_>,
    ) -> Action {
        self.sub = SubState::Ready;
        if let KeyCode::Char(ch) = key.code {
            let n = self.take_count();
            self.last_find = Some((kind, ch));
            let mut pos = *ctx.cpos;
            for _ in 0..n {
                if let Some(p) = find_char(ctx.buf, pos, kind, ch) {
                    pos = p;
                }
            }
            *ctx.cpos = pos;
        }
        self.reset_pending();
        Action::Consumed
    }

    fn handle_waiting_op_find(
        &mut self,
        key: KeyEvent,
        op: Op,
        kind: FindKind,
        ctx: &mut VimContext<'_>,
    ) -> Action {
        self.sub = SubState::Ready;
        if let KeyCode::Char(ch) = key.code {
            let n = self.effective_count();
            self.last_find = Some((kind, ch));
            let origin = *ctx.cpos;
            // For operators, always find the actual char position (Forward/Backward),
            // then adjust the range for till variants.
            let raw_kind = match kind {
                FindKind::ForwardTill => FindKind::Forward,
                FindKind::BackwardTill => FindKind::Backward,
                other => other,
            };
            let mut pos = origin;
            for _ in 0..n {
                if let Some(p) = find_char(ctx.buf, pos, raw_kind, ch) {
                    pos = p;
                }
            }
            if pos != origin {
                // f is inclusive (include target char), t excludes target char.
                let (start, end) = match kind {
                    FindKind::Forward => (*ctx.cpos, advance_chars(ctx.buf, pos, 1)),
                    FindKind::ForwardTill => (*ctx.cpos, pos),
                    FindKind::Backward => (pos, *ctx.cpos),
                    FindKind::BackwardTill => (advance_chars(ctx.buf, pos, 1), *ctx.cpos),
                };
                if start < end {
                    return self.apply_charwise_op(op, ctx, start, end);
                }
            }
        }
        self.reset_pending();
        Action::Consumed
    }

    fn handle_waiting_g(&mut self, key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
        self.sub = SubState::Ready;
        let action = match key.code {
            KeyCode::Char('g') => {
                // gg → start of buffer.
                if let Some(n) = self.count1.take() {
                    *ctx.cpos = goto_line(ctx.buf, n.saturating_sub(1));
                } else {
                    *ctx.cpos = 0;
                }
                Action::Consumed
            }
            _ => Action::Consumed,
        };
        self.count1 = None;
        self.count2 = None;
        action
    }

    fn handle_waiting_op_g(&mut self, key: KeyEvent, op: Op, ctx: &mut VimContext<'_>) -> Action {
        self.sub = SubState::Ready;
        if let KeyCode::Char('g') = key.code {
            let target = if let Some(n) = self.count1.take() {
                goto_line(ctx.buf, n.saturating_sub(1))
            } else {
                0
            };
            let origin = *ctx.cpos;
            if target != origin {
                let (s, e) = if target < origin {
                    (target, origin)
                } else {
                    (origin, target)
                };
                let ls = line_start(ctx.buf, s);
                let le = line_end(ctx.buf, e);
                self.reset_pending();
                return self.apply_linewise_op(op, ctx, ls, le);
            }
        }
        self.reset_pending();
        Action::Consumed
    }

    fn handle_waiting_textobj(
        &mut self,
        key: KeyEvent,
        op: Op,
        inner: bool,
        ctx: &mut VimContext<'_>,
    ) -> Action {
        self.sub = SubState::Ready;
        if let KeyCode::Char(c) = key.code {
            if let Some((start, end)) = text_object(ctx.buf, *ctx.cpos, inner, c) {
                let n = self.effective_count();
                let _ = n;
                return self.apply_charwise_op(op, ctx, start, end);
            }
        }
        self.reset_pending();
        Action::Consumed
    }

    /// Operator pending + a motion key.
    fn execute_op_motion(&mut self, key: KeyEvent, op: Op, ctx: &mut VimContext<'_>) -> Action {
        let n = self.effective_count();
        let origin = *ctx.cpos;

        // Resolve motion target and whether the motion is linewise.
        let (target, linewise) = match key.code {
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
                let mut p = origin;
                for _ in 0..n {
                    p = move_left(ctx.buf, p);
                }
                (Some(p), false)
            }
            KeyCode::Char('l') | KeyCode::Right => {
                let mut p = origin;
                for _ in 0..n {
                    p = move_right_inclusive(ctx.buf, p);
                }
                (Some(p), false)
            }
            KeyCode::Char('j') => {
                let mut p = origin;
                for _ in 0..n {
                    p = move_down(ctx.buf, p);
                }
                (Some(p), true)
            }
            KeyCode::Char('k') => {
                let mut p = origin;
                for _ in 0..n {
                    p = move_up(ctx.buf, p);
                }
                (Some(p), true)
            }
            KeyCode::Char('w') => {
                let mut p = origin;
                // vim special case: cw behaves like ce when cursor is on a word char.
                let use_end = op == Op::Change
                    && p < ctx.buf.len()
                    && char_class(ctx.buf[p..].chars().next().unwrap(), CharClass::Word) != 0;
                for _ in 0..n {
                    if use_end {
                        p = word_end_pos(ctx.buf, p, CharClass::Word);
                        p = advance_chars(ctx.buf, p, 1); // inclusive end
                    } else {
                        p = word_forward_pos(ctx.buf, p, CharClass::Word);
                    }
                }
                (Some(p), false)
            }
            KeyCode::Char('W') => {
                let mut p = origin;
                let use_end = op == Op::Change
                    && p < ctx.buf.len()
                    && char_class(ctx.buf[p..].chars().next().unwrap(), CharClass::WORD) != 0;
                for _ in 0..n {
                    if use_end {
                        p = word_end_pos(ctx.buf, p, CharClass::WORD);
                        p = advance_chars(ctx.buf, p, 1);
                    } else {
                        p = word_forward_pos(ctx.buf, p, CharClass::WORD);
                    }
                }
                (Some(p), false)
            }
            KeyCode::Char('b') => {
                let mut p = origin;
                for _ in 0..n {
                    p = word_backward_pos(ctx.buf, p, CharClass::Word);
                }
                (Some(p), false)
            }
            KeyCode::Char('B') => {
                let mut p = origin;
                for _ in 0..n {
                    p = word_backward_pos(ctx.buf, p, CharClass::WORD);
                }
                (Some(p), false)
            }
            KeyCode::Char('e') => {
                let mut p = origin;
                for _ in 0..n {
                    p = word_end_pos(ctx.buf, p, CharClass::Word);
                }
                (Some(advance_chars(ctx.buf, p, 1)), false)
            }
            KeyCode::Char('E') => {
                let mut p = origin;
                for _ in 0..n {
                    p = word_end_pos(ctx.buf, p, CharClass::WORD);
                }
                (Some(advance_chars(ctx.buf, p, 1)), false)
            }
            KeyCode::Char('0') => (Some(line_start(ctx.buf, origin)), false),
            KeyCode::Char('^' | '_') => (Some(first_non_blank(ctx.buf, origin)), false),
            KeyCode::Char('$') => (Some(line_end(ctx.buf, origin)), false),
            KeyCode::Char('%') => {
                if let Some(t) = find_matching_bracket(ctx.buf, origin) {
                    let lo = origin.min(t);
                    let hi = advance_chars(ctx.buf, origin.max(t), 1);
                    return self.apply_charwise_op(op, ctx, lo, hi);
                }
                (None, false)
            }
            KeyCode::Char('G') => (Some(ctx.buf.len()), true), // linewise
            KeyCode::Char('g') => {
                self.sub = SubState::WaitingOpG(op);
                return Action::Consumed;
            }
            KeyCode::Char('f') => {
                self.sub = SubState::WaitingOpFind(op, FindKind::Forward);
                return Action::Consumed;
            }
            KeyCode::Char('F') => {
                self.sub = SubState::WaitingOpFind(op, FindKind::Backward);
                return Action::Consumed;
            }
            KeyCode::Char('t') => {
                self.sub = SubState::WaitingOpFind(op, FindKind::ForwardTill);
                return Action::Consumed;
            }
            KeyCode::Char('T') => {
                self.sub = SubState::WaitingOpFind(op, FindKind::BackwardTill);
                return Action::Consumed;
            }
            KeyCode::Home => (Some(line_start(ctx.buf, origin)), false),
            KeyCode::End => (Some(line_end(ctx.buf, origin)), false),
            _ => (None, false),
        };

        let Some(target) = target else {
            // Invalid motion — cancel.
            return Action::Consumed;
        };

        if linewise {
            // Linewise: delegate to the existing linewise operator logic
            // which handles newline inclusion, first-non-blank, etc.
            let (start, end) = if target < origin {
                (target, origin)
            } else {
                (origin, target)
            };
            // Expand to full lines.
            let ls = line_start(ctx.buf, start);
            let le = line_end(ctx.buf, end);
            return self.apply_linewise_op(op, ctx, ls, le);
        }

        let (start, end) = if target < origin {
            (target, origin)
        } else {
            (origin, target)
        };

        if start == end {
            return Action::Consumed;
        }

        self.apply_charwise_op(op, ctx, start, end)
    }

    fn execute_linewise_op(&mut self, op: Op, ctx: &mut VimContext<'_>) -> Action {
        let n = self.effective_count();
        self.reset_counts();
        self.sub = SubState::Ready;

        let start = line_start(ctx.buf, *ctx.cpos);
        let mut end_pos = *ctx.cpos;
        for _ in 1..n {
            let next = line_end(ctx.buf, end_pos);
            if next < ctx.buf.len() {
                end_pos = next + 1;
            }
        }
        let end = line_end(ctx.buf, end_pos);
        self.apply_linewise_op(op, ctx, start, end)
    }

    /// Apply a charwise operator over the byte range [start..end).
    fn apply_charwise_op(
        &mut self,
        op: Op,
        ctx: &mut VimContext<'_>,
        start: usize,
        end: usize,
    ) -> Action {
        match op {
            Op::Delete => {
                ctx.save_undo();
                ctx.yank_range(start, end, false);
                ctx.buf.drain(start..end);
                *ctx.cpos = start;
                clamp_normal(ctx.buf, ctx.cpos);
            }
            Op::Change => {
                ctx.save_undo();
                ctx.yank_range(start, end, false);
                ctx.buf.drain(start..end);
                *ctx.cpos = start;
                self.enter_insert_mode();
                self.reset_counts();
                return Action::Consumed;
            }
            Op::Yank => {
                ctx.yank_range(start, end, false);
                ctx.kill_ring.mark_yanked();
                *ctx.cpos = start;
            }
        }
        Action::Consumed
    }

    /// Apply a linewise operator over the content range [start..end].
    /// `start` is the first byte of the first line, `end` is the last byte
    /// of the last line (before its newline). This function handles newline
    /// inclusion at buffer boundaries and cursor placement.
    fn apply_linewise_op(
        &mut self,
        op: Op,
        ctx: &mut VimContext<'_>,
        start: usize,
        end: usize,
    ) -> Action {
        let mut s = start;
        let mut e = end;
        let mut has_trailing_nl = false;
        // Include trailing newline if present.
        if e < ctx.buf.len() && ctx.buf.as_bytes()[e] == b'\n' {
            e += 1;
            has_trailing_nl = true;
        } else if e < ctx.buf.len() {
            e = line_end(ctx.buf, e);
            if e < ctx.buf.len() {
                e += 1;
                has_trailing_nl = true;
            }
        }
        // At end of buffer with no trailing newline — include preceding
        // newline to avoid leaving a dangling one.
        if !has_trailing_nl && e >= ctx.buf.len() && s > 0 {
            s -= 1;
        }

        match op {
            Op::Delete => {
                ctx.save_undo();
                ctx.yank_range(s, e, true);
                ctx.buf.drain(s..e);
                *ctx.cpos = s.min(ctx.buf.len());
                if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
                    *ctx.cpos = first_non_blank_at(ctx.buf, *ctx.cpos);
                }
                clamp_normal(ctx.buf, ctx.cpos);
            }
            Op::Change => {
                ctx.save_undo();
                // Clear line content but keep the line structure.
                let content_start = first_non_blank_at(ctx.buf, s);
                let content_end = line_end(ctx.buf, e.saturating_sub(1).max(s));
                ctx.yank_range(content_start, content_end, true);
                ctx.buf.drain(content_start..content_end);
                *ctx.cpos = content_start;
                self.enter_insert_mode();
                return Action::Consumed;
            }
            Op::Yank => {
                // `yy` / `Y`: linewise yank leaves the cursor in place,
                // matching vim's default cpoptions. Only delete / change
                // operators (and visual-mode yank) reposition.
                ctx.yank_range(s, e, true);
                ctx.kill_ring.mark_yanked();
            }
        }
        Action::Consumed
    }

    // ── Mode transitions ────────────────────────────────────────────────

    fn enter_insert_mode(&mut self) {
        self.mode = ViMode::Insert;
        self.sub = SubState::Ready;
    }

    fn exit_visual(&mut self) {
        self.mode = ViMode::Normal;
        self.reset_pending();
    }

    fn enter_normal(&mut self, buf: &str, cpos: &mut usize) {
        self.mode = ViMode::Normal;
        self.sub = SubState::Ready;
        self.reset_counts();
        // Standard vim: cursor moves left one when leaving insert mode,
        // unless at the start of a line.
        let sol = line_start(buf, *cpos);
        if *cpos > sol {
            *cpos = prev_char_boundary(buf, *cpos);
        }
        clamp_normal(buf, cpos);
    }

    // ── Count helpers ───────────────────────────────────────────────────

    fn take_count(&mut self) -> usize {
        let n = self.count1.unwrap_or(1);
        self.count1 = None;
        self.count2 = None;
        n
    }

    fn effective_count(&mut self) -> usize {
        let c1 = self.count1.unwrap_or(1);
        let c2 = self.count2.unwrap_or(1);
        self.count1 = None;
        self.count2 = None;
        c1 * c2
    }

    fn reset_counts(&mut self) {
        self.count1 = None;
        self.count2 = None;
    }

    fn reset_pending(&mut self) {
        self.sub = SubState::Ready;
        self.reset_counts();
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(unused_assignments)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn key_ctrl(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    /// Owns the cross-call state (kill ring + undo history) that vim borrows.
    struct TestHarness {
        vim: Vim,
        buf: String,
        cpos: usize,
        attachments: Vec<AttachmentId>,
        kill_ring: KillRing,
        history: UndoHistory,
    }

    impl TestHarness {
        fn new(text: &str) -> Self {
            let mut vim = Vim::new();
            vim.mode = ViMode::Normal;
            vim.sub = SubState::Ready;
            Self {
                vim,
                buf: text.to_string(),
                cpos: 0,
                attachments: Vec::new(),
                kill_ring: KillRing::new(),
                history: UndoHistory::new(None),
            }
        }

        fn handle(&mut self, k: KeyEvent) -> Action {
            let mut clipboard = crate::clipboard::NullClipboard;
            let mut ctx = VimContext {
                buf: &mut self.buf,
                cpos: &mut self.cpos,
                attachments: &mut self.attachments,
                kill_ring: &mut self.kill_ring,
                history: &mut self.history,
                clipboard: &mut clipboard,
            };
            self.vim.handle_key(k, &mut ctx)
        }

        fn handle_with_clipboard(
            &mut self,
            k: KeyEvent,
            clipboard: &mut dyn crate::clipboard::Clipboard,
        ) -> Action {
            let mut ctx = VimContext {
                buf: &mut self.buf,
                cpos: &mut self.cpos,
                attachments: &mut self.attachments,
                kill_ring: &mut self.kill_ring,
                history: &mut self.history,
                clipboard,
            };
            self.vim.handle_key(k, &mut ctx)
        }
    }

    /// In-memory `Clipboard` for testing the sync between the kill
    /// ring and the system clipboard without shelling out.
    struct MemClipboard {
        text: Option<String>,
        writes: usize,
    }
    impl MemClipboard {
        fn new(initial: Option<&str>) -> Self {
            Self {
                text: initial.map(str::to_string),
                writes: 0,
            }
        }
    }
    impl crate::clipboard::Clipboard for MemClipboard {
        fn read(&mut self) -> Option<String> {
            self.text.clone()
        }
        fn write(&mut self, text: &str) -> Result<(), String> {
            self.text = Some(text.to_string());
            self.writes += 1;
            Ok(())
        }
    }

    #[test]
    fn test_word_forward() {
        let mut h = TestHarness::new("hello world foo");
        h.handle(key('w'));
        assert_eq!(h.cpos, 6);
        h.handle(key('w'));
        assert_eq!(h.cpos, 12);
    }

    #[test]
    fn test_word_backward() {
        let mut h = TestHarness::new("hello world");
        h.cpos = 6;
        h.handle(key('b'));
        assert_eq!(h.cpos, 0);
    }

    #[test]
    fn test_word_end() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('e'));
        assert_eq!(h.cpos, 4);
    }

    #[test]
    fn test_delete_word() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('d'));
        h.handle(key('w'));
        assert_eq!(h.buf, "world");
        assert_eq!(h.cpos, 0);
    }

    #[test]
    fn test_delete_inner_word() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('d'));
        h.handle(key('i'));
        h.handle(key('w'));
        assert_eq!(h.buf, " world");
    }

    #[test]
    fn test_change_word() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('c'));
        h.handle(key('w'));
        assert_eq!(h.buf, " world");
        assert_eq!(h.vim.mode(), ViMode::Insert);
    }

    #[test]
    fn test_dd_single_line() {
        let mut h = TestHarness::new("hello");
        h.handle(key('d'));
        h.handle(key('d'));
        assert_eq!(h.buf, "");
    }

    #[test]
    fn test_dd_multiline() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 4;
        h.handle(key('d'));
        h.handle(key('d'));
        assert_eq!(h.buf, "aaa\nccc");
    }

    #[test]
    fn test_dd_middle_line_with_empty_neighbors() {
        let mut h = TestHarness::new("\nfoo\n");
        h.cpos = 1;
        h.handle(key('d'));
        h.handle(key('d'));
        assert_eq!(h.buf, "\n");
    }

    #[test]
    fn test_undo_redo() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('d'));
        h.handle(key('w'));
        assert_eq!(h.buf, "world");
        h.handle(key('u'));
        assert_eq!(h.buf, "hello world");
        h.handle(key_ctrl('r'));
        assert_eq!(h.buf, "world");
    }

    #[test]
    fn test_count_motion() {
        let mut h = TestHarness::new("one two three four");
        h.handle(key('2'));
        h.handle(key('w'));
        assert_eq!(h.cpos, 8);
    }

    #[test]
    fn test_count_delete() {
        let mut h = TestHarness::new("one two three four");
        h.handle(key('2'));
        h.handle(key('d'));
        h.handle(key('w'));
        assert_eq!(h.buf, "three four");
    }

    #[test]
    fn test_find_char() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('f'));
        h.handle(key('o'));
        assert_eq!(h.cpos, 4);
        h.handle(key(';'));
        assert_eq!(h.cpos, 7);
        h.handle(key(','));
        assert_eq!(h.cpos, 4);
    }

    #[test]
    fn test_till_char() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('t'));
        h.handle(key('o'));
        assert_eq!(h.cpos, 3);
    }

    #[test]
    fn test_text_object_pair() {
        let mut h = TestHarness::new("foo(bar)baz");
        h.cpos = 5;
        h.handle(key('d'));
        h.handle(key('i'));
        h.handle(key('('));
        assert_eq!(h.buf, "foo()baz");
    }

    #[test]
    fn test_text_object_quote() {
        let mut h = TestHarness::new("foo \"bar\" baz");
        h.cpos = 6;
        h.handle(key('d'));
        h.handle(key('i'));
        h.handle(key('"'));
        assert_eq!(h.buf, "foo \"\" baz");
    }

    #[test]
    fn test_paste() {
        let mut h = TestHarness::new("hello");
        h.kill_ring.set_with_linewise(" world".to_string(), false);
        h.cpos = 4;
        h.handle(key('p'));
        assert_eq!(h.buf, "hello world");
    }

    #[test]
    fn test_tilde() {
        let mut h = TestHarness::new("hello");
        h.handle(key('~'));
        assert_eq!(h.buf, "Hello");
        assert_eq!(h.cpos, 1);
    }

    #[test]
    fn test_replace() {
        let mut h = TestHarness::new("hello");
        h.handle(key('r'));
        h.handle(key('X'));
        assert_eq!(h.buf, "Xello");
        assert_eq!(h.cpos, 0);
    }

    #[test]
    fn test_replace_with_enter() {
        let mut h = TestHarness::new("hello");
        h.handle(key('r'));
        h.handle(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        });
        assert_eq!(h.buf, "\nello");
    }

    #[test]
    fn test_insert_ctrl_w_passthrough() {
        let mut h = TestHarness::new("hello");
        h.handle(key('i'));
        assert_eq!(h.vim.mode(), ViMode::Insert);
        let result = h.handle(key_ctrl('w'));
        assert_eq!(result, Action::Passthrough);
    }

    #[test]
    fn test_line_movement() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('j'));
        assert_eq!(h.cpos, 4);
        h.handle(key('j'));
        assert_eq!(h.cpos, 8);
        h.handle(key('k'));
        assert_eq!(h.cpos, 4);
    }

    #[test]
    fn test_open_line_and_navigate() {
        // 'o' from normal mode opens line below, press Esc, then navigate with j/k.
        let mut h = TestHarness::new("hello");
        // 'o' opens line below → buf = "hello\n", cpos = 6, insert mode.
        h.handle(key('o'));
        assert_eq!(h.buf, "hello\n");
        assert_eq!(h.cpos, 6);
        assert_eq!(h.vim.mode(), ViMode::Insert);

        // Esc → normal mode, cursor stays on empty trailing line.
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        h.handle(esc);
        assert_eq!(h.vim.mode(), ViMode::Normal);
        assert_eq!(h.cpos, 6); // On the empty second line.

        // 'k' should go up to "hello" line.
        h.handle(key('k'));
        assert_eq!(h.cpos, 0);

        // 'j' should go back down to the empty line.
        h.handle(key('j'));
        assert_eq!(h.cpos, 6);
    }

    #[test]
    fn test_esc_moves_cursor_back() {
        let mut h = TestHarness::new("hello");
        h.vim.mode = ViMode::Insert;
        h.cpos = 5;
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        h.handle(esc);
        assert_eq!(h.cpos, 4);
        assert_eq!(h.vim.mode(), ViMode::Normal);
    }

    #[test]
    fn test_esc_at_line_start_stays() {
        let mut h = TestHarness::new("hello");
        h.vim.mode = ViMode::Insert;
        h.cpos = 0;
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        h.handle(esc);
        assert_eq!(h.cpos, 0);
    }

    #[test]
    fn test_h_l_stay_within_line() {
        let mut h = TestHarness::new("aa\nbb");
        h.handle(key('$'));
        assert_eq!(h.cpos, 1);
        h.handle(key('l'));
        assert_eq!(h.cpos, 1);
        h.handle(key('j'));
        h.handle(key('0'));
        assert_eq!(h.cpos, 3);
        h.handle(key('h'));
        assert_eq!(h.cpos, 3);
    }

    #[test]
    fn test_empty_buffer() {
        let mut h = TestHarness::new("");
        h.handle(key('x'));
        assert_eq!(h.buf, "");
        h.handle(key('d'));
        h.handle(key('w'));
        assert_eq!(h.buf, "");
    }

    #[test]
    fn test_gg() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 8;
        h.handle(key('g'));
        h.handle(key('g'));
        assert_eq!(h.cpos, 0);
    }

    #[test]
    fn test_dollar_and_zero() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('$'));
        assert_eq!(h.cpos, 10);
        h.handle(key('0'));
        assert_eq!(h.cpos, 0);
    }

    #[test]
    fn test_yank_paste() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('y'));
        h.handle(key('w'));
        h.handle(key('$'));
        h.handle(key('p'));
        assert_eq!(h.buf, "hello worldhello ");
    }

    #[test]
    fn yank_mirrors_to_clipboard() {
        let mut h = TestHarness::new("hello world");
        let mut clip = MemClipboard::new(None);
        h.handle_with_clipboard(key('y'), &mut clip);
        h.handle_with_clipboard(key('w'), &mut clip);
        assert_eq!(clip.text.as_deref(), Some("hello "));
        assert_eq!(clip.writes, 1);
        assert_eq!(h.kill_ring.last_clipboard_write(), Some("hello "));
    }

    #[test]
    fn paste_prefers_external_clipboard_when_updated() {
        // External tool put "pasted" on the clipboard. `p` should use
        // that instead of whatever is in the kill ring.
        let mut h = TestHarness::new("abc");
        h.kill_ring.set_with_linewise("stale".to_string(), false);
        let mut clip = MemClipboard::new(Some("pasted"));
        // Move cursor to end so `p` inserts after.
        h.handle_with_clipboard(key('$'), &mut clip);
        h.handle_with_clipboard(key('p'), &mut clip);
        assert_eq!(h.buf, "abcpasted");
    }

    #[test]
    fn paste_keeps_kill_ring_when_clipboard_matches_last_write() {
        // Kill ring was the last writer — its linewise flag matters
        // for `p` placement, so we must not overwrite charwise.
        let mut h = TestHarness::new("abc\n");
        // Simulate a prior `yy`: linewise + clipboard mirror.
        h.kill_ring.set_with_linewise("line\n".to_string(), true);
        h.kill_ring.record_clipboard_write("line\n".to_string());
        let mut clip = MemClipboard::new(Some("line\n"));
        // Position on first line, then `p` — linewise pastes below.
        h.handle_with_clipboard(key('p'), &mut clip);
        assert!(h.buf.contains("line\n"));
        assert!(h.kill_ring.is_linewise());
    }

    #[test]
    fn test_yy_keeps_cursor_in_place() {
        // Regression: `yy` used to snap the cursor to column 0 of the
        // yanked line. Vim's default behavior is "linewise yank does
        // not move the cursor"; both `yy` and `Y` should leave the
        // cursor exactly where it was.
        let mut h = TestHarness::new("hello world\nsecond line");
        h.handle(key('l')); // cpos=1
        h.handle(key('l')); // cpos=2
        h.handle(key('l')); // cpos=3
        let before = h.cpos;
        h.handle(key('y'));
        h.handle(key('y'));
        assert_eq!(h.cpos, before, "yy must not move cursor");
        assert_eq!(h.kill_ring.current(), "hello world\n");
    }

    #[test]
    fn test_capital_y_keeps_cursor_in_place() {
        let mut h = TestHarness::new("hello world\nsecond line");
        h.handle(key('l'));
        h.handle(key('l'));
        let before = h.cpos;
        h.handle(key('Y'));
        assert_eq!(h.cpos, before, "Y must not move cursor");
    }

    #[test]
    fn test_visual_select_and_delete() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        assert_eq!(h.vim.mode(), ViMode::Visual);
        h.handle(key('e'));
        h.handle(key('d'));
        assert_eq!(h.buf, " world");
        assert_eq!(h.vim.mode(), ViMode::Normal);
    }

    #[test]
    fn test_visual_yank() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('e'));
        h.handle(key('y'));
        assert_eq!(h.buf, "hello world");
        assert_eq!(h.vim.mode(), ViMode::Normal);
        h.handle(key('$'));
        h.handle(key('p'));
        assert_eq!(h.buf, "hello worldhello");
    }

    #[test]
    fn test_visual_change() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('e'));
        h.handle(key('c'));
        assert_eq!(h.buf, " world");
        assert_eq!(h.vim.mode(), ViMode::Insert);
    }

    #[test]
    fn test_visual_line_delete() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 4;
        h.handle(key('V'));
        assert_eq!(h.vim.mode(), ViMode::VisualLine);
        h.handle(key('d'));
        assert_eq!(h.buf, "aaa\nccc");
        assert_eq!(h.vim.mode(), ViMode::Normal);
    }

    #[test]
    fn test_visual_swap_anchor() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('w'));
        assert_eq!(h.cpos, 6);
        h.handle(key('o'));
        assert_eq!(h.cpos, 0);
    }

    #[test]
    fn test_visual_esc_returns_to_normal() {
        let mut h = TestHarness::new("hello");
        h.handle(key('v'));
        assert_eq!(h.vim.mode(), ViMode::Visual);
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        h.handle(esc);
        assert_eq!(h.vim.mode(), ViMode::Normal);
    }

    #[test]
    fn test_visual_tilde() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('e'));
        h.handle(key('~'));
        assert_eq!(h.buf, "HELLO world");
    }

    #[test]
    fn test_visual_switch_modes() {
        let mut h = TestHarness::new("hello");
        h.handle(key('v'));
        assert_eq!(h.vim.mode(), ViMode::Visual);
        h.handle(key('V'));
        assert_eq!(h.vim.mode(), ViMode::VisualLine);
        h.handle(key('v'));
        assert_eq!(h.vim.mode(), ViMode::Visual);
        h.handle(key('v'));
        assert_eq!(h.vim.mode(), ViMode::Normal);
    }

    #[test]
    fn test_visual_delete_multiline() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('v'));
        h.handle(key('j'));
        h.handle(key('d'));
        assert_eq!(h.buf, "bb\nccc");
        assert_eq!(h.cpos, 0);
    }

    #[test]
    fn test_visual_select_backwards() {
        let mut h = TestHarness::new("hello world");
        h.cpos = 10;
        h.handle(key('v'));
        h.handle(key('b'));
        assert_eq!(h.cpos, 6);
        h.handle(key('d'));
        assert_eq!(h.buf, "hello ");
    }

    #[test]
    fn test_visual_line_multiline() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('V'));
        h.handle(key('j'));
        h.handle(key('d'));
        assert_eq!(h.buf, "ccc");
    }

    #[test]
    fn test_visual_line_last_line() {
        let mut h = TestHarness::new("aaa\nbbb");
        h.cpos = 4;
        h.handle(key('V'));
        h.handle(key('d'));
        assert_eq!(h.buf, "aaa");
    }

    #[test]
    fn test_visual_empty_buffer() {
        let mut h = TestHarness::new("");
        h.handle(key('v'));
        assert_eq!(h.vim.mode(), ViMode::Visual);
        h.handle(key('d'));
        assert_eq!(h.buf, "");
        assert_eq!(h.vim.mode(), ViMode::Normal);
    }

    #[test]
    fn test_visual_single_char() {
        let mut h = TestHarness::new("x");
        h.handle(key('v'));
        h.handle(key('d'));
        assert_eq!(h.buf, "");
    }

    #[test]
    fn test_visual_paste_replaces() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('y'));
        h.handle(key('w'));
        h.handle(key('w'));
        h.handle(key('v'));
        h.handle(key('e'));
        h.handle(key('p'));
        assert_eq!(h.buf, "hello hello ");
    }

    #[test]
    fn test_visual_join_lines() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('V'));
        h.handle(key('j'));
        h.handle(key('J'));
        assert_eq!(h.buf, "aaa bbb\nccc");
        assert_eq!(h.vim.mode(), ViMode::Normal);
    }

    #[test]
    fn test_visual_yank_cursor_goes_to_start() {
        let mut h = TestHarness::new("hello world");
        h.cpos = 6;
        h.handle(key('v'));
        h.handle(key('e'));
        assert_eq!(h.cpos, 10);
        h.handle(key('y'));
        assert_eq!(h.cpos, 6);
    }

    #[test]
    fn test_visual_count_motion() {
        let mut h = TestHarness::new("one two three four");
        h.handle(key('v'));
        h.handle(key('2'));
        h.handle(key('w'));
        h.handle(key('d'));
        assert_eq!(h.buf, "hree four");
    }

    #[test]
    fn test_visual_find_motion() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('f'));
        h.handle(key('w'));
        h.handle(key('d'));
        assert_eq!(h.buf, "orld");
    }

    #[test]
    fn test_visual_dollar_motion() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('$'));
        h.handle(key('d'));
        assert_eq!(h.buf, "");
    }

    #[test]
    fn test_visual_range_anchor_after_cursor() {
        let mut h = TestHarness::new("abcdef");
        h.cpos = 3;
        h.handle(key('v'));
        h.handle(key('h'));
        h.handle(key('h'));
        assert_eq!(h.cpos, 1);
        h.handle(key('d'));
        assert_eq!(h.buf, "aef");
    }

    #[test]
    fn test_visual_uppercase() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('e'));
        h.handle(key('U'));
        assert_eq!(h.buf, "HELLO world");
        assert_eq!(h.vim.mode(), ViMode::Normal);
    }

    #[test]
    fn test_visual_lowercase() {
        let mut h = TestHarness::new("HELLO world");
        h.handle(key('v'));
        h.handle(key('e'));
        h.handle(key('u'));
        assert_eq!(h.buf, "hello world");
    }

    #[test]
    fn test_visual_line_single_line_buffer() {
        let mut h = TestHarness::new("hello");
        h.handle(key('V'));
        h.handle(key('d'));
        assert_eq!(h.buf, "");
    }

    #[test]
    fn test_visual_line_first_line() {
        let mut h = TestHarness::new("aaa\nbbb");
        h.handle(key('V'));
        h.handle(key('d'));
        assert_eq!(h.buf, "bbb");
    }

    #[test]
    fn test_visual_undo() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('e'));
        h.handle(key('d'));
        assert_eq!(h.buf, " world");
        h.handle(key('u'));
        assert_eq!(h.buf, "hello world");
    }

    #[test]
    fn test_visual_line_yank_and_paste() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('V'));
        h.handle(key('y'));
        h.handle(key('G'));
        h.handle(key('p'));
        assert_eq!(h.buf, "aaa\nbbb\nccc\naaa");
    }

    #[test]
    fn test_visual_ctrl_c_passes_through() {
        let mut h = TestHarness::new("hello");
        h.handle(key('v'));
        let result = h.handle(key_ctrl('c'));
        assert_eq!(result, Action::Passthrough);
    }

    #[test]
    fn test_open_line_above() {
        let mut h = TestHarness::new("hello");
        h.handle(key('O'));
        assert_eq!(h.buf, "\nhello");
        assert_eq!(h.cpos, 0);
        assert_eq!(h.vim.mode(), ViMode::Insert);
    }

    #[test]
    fn test_open_line_above_multiline() {
        let mut h = TestHarness::new("aaa\nbbb");
        h.cpos = 4;
        h.handle(key('O'));
        assert_eq!(h.buf, "aaa\n\nbbb");
        assert_eq!(h.cpos, 4);
        assert_eq!(h.vim.mode(), ViMode::Insert);
    }

    #[test]
    fn test_visual_gg() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 8;
        h.handle(key('v'));
        h.handle(key('g'));
        h.handle(key('g'));
        assert_eq!(h.cpos, 0);
        assert_eq!(h.vim.mode(), ViMode::Visual);
        h.handle(key('d'));
        assert_eq!(h.buf, "cc");
    }

    #[test]
    fn test_visual_go_end() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('v'));
        h.handle(key('G'));
        h.handle(key('d'));
        assert_eq!(h.buf, "");
    }

    #[test]
    fn test_visual_line_change_middle() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 4;
        h.handle(key('V'));
        h.handle(key('c'));
        assert_eq!(h.buf, "aaa\n\nccc");
        assert_eq!(h.vim.mode(), ViMode::Insert);
    }

    #[test]
    fn test_visual_join_three_lines() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('V'));
        h.handle(key('j'));
        h.handle(key('j'));
        h.handle(key('J'));
        assert_eq!(h.buf, "aaa bbb ccc");
    }

    #[test]
    fn test_visual_join_with_leading_spaces() {
        let mut h = TestHarness::new("aaa\n  bbb\n  ccc");
        h.handle(key('V'));
        h.handle(key('j'));
        h.handle(key('J'));
        assert_eq!(h.buf, "aaa bbb\n  ccc");
    }

    #[test]
    fn test_iw_single_line() {
        let mut h = TestHarness::new("hello world");
        h.cpos = 2;
        h.handle(key('d'));
        h.handle(key('i'));
        h.handle(key('w'));
        assert_eq!(h.buf, " world");
    }

    #[test]
    fn test_iw_does_not_cross_newline() {
        let mut h = TestHarness::new("hello\nworld");
        h.cpos = 2;
        h.handle(key('d'));
        h.handle(key('i'));
        h.handle(key('w'));
        assert_eq!(h.buf, "\nworld");
    }

    #[test]
    fn test_aw_includes_trailing_space() {
        let mut h = TestHarness::new("hello world");
        h.cpos = 2;
        h.handle(key('d'));
        h.handle(key('a'));
        h.handle(key('w'));
        assert_eq!(h.buf, "world");
    }

    #[test]
    fn test_aw_does_not_cross_newline() {
        let mut h = TestHarness::new("hello\nworld");
        h.cpos = 2;
        h.handle(key('d'));
        h.handle(key('a'));
        h.handle(key('w'));
        assert_eq!(h.buf, "\nworld");
    }

    #[test]
    fn test_viw_selects_word() {
        let mut h = TestHarness::new("hello world");
        h.cpos = 7;
        h.handle(key('v'));
        h.handle(key('i'));
        h.handle(key('w'));
        h.handle(key('d'));
        assert_eq!(h.buf, "hello ");
    }

    #[test]
    fn test_viw_does_not_cross_newline() {
        let mut h = TestHarness::new("hello\nworld");
        h.cpos = 2;
        h.handle(key('v'));
        h.handle(key('i'));
        h.handle(key('w'));
        h.handle(key('d'));
        assert_eq!(h.buf, "\nworld");
    }

    #[test]
    fn test_iw_on_whitespace() {
        let mut h = TestHarness::new("hello   world");
        h.cpos = 6;
        h.handle(key('d'));
        h.handle(key('i'));
        h.handle(key('w'));
        assert_eq!(h.buf, "helloworld");
    }

    #[test]
    fn test_iw_on_newline() {
        let mut h = TestHarness::new("hello\nworld");
        h.cpos = 5;
        h.handle(key('d'));
        h.handle(key('i'));
        h.handle(key('w'));
        assert_eq!(h.buf, "helloworld");
    }

    #[test]
    fn test_viw_middle_of_line() {
        let mut h = TestHarness::new("aaa bbb ccc");
        h.cpos = 5;
        h.handle(key('v'));
        h.handle(key('i'));
        h.handle(key('w'));
        h.handle(key('d'));
        assert_eq!(h.buf, "aaa  ccc");
    }

    #[test]
    fn test_cw_on_word_acts_like_ce() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('c'));
        h.handle(key('w'));
        assert_eq!(h.buf, " world");
        assert_eq!(h.vim.mode(), ViMode::Insert);
    }

    #[test]
    fn test_cw_on_whitespace_acts_normally() {
        let mut h = TestHarness::new("hello   world");
        h.cpos = 5;
        h.handle(key('c'));
        h.handle(key('w'));
        assert_eq!(h.buf, "helloworld");
        assert_eq!(h.vim.mode(), ViMode::Insert);
    }

    #[test]
    fn test_semicolon_after_t_not_stuck() {
        let mut h = TestHarness::new("abcxdefxghi");
        h.handle(key('t'));
        h.handle(key('x'));
        assert_eq!(h.cpos, 2);
        h.handle(key(';'));
        assert_eq!(h.cpos, 6);
    }

    #[test]
    fn test_p_cursor_on_last_pasted_char() {
        let mut h = TestHarness::new("world");
        h.handle(key('y'));
        h.handle(key('w'));
        h.handle(key('$'));
        h.handle(key('p'));
        assert_eq!(h.buf, "worldworld");
        assert_eq!(h.cpos, 9);
    }

    #[test]
    fn test_curswant_through_short_line() {
        let mut h = TestHarness::new("abcde\nf\nghijk");
        h.cpos = 4;
        h.handle(key('j'));
        assert_eq!(h.cpos, 6);
        h.handle(key('j'));
        assert_eq!(h.cpos, 12);
    }

    #[test]
    fn test_curswant_cleared_by_horizontal_motion() {
        let mut h = TestHarness::new("abcde\nf\nghijk");
        h.cpos = 4;
        h.handle(key('j'));
        assert_eq!(h.cpos, 6);
        h.handle(key('0'));
        assert_eq!(h.cpos, 6);
        h.handle(key('j'));
        assert_eq!(h.cpos, 8);
    }

    #[test]
    fn test_dj_deletes_two_lines() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('d'));
        h.handle(key('j'));
        assert_eq!(h.buf, "ccc");
    }

    #[test]
    fn test_dk_deletes_two_lines() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 4;
        h.handle(key('d'));
        h.handle(key('k'));
        assert_eq!(h.buf, "ccc");
    }

    #[test]
    fn test_d_big_g_deletes_to_end_linewise() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 5;
        h.handle(key('d'));
        h.handle(key('G'));
        assert_eq!(h.buf, "aaa");
    }

    #[test]
    fn test_dgg_deletes_to_start_linewise() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 8;
        h.handle(key('d'));
        h.handle(key('g'));
        h.handle(key('g'));
        assert_eq!(h.buf, "");
    }

    #[test]
    fn test_insert_undo_groups_entire_session() {
        let mut h = TestHarness::new("");
        h.handle(key('i'));
        assert_eq!(h.vim.mode(), ViMode::Insert);
        h.buf.push_str("abc");
        h.cpos = 3;
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        h.handle(esc);
        assert_eq!(h.vim.mode(), ViMode::Normal);
        assert_eq!(h.buf, "abc");
        h.handle(key('u'));
        assert_eq!(h.buf, "");
    }

    #[test]
    fn test_insert_after_change_single_undo() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('c'));
        h.handle(key('w'));
        assert_eq!(h.buf, " world");
        assert_eq!(h.vim.mode(), ViMode::Insert);
        h.buf.insert_str(0, "hi");
        h.cpos = 2;
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        h.handle(esc);
        assert_eq!(h.buf, "hi world");
        h.handle(key('u'));
        assert_eq!(h.buf, "hello world");
    }

    #[test]
    fn test_visual_s_substitutes() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('e'));
        h.handle(key('s'));
        assert_eq!(h.buf, " world");
        assert_eq!(h.vim.mode(), ViMode::Insert);
    }

    #[test]
    fn test_visual_s_capital_linewise() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 4;
        h.handle(key('v'));
        h.handle(key('l'));
        h.handle(key('S'));
        assert_eq!(h.vim.mode(), ViMode::Insert);
        assert!(h.buf.contains("aaa"));
        assert!(h.buf.contains("ccc"));
        assert!(!h.buf.contains("bbb"));
    }

    #[test]
    fn test_g_with_count() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 8;
        h.handle(key('2'));
        h.handle(key('G'));
        assert_eq!(h.cpos, 4);
    }

    #[test]
    fn test_g_without_count_goes_to_end() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('G'));
        assert_eq!(h.cpos, 10);
    }

    #[test]
    fn test_r_with_count_cursor_on_last_replaced() {
        let mut h = TestHarness::new("hello");
        h.handle(key('3'));
        h.handle(key('r'));
        h.handle(key('x'));
        assert_eq!(h.buf, "xxxlo");
        assert_eq!(h.cpos, 2);
    }

    #[test]
    fn test_capital_p_cursor_on_last_pasted_char() {
        let mut h = TestHarness::new("world");
        h.kill_ring.set_with_linewise("hello".to_string(), false);
        h.handle(key('P'));
        assert_eq!(h.buf, "helloworld");
        assert_eq!(h.cpos, 4);
    }

    #[test]
    fn test_j_with_count() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('3'));
        h.handle(key('J'));
        assert_eq!(h.buf, "aaa bbb ccc");
    }

    #[test]
    fn test_j_default_joins_two_lines() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('J'));
        assert_eq!(h.buf, "aaa bbb\nccc");
    }

    #[test]
    fn test_percent_forward() {
        let mut h = TestHarness::new("foo(bar)baz");
        h.cpos = 3;
        h.handle(key('%'));
        assert_eq!(h.cpos, 7);
    }

    #[test]
    fn test_percent_backward() {
        let mut h = TestHarness::new("foo(bar)baz");
        h.cpos = 7;
        h.handle(key('%'));
        assert_eq!(h.cpos, 3);
    }

    #[test]
    fn test_percent_from_before_bracket() {
        let mut h = TestHarness::new("foo(bar)baz");
        h.cpos = 0;
        h.handle(key('%'));
        assert_eq!(h.cpos, 7);
    }

    #[test]
    fn test_d_percent() {
        let mut h = TestHarness::new("foo(bar)baz");
        h.cpos = 3;
        h.handle(key('d'));
        h.handle(key('%'));
        assert_eq!(h.buf, "foobaz");
        assert_eq!(h.cpos, 3);
    }

    #[test]
    fn test_visual_semicolon_till_advances() {
        let mut h = TestHarness::new("abcabc");
        h.handle(key('t'));
        h.handle(key('c'));
        assert_eq!(h.cpos, 1);
        h.handle(key('v'));
        h.handle(key(';'));
        assert_eq!(h.cpos, 4);
    }
}
