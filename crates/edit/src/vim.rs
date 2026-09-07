use super::motions::{
    advance_chars, clamp_normal, current_line_content_range, current_line_range, find_char,
    find_matching_bracket, first_non_blank, first_non_blank_at, goto_line, line_end_normal,
    move_down, move_down_col, move_left, move_right_inclusive, move_right_normal, move_up,
    move_up_col, repeat_find, retreat_chars, word_end_pos, FindKind,
};
use super::text::{
    char_class, line_end, line_start, next_grapheme_boundary, prev_grapheme_boundary,
    word_backward_pos, word_forward_pos, CharClass,
};
use super::text_objects::{
    surrounding_delimiters, text_object, text_object_for_spec, TextObjectKind, TextObjectSpec,
};
use super::window::{DocumentCommand, DocumentKeyResult, DocumentTextObject};
use super::{Clipboard, UndoEntry, UndoHistory};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use smelt_buffer::attached::AttachedTextMut;
use smelt_buffer::attachment::ATTACHMENT_MARKER;

fn toggle_case(text: &str) -> String {
    let mut toggled = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_uppercase() {
            toggled.extend(ch.to_lowercase());
        } else {
            toggled.extend(ch.to_uppercase());
        }
    }
    toggled
}

// ── Public types ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VimMode {
    Insert,
    #[default]
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
    /// Center the input viewport on the cursor (zz).
    CenterScroll,
    /// Pan the viewport horizontally by `delta` cells (vim `zh`/`zl`).
    /// Positive = pan right, negative = pan left. Cursor stays put - same
    /// semantics as nvim's `zh`/`zl`.
    PanColumns(isize),
    /// Key not handled - caller should use its own logic.
    Passthrough,
}

/// Shared mutable state for vim operations. Borrowed from the host per keypress.
/// `buf` exposes source + attachment ids together as one wrapper - there is no
/// `&mut String` to grab, so every text mutation goes through methods that keep
/// the two halves in sync.
pub struct VimContext<'a> {
    pub buf: AttachedTextMut<'a>,
    pub cpos: &'a mut usize,
    pub history: &'a mut UndoHistory,
    pub clipboard: &'a mut Clipboard,
    pub mode: &'a mut VimMode,
    pub curswant: &'a mut Option<usize>,
    pub vim_state: &'a mut VimWindowState,
    /// Host clock at dispatch time; used to stamp yank-flash deadlines so the
    /// flash window is observable via a virtual clock in sim/fuzz runs.
    pub now: std::time::Instant,
}

impl VimContext<'_> {
    /// Snapshot to undo history before mutating.
    fn save_undo(&mut self) {
        self.history.save(UndoEntry::snapshot(
            self.buf.as_str(),
            *self.cpos,
            self.buf.ids(),
        ));
    }

    /// Install an undo/redo entry: swap source + attachments, restore cpos,
    /// and clamp every offset that survived the source swap (cpos and the
    /// vim visual anchor - either can land past end-of-source if the entry
    /// shrunk the buffer).
    fn restore(&mut self, entry: UndoEntry) {
        self.buf.install(entry.buf, entry.attachments);
        *self.cpos = entry.cpos;
        clamp_normal(self.buf.as_str(), self.cpos);
        self.vim_state.clamp_visual_anchor(self.buf.as_str());
    }

    /// Undo: pop the most recent snapshot, stash current state for redo.
    fn undo(&mut self) {
        let current = UndoEntry::snapshot(self.buf.as_str(), *self.cpos, self.buf.ids());
        if let Some(entry) = self.history.undo(current) {
            self.restore(entry);
        }
    }

    /// Redo: pop the most recent redo, stash current state for undo.
    fn redo(&mut self) {
        let current = UndoEntry::snapshot(self.buf.as_str(), *self.cpos, self.buf.ids());
        if let Some(entry) = self.history.redo(current) {
            self.restore(entry);
        }
    }

    /// Stage `buf[start..end]` in the kill ring with its source byte range.
    /// The system clipboard is **not** written here - `Window::handle_key`'s
    /// caller observes the kill-ring's `source_range` change and pushes the
    /// rendered (`BufferCopy::copy`) form to the system clipboard. Keeping
    /// vim out of the system clipboard means raw markers stay in the kill
    /// ring for paste-back fidelity, while the clipboard gets `[label]` etc.
    fn yank_range(&mut self, start: usize, end: usize, linewise: bool) {
        // The kill ring carries plain text only; an attachment id has no
        // representation here. If the yanked range covers an
        // ATTACHMENT_MARKER, dropping the marker is the only way to keep
        // a later `p`/`P` from producing orphan marker bytes in source.
        let text = self.buf.as_str()[start..end].replace(ATTACHMENT_MARKER, "");
        self.clipboard
            .kill_ring
            .set_with_source(text, linewise, start, end);
    }

    fn register(&self) -> &str {
        self.clipboard.kill_ring.current()
    }

    fn register_linewise(&self) -> bool {
        self.clipboard.kill_ring.is_linewise()
    }

    /// Sync the kill ring from the system clipboard before a paste.
    /// If the clipboard was updated externally, overwrite the kill ring (charwise).
    fn sync_paste_from_clipboard(&mut self) {
        let current = self.clipboard.read();
        let Some(text) = current else { return };
        let prev = self
            .clipboard
            .kill_ring
            .last_clipboard_write()
            .map(str::to_owned);
        if prev.as_deref() == Some(text.as_str()) {
            return;
        }
        self.clipboard
            .kill_ring
            .set_with_linewise(text.clone(), false);
        self.clipboard.kill_ring.record_clipboard_write(text);
    }

    /// Delete `buf[start..end]`. Attachment ids whose markers lived in the
    /// range are dropped automatically. Endpoints are snapped.
    fn delete_range(&mut self, start: usize, end: usize) {
        self.buf.replace_range(start..end, "");
        self.vim_state.clamp_visual_anchor(self.buf.as_str());
    }

    /// Replace `buf[start..end]` with `text`. Attachment ids whose markers
    /// survive into `text` (e.g. case-mapped markers fold to themselves)
    /// keep their ids; only markers actually removed are dropped. Endpoints
    /// are snapped.
    fn replace_range(&mut self, start: usize, end: usize, text: &str) {
        self.buf.replace_range(start..end, text);
        self.vim_state.clamp_visual_anchor(self.buf.as_str());
    }

    /// Insert `text` at `at`, returning the inserted position. Mirrors
    /// `buf.insert_str` but also clamps the visual anchor: a paste shifts
    /// every offset past `at` right by `text.len()`, so a pre-paste anchor
    /// can land mid-codepoint in the post-paste source.
    fn insert_str(&mut self, at: usize, text: &str) -> usize {
        let p = self.buf.insert_str(at, text);
        self.vim_state.clamp_visual_anchor(self.buf.as_str());
        p
    }
}

// ── Internal types ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
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

fn find_kind_char(kind: FindKind) -> char {
    match kind {
        FindKind::Forward => 'f',
        FindKind::ForwardTill => 't',
        FindKind::Backward => 'F',
        FindKind::BackwardTill => 'T',
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum SubState {
    #[default]
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
    /// `ys` pressed, waiting for a motion or text-object prefix.
    WaitingSurroundMotion,
    /// `ysi`/`ysa` pressed, waiting for object type char.
    WaitingSurroundTextObj(bool),
    /// A surround target range was resolved, waiting for the delimiter to add.
    WaitingSurroundChar(usize, usize),
    /// `ds` pressed, waiting for delimiter kind to remove.
    WaitingDeleteSurround,
    /// `cs` pressed, waiting for delimiter kind to replace.
    WaitingChangeSurroundTarget,
    /// `cs{target}` pressed, waiting for replacement delimiter.
    WaitingChangeSurroundReplacement(char),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RepeatKey {
    Char(char),
    Left,
    Right,
    Backspace,
    Home,
    End,
}

impl RepeatKey {
    fn from_key(key: KeyEvent) -> Option<Self> {
        match key.code {
            KeyCode::Char(c) => Some(Self::Char(c)),
            KeyCode::Left => Some(Self::Left),
            KeyCode::Right => Some(Self::Right),
            KeyCode::Backspace => Some(Self::Backspace),
            KeyCode::Home => Some(Self::Home),
            KeyCode::End => Some(Self::End),
            _ => None,
        }
    }

    fn key_event(self) -> KeyEvent {
        KeyEvent::new(
            match self {
                Self::Char(c) => KeyCode::Char(c),
                Self::Left => KeyCode::Left,
                Self::Right => KeyCode::Right,
                Self::Backspace => KeyCode::Backspace,
                Self::Home => KeyCode::Home,
                Self::End => KeyCode::End,
            },
            KeyModifiers::empty(),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RepeatCommand {
    Direct {
        command: char,
        count: usize,
    },
    Replace {
        count: usize,
        replacement: char,
    },
    OpMotion {
        op: Op,
        motion: RepeatKey,
        count: usize,
    },
    OpFind {
        op: Op,
        kind: FindKind,
        target: char,
        count: usize,
    },
    Linewise {
        op: Op,
        count: usize,
    },
    TextObject {
        op: Op,
        inner: bool,
        object: char,
        count: usize,
    },
    DeleteSurround {
        target: char,
    },
    ChangeSurround {
        target: char,
        replacement: char,
    },
}

// ── Vim state ───────────────────────────────────────────────────────────────

/// Per-Window vim state: persistent slots (`visual_anchor`, `last_find`) and
/// in-flight key-sequence accumulators (`sub`, `count1`, `count2`).
#[derive(Clone, Copy, Debug, Default)]
pub struct VimWindowState {
    /// Visual-mode anchor byte; meaningful only in `Visual`/`VisualLine`.
    pub(crate) visual_anchor: usize,
    /// Last `f`/`t`/`F`/`T` target for `;`/`,` replay.
    pub(crate) last_find: Option<(FindKind, char)>,
    /// Last mutating Normal-mode command for `.` repeat.
    last_change: Option<RepeatCommand>,
    /// Guard used while replaying `.` so the repeated command does not replace itself.
    replaying_change: bool,
    /// In-flight sub-state for multi-key sequences.
    pub(crate) sub: SubState,
    /// Count before the operator (or standalone motion).
    pub(crate) count1: Option<usize>,
    /// Count after the operator, before the motion.
    pub(crate) count2: Option<usize>,
}

impl VimWindowState {
    /// Visual anchor, snapped to a grapheme boundary in `buf`. All consumers
    /// must go through this accessor - the raw field may be stale.
    pub fn visual_anchor_at(&self, buf: &str) -> usize {
        smelt_buffer::text::snap_grapheme(buf, self.visual_anchor)
    }

    /// Raw stored anchor without snapping. For invariant checks that
    /// want to detect drift past `text().len()` before the snap happens.
    pub fn visual_anchor_raw(&self) -> usize {
        self.visual_anchor
    }

    /// Reset the visual anchor to 0. Call after wholesale source swaps so
    /// stale anchors can't outlive the bytes they pointed at.
    pub fn clear_visual_anchor(&mut self) {
        self.visual_anchor = 0;
    }

    /// Shift the visual anchor right by `delta` bytes. Call after
    /// prepending text to the buffer so the anchor keeps pointing at the
    /// same character (cpos is shifted by the caller too).
    pub fn shift_visual_anchor(&mut self, delta: usize, source: &str) {
        self.visual_anchor =
            smelt_buffer::text::ceil_grapheme(source, self.visual_anchor.saturating_add(delta));
    }

    /// Clamp the visual anchor into `source` and snap it to a grapheme
    /// boundary. Call after any in-place source shrink so the anchor
    /// preserved for `gv` can never outlive its terminal glyph.
    pub fn clamp_visual_anchor(&mut self, source: &str) {
        if self.visual_anchor > source.len() {
            self.visual_anchor = source.len();
        }
        self.visual_anchor = smelt_buffer::text::snap_grapheme(source, self.visual_anchor);
    }

    fn record_change(&mut self, command: RepeatCommand) {
        if !self.replaying_change {
            self.last_change = Some(command);
        }
    }

    fn repeat_count(&mut self, stored: usize) -> usize {
        self.count1.take().unwrap_or(stored.max(1))
    }

    /// Pop count1 (default 1), clearing both accumulators.
    fn take_count(&mut self) -> usize {
        let n = self.count1.unwrap_or(1);
        self.count1 = None;
        self.count2 = None;
        n
    }

    /// Pop count1 × count2 (each default 1) and clear both.
    fn effective_count(&mut self) -> usize {
        let c1 = self.count1.unwrap_or(1);
        let c2 = self.count2.unwrap_or(1);
        self.count1 = None;
        self.count2 = None;
        c1 * c2
    }

    /// Clear count accumulators; leaves `sub` untouched.
    fn reset_counts(&mut self) {
        self.count1 = None;
        self.count2 = None;
    }

    /// Reset the entire pending sequence: `sub = Ready`, counts cleared.
    fn reset_pending(&mut self) {
        self.sub = SubState::Ready;
        self.reset_counts();
    }

    /// True when no multi-key sequence and no count accumulator are pending,
    /// i.e. Esc in Normal mode would be a no-op.
    pub fn is_idle(&self) -> bool {
        self.pending_input().is_none()
    }

    /// User-facing key sequence currently waiting for more vim input.
    ///
    /// Counts are included because they are part of the pending command: after
    /// `3d2` the next key is still a motion for that exact sequence.
    pub fn pending_input(&self) -> Option<String> {
        let mut out = String::new();
        if let Some(count) = self.count1 {
            out.push_str(&count.to_string());
        }

        let push_op = |out: &mut String, op: Op| {
            out.push(op.char());
            if let Some(count) = self.count2 {
                out.push_str(&count.to_string());
            }
        };

        match self.sub {
            SubState::Ready => {}
            SubState::WaitingOp(op) => push_op(&mut out, op),
            SubState::WaitingG => out.push('g'),
            SubState::WaitingZ => out.push('z'),
            SubState::WaitingOpG(op) => {
                push_op(&mut out, op);
                out.push('g');
            }
            SubState::WaitingR => out.push('r'),
            SubState::WaitingFind(kind) => out.push(find_kind_char(kind)),
            SubState::WaitingOpFind(op, kind) => {
                push_op(&mut out, op);
                out.push(find_kind_char(kind));
            }
            SubState::WaitingTextObj(op, inner) => {
                push_op(&mut out, op);
                out.push(if inner { 'i' } else { 'a' });
            }
            SubState::WaitingVisualTextObj(inner) => out.push(if inner { 'i' } else { 'a' }),
            SubState::WaitingSurroundMotion => out.push_str("ys"),
            SubState::WaitingSurroundTextObj(inner) => {
                out.push_str("ys");
                out.push(if inner { 'i' } else { 'a' });
            }
            SubState::WaitingSurroundChar(_, _) => out.push_str("ys"),
            SubState::WaitingDeleteSurround => out.push_str("ds"),
            SubState::WaitingChangeSurroundTarget => out.push_str("cs"),
            SubState::WaitingChangeSurroundReplacement(target) => {
                out.push_str("cs");
                out.push(target);
            }
        }

        (!out.is_empty()).then_some(out)
    }

    /// Set mode and clear the pending sequence.
    pub fn set_mode(&mut self, mode_ref: &mut VimMode, mode: VimMode) {
        *mode_ref = mode;
        self.reset_pending();
    }

    /// Anchor a visual selection at `cpos` and enter the given visual mode.
    pub fn begin_visual(&mut self, mode_ref: &mut VimMode, mode: VimMode, cpos: usize) {
        *mode_ref = mode;
        self.reset_pending();
        self.visual_anchor = cpos;
    }
}

/// Visual selection as ordered byte offsets, or `None` outside Visual modes.
pub fn visual_range(
    state: &VimWindowState,
    buf: &str,
    cpos: usize,
    mode: VimMode,
) -> Option<(usize, usize)> {
    match mode {
        VimMode::Visual => {
            let anchor = state.visual_anchor_at(buf);
            let cursor = smelt_buffer::text::snap_grapheme(buf, cpos);
            let (a, b) = if anchor <= cursor {
                (anchor, next_grapheme_boundary(buf, cursor).min(buf.len()))
            } else {
                (cursor, next_grapheme_boundary(buf, anchor).min(buf.len()))
            };
            Some((a, b))
        }
        VimMode::VisualLine => {
            let anchor = state.visual_anchor_at(buf);
            let cursor = smelt_buffer::text::snap_grapheme(buf, cpos);
            let start = line_start(buf, anchor).min(line_start(buf, cursor));
            let end = line_end(buf, anchor).max(line_end(buf, cursor));
            Some((start, end))
        }
        _ => None,
    }
}

/// Byte range removed by linewise operations for a line-content range.
pub fn linewise_delete_range(buf: &str, range: std::ops::Range<usize>) -> std::ops::Range<usize> {
    if range.end < buf.len() && buf.as_bytes()[range.end] == b'\n' {
        range.start..range.end + 1
    } else if range.start > 0 && buf.as_bytes()[range.start - 1] == b'\n' {
        range.start - 1..range.end
    } else {
        range
    }
}

/// Visual-mode anchor byte (snapped against `buf`), or `None` in Normal/Insert.
pub fn visual_anchor(state: &VimWindowState, buf: &str, mode: VimMode) -> Option<usize> {
    match mode {
        VimMode::Visual | VimMode::VisualLine => Some(state.visual_anchor_at(buf)),
        _ => None,
    }
}

pub fn handle_viewer_key(
    key: KeyEvent,
    mode: &mut VimMode,
    state: &mut VimWindowState,
) -> DocumentKeyResult {
    if *mode == VimMode::Insert {
        state.set_mode(mode, VimMode::Normal);
        return DocumentKeyResult::Consumed;
    }

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
        return DocumentKeyResult::Passthrough;
    }

    match state.sub {
        SubState::WaitingOp(Op::Yank) => {
            state.sub = SubState::Ready;
            let cmd = match key.code {
                KeyCode::Char('y') => Some(DocumentCommand::YankLines(
                    state.effective_count() as crate::RowIndex
                )),
                _ => None,
            };
            state.count1 = None;
            state.count2 = None;
            return cmd.map_or(DocumentKeyResult::Consumed, DocumentKeyResult::Command);
        }
        SubState::WaitingVisualTextObj(inner) => {
            state.sub = SubState::Ready;
            let cmd = match key.code {
                KeyCode::Char(kind) => {
                    DocumentTextObject::new(inner, kind).map(DocumentCommand::TextObject)
                }
                _ => None,
            };
            state.count1 = None;
            state.count2 = None;
            return cmd.map_or(DocumentKeyResult::Consumed, DocumentKeyResult::Command);
        }
        SubState::WaitingG => {
            state.sub = SubState::Ready;
            let cmd = match key.code {
                KeyCode::Char('g') => {
                    let row = state
                        .count1
                        .take()
                        .map(|n| n.saturating_sub(1))
                        .unwrap_or(0);
                    Some(DocumentCommand::GotoRow(row as crate::RowIndex))
                }
                KeyCode::Char('f') => Some(DocumentCommand::OpenAction),
                _ => None,
            };
            state.count1 = None;
            state.count2 = None;
            return cmd.map_or(DocumentKeyResult::Consumed, DocumentKeyResult::Command);
        }
        SubState::WaitingZ => {
            state.sub = SubState::Ready;
            let cmd = match key.code {
                KeyCode::Char('z') => Some(DocumentCommand::CenterScroll),
                KeyCode::Char('h') => Some(DocumentCommand::PanColumns(-1)),
                KeyCode::Char('l') => Some(DocumentCommand::PanColumns(1)),
                _ => None,
            };
            state.count1 = None;
            state.count2 = None;
            return cmd.map_or(DocumentKeyResult::Consumed, DocumentKeyResult::Command);
        }
        _ => {}
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('u') => DocumentKeyResult::Command(DocumentCommand::HalfPageRows(-1)),
            KeyCode::Char('d') => DocumentKeyResult::Command(DocumentCommand::HalfPageRows(1)),
            KeyCode::Char('b') => DocumentKeyResult::Command(DocumentCommand::PageRows(-1)),
            KeyCode::Char('f') => DocumentKeyResult::Command(DocumentCommand::PageRows(1)),
            KeyCode::Char('y') => DocumentKeyResult::Command(DocumentCommand::ScrollRows(-1)),
            KeyCode::Char('e') => DocumentKeyResult::Command(DocumentCommand::ScrollRows(1)),
            _ => DocumentKeyResult::Passthrough,
        };
    }

    let count = |state: &mut VimWindowState| state.take_count() as isize;
    match key.code {
        KeyCode::Char(c @ '1'..='9') => {
            let digit = c.to_digit(10).unwrap() as usize;
            let prev = state.count1.unwrap_or(0);
            state.count1 = Some(prev.saturating_mul(10).saturating_add(digit));
            DocumentKeyResult::Consumed
        }
        KeyCode::Char('0') if state.count1.is_some() => {
            let prev = state.count1.unwrap_or(0);
            state.count1 = Some(prev.saturating_mul(10));
            DocumentKeyResult::Consumed
        }
        KeyCode::Char('g') => {
            state.sub = SubState::WaitingG;
            DocumentKeyResult::Consumed
        }
        KeyCode::Char('G') => {
            if let Some(n) = state.count1.take() {
                DocumentKeyResult::Command(DocumentCommand::GotoRow(
                    n.saturating_sub(1) as crate::RowIndex
                ))
            } else {
                DocumentKeyResult::Command(DocumentCommand::BufferEnd)
            }
        }
        KeyCode::Char('j') | KeyCode::Down => {
            DocumentKeyResult::Command(DocumentCommand::MoveRows(count(state)))
        }
        KeyCode::Char('k') | KeyCode::Up => {
            DocumentKeyResult::Command(DocumentCommand::MoveRows(-count(state)))
        }
        KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
            let c = -count(state);
            DocumentKeyResult::Command(DocumentCommand::MoveCursorCol(c))
        }
        KeyCode::Char('l') | KeyCode::Right => {
            let c = count(state);
            DocumentKeyResult::Command(DocumentCommand::MoveCursorCol(c))
        }
        KeyCode::Char('0') => DocumentKeyResult::Command(DocumentCommand::LineStart),
        KeyCode::Char('$') => DocumentKeyResult::Command(DocumentCommand::LineEnd),
        KeyCode::Char('^' | '_') => DocumentKeyResult::Command(DocumentCommand::LineStart),
        KeyCode::Char('w') => DocumentKeyResult::Command(DocumentCommand::WordForward(
            state.take_count() as crate::RowIndex,
        )),
        KeyCode::Char('b') => DocumentKeyResult::Command(DocumentCommand::WordBackward(
            state.take_count() as crate::RowIndex,
        )),
        KeyCode::Char('e') => DocumentKeyResult::Command(DocumentCommand::WordEnd(
            state.take_count() as crate::RowIndex,
        )),
        KeyCode::Char('v') => {
            state.set_mode(mode, VimMode::Visual);
            DocumentKeyResult::Command(DocumentCommand::StartVisual)
        }
        KeyCode::Char('V') => {
            state.set_mode(mode, VimMode::VisualLine);
            DocumentKeyResult::Command(DocumentCommand::StartVisualLine)
        }
        KeyCode::Char('i') if matches!(*mode, VimMode::Visual | VimMode::VisualLine) => {
            state.sub = SubState::WaitingVisualTextObj(true);
            DocumentKeyResult::Consumed
        }
        KeyCode::Char('a') if matches!(*mode, VimMode::Visual | VimMode::VisualLine) => {
            state.sub = SubState::WaitingVisualTextObj(false);
            DocumentKeyResult::Consumed
        }
        KeyCode::Char('y') => {
            if matches!(*mode, VimMode::Visual | VimMode::VisualLine) {
                let linewise = matches!(*mode, VimMode::VisualLine);
                state.set_mode(mode, VimMode::Normal);
                DocumentKeyResult::Command(if linewise {
                    DocumentCommand::YankSelectionLinewise
                } else {
                    DocumentCommand::YankSelection
                })
            } else {
                state.sub = SubState::WaitingOp(Op::Yank);
                DocumentKeyResult::Consumed
            }
        }
        KeyCode::Esc => {
            state.reset_pending();
            state.set_mode(mode, VimMode::Normal);
            DocumentKeyResult::Command(DocumentCommand::ClearSelection)
        }
        KeyCode::Char('z') => {
            state.sub = SubState::WaitingZ;
            DocumentKeyResult::Consumed
        }
        _ if !state.is_idle() => {
            // An invalid counted motion cancels the sequence, not the viewer.
            state.reset_pending();
            DocumentKeyResult::Consumed
        }
        _ => DocumentKeyResult::Passthrough,
    }
}

/// Process a key event, mutating `ctx` (buffer, cursor, kill ring, undo, mode).
pub fn handle_key(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    match *ctx.mode {
        VimMode::Insert => handle_insert(key, ctx),
        VimMode::Normal => handle_normal(key, ctx),
        VimMode::Visual | VimMode::VisualLine => handle_visual(key, ctx),
    }
}

// ── Insert mode ─────────────────────────────────────────────────────

fn handle_insert(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    match key {
        KeyEvent {
            code: KeyCode::Esc, ..
        }
        | KeyEvent {
            code: KeyCode::Char('['),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => {
            enter_normal(ctx);
            Action::Consumed
        }
        KeyEvent {
            code: KeyCode::Char('w' | 'u'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => Action::Passthrough,
        KeyEvent {
            code: KeyCode::Char('h'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => Action::Passthrough,
        _ => Action::Passthrough,
    }
}

// ── Normal mode ─────────────────────────────────────────────────────

fn handle_normal(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('r') => {
                ctx.redo();
                return Action::Consumed;
            }
            // Pass through keys that the main handler needs.
            KeyCode::Char(
                'c' | 'd' | 'u' | 't' | 'k' | 'l' | 'f' | 'b' | 'j' | 'n' | 'p' | 's' | 'y' | 'e',
            ) => return Action::Passthrough,
            _ => return Action::Consumed,
        }
    }

    if key.code == KeyCode::BackTab {
        return Action::Passthrough;
    }

    // Shift+arrows pass through for shared shift-selection actions.
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

    match ctx.vim_state.sub {
        SubState::WaitingR => return handle_waiting_r(key, ctx),
        SubState::WaitingZ => {
            ctx.vim_state.sub = SubState::Ready;
            return match key.code {
                KeyCode::Char('z') => Action::CenterScroll,
                KeyCode::Char('h') => Action::PanColumns(-1),
                KeyCode::Char('l') => Action::PanColumns(1),
                _ => Action::Consumed,
            };
        }
        SubState::WaitingFind(kind) => return handle_waiting_find(key, kind, ctx),
        SubState::WaitingOpFind(op, kind) => return handle_waiting_op_find(key, op, kind, ctx),
        SubState::WaitingG => return handle_waiting_g(key, ctx),
        SubState::WaitingOpG(op) => return handle_waiting_op_g(key, op, ctx),
        SubState::WaitingTextObj(op, inner) => return handle_waiting_textobj(key, op, inner, ctx),
        SubState::WaitingSurroundMotion => return handle_waiting_surround_motion(key, ctx),
        SubState::WaitingSurroundTextObj(inner) => {
            return handle_waiting_surround_textobj(key, inner, ctx)
        }
        SubState::WaitingSurroundChar(start, end) => {
            return handle_waiting_surround_char(key, start, end, ctx)
        }
        SubState::WaitingDeleteSurround => return handle_waiting_delete_surround(key, ctx),
        SubState::WaitingChangeSurroundTarget => {
            return handle_waiting_change_surround_target(key, ctx)
        }
        SubState::WaitingChangeSurroundReplacement(target) => {
            return handle_waiting_change_surround_replacement(key, target, ctx)
        }
        SubState::WaitingOp(op) => {
            if let KeyCode::Char(c) = key.code {
                if c.is_ascii_digit() && (c != '0' || ctx.vim_state.count2.is_some()) {
                    ctx.vim_state.count2 = Some(
                        ctx.vim_state.count2.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize,
                    );
                    return Action::Consumed;
                }
                if c == op.char() {
                    let count =
                        ctx.vim_state.count1.unwrap_or(1) * ctx.vim_state.count2.unwrap_or(1);
                    let action = execute_linewise_op(op, ctx);
                    if op != Op::Yank {
                        ctx.vim_state
                            .record_change(RepeatCommand::Linewise { op, count });
                    }
                    return action;
                }
                if c == 's' {
                    ctx.vim_state.sub = match op {
                        Op::Yank => SubState::WaitingSurroundMotion,
                        Op::Delete => SubState::WaitingDeleteSurround,
                        Op::Change => SubState::WaitingChangeSurroundTarget,
                    };
                    return Action::Consumed;
                }
                if c == 'i' || c == 'a' {
                    ctx.vim_state.sub = SubState::WaitingTextObj(op, c == 'i');
                    return Action::Consumed;
                }
            }
            let repeat_key = RepeatKey::from_key(key);
            let count = ctx.vim_state.count1.unwrap_or(1) * ctx.vim_state.count2.unwrap_or(1);
            let result = execute_op_motion(key, op, ctx);
            if op != Op::Yank
                && !matches!(
                    ctx.vim_state.sub,
                    SubState::WaitingOpFind(_, _) | SubState::WaitingOpG(_)
                )
            {
                if let Some(motion) = repeat_key {
                    ctx.vim_state
                        .record_change(RepeatCommand::OpMotion { op, motion, count });
                }
            }
            // Don't reset if a new substate was set (e.g. WaitingOpFind for df/dt).
            if matches!(ctx.vim_state.sub, SubState::WaitingOp(_)) {
                ctx.vim_state.reset_pending();
            }
            return result;
        }
        SubState::WaitingVisualTextObj(_) | SubState::Ready => {}
    }

    if let KeyCode::Char(c) = key.code {
        if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
            return handle_normal_char(c, ctx);
        }
    }

    match key.code {
        KeyCode::Esc => {
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        KeyCode::Enter => Action::Submit,
        KeyCode::Left => {
            *ctx.cpos = move_left(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        KeyCode::Right => {
            *ctx.cpos = move_right_normal(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        KeyCode::Up => Action::HistoryPrev,
        KeyCode::Down => Action::HistoryNext,
        KeyCode::Home => {
            *ctx.cpos = line_start(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        KeyCode::End => {
            *ctx.cpos = line_end_normal(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        KeyCode::Backspace => {
            *ctx.cpos = move_left(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        _ => Action::Consumed,
    }
}

fn repeat_last_change(ctx: &mut VimContext<'_>) -> Action {
    let Some(command) = ctx.vim_state.last_change else {
        ctx.vim_state.reset_pending();
        return Action::Consumed;
    };

    let was_replaying = ctx.vim_state.replaying_change;
    ctx.vim_state.replaying_change = true;
    let action = match command {
        RepeatCommand::Direct { command, count } => {
            let n = ctx.vim_state.repeat_count(count);
            ctx.vim_state.count1 = Some(n);
            handle_normal_char(command, ctx)
        }
        RepeatCommand::Replace { count, replacement } => {
            let n = ctx.vim_state.repeat_count(count);
            ctx.vim_state.count1 = Some(n);
            handle_waiting_r(RepeatKey::Char(replacement).key_event(), ctx)
        }
        RepeatCommand::OpMotion { op, motion, count } => {
            let n = ctx.vim_state.repeat_count(count);
            ctx.vim_state.count1 = Some(n);
            execute_op_motion(motion.key_event(), op, ctx)
        }
        RepeatCommand::OpFind {
            op,
            kind,
            target,
            count,
        } => {
            let n = ctx.vim_state.repeat_count(count);
            ctx.vim_state.count1 = Some(n);
            handle_waiting_op_find(RepeatKey::Char(target).key_event(), op, kind, ctx)
        }
        RepeatCommand::Linewise { op, count } => {
            let n = ctx.vim_state.repeat_count(count);
            ctx.vim_state.count1 = Some(n);
            execute_linewise_op(op, ctx)
        }
        RepeatCommand::TextObject {
            op,
            inner,
            object,
            count,
        } => {
            let n = ctx.vim_state.repeat_count(count);
            ctx.vim_state.count1 = Some(n);
            handle_waiting_textobj(RepeatKey::Char(object).key_event(), op, inner, ctx)
        }
        RepeatCommand::DeleteSurround { target } => {
            delete_surround(ctx, target);
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        RepeatCommand::ChangeSurround {
            target,
            replacement,
        } => {
            change_surround(ctx, target, replacement);
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
    };
    ctx.vim_state.replaying_change = was_replaying;
    action
}

fn handle_normal_char(c: char, ctx: &mut VimContext<'_>) -> Action {
    if c != 'j' && c != 'k' && !c.is_ascii_digit() {
        *ctx.curswant = None;
    }

    if c.is_ascii_digit() && (c != '0' || ctx.vim_state.count1.is_some()) {
        ctx.vim_state.count1 =
            Some(ctx.vim_state.count1.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize);
        return Action::Consumed;
    }

    match c {
        // ── Repeat last change ───────────────────────────────────────
        '.' => repeat_last_change(ctx),

        // ── Operators ───────────────────────────────────────────────
        'd' => {
            ctx.vim_state.sub = SubState::WaitingOp(Op::Delete);
            Action::Consumed
        }
        'c' => {
            ctx.vim_state.sub = SubState::WaitingOp(Op::Change);
            Action::Consumed
        }
        'y' => {
            ctx.vim_state.sub = SubState::WaitingOp(Op::Yank);
            Action::Consumed
        }

        // ── Operator shortcuts ──────────────────────────────────────
        'D' => {
            ctx.save_undo();
            let end = line_end(ctx.buf.as_str(), *ctx.cpos);
            ctx.yank_range(*ctx.cpos, end, false);
            ctx.delete_range(*ctx.cpos, end);
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
            ctx.vim_state.record_change(RepeatCommand::Direct {
                command: 'D',
                count: 1,
            });
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        'C' => {
            ctx.save_undo();
            let end = line_end(ctx.buf.as_str(), *ctx.cpos);
            ctx.yank_range(*ctx.cpos, end, false);
            ctx.delete_range(*ctx.cpos, end);
            ctx.vim_state.record_change(RepeatCommand::Direct {
                command: 'C',
                count: 1,
            });
            enter_insert_mode(ctx);
            Action::Consumed
        }
        'Y' => {
            let (start, end) = current_line_range(ctx.buf.as_str(), *ctx.cpos);
            ctx.yank_range(start, end, true);
            ctx.clipboard.kill_ring.mark_yanked(ctx.now);
            ctx.vim_state.reset_pending();
            Action::Consumed
        }

        // ── Direct edits ────────────────────────────────────────────
        'x' => {
            let n = ctx.vim_state.take_count();
            if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
                ctx.save_undo();
                let end = advance_chars(ctx.buf.as_str(), *ctx.cpos, n);
                ctx.yank_range(*ctx.cpos, end, false);
                ctx.delete_range(*ctx.cpos, end);
                clamp_normal(ctx.buf.as_str(), ctx.cpos);
            }
            ctx.vim_state.record_change(RepeatCommand::Direct {
                command: 'x',
                count: n,
            });
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        'X' => {
            let n = ctx.vim_state.take_count();
            if *ctx.cpos > 0 {
                ctx.save_undo();
                let start = retreat_chars(ctx.buf.as_str(), *ctx.cpos, n);
                ctx.yank_range(start, *ctx.cpos, false);
                ctx.delete_range(start, *ctx.cpos);
                *ctx.cpos = start;
                clamp_normal(ctx.buf.as_str(), ctx.cpos);
            }
            ctx.vim_state.record_change(RepeatCommand::Direct {
                command: 'X',
                count: n,
            });
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        's' => {
            let n = ctx.vim_state.take_count();
            ctx.save_undo();
            if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
                let end = advance_chars(ctx.buf.as_str(), *ctx.cpos, n);
                ctx.yank_range(*ctx.cpos, end, false);
                ctx.delete_range(*ctx.cpos, end);
            }
            ctx.vim_state.record_change(RepeatCommand::Direct {
                command: 's',
                count: n,
            });
            enter_insert_mode(ctx);
            Action::Consumed
        }
        'S' => {
            ctx.save_undo();
            let (start, end) = current_line_content_range(ctx.buf.as_str(), *ctx.cpos);
            ctx.yank_range(start, end, false);
            ctx.delete_range(start, end);
            *ctx.cpos = start;
            ctx.vim_state.record_change(RepeatCommand::Direct {
                command: 'S',
                count: 1,
            });
            enter_insert_mode(ctx);
            Action::Consumed
        }
        'r' => {
            ctx.vim_state.sub = SubState::WaitingR;
            Action::Consumed
        }
        '~' => {
            let n = ctx.vim_state.take_count();
            if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
                ctx.save_undo();
                let start = *ctx.cpos;
                let end = advance_chars(ctx.buf.as_str(), start, n);
                let toggled = toggle_case(smelt_buffer::text::slice(ctx.buf.as_str(), start..end));
                ctx.replace_range(start, end, &toggled);
                *ctx.cpos = start + toggled.len();
                clamp_normal(ctx.buf.as_str(), ctx.cpos);
            }
            ctx.vim_state.record_change(RepeatCommand::Direct {
                command: '~',
                count: n,
            });
            ctx.vim_state.reset_pending();
            Action::Consumed
        }

        // ── Paste ───────────────────────────────────────────────────
        'p' => {
            ctx.sync_paste_from_clipboard();
            if !ctx.register().is_empty() {
                ctx.save_undo();
                if ctx.register_linewise() {
                    let eol = line_end(ctx.buf.as_str(), *ctx.cpos);
                    let text = ctx.register().to_string();
                    let insert = format!("\n{}", text);
                    let p = ctx.insert_str(eol, &insert);
                    *ctx.cpos = first_non_blank_at(ctx.buf.as_str(), p + 1);
                } else {
                    let after = advance_chars(ctx.buf.as_str(), *ctx.cpos, 1).min(ctx.buf.len());
                    let text = ctx.register().to_string();
                    let p = ctx.insert_str(after, &text);
                    let paste_end = p + text.len();
                    *ctx.cpos = prev_grapheme_boundary(ctx.buf.as_str(), paste_end).max(p);
                    clamp_normal(ctx.buf.as_str(), ctx.cpos);
                }
            }
            ctx.vim_state.record_change(RepeatCommand::Direct {
                command: 'p',
                count: 1,
            });
            Action::Consumed
        }
        'P' => {
            ctx.sync_paste_from_clipboard();
            if !ctx.register().is_empty() {
                ctx.save_undo();
                if ctx.register_linewise() {
                    let sol = line_start(ctx.buf.as_str(), *ctx.cpos);
                    let text = ctx.register().to_string();
                    let insert = format!("{}\n", text);
                    let p = ctx.insert_str(sol, &insert);
                    *ctx.cpos = first_non_blank_at(ctx.buf.as_str(), p);
                } else {
                    let text = ctx.register().to_string();
                    let p = ctx.insert_str(*ctx.cpos, &text);
                    let plen = text.len();
                    if plen > 0 {
                        let paste_end = p + plen;
                        *ctx.cpos = prev_grapheme_boundary(ctx.buf.as_str(), paste_end).max(p);
                        clamp_normal(ctx.buf.as_str(), ctx.cpos);
                    } else {
                        *ctx.cpos = p;
                    }
                }
            }
            ctx.vim_state.record_change(RepeatCommand::Direct {
                command: 'P',
                count: 1,
            });
            Action::Consumed
        }

        // ── Undo / Redo ─────────────────────────────────────────────
        'u' => {
            ctx.undo();
            Action::Consumed
        }

        // ── Visual mode ─────────────────────────────────────────────
        'v' => {
            ctx.vim_state.visual_anchor = *ctx.cpos;
            *ctx.mode = VimMode::Visual;
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        'V' => {
            ctx.vim_state.visual_anchor = *ctx.cpos;
            *ctx.mode = VimMode::VisualLine;
            ctx.vim_state.reset_pending();
            Action::Consumed
        }

        // ── Enter insert mode ───────────────────────────────────────
        'i' => {
            ctx.vim_state.take_count();
            ctx.save_undo();
            enter_insert_mode(ctx);
            Action::Consumed
        }
        'I' => {
            ctx.vim_state.take_count();
            ctx.save_undo();
            *ctx.cpos = first_non_blank(ctx.buf.as_str(), *ctx.cpos);
            enter_insert_mode(ctx);
            Action::Consumed
        }
        'a' => {
            ctx.vim_state.take_count();
            ctx.save_undo();
            if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
                *ctx.cpos = advance_chars(ctx.buf.as_str(), *ctx.cpos, 1);
            }
            enter_insert_mode(ctx);
            Action::Consumed
        }
        'A' => {
            ctx.vim_state.take_count();
            ctx.save_undo();
            *ctx.cpos = line_end(ctx.buf.as_str(), *ctx.cpos);
            enter_insert_mode(ctx);
            Action::Consumed
        }
        'o' => {
            ctx.save_undo();
            let eol = line_end(ctx.buf.as_str(), *ctx.cpos);
            let p = ctx.buf.insert(eol, '\n');
            *ctx.cpos = p + 1;
            enter_insert_mode(ctx);
            Action::Consumed
        }
        'O' => {
            ctx.save_undo();
            let sol = line_start(ctx.buf.as_str(), *ctx.cpos);
            let p = ctx.buf.insert(sol, '\n');
            *ctx.cpos = p;
            enter_insert_mode(ctx);
            Action::Consumed
        }

        // ── Find ────────────────────────────────────────────────────
        'f' => {
            ctx.vim_state.sub = SubState::WaitingFind(FindKind::Forward);
            Action::Consumed
        }
        'F' => {
            ctx.vim_state.sub = SubState::WaitingFind(FindKind::Backward);
            Action::Consumed
        }
        't' => {
            ctx.vim_state.sub = SubState::WaitingFind(FindKind::ForwardTill);
            Action::Consumed
        }
        'T' => {
            ctx.vim_state.sub = SubState::WaitingFind(FindKind::BackwardTill);
            Action::Consumed
        }
        ';' => {
            if let Some((kind, ch)) = ctx.vim_state.last_find {
                let n = ctx.vim_state.take_count();
                *ctx.cpos = repeat_find(ctx.buf.as_str(), *ctx.cpos, kind, ch, n);
            }
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        ',' => {
            if let Some((kind, ch)) = ctx.vim_state.last_find {
                let n = ctx.vim_state.take_count();
                *ctx.cpos = repeat_find(ctx.buf.as_str(), *ctx.cpos, kind.reversed(), ch, n);
            }
            ctx.vim_state.reset_pending();
            Action::Consumed
        }

        // ── Wait-for-second-char ────────────────────────────────────
        'g' => {
            ctx.vim_state.sub = SubState::WaitingG;
            Action::Consumed
        }
        'z' => {
            ctx.vim_state.sub = SubState::WaitingZ;
            Action::Consumed
        }

        // ── Motions ─────────────────────────────────────────────────
        'h' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = move_left(ctx.buf.as_str(), *ctx.cpos);
            }
            Action::Consumed
        }
        'l' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = move_right_normal(ctx.buf.as_str(), *ctx.cpos);
            }
            Action::Consumed
        }
        'j' => {
            let n = ctx.vim_state.take_count();
            if ctx.buf.contains('\n') {
                let (new_pos, col) = move_down_col(ctx.buf.as_str(), *ctx.cpos, *ctx.curswant);
                if new_pos == *ctx.cpos && n <= 1 {
                    ctx.vim_state.reset_pending();
                    return Action::HistoryNext;
                }
                *ctx.curswant = Some(col);
                *ctx.cpos = new_pos;
                for _ in 1..n {
                    (*ctx.cpos, _) = move_down_col(ctx.buf.as_str(), *ctx.cpos, *ctx.curswant);
                }
                clamp_normal(ctx.buf.as_str(), ctx.cpos);
                return Action::Consumed;
            }
            ctx.vim_state.reset_pending();
            if n <= 1 {
                Action::HistoryNext
            } else {
                Action::Consumed
            }
        }
        'k' => {
            let n = ctx.vim_state.take_count();
            if ctx.buf.contains('\n') {
                let (new_pos, col) = move_up_col(ctx.buf.as_str(), *ctx.cpos, *ctx.curswant);
                if new_pos == *ctx.cpos && n <= 1 {
                    ctx.vim_state.reset_pending();
                    return Action::HistoryPrev;
                }
                *ctx.curswant = Some(col);
                *ctx.cpos = new_pos;
                for _ in 1..n {
                    (*ctx.cpos, _) = move_up_col(ctx.buf.as_str(), *ctx.cpos, *ctx.curswant);
                }
                clamp_normal(ctx.buf.as_str(), ctx.cpos);
                return Action::Consumed;
            }
            ctx.vim_state.reset_pending();
            if n <= 1 {
                Action::HistoryPrev
            } else {
                Action::Consumed
            }
        }
        'w' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_forward_pos(ctx.buf.as_str(), *ctx.cpos, CharClass::Word);
            }
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
            Action::Consumed
        }
        'W' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_forward_pos(ctx.buf.as_str(), *ctx.cpos, CharClass::WORD);
            }
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
            Action::Consumed
        }
        'b' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_backward_pos(ctx.buf.as_str(), *ctx.cpos, CharClass::Word);
            }
            Action::Consumed
        }
        'B' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_backward_pos(ctx.buf.as_str(), *ctx.cpos, CharClass::WORD);
            }
            Action::Consumed
        }
        'e' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_end_pos(ctx.buf.as_str(), *ctx.cpos, CharClass::Word);
            }
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
            Action::Consumed
        }
        'E' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_end_pos(ctx.buf.as_str(), *ctx.cpos, CharClass::WORD);
            }
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
            Action::Consumed
        }
        '0' => {
            *ctx.cpos = line_start(ctx.buf.as_str(), *ctx.cpos);
            *ctx.curswant = None;
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        '^' | '_' => {
            *ctx.cpos = first_non_blank(ctx.buf.as_str(), *ctx.cpos);
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        '$' => {
            let n = ctx.vim_state.take_count();
            for _ in 1..n {
                *ctx.cpos = move_down(ctx.buf.as_str(), *ctx.cpos);
            }
            *ctx.cpos = line_end_normal(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        'G' => {
            let had_count = ctx.vim_state.count1.is_some();
            let n = ctx.vim_state.take_count();
            *ctx.cpos = if had_count {
                goto_line(ctx.buf.as_str(), n.saturating_sub(1))
            } else {
                ctx.buf.len()
            };
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
            Action::Consumed
        }

        // ── Match bracket ────────────────────────────────────────────
        '%' => {
            ctx.vim_state.reset_counts();
            if let Some(p) = find_matching_bracket(ctx.buf.as_str(), *ctx.cpos) {
                *ctx.cpos = p;
            }
            Action::Consumed
        }

        'J' => {
            let count = ctx.vim_state.take_count().max(2);
            let eol = line_end(ctx.buf.as_str(), *ctx.cpos);
            if eol < ctx.buf.len() {
                ctx.save_undo();
                let mut join_pos = *ctx.cpos;
                for _ in 1..count {
                    let after = &ctx.buf.as_str()[join_pos..];
                    if let Some(nl) = after.find('\n') {
                        let abs = join_pos + nl;
                        let end = first_non_blank_at(ctx.buf.as_str(), abs + 1);
                        ctx.replace_range(abs, end, " ");
                        join_pos = abs;
                    } else {
                        break;
                    }
                }
                *ctx.cpos = join_pos;
            }
            ctx.vim_state.record_change(RepeatCommand::Direct {
                command: 'J',
                count,
            });
            Action::Consumed
        }

        _ => {
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
    }
}

// ── Visual mode ──────────────────────────────────────────────────────

fn handle_visual(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    if let SubState::WaitingVisualTextObj(inner) = ctx.vim_state.sub {
        ctx.vim_state.sub = SubState::Ready;
        if let KeyCode::Char(c) = key.code {
            if let Some(spec) = TextObjectSpec::new(inner, c) {
                if let Some((start, end)) = text_object_for_spec(ctx.buf.as_str(), *ctx.cpos, spec)
                {
                    ctx.vim_state.visual_anchor = start;
                    *ctx.cpos = if spec.kind == TextObjectKind::Paragraph {
                        *ctx.mode = VimMode::VisualLine;
                        if end > start {
                            line_start(
                                ctx.buf.as_str(),
                                prev_grapheme_boundary(ctx.buf.as_str(), end),
                            )
                        } else {
                            start
                        }
                    } else if end > 0 {
                        prev_grapheme_boundary(ctx.buf.as_str(), end)
                    } else {
                        end
                    };
                }
            }
        }
        return Action::Consumed;
    }
    if let SubState::WaitingFind(kind) = ctx.vim_state.sub {
        return handle_waiting_find(key, kind, ctx);
    }
    if let SubState::WaitingG = ctx.vim_state.sub {
        return handle_waiting_g(key, ctx);
    }
    if let SubState::WaitingZ = ctx.vim_state.sub {
        ctx.vim_state.sub = SubState::Ready;
        return match key.code {
            KeyCode::Char('z') => Action::CenterScroll,
            KeyCode::Char('h') => Action::PanColumns(-1),
            KeyCode::Char('l') => Action::PanColumns(1),
            _ => Action::Consumed,
        };
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::Passthrough;
    }

    if let KeyCode::Char(c) = key.code {
        if c.is_ascii_digit() && (c != '0' || ctx.vim_state.count1.is_some()) {
            ctx.vim_state.count1 =
                Some(ctx.vim_state.count1.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize);
            return Action::Consumed;
        }
        if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
            return handle_visual_char(c, ctx);
        }
    }

    match key.code {
        KeyCode::Esc => {
            exit_visual(ctx);
            Action::Consumed
        }
        KeyCode::Enter => Action::Submit,
        KeyCode::Left => {
            *ctx.cpos = move_left(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        KeyCode::Right => {
            *ctx.cpos = move_right_normal(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        KeyCode::Up => {
            *ctx.cpos = move_up(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        KeyCode::Down => {
            *ctx.cpos = move_down(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        KeyCode::Home => {
            *ctx.cpos = line_start(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        KeyCode::End => {
            *ctx.cpos = line_end_normal(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        _ => Action::Consumed,
    }
}

fn handle_visual_char(c: char, ctx: &mut VimContext<'_>) -> Action {
    if c != 'j' && c != 'k' && !c.is_ascii_digit() {
        *ctx.curswant = None;
    }
    match c {
        // ── Escape visual mode ─────────────────────────────────────
        'v' if *ctx.mode == VimMode::Visual => {
            exit_visual(ctx);
            Action::Consumed
        }
        'V' if *ctx.mode == VimMode::VisualLine => {
            exit_visual(ctx);
            Action::Consumed
        }
        // Switch between visual modes
        'v' if *ctx.mode == VimMode::VisualLine => {
            *ctx.mode = VimMode::Visual;
            Action::Consumed
        }
        'V' if *ctx.mode == VimMode::Visual => {
            *ctx.mode = VimMode::VisualLine;
            Action::Consumed
        }

        // ── Substitute (s → change, S → linewise change) ────────
        's' => handle_visual_char('c', ctx),
        'S' => {
            *ctx.mode = VimMode::VisualLine;
            handle_visual_char('c', ctx)
        }

        // ── Operators on selection ──────────────────────────────────
        'd' | 'x' => {
            if let Some((start, end)) =
                visual_range(ctx.vim_state, ctx.buf.as_str(), *ctx.cpos, *ctx.mode)
            {
                let linewise = *ctx.mode == VimMode::VisualLine;
                ctx.save_undo();
                ctx.yank_range(start, end, linewise);
                if linewise {
                    let drain_end = if end < ctx.buf.len() && ctx.buf.as_bytes()[end] == b'\n' {
                        end + 1
                    } else if start > 0 && ctx.buf.as_bytes()[start - 1] == b'\n' {
                        let s = start - 1;
                        ctx.delete_range(s, end);
                        *ctx.cpos = s.min(ctx.buf.len());
                        clamp_normal(ctx.buf.as_str(), ctx.cpos);
                        if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
                            *ctx.cpos = first_non_blank_at(
                                ctx.buf.as_str(),
                                line_start(ctx.buf.as_str(), *ctx.cpos),
                            );
                        }
                        exit_visual(ctx);
                        return Action::Consumed;
                    } else {
                        end
                    };
                    ctx.delete_range(start, drain_end);
                } else {
                    ctx.delete_range(start, end);
                }
                *ctx.cpos = start.min(ctx.buf.len());
                clamp_normal(ctx.buf.as_str(), ctx.cpos);
            }
            exit_visual(ctx);
            Action::Consumed
        }
        'c' => {
            if let Some((start, end)) =
                visual_range(ctx.vim_state, ctx.buf.as_str(), *ctx.cpos, *ctx.mode)
            {
                let linewise = *ctx.mode == VimMode::VisualLine;
                ctx.save_undo();
                ctx.yank_range(start, end, linewise);
                if linewise {
                    let content_start = first_non_blank_at(ctx.buf.as_str(), start);
                    ctx.delete_range(content_start, end);
                    *ctx.cpos = content_start;
                } else {
                    ctx.delete_range(start, end);
                    *ctx.cpos = start;
                }
                *ctx.mode = VimMode::Insert;
                ctx.vim_state.sub = SubState::Ready;
                ctx.vim_state.reset_counts();
                return Action::Consumed;
            }
            exit_visual(ctx);
            Action::Consumed
        }
        'y' => {
            if let Some((start, end)) =
                visual_range(ctx.vim_state, ctx.buf.as_str(), *ctx.cpos, *ctx.mode)
            {
                let linewise = *ctx.mode == VimMode::VisualLine;
                ctx.yank_range(start, end, linewise);
                ctx.clipboard.kill_ring.mark_yanked(ctx.now);
                *ctx.cpos = start;
            }
            exit_visual(ctx);
            Action::Consumed
        }

        // ── Case toggling on selection ─────────────────────────────
        '~' => {
            if let Some((start, end)) =
                visual_range(ctx.vim_state, ctx.buf.as_str(), *ctx.cpos, *ctx.mode)
            {
                ctx.save_undo();
                let toggled = toggle_case(smelt_buffer::text::slice(ctx.buf.as_str(), start..end));
                ctx.replace_range(start, end, &toggled);
                *ctx.cpos = start;
                clamp_normal(ctx.buf.as_str(), ctx.cpos);
            }
            exit_visual(ctx);
            Action::Consumed
        }
        'U' => {
            if let Some((start, end)) =
                visual_range(ctx.vim_state, ctx.buf.as_str(), *ctx.cpos, *ctx.mode)
            {
                ctx.save_undo();
                let upper = ctx.buf.as_str()[start..end].to_uppercase();
                ctx.replace_range(start, end, &upper);
                *ctx.cpos = start;
                clamp_normal(ctx.buf.as_str(), ctx.cpos);
            }
            exit_visual(ctx);
            Action::Consumed
        }
        'u' => {
            if let Some((start, end)) =
                visual_range(ctx.vim_state, ctx.buf.as_str(), *ctx.cpos, *ctx.mode)
            {
                ctx.save_undo();
                let lower = ctx.buf.as_str()[start..end].to_lowercase();
                ctx.replace_range(start, end, &lower);
                *ctx.cpos = start;
                clamp_normal(ctx.buf.as_str(), ctx.cpos);
            }
            exit_visual(ctx);
            Action::Consumed
        }

        // ── Join lines ─────────────────────────────────────────────
        'J' => {
            if let Some((start, end)) =
                visual_range(ctx.vim_state, ctx.buf.as_str(), *ctx.cpos, *ctx.mode)
            {
                ctx.save_undo();
                let mut pos = start;
                let mut remaining = end;
                while pos < remaining.min(ctx.buf.len()) {
                    if let Some(nl) = ctx.buf.as_str()[pos..remaining.min(ctx.buf.len())].find('\n')
                    {
                        let abs = pos + nl;
                        let ws_end = first_non_blank_at(ctx.buf.as_str(), abs + 1);
                        let removed = ws_end - abs;
                        ctx.replace_range(abs, ws_end, " ");
                        remaining -= removed - 1; // replaced N chars with 1
                        pos = abs + 1;
                    } else {
                        break;
                    }
                }
                *ctx.cpos = start;
            }
            exit_visual(ctx);
            Action::Consumed
        }

        // ── Paste over selection ───────────────────────────────────
        'p' | 'P' => {
            ctx.sync_paste_from_clipboard();
            if !ctx.register().is_empty() {
                if let Some((start, end)) =
                    visual_range(ctx.vim_state, ctx.buf.as_str(), *ctx.cpos, *ctx.mode)
                {
                    ctx.save_undo();
                    let old = ctx.buf.as_str()[start..end].to_string();
                    let text = ctx.register().to_string();
                    ctx.replace_range(start, end, &text);
                    *ctx.cpos = start;
                    clamp_normal(ctx.buf.as_str(), ctx.cpos);
                    // Replaced text goes into register; mirror to clipboard.
                    ctx.clipboard
                        .kill_ring
                        .set_with_linewise(old.clone(), false);
                    if ctx.clipboard.write(&old).is_ok() {
                        ctx.clipboard.kill_ring.record_clipboard_write(old);
                    }
                }
            }
            exit_visual(ctx);
            Action::Consumed
        }

        // ── Motions (move cursor, anchor stays) ────────────────────
        'h' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = move_left(ctx.buf.as_str(), *ctx.cpos);
            }
            Action::Consumed
        }
        'l' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = move_right_normal(ctx.buf.as_str(), *ctx.cpos);
            }
            Action::Consumed
        }
        'j' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                let col;
                (*ctx.cpos, col) = move_down_col(ctx.buf.as_str(), *ctx.cpos, *ctx.curswant);
                *ctx.curswant = Some(col);
            }
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
            Action::Consumed
        }
        'k' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                let col;
                (*ctx.cpos, col) = move_up_col(ctx.buf.as_str(), *ctx.cpos, *ctx.curswant);
                *ctx.curswant = Some(col);
            }
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
            Action::Consumed
        }
        'w' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_forward_pos(ctx.buf.as_str(), *ctx.cpos, CharClass::Word);
            }
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
            Action::Consumed
        }
        'W' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_forward_pos(ctx.buf.as_str(), *ctx.cpos, CharClass::WORD);
            }
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
            Action::Consumed
        }
        'b' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_backward_pos(ctx.buf.as_str(), *ctx.cpos, CharClass::Word);
            }
            Action::Consumed
        }
        'B' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_backward_pos(ctx.buf.as_str(), *ctx.cpos, CharClass::WORD);
            }
            Action::Consumed
        }
        'e' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_end_pos(ctx.buf.as_str(), *ctx.cpos, CharClass::Word);
            }
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
            Action::Consumed
        }
        'E' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_end_pos(ctx.buf.as_str(), *ctx.cpos, CharClass::WORD);
            }
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
            Action::Consumed
        }
        '0' => {
            *ctx.cpos = line_start(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        '^' | '_' => {
            *ctx.cpos = first_non_blank(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        '$' => {
            *ctx.cpos = line_end_normal(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        'G' => {
            let had_count = ctx.vim_state.count1.is_some();
            let n = ctx.vim_state.take_count();
            *ctx.cpos = if had_count {
                goto_line(ctx.buf.as_str(), n.saturating_sub(1))
            } else {
                ctx.buf.len()
            };
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
            Action::Consumed
        }
        '%' => {
            ctx.vim_state.reset_counts();
            if let Some(p) = find_matching_bracket(ctx.buf.as_str(), *ctx.cpos) {
                *ctx.cpos = p;
            }
            Action::Consumed
        }
        'g' => {
            ctx.vim_state.sub = SubState::WaitingG;
            Action::Consumed
        }
        'f' => {
            ctx.vim_state.sub = SubState::WaitingFind(FindKind::Forward);
            Action::Consumed
        }
        'F' => {
            ctx.vim_state.sub = SubState::WaitingFind(FindKind::Backward);
            Action::Consumed
        }
        't' => {
            ctx.vim_state.sub = SubState::WaitingFind(FindKind::ForwardTill);
            Action::Consumed
        }
        'T' => {
            ctx.vim_state.sub = SubState::WaitingFind(FindKind::BackwardTill);
            Action::Consumed
        }
        ';' => {
            if let Some((kind, ch)) = ctx.vim_state.last_find {
                let n = ctx.vim_state.take_count();
                *ctx.cpos = repeat_find(ctx.buf.as_str(), *ctx.cpos, kind, ch, n);
            }
            Action::Consumed
        }
        ',' => {
            if let Some((kind, ch)) = ctx.vim_state.last_find {
                let n = ctx.vim_state.take_count();
                *ctx.cpos = repeat_find(ctx.buf.as_str(), *ctx.cpos, kind.reversed(), ch, n);
            }
            Action::Consumed
        }

        // ── Count digits ───────────────────────────────────────────
        c if c.is_ascii_digit() && (c != '0' || ctx.vim_state.count1.is_some()) => {
            ctx.vim_state.count1 =
                Some(ctx.vim_state.count1.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize);
            Action::Consumed
        }

        // ── Swap anchor and cursor ─────────────────────────────────
        'o' => {
            // Snap through `visual_anchor_at` first: the raw field outlives
            // buffer mutations (e.g. a paste that replaces the visual
            // selection shrinks `source` below the old anchor), and a raw
            // swap would land `cpos` past `source.len()`.
            let anchor = ctx.vim_state.visual_anchor_at(ctx.buf.as_str());
            ctx.vim_state.visual_anchor = *ctx.cpos;
            *ctx.cpos = anchor;
            Action::Consumed
        }

        // ── Text objects (iw, aw, i", a( etc.) ────────────────────
        'i' => {
            ctx.vim_state.sub = SubState::WaitingVisualTextObj(true);
            Action::Consumed
        }
        'a' => {
            ctx.vim_state.sub = SubState::WaitingVisualTextObj(false);
            Action::Consumed
        }

        _ => Action::Consumed,
    }
}

// ── Sub-state handlers ──────────────────────────────────────────────

fn handle_waiting_r(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    let replacement_char = match key.code {
        KeyCode::Char(c) => Some(c),
        KeyCode::Enter => Some('\n'),
        _ => None,
    };
    if let Some(c) = replacement_char.filter(|c| *c != ATTACHMENT_MARKER) {
        if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
            let n = ctx.vim_state.take_count();
            ctx.vim_state.record_change(RepeatCommand::Replace {
                count: n,
                replacement: c,
            });
            ctx.save_undo();

            let start = *ctx.cpos;
            let mut end = start;
            let mut replacement = String::new();
            for _ in 0..n {
                if end >= ctx.buf.len() {
                    break;
                }
                end = next_grapheme_boundary(ctx.buf.as_str(), end);
                replacement.push(c);
            }
            ctx.replace_range(start, end, &replacement);
            let replacement_end = start + replacement.len();
            *ctx.cpos = prev_grapheme_boundary(ctx.buf.as_str(), replacement_end).max(start);
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

fn handle_waiting_find(key: KeyEvent, kind: FindKind, ctx: &mut VimContext<'_>) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    if let KeyCode::Char(ch) = key.code {
        let n = ctx.vim_state.take_count();
        ctx.vim_state.last_find = Some((kind, ch));
        let mut pos = *ctx.cpos;
        for _ in 0..n {
            if let Some(p) = find_char(ctx.buf.as_str(), pos, kind, ch) {
                pos = p;
            }
        }
        *ctx.cpos = pos;
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

fn handle_waiting_op_find(
    key: KeyEvent,
    op: Op,
    kind: FindKind,
    ctx: &mut VimContext<'_>,
) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    if let KeyCode::Char(ch) = key.code {
        let count = ctx.vim_state.count1.unwrap_or(1) * ctx.vim_state.count2.unwrap_or(1);
        let n = ctx.vim_state.effective_count();
        ctx.vim_state.last_find = Some((kind, ch));
        let origin = *ctx.cpos;
        let raw_kind = match kind {
            FindKind::ForwardTill => FindKind::Forward,
            FindKind::BackwardTill => FindKind::Backward,
            other => other,
        };
        let mut pos = origin;
        for _ in 0..n {
            if let Some(p) = find_char(ctx.buf.as_str(), pos, raw_kind, ch) {
                pos = p;
            }
        }
        if pos != origin {
            let (start, end) = match kind {
                FindKind::Forward => (*ctx.cpos, advance_chars(ctx.buf.as_str(), pos, 1)),
                FindKind::ForwardTill => (*ctx.cpos, pos),
                FindKind::Backward => (pos, *ctx.cpos),
                FindKind::BackwardTill => (advance_chars(ctx.buf.as_str(), pos, 1), *ctx.cpos),
            };
            if start < end {
                if op != Op::Yank {
                    ctx.vim_state.record_change(RepeatCommand::OpFind {
                        op,
                        kind,
                        target: ch,
                        count,
                    });
                }
                return apply_charwise_op(op, ctx, start, end);
            }
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

fn handle_waiting_g(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    let action = match key.code {
        KeyCode::Char('g') => {
            if let Some(n) = ctx.vim_state.count1.take() {
                *ctx.cpos = goto_line(ctx.buf.as_str(), n.saturating_sub(1));
            } else {
                *ctx.cpos = 0;
            }
            Action::Consumed
        }
        _ => Action::Consumed,
    };
    ctx.vim_state.count1 = None;
    ctx.vim_state.count2 = None;
    action
}

fn handle_waiting_op_g(key: KeyEvent, op: Op, ctx: &mut VimContext<'_>) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    if let KeyCode::Char('g') = key.code {
        let target = if let Some(n) = ctx.vim_state.count1.take() {
            goto_line(ctx.buf.as_str(), n.saturating_sub(1))
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
            let ls = line_start(ctx.buf.as_str(), s);
            let le = line_end(ctx.buf.as_str(), e);
            ctx.vim_state.reset_pending();
            return apply_linewise_op(op, ctx, ls, le);
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

fn handle_waiting_textobj(key: KeyEvent, op: Op, inner: bool, ctx: &mut VimContext<'_>) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    if let KeyCode::Char(c) = key.code {
        if let Some((start, end)) = text_object(ctx.buf.as_str(), *ctx.cpos, inner, c) {
            let count = ctx.vim_state.count1.unwrap_or(1) * ctx.vim_state.count2.unwrap_or(1);
            let n = ctx.vim_state.effective_count();
            let _ = n;
            if op != Op::Yank {
                ctx.vim_state.record_change(RepeatCommand::TextObject {
                    op,
                    inner,
                    object: c,
                    count,
                });
            }
            return apply_charwise_op(op, ctx, start, end);
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

fn handle_waiting_surround_motion(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    if let KeyCode::Char(c) = key.code {
        if c == 'i' || c == 'a' {
            ctx.vim_state.sub = SubState::WaitingSurroundTextObj(c == 'i');
            return Action::Consumed;
        }
        if c == 's' {
            let (start, end) = current_line_content_range(ctx.buf.as_str(), *ctx.cpos);
            ctx.vim_state.sub = SubState::WaitingSurroundChar(start, end);
            ctx.vim_state.reset_counts();
            return Action::Consumed;
        }
    }

    match resolve_motion_range(key, ctx, None) {
        MotionRange::Charwise(start, end) | MotionRange::Linewise(start, end) => {
            ctx.vim_state.sub = SubState::WaitingSurroundChar(start, end);
            Action::Consumed
        }
        MotionRange::Pending(_) | MotionRange::None => {
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
    }
}

fn handle_waiting_surround_textobj(key: KeyEvent, inner: bool, ctx: &mut VimContext<'_>) -> Action {
    if let KeyCode::Char(c) = key.code {
        if let Some((start, end)) = text_object(ctx.buf.as_str(), *ctx.cpos, inner, c) {
            ctx.vim_state.sub = SubState::WaitingSurroundChar(start, end);
            ctx.vim_state.reset_counts();
            return Action::Consumed;
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

fn handle_waiting_surround_char(
    key: KeyEvent,
    start: usize,
    end: usize,
    ctx: &mut VimContext<'_>,
) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    if let KeyCode::Char(c) = key.code {
        if let Some((open, close)) = surround_pair(c) {
            add_surround(ctx, start, end, open, close);
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

fn handle_waiting_delete_surround(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    if let KeyCode::Char(c) = key.code {
        if delete_surround(ctx, c) {
            ctx.vim_state
                .record_change(RepeatCommand::DeleteSurround { target: c });
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

fn handle_waiting_change_surround_target(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    if let KeyCode::Char(c) = key.code {
        if surrounding_delimiters(ctx.buf.as_str(), *ctx.cpos, c).is_some() {
            ctx.vim_state.sub = SubState::WaitingChangeSurroundReplacement(c);
            return Action::Consumed;
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

fn handle_waiting_change_surround_replacement(
    key: KeyEvent,
    target: char,
    ctx: &mut VimContext<'_>,
) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    if let KeyCode::Char(replacement) = key.code {
        if change_surround(ctx, target, replacement) {
            ctx.vim_state.record_change(RepeatCommand::ChangeSurround {
                target,
                replacement,
            });
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

fn surround_pair(c: char) -> Option<(&'static str, &'static str)> {
    match c {
        '"' => Some(("\"", "\"")),
        '\'' => Some(("'", "'")),
        '`' => Some(("`", "`")),
        '(' => Some(("( ", " )")),
        ')' | 'b' => Some(("(", ")")),
        '[' => Some(("[ ", " ]")),
        ']' | 'r' => Some(("[", "]")),
        '{' => Some(("{ ", " }")),
        '}' | 'B' => Some(("{", "}")),
        '<' => Some(("< ", " >")),
        '>' | 'a' => Some(("<", ">")),
        'q' => Some(("\"", "\"")),
        _ => None,
    }
}

fn add_surround(ctx: &mut VimContext<'_>, start: usize, end: usize, open: &str, close: &str) {
    let start = smelt_buffer::text::snap_grapheme(ctx.buf.as_str(), start.min(ctx.buf.len()));
    let end = smelt_buffer::text::snap_grapheme(ctx.buf.as_str(), end.min(ctx.buf.len()));
    if start > end {
        return;
    }
    ctx.save_undo();
    ctx.insert_str(end, close);
    ctx.insert_str(start, open);
    *ctx.cpos = start + open.len();
    clamp_normal(ctx.buf.as_str(), ctx.cpos);
}

fn delete_surround(ctx: &mut VimContext<'_>, kind: char) -> bool {
    let Some(delims) = surrounding_delimiters(ctx.buf.as_str(), *ctx.cpos, kind) else {
        return false;
    };
    ctx.save_undo();
    ctx.delete_range(delims.close_start, delims.close_end);
    ctx.delete_range(delims.open_start, delims.open_end);
    *ctx.cpos = delims.open_start.min(ctx.buf.len());
    clamp_normal(ctx.buf.as_str(), ctx.cpos);
    true
}

fn change_surround(ctx: &mut VimContext<'_>, target: char, replacement: char) -> bool {
    let Some((open, close)) = surround_pair(replacement) else {
        return false;
    };
    let Some(delims) = surrounding_delimiters(ctx.buf.as_str(), *ctx.cpos, target) else {
        return false;
    };
    ctx.save_undo();
    ctx.replace_range(delims.close_start, delims.close_end, close);
    ctx.replace_range(delims.open_start, delims.open_end, open);
    *ctx.cpos = delims.open_start + open.len();
    clamp_normal(ctx.buf.as_str(), ctx.cpos);
    true
}

#[derive(Clone, Copy, Debug)]
enum MotionRange {
    Charwise(usize, usize),
    Linewise(usize, usize),
    Pending(SubState),
    None,
}

fn ordered_range(a: usize, b: usize) -> Option<(usize, usize)> {
    let (start, end) = if b < a { (b, a) } else { (a, b) };
    (start < end).then_some((start, end))
}

fn resolve_motion_range(key: KeyEvent, ctx: &mut VimContext<'_>, op: Option<Op>) -> MotionRange {
    let n = ctx.vim_state.effective_count();
    let origin = *ctx.cpos;
    let target = match key.code {
        KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
            let mut p = origin;
            for _ in 0..n {
                p = move_left(ctx.buf.as_str(), p);
            }
            p
        }
        KeyCode::Char('l') | KeyCode::Right => {
            let mut p = origin;
            for _ in 0..n {
                p = move_right_inclusive(ctx.buf.as_str(), p);
            }
            p
        }
        KeyCode::Char('j') => {
            let mut p = origin;
            for _ in 0..n {
                p = move_down(ctx.buf.as_str(), p);
            }
            let (start, end) = if p < origin { (p, origin) } else { (origin, p) };
            return MotionRange::Linewise(
                line_start(ctx.buf.as_str(), start),
                line_end(ctx.buf.as_str(), end),
            );
        }
        KeyCode::Char('k') => {
            let mut p = origin;
            for _ in 0..n {
                p = move_up(ctx.buf.as_str(), p);
            }
            let (start, end) = if p < origin { (p, origin) } else { (origin, p) };
            return MotionRange::Linewise(
                line_start(ctx.buf.as_str(), start),
                line_end(ctx.buf.as_str(), end),
            );
        }
        KeyCode::Char('w') => {
            let mut p = origin;
            let use_end = op == Some(Op::Change)
                && p < ctx.buf.len()
                && char_class(
                    ctx.buf.as_str()[p..].chars().next().unwrap(),
                    CharClass::Word,
                ) != 0;
            for _ in 0..n {
                if use_end {
                    p = word_end_pos(ctx.buf.as_str(), p, CharClass::Word);
                    p = advance_chars(ctx.buf.as_str(), p, 1);
                } else {
                    p = word_forward_pos(ctx.buf.as_str(), p, CharClass::Word);
                }
            }
            p
        }
        KeyCode::Char('W') => {
            let mut p = origin;
            let use_end = op == Some(Op::Change)
                && p < ctx.buf.len()
                && char_class(
                    ctx.buf.as_str()[p..].chars().next().unwrap(),
                    CharClass::WORD,
                ) != 0;
            for _ in 0..n {
                if use_end {
                    p = word_end_pos(ctx.buf.as_str(), p, CharClass::WORD);
                    p = advance_chars(ctx.buf.as_str(), p, 1);
                } else {
                    p = word_forward_pos(ctx.buf.as_str(), p, CharClass::WORD);
                }
            }
            p
        }
        KeyCode::Char('b') => {
            let mut p = origin;
            for _ in 0..n {
                p = word_backward_pos(ctx.buf.as_str(), p, CharClass::Word);
            }
            p
        }
        KeyCode::Char('B') => {
            let mut p = origin;
            for _ in 0..n {
                p = word_backward_pos(ctx.buf.as_str(), p, CharClass::WORD);
            }
            p
        }
        KeyCode::Char('e') => {
            let mut p = origin;
            for _ in 0..n {
                p = word_end_pos(ctx.buf.as_str(), p, CharClass::Word);
            }
            advance_chars(ctx.buf.as_str(), p, 1)
        }
        KeyCode::Char('E') => {
            let mut p = origin;
            for _ in 0..n {
                p = word_end_pos(ctx.buf.as_str(), p, CharClass::WORD);
            }
            advance_chars(ctx.buf.as_str(), p, 1)
        }
        KeyCode::Char('0') | KeyCode::Home => line_start(ctx.buf.as_str(), origin),
        KeyCode::Char('^' | '_') => first_non_blank(ctx.buf.as_str(), origin),
        KeyCode::Char('$') | KeyCode::End => line_end(ctx.buf.as_str(), origin),
        KeyCode::Char('%') => {
            let Some(t) = find_matching_bracket(ctx.buf.as_str(), origin) else {
                return MotionRange::None;
            };
            let lo = origin.min(t);
            let hi = advance_chars(ctx.buf.as_str(), origin.max(t), 1);
            return ordered_range(lo, hi)
                .map(|(start, end)| MotionRange::Charwise(start, end))
                .unwrap_or(MotionRange::None);
        }
        KeyCode::Char('G') => {
            let (start, end) = if ctx.buf.len() < origin {
                (ctx.buf.len(), origin)
            } else {
                (origin, ctx.buf.len())
            };
            return MotionRange::Linewise(
                line_start(ctx.buf.as_str(), start),
                line_end(ctx.buf.as_str(), end),
            );
        }
        KeyCode::Char('g') => {
            return op
                .map(|op| MotionRange::Pending(SubState::WaitingOpG(op)))
                .unwrap_or(MotionRange::None);
        }
        KeyCode::Char('f') => {
            return op
                .map(|op| MotionRange::Pending(SubState::WaitingOpFind(op, FindKind::Forward)))
                .unwrap_or(MotionRange::None);
        }
        KeyCode::Char('F') => {
            return op
                .map(|op| MotionRange::Pending(SubState::WaitingOpFind(op, FindKind::Backward)))
                .unwrap_or(MotionRange::None);
        }
        KeyCode::Char('t') => {
            return op
                .map(|op| MotionRange::Pending(SubState::WaitingOpFind(op, FindKind::ForwardTill)))
                .unwrap_or(MotionRange::None);
        }
        KeyCode::Char('T') => {
            return op
                .map(|op| MotionRange::Pending(SubState::WaitingOpFind(op, FindKind::BackwardTill)))
                .unwrap_or(MotionRange::None);
        }
        _ => return MotionRange::None,
    };

    ordered_range(origin, target)
        .map(|(start, end)| MotionRange::Charwise(start, end))
        .unwrap_or(MotionRange::None)
}

/// Operator-pending motion dispatch.
fn execute_op_motion(key: KeyEvent, op: Op, ctx: &mut VimContext<'_>) -> Action {
    match resolve_motion_range(key, ctx, Some(op)) {
        MotionRange::Charwise(start, end) => apply_charwise_op(op, ctx, start, end),
        MotionRange::Linewise(start, end) => apply_linewise_op(op, ctx, start, end),
        MotionRange::Pending(sub) => {
            ctx.vim_state.sub = sub;
            Action::Consumed
        }
        MotionRange::None => Action::Consumed,
    }
}

fn execute_linewise_op(op: Op, ctx: &mut VimContext<'_>) -> Action {
    let n = ctx.vim_state.effective_count();
    ctx.vim_state.reset_counts();
    ctx.vim_state.sub = SubState::Ready;

    let start = line_start(ctx.buf.as_str(), *ctx.cpos);
    let mut end_pos = *ctx.cpos;
    for _ in 1..n {
        let next = line_end(ctx.buf.as_str(), end_pos);
        if next < ctx.buf.len() {
            end_pos = next + 1;
        }
    }
    let end = line_end(ctx.buf.as_str(), end_pos);
    apply_linewise_op(op, ctx, start, end)
}

/// Apply a charwise operator over the byte range [start..end).
fn apply_charwise_op(op: Op, ctx: &mut VimContext<'_>, start: usize, end: usize) -> Action {
    match op {
        Op::Delete => {
            ctx.save_undo();
            ctx.yank_range(start, end, false);
            ctx.delete_range(start, end);
            *ctx.cpos = start;
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
        }
        Op::Change => {
            ctx.save_undo();
            ctx.yank_range(start, end, false);
            ctx.delete_range(start, end);
            *ctx.cpos = start;
            enter_insert_mode(ctx);
            ctx.vim_state.reset_counts();
            return Action::Consumed;
        }
        Op::Yank => {
            ctx.yank_range(start, end, false);
            ctx.clipboard.kill_ring.mark_yanked(ctx.now);
            *ctx.cpos = start;
        }
    }
    Action::Consumed
}

/// Apply linewise operator over [start..end] (line boundaries).
fn apply_linewise_op(op: Op, ctx: &mut VimContext<'_>, start: usize, end: usize) -> Action {
    let mut s = start;
    let mut e = end;
    let mut has_trailing_nl = false;
    if e < ctx.buf.len() && ctx.buf.as_bytes()[e] == b'\n' {
        e += 1;
        has_trailing_nl = true;
    } else if e < ctx.buf.len() {
        e = line_end(ctx.buf.as_str(), e);
        if e < ctx.buf.len() {
            e += 1;
            has_trailing_nl = true;
        }
    }
    // No trailing newline at buffer end: include the preceding newline instead.
    if !has_trailing_nl && e >= ctx.buf.len() && s > 0 {
        s -= 1;
    }

    match op {
        Op::Delete => {
            ctx.save_undo();
            ctx.yank_range(s, e, true);
            ctx.delete_range(s, e);
            *ctx.cpos = s.min(ctx.buf.len());
            if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
                *ctx.cpos = first_non_blank_at(ctx.buf.as_str(), *ctx.cpos);
            }
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
        }
        Op::Change => {
            ctx.save_undo();
            let content_start = first_non_blank_at(ctx.buf.as_str(), s);
            let content_end = line_end(ctx.buf.as_str(), e.saturating_sub(1).max(s));
            ctx.yank_range(content_start, content_end, true);
            ctx.delete_range(content_start, content_end);
            *ctx.cpos = content_start;
            enter_insert_mode(ctx);
            return Action::Consumed;
        }
        Op::Yank => {
            // Linewise yank does not reposition the cursor (vim default).
            // Use the original line-content range (no trailing newline) so
            // linewise paste can consistently prepend/append its own newline.
            ctx.yank_range(start, end, true);
            ctx.clipboard.kill_ring.mark_yanked(ctx.now);
        }
    }
    Action::Consumed
}

// ── Mode transitions ────────────────────────────────────────────────

fn enter_insert_mode(ctx: &mut VimContext<'_>) {
    *ctx.mode = VimMode::Insert;
    ctx.vim_state.sub = SubState::Ready;
}

fn exit_visual(ctx: &mut VimContext<'_>) {
    *ctx.mode = VimMode::Normal;
    ctx.vim_state.reset_pending();
}

fn enter_normal(ctx: &mut VimContext<'_>) {
    *ctx.mode = VimMode::Normal;
    ctx.vim_state.sub = SubState::Ready;
    ctx.vim_state.reset_counts();
    // Leaving insert mode moves cursor left one, unless at start of line.
    let sol = line_start(ctx.buf.as_str(), *ctx.cpos);
    if *ctx.cpos > sol {
        *ctx.cpos = prev_grapheme_boundary(ctx.buf.as_str(), *ctx.cpos);
    }
    clamp_normal(ctx.buf.as_str(), ctx.cpos);
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
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

    #[test]
    fn pending_input_renders_counts_and_substates() {
        let mut state = VimWindowState::default();
        assert_eq!(state.pending_input(), None);

        state.count1 = Some(3);
        assert_eq!(state.pending_input().as_deref(), Some("3"));

        state.sub = SubState::WaitingOp(Op::Delete);
        assert_eq!(state.pending_input().as_deref(), Some("3d"));

        state.count2 = Some(2);
        assert_eq!(state.pending_input().as_deref(), Some("3d2"));

        state.sub = SubState::WaitingOpFind(Op::Delete, FindKind::ForwardTill);
        assert_eq!(state.pending_input().as_deref(), Some("3d2t"));
    }

    #[test]
    fn pending_input_renders_motion_prefixes() {
        let cases = [
            (SubState::WaitingG, "g"),
            (SubState::WaitingZ, "z"),
            (SubState::WaitingR, "r"),
            (SubState::WaitingFind(FindKind::Backward), "F"),
            (SubState::WaitingTextObj(Op::Change, true), "ci"),
            (SubState::WaitingVisualTextObj(false), "a"),
        ];

        for (sub, expected) in cases {
            let state = VimWindowState {
                sub,
                ..Default::default()
            };
            assert_eq!(state.pending_input().as_deref(), Some(expected));
        }
    }

    struct MemSinkInner {
        text: Option<String>,
        writes: usize,
    }
    struct MemSink(std::rc::Rc<std::cell::RefCell<MemSinkInner>>);
    impl smelt_buffer::clipboard::Sink for MemSink {
        fn read(&mut self) -> Option<String> {
            self.0.borrow().text.clone()
        }
        fn write(&mut self, text: &str) -> Result<(), String> {
            let mut inner = self.0.borrow_mut();
            inner.text = Some(text.to_string());
            inner.writes += 1;
            Ok(())
        }
    }
    // SAFETY: tests are single-threaded; Rc stays local to the test thread.
    unsafe impl Send for MemSink {}

    fn mem_sink(initial: Option<&str>) -> std::rc::Rc<std::cell::RefCell<MemSinkInner>> {
        std::rc::Rc::new(std::cell::RefCell::new(MemSinkInner {
            text: initial.map(str::to_string),
            writes: 0,
        }))
    }

    /// Owns the cross-call state (clipboard + kill ring + undo history +
    /// mode + curswant + per-window vim state) that vim borrows.
    /// `mode` mirrors the TuiApp-owned single-global VimMode in production
    /// code; tests own one locally. `curswant` and `vim_state` mirror
    /// the per-Window state that production carries on `ui::Window`.
    struct TestHarness {
        buf: String,
        cpos: usize,
        attachments: Vec<smelt_buffer::attachment::AttachmentId>,
        clipboard: Clipboard,
        history: UndoHistory,
        mode: VimMode,
        curswant: Option<usize>,
        vim_state: VimWindowState,
    }

    impl TestHarness {
        fn new(text: &str) -> Self {
            Self::with_clipboard(text, Clipboard::null())
        }

        fn with_clipboard(text: &str, clipboard: Clipboard) -> Self {
            Self {
                buf: text.to_string(),
                cpos: 0,
                attachments: Vec::new(),
                clipboard,
                history: UndoHistory::new(None),
                mode: VimMode::Normal,
                curswant: None,
                vim_state: VimWindowState::default(),
            }
        }

        fn handle(&mut self, k: KeyEvent) -> Action {
            let mut ctx = VimContext {
                buf: AttachedTextMut::new(&mut self.buf, &mut self.attachments),
                cpos: &mut self.cpos,
                history: &mut self.history,
                clipboard: &mut self.clipboard,
                mode: &mut self.mode,
                curswant: &mut self.curswant,
                vim_state: &mut self.vim_state,
                now: std::time::Instant::now(),
            };
            handle_key(k, &mut ctx)
        }
    }

    #[test]
    fn document_view_gf_opens_action() {
        let mut mode = VimMode::Normal;
        let mut state = VimWindowState::default();

        assert_eq!(
            handle_viewer_key(key('g'), &mut mode, &mut state),
            DocumentKeyResult::Consumed
        );
        assert_eq!(
            handle_viewer_key(key('f'), &mut mode, &mut state),
            DocumentKeyResult::Command(DocumentCommand::OpenAction)
        );
    }

    #[test]
    fn document_view_visual_text_object_prefix_returns_command() {
        let mut mode = VimMode::Normal;
        let mut state = VimWindowState::default();

        assert_eq!(
            handle_viewer_key(key('v'), &mut mode, &mut state),
            DocumentKeyResult::Command(DocumentCommand::StartVisual)
        );
        assert_eq!(mode, VimMode::Visual);
        assert_eq!(
            handle_viewer_key(key('i'), &mut mode, &mut state),
            DocumentKeyResult::Consumed
        );
        assert_eq!(state.pending_input().as_deref(), Some("i"));
        assert_eq!(
            handle_viewer_key(key('p'), &mut mode, &mut state),
            DocumentKeyResult::Command(DocumentCommand::TextObject(
                DocumentTextObject::new(true, 'p').unwrap()
            ))
        );
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
    fn dollar_lands_on_last_character_in_normal_mode() {
        let mut h = TestHarness::new("abc");
        h.handle(key('$'));
        assert_eq!(h.cpos, 2);
        assert_eq!(h.mode, VimMode::Normal);
    }

    #[test]
    fn capital_a_enters_insert_after_last_character_then_esc_returns_to_last_character() {
        let mut h = TestHarness::new("abc");
        h.handle(key('A'));
        assert_eq!(h.cpos, 3);
        assert_eq!(h.mode, VimMode::Insert);

        h.handle(KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        });
        assert_eq!(h.cpos, 2);
        assert_eq!(h.mode, VimMode::Normal);
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
    fn dot_repeats_delete_word_operator() {
        let mut h = TestHarness::new("one two three");
        h.handle(key('d'));
        h.handle(key('w'));
        assert_eq!(h.buf, "two three");

        h.handle(key('.'));
        assert_eq!(h.buf, "three");
        assert_eq!(h.cpos, 0);
    }

    #[test]
    fn dot_count_overrides_repeated_operator_count() {
        let mut h = TestHarness::new("one two three four");
        h.handle(key('d'));
        h.handle(key('w'));
        assert_eq!(h.buf, "two three four");

        h.handle(key('2'));
        h.handle(key('.'));
        assert_eq!(h.buf, "four");
    }

    #[test]
    fn dot_repeats_find_operator() {
        let mut h = TestHarness::new("abc def ghi");
        h.handle(key('d'));
        h.handle(key('f'));
        h.handle(key(' '));
        assert_eq!(h.buf, "def ghi");

        h.handle(key('.'));
        assert_eq!(h.buf, "ghi");
    }

    #[test]
    fn dot_repeats_replace_char() {
        let mut h = TestHarness::new("abc");
        h.handle(key('r'));
        h.handle(key('x'));
        assert_eq!(h.buf, "xbc");

        h.handle(key('l'));
        h.handle(key('.'));
        assert_eq!(h.buf, "xxc");
    }

    #[test]
    fn dot_repeats_linewise_delete() {
        let mut h = TestHarness::new("one\ntwo\nthree");
        h.handle(key('d'));
        h.handle(key('d'));
        assert_eq!(h.buf, "two\nthree");

        h.handle(key('.'));
        assert_eq!(h.buf, "three");
    }

    #[test]
    fn dot_repeats_direct_delete_and_toggle() {
        let mut h = TestHarness::new("abCD");
        h.handle(key('x'));
        assert_eq!(h.buf, "bCD");
        h.handle(key('.'));
        assert_eq!(h.buf, "CD");

        h.handle(key('~'));
        assert_eq!(h.buf, "cD");
        h.handle(key('.'));
        assert_eq!(h.buf, "cd");
    }

    #[test]
    fn dot_repeats_end_of_line_delete() {
        let mut h = TestHarness::new("one two\nthree four");
        h.handle(key('w'));
        h.handle(key('D'));
        assert_eq!(h.buf, "one \nthree four");

        h.handle(key('j'));
        h.handle(key('w'));
        h.handle(key('.'));
        assert_eq!(h.buf, "one \nthree ");
    }

    #[test]
    fn dot_repeats_substitute_without_inserted_text() {
        let mut h = TestHarness::new("abc");
        h.handle(key('s'));
        assert_eq!(h.buf, "bc");
        assert_eq!(h.mode, VimMode::Insert);
        h.handle(KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        });

        h.handle(key('.'));
        assert_eq!(h.buf, "c");
    }

    #[test]
    fn dot_repeats_join_lines() {
        let mut h = TestHarness::new("one\ntwo\nthree");
        h.handle(key('J'));
        assert_eq!(h.buf, "one two\nthree");

        h.handle(key('.'));
        assert_eq!(h.buf, "one two three");
    }

    #[test]
    fn dot_repeats_paste() {
        let mut h = TestHarness::new("ab");
        h.clipboard
            .kill_ring
            .set_with_linewise("X".to_string(), false);
        h.handle(key('p'));
        assert_eq!(h.buf, "aXb");

        h.handle(key('.'));
        assert_eq!(h.buf, "aXXb");
    }

    #[test]
    fn dot_repeats_text_object_operator() {
        let mut h = TestHarness::new("one two three");
        h.cpos = 4;
        h.handle(key('d'));
        h.handle(key('i'));
        h.handle(key('w'));
        assert_eq!(h.buf, "one  three");

        h.cpos = 5;
        h.handle(key('.'));
        assert_eq!(h.buf, "one  ");
    }

    #[test]
    fn dot_repeats_surround_delete_and_change() {
        let mut h = TestHarness::new("(one) (two)");
        h.cpos = 1;
        h.handle(key('d'));
        h.handle(key('s'));
        h.handle(key(')'));
        assert_eq!(h.buf, "one (two)");

        h.cpos = 5;
        h.handle(key('.'));
        assert_eq!(h.buf, "one two");

        let mut h = TestHarness::new("(one) (two)");
        h.cpos = 1;
        h.handle(key('c'));
        h.handle(key('s'));
        h.handle(key(')'));
        h.handle(key(']'));
        assert_eq!(h.buf, "[one] (two)");

        h.cpos = 7;
        h.handle(key('.'));
        assert_eq!(h.buf, "[one] [two]");
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
        assert_eq!(h.mode, VimMode::Insert);
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
    fn ysiw_adds_quotes_around_inner_word() {
        let mut h = TestHarness::new("hello world");
        h.cpos = 6;
        h.handle(key('y'));
        h.handle(key('s'));
        h.handle(key('i'));
        h.handle(key('w'));
        h.handle(key('"'));
        assert_eq!(h.buf, "hello \"world\"");
        assert_eq!(h.mode, VimMode::Normal);
    }

    #[test]
    fn ys_motion_adds_quotes_around_motion_range() {
        let mut h = TestHarness::new("make strings");
        h.handle(key('y'));
        h.handle(key('s'));
        h.handle(key('$'));
        h.handle(key('"'));
        assert_eq!(h.buf, "\"make strings\"");
    }

    #[test]
    fn ds_quote_deletes_surrounding_string_quotes() {
        let mut h = TestHarness::new("say \"hello\" now");
        h.cpos = 6;
        h.handle(key('d'));
        h.handle(key('s'));
        h.handle(key('"'));
        assert_eq!(h.buf, "say hello now");
        assert_eq!(h.cpos, 4);
    }

    #[test]
    fn cs_quote_changes_surrounding_string_quotes() {
        let mut h = TestHarness::new("say 'hello' now");
        h.cpos = 6;
        h.handle(key('c'));
        h.handle(key('s'));
        h.handle(key('\''));
        h.handle(key('"'));
        assert_eq!(h.buf, "say \"hello\" now");
    }

    #[test]
    fn surround_edits_remove_complete_delimiter_graphemes() {
        let mut deleted = TestHarness::new("say \"\u{301}hello\"\u{301} now");
        deleted.cpos = "say \"\u{301}".len();
        deleted.handle(key('d'));
        deleted.handle(key('s'));
        deleted.handle(key('"'));
        assert_eq!(deleted.buf, "say hello now");
        assert_eq!(
            smelt_buffer::text::snap_grapheme(&deleted.buf, deleted.cpos),
            deleted.cpos
        );

        let mut changed = TestHarness::new("say '\u{301}hello'\u{301} now");
        changed.cpos = "say '\u{301}".len();
        changed.handle(key('c'));
        changed.handle(key('s'));
        changed.handle(key('\''));
        changed.handle(key('"'));
        assert_eq!(changed.buf, "say \"hello\" now");
        assert_eq!(
            smelt_buffer::text::snap_grapheme(&changed.buf, changed.cpos),
            changed.cpos
        );
    }

    #[test]
    fn q_alias_deletes_nearest_quote_surrounding() {
        let mut h = TestHarness::new(r#"say `hello` and 'bye'"#);
        h.cpos = 5;
        h.handle(key('d'));
        h.handle(key('s'));
        h.handle(key('q'));
        assert_eq!(h.buf, "say hello and 'bye'");
    }

    #[test]
    fn dis_deletes_inner_string_alias() {
        let mut h = TestHarness::new("say \"hello\" now");
        h.cpos = 6;
        h.handle(key('d'));
        h.handle(key('i'));
        h.handle(key('s'));
        assert_eq!(h.buf, "say \"\" now");
    }

    #[test]
    fn tag_text_object_works_with_operators() {
        let mut h = TestHarness::new("x <b>bold</b> y");
        h.cpos = 5;
        h.handle(key('d'));
        h.handle(key('i'));
        h.handle(key('t'));
        assert_eq!(h.buf, "x <b></b> y");
    }

    #[test]
    fn dst_deletes_surrounding_tags() {
        let mut h = TestHarness::new("x <b>bold</b> y");
        h.cpos = 5;
        h.handle(key('d'));
        h.handle(key('s'));
        h.handle(key('t'));
        assert_eq!(h.buf, "x bold y");
    }

    #[test]
    fn cst_changes_tags_to_quotes() {
        let mut h = TestHarness::new("x <b>bold</b> y");
        h.cpos = 5;
        h.handle(key('c'));
        h.handle(key('s'));
        h.handle(key('t'));
        h.handle(key('"'));
        assert_eq!(h.buf, "x \"bold\" y");
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
    fn dip_deletes_the_paragraph_around_the_cursor() {
        let mut h = TestHarness::new("a\nb\nc\n\nd\n");
        h.cpos = 2;
        h.handle(key('d'));
        h.handle(key('i'));
        h.handle(key('p'));
        assert_eq!(h.buf, "\nd\n");
    }

    #[test]
    fn dap_also_consumes_the_trailing_blank_lines() {
        let mut h = TestHarness::new("a\nb\n\n\nc\n");
        h.cpos = 0;
        h.handle(key('d'));
        h.handle(key('a'));
        h.handle(key('p'));
        assert_eq!(h.buf, "c\n");
    }

    #[test]
    fn vip_deletes_the_inner_paragraph() {
        let mut h = TestHarness::new("a\nb\n\nc\nd\n\ne\n");
        h.cpos = 5;
        h.handle(key('v'));
        h.handle(key('i'));
        h.handle(key('p'));
        assert_eq!(h.mode, VimMode::VisualLine);
        assert_eq!(h.cpos, h.buf.find('d').unwrap());
        h.handle(key('d'));
        assert_eq!(h.buf, "a\nb\n\n\ne\n");
    }

    #[test]
    fn vap_deletes_the_paragraph_and_trailing_blank() {
        let mut h = TestHarness::new("a\nb\n\nc\nd\n\ne\n");
        h.cpos = 5;
        h.handle(key('v'));
        h.handle(key('a'));
        h.handle(key('p'));
        assert_eq!(h.mode, VimMode::VisualLine);
        assert_eq!(h.cpos, h.buf.find("\ne").unwrap());
        h.handle(key('d'));
        assert_eq!(h.buf, "a\nb\n\ne\n");
    }

    #[test]
    fn test_paste() {
        let mut h = TestHarness::new("hello");
        h.clipboard
            .kill_ring
            .set_with_linewise(" world".to_string(), false);
        h.cpos = 4;
        h.handle(key('p'));
        assert_eq!(h.buf, "hello world");
    }

    #[test]
    fn paste_cursor_stays_on_a_grapheme_boundary_after_resegmentation() {
        let mut after = TestHarness::new("a");
        after
            .clipboard
            .kill_ring
            .set_with_linewise("\u{301}".to_string(), false);
        after.handle(key('p'));
        assert_eq!(after.buf, "a\u{301}");
        assert_eq!(after.cpos, 0);

        let mut before = TestHarness::new("🇨🇦");
        before
            .clipboard
            .kill_ring
            .set_with_linewise("🇧".to_string(), false);
        before.handle(key('P'));
        assert_eq!(before.buf, "🇧🇨🇦");
        assert_eq!(before.cpos, 0);
        assert_eq!(
            smelt_buffer::text::snap_grapheme(&before.buf, before.cpos),
            before.cpos
        );
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
    fn replace_and_tilde_operate_on_complete_graphemes() {
        for grapheme in ["e\u{301}", "👩\u{200d}💻", "9\u{fe0f}", "🇨🇦"] {
            let mut replace = TestHarness::new(&format!("{grapheme}z"));
            replace.handle(key('r'));
            replace.handle(key('x'));
            assert_eq!(replace.buf, "xz", "{grapheme:?}");
        }

        let mut toggle = TestHarness::new("e\u{301}x");
        toggle.handle(key('~'));
        assert_eq!(toggle.buf, "E\u{301}x");
        assert_eq!(toggle.cpos, "E\u{301}".len());

        let mut counted = TestHarness::new("ßx");
        counted.handle(key('2'));
        counted.handle(key('~'));
        assert_eq!(counted.buf, "SSX");
        assert_eq!(counted.cpos, 2);
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
        assert_eq!(h.mode, VimMode::Insert);
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
        assert_eq!(h.mode, VimMode::Insert);

        // Esc → normal mode, cursor stays on empty trailing line.
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        h.handle(esc);
        assert_eq!(h.mode, VimMode::Normal);
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
        h.mode = VimMode::Insert;
        h.cpos = 5;
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        h.handle(esc);
        assert_eq!(h.cpos, 4);
        assert_eq!(h.mode, VimMode::Normal);
    }

    #[test]
    fn test_esc_at_line_start_stays() {
        let mut h = TestHarness::new("hello");
        h.mode = VimMode::Insert;
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
    fn yank_stages_in_kill_ring_with_source_range() {
        // Vim deliberately does NOT push to the system clipboard - the host
        // observes `kill_ring.yank_tick()` and pushes the rendered form via
        // `Buffer::sync_clipboard_from_kill_ring`, so the prompt/transcript
        // copier can transform attachment markers / fold markers before paste.
        let inner = mem_sink(None);
        let clipboard = Clipboard::new(Box::new(MemSink(inner.clone())));
        let mut h = TestHarness::with_clipboard("hello world", clipboard);
        let tick_before = h.clipboard.kill_ring.yank_tick();
        h.handle(key('y'));
        h.handle(key('w'));
        assert_eq!(h.clipboard.kill_ring.current(), "hello ");
        assert_eq!(h.clipboard.kill_ring.source_range(), Some((0, 6)));
        assert!(h.clipboard.kill_ring.yank_tick() > tick_before);
        // System clipboard was not touched by vim.
        let s = inner.borrow();
        assert_eq!(s.writes, 0);
    }

    #[test]
    fn paste_prefers_external_clipboard_when_updated() {
        // External tool put "pasted" on the clipboard. `p` should use
        // that instead of whatever is in the kill ring.
        let inner = mem_sink(Some("pasted"));
        let clipboard = Clipboard::new(Box::new(MemSink(inner)));
        let mut h = TestHarness::with_clipboard("abc", clipboard);
        h.clipboard
            .kill_ring
            .set_with_linewise("stale".to_string(), false);
        // Move cursor to end so `p` inserts after.
        h.handle(key('$'));
        h.handle(key('p'));
        assert_eq!(h.buf, "abcpasted");
    }

    #[test]
    fn paste_keeps_kill_ring_when_clipboard_matches_last_write() {
        // Kill ring was the last writer - its linewise flag matters
        // for `p` placement, so we must not overwrite charwise.
        let inner = mem_sink(Some("line\n"));
        let clipboard = Clipboard::new(Box::new(MemSink(inner)));
        let mut h = TestHarness::with_clipboard("abc\n", clipboard);
        // Simulate a prior `yy`: linewise + clipboard mirror.
        h.clipboard
            .kill_ring
            .set_with_linewise("line\n".to_string(), true);
        h.clipboard
            .kill_ring
            .record_clipboard_write("line\n".to_string());
        // Position on first line, then `p` - linewise pastes below.
        h.handle(key('p'));
        assert!(h.buf.contains("line\n"));
        assert!(h.clipboard.kill_ring.is_linewise());
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
        assert_eq!(h.clipboard.kill_ring.current(), "hello world");
    }

    #[test]
    fn test_yy_p_does_not_add_extra_newline() {
        // Regression: `yy` used to include a trailing newline in the kill
        // ring; `p` then prepended its own, producing a blank line.
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('y'));
        h.handle(key('y'));
        h.handle(key('p'));
        assert_eq!(h.buf, "aaa\naaa\nbbb\nccc");
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
        assert_eq!(h.mode, VimMode::Visual);
        h.handle(key('e'));
        h.handle(key('d'));
        assert_eq!(h.buf, " world");
        assert_eq!(h.mode, VimMode::Normal);
    }

    #[test]
    fn test_visual_yank() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('e'));
        h.handle(key('y'));
        assert_eq!(h.buf, "hello world");
        assert_eq!(h.mode, VimMode::Normal);
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
        assert_eq!(h.mode, VimMode::Insert);
    }

    #[test]
    fn test_visual_line_delete() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 4;
        h.handle(key('V'));
        assert_eq!(h.mode, VimMode::VisualLine);
        h.handle(key('d'));
        assert_eq!(h.buf, "aaa\nccc");
        assert_eq!(h.mode, VimMode::Normal);
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
        assert_eq!(h.mode, VimMode::Visual);
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        h.handle(esc);
        assert_eq!(h.mode, VimMode::Normal);
    }

    #[test]
    fn test_visual_tilde() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('e'));
        h.handle(key('~'));
        assert_eq!(h.buf, "HELLO world");

        let mut expanded = TestHarness::new("ßx");
        expanded.handle(key('v'));
        expanded.handle(key('~'));
        assert_eq!(expanded.buf, "SSx");

        let mut decomposed = TestHarness::new("e\u{301}x");
        decomposed.handle(key('v'));
        decomposed.handle(key('~'));
        assert_eq!(decomposed.buf, "E\u{301}x");
    }

    #[test]
    fn test_visual_switch_modes() {
        let mut h = TestHarness::new("hello");
        h.handle(key('v'));
        assert_eq!(h.mode, VimMode::Visual);
        h.handle(key('V'));
        assert_eq!(h.mode, VimMode::VisualLine);
        h.handle(key('v'));
        assert_eq!(h.mode, VimMode::Visual);
        h.handle(key('v'));
        assert_eq!(h.mode, VimMode::Normal);
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
        assert_eq!(h.mode, VimMode::Visual);
        h.handle(key('d'));
        assert_eq!(h.buf, "");
        assert_eq!(h.mode, VimMode::Normal);
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
        assert_eq!(h.mode, VimMode::Normal);
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
        assert_eq!(h.mode, VimMode::Normal);
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
    fn visual_case_mapping_keeps_cursor_on_a_grapheme_boundary() {
        let mut h = TestHarness::new("İx");
        h.handle(key('v'));
        h.handle(key('l'));
        h.handle(key('u'));

        assert_eq!(h.buf, "i\u{307}x");
        assert_eq!(h.cpos, 0);
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
        assert_eq!(h.mode, VimMode::Insert);
    }

    #[test]
    fn test_open_line_above_multiline() {
        let mut h = TestHarness::new("aaa\nbbb");
        h.cpos = 4;
        h.handle(key('O'));
        assert_eq!(h.buf, "aaa\n\nbbb");
        assert_eq!(h.cpos, 4);
        assert_eq!(h.mode, VimMode::Insert);
    }

    #[test]
    fn test_visual_gg() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 8;
        h.handle(key('v'));
        h.handle(key('g'));
        h.handle(key('g'));
        assert_eq!(h.cpos, 0);
        assert_eq!(h.mode, VimMode::Visual);
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
        assert_eq!(h.mode, VimMode::Insert);
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
        assert_eq!(h.mode, VimMode::Insert);
    }

    #[test]
    fn test_cw_on_whitespace_acts_normally() {
        let mut h = TestHarness::new("hello   world");
        h.cpos = 5;
        h.handle(key('c'));
        h.handle(key('w'));
        assert_eq!(h.buf, "helloworld");
        assert_eq!(h.mode, VimMode::Insert);
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
        assert_eq!(h.mode, VimMode::Insert);
        h.buf.push_str("abc");
        h.cpos = 3;
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        h.handle(esc);
        assert_eq!(h.mode, VimMode::Normal);
        assert_eq!(h.buf, "abc");
        h.handle(key('u'));
        assert_eq!(h.buf, "");
    }

    #[test]
    fn test_undo_clamps_visual_anchor_when_source_shrinks() {
        let mut h = TestHarness::new("");
        // Snapshot empty buffer, then grow source out of band and set the
        // anchor past byte 0.
        h.history.save(UndoEntry::snapshot("", 0, &[]));
        h.buf.push_str("ab");
        h.cpos = 2;
        h.vim_state.visual_anchor = 2;
        // `u` restores empty source - the stale anchor must follow.
        h.handle(key('u'));
        assert_eq!(h.buf, "");
        assert!(
            h.vim_state.visual_anchor <= h.buf.len(),
            "undo left stale visual_anchor {} past source len {}",
            h.vim_state.visual_anchor,
            h.buf.len(),
        );
    }

    #[test]
    fn test_insert_after_change_single_undo() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('c'));
        h.handle(key('w'));
        assert_eq!(h.buf, " world");
        assert_eq!(h.mode, VimMode::Insert);
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
        assert_eq!(h.mode, VimMode::Insert);
    }

    #[test]
    fn test_visual_s_capital_linewise() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 4;
        h.handle(key('v'));
        h.handle(key('l'));
        h.handle(key('S'));
        assert_eq!(h.mode, VimMode::Insert);
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
    fn replace_count_uses_original_grapheme_ranges_when_replacements_join() {
        let mut h = TestHarness::new("🇦🇧🇨🇩x");
        h.handle(key('2'));
        h.handle(key('r'));
        h.handle(key('🇺'));

        assert_eq!(h.buf, "🇺🇺x");
        assert_eq!(h.cpos, 0);
    }

    #[test]
    fn test_capital_p_cursor_on_last_pasted_char() {
        let mut h = TestHarness::new("world");
        h.clipboard
            .kill_ring
            .set_with_linewise("hello".to_string(), false);
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
    fn join_removes_complete_leading_whitespace_graphemes() {
        let mut h = TestHarness::new("aaa\n \u{301}bbb");
        h.handle(key('J'));
        assert_eq!(h.buf, "aaa bbb");
        assert!(h.buf.is_char_boundary(h.cpos));
        assert_eq!(smelt_buffer::text::snap_grapheme(&h.buf, h.cpos), h.cpos);
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
