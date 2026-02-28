use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

// ── Public types ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ViMode {
    Insert,
    Normal,
}

/// What the caller should do after a key is processed.
#[derive(PartialEq)]
pub enum Action {
    /// Key consumed, buf/cpos may have changed.
    Consumed,
    /// Submit the input (Enter).
    Submit,
    /// Navigate history up.
    HistoryPrev,
    /// Navigate history down.
    HistoryNext,
    /// Key not handled — caller should use its own logic.
    Passthrough,
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
    WaitingR,
    WaitingFind(FindKind),
    /// Operator pending + find motion (e.g. `df`, `dt`), waiting for the target char.
    WaitingOpFind(Op, FindKind),
    /// Operator + `i`/`a` pressed, waiting for object type char.
    WaitingTextObj(Op, bool),
}

struct UndoEntry {
    buf: String,
    cpos: usize,
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
    register: String,
    register_linewise: bool,
    undo_stack: Vec<UndoEntry>,
    redo_stack: Vec<UndoEntry>,
    /// Snapshot saved when entering insert mode — committed to undo_stack on exit.
    insert_snapshot: Option<UndoEntry>,
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
            register: String::new(),
            register_linewise: false,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            insert_snapshot: None,
        }
    }

    pub fn mode(&self) -> ViMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: ViMode) {
        self.mode = mode;
        self.sub = SubState::Ready;
        self.reset_counts();
    }

    /// Process a key event. Mutates `buf` and `cpos` as needed.
    pub fn handle_key(&mut self, key: KeyEvent, buf: &mut String, cpos: &mut usize) -> Action {
        match self.mode {
            ViMode::Insert => self.handle_insert(key, buf, cpos),
            ViMode::Normal => self.handle_normal(key, buf, cpos),
        }
    }

    // ── Insert mode ─────────────────────────────────────────────────────

    fn handle_insert(&mut self, key: KeyEvent, buf: &mut String, cpos: &mut usize) -> Action {
        match key {
            // Esc or Ctrl+[ → normal mode
            KeyEvent { code: KeyCode::Esc, .. }
            | KeyEvent {
                code: KeyCode::Char('['),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.enter_normal(buf, cpos);
                Action::Consumed
            }
            // Ctrl+W → delete word backward
            KeyEvent {
                code: KeyCode::Char('w'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.save_undo(buf, *cpos);
                let target = word_backward_pos(buf, *cpos, CharClass::Word);
                buf.drain(target..*cpos);
                *cpos = target;
                Action::Consumed
            }
            // Ctrl+U → delete to line start
            KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.save_undo(buf, *cpos);
                let start = line_start(buf, *cpos);
                buf.drain(start..*cpos);
                *cpos = start;
                Action::Consumed
            }
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

    fn handle_normal(&mut self, key: KeyEvent, buf: &mut String, cpos: &mut usize) -> Action {
        // Ctrl+C / Ctrl+D always pass through for cancel.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c' | 'd' | 'u') => return Action::Passthrough,
                KeyCode::Char('r') => {
                    self.redo(buf, cpos);
                    return Action::Consumed;
                }
                _ => return Action::Consumed,
            }
        }

        // BackTab passes through for mode toggle.
        if key.code == KeyCode::BackTab {
            return Action::Passthrough;
        }

        // Handle sub-states first.
        match self.sub {
            SubState::WaitingR => return self.handle_waiting_r(key, buf, cpos),
            SubState::WaitingFind(kind) => return self.handle_waiting_find(key, kind, buf, cpos),
            SubState::WaitingOpFind(op, kind) => {
                return self.handle_waiting_op_find(key, op, kind, buf, cpos)
            }
            SubState::WaitingG => return self.handle_waiting_g(key, buf, cpos),
            SubState::WaitingTextObj(op, inner) => {
                return self.handle_waiting_textobj(key, op, inner, buf, cpos)
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
                        return self.execute_linewise_op(op, buf, cpos);
                    }
                    // Text object prefix.
                    if c == 'i' || c == 'a' {
                        self.sub = SubState::WaitingTextObj(op, c == 'i');
                        return Action::Consumed;
                    }
                }
                // Otherwise try as a motion.
                let result = self.execute_op_motion(key, op, buf, cpos);
                // Don't reset if execute_op_motion transitioned to a new substate
                // (e.g. WaitingOpFind for df/dt combos).
                if matches!(self.sub, SubState::WaitingOp(_)) {
                    self.reset_pending();
                }
                return result;
            }
            SubState::Ready => {}
        }

        // Ready state — handle count digits, commands, motions.
        if let KeyCode::Char(c) = key.code {
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                return self.handle_normal_char(c, buf, cpos);
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
                *cpos = move_left(buf, *cpos);
                Action::Consumed
            }
            KeyCode::Right => {
                *cpos = move_right_normal(buf, *cpos);
                Action::Consumed
            }
            KeyCode::Up => Action::HistoryPrev,
            KeyCode::Down => Action::HistoryNext,
            KeyCode::Home => {
                *cpos = line_start(buf, *cpos);
                Action::Consumed
            }
            KeyCode::End => {
                *cpos = line_end_normal(buf, *cpos);
                Action::Consumed
            }
            KeyCode::Backspace => {
                *cpos = move_left(buf, *cpos);
                Action::Consumed
            }
            _ => Action::Consumed,
        }
    }

    fn handle_normal_char(&mut self, c: char, buf: &mut String, cpos: &mut usize) -> Action {
        // Count digit accumulation.
        if c.is_ascii_digit() && (c != '0' || self.count1.is_some()) {
            self.count1 =
                Some(self.count1.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize);
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
                self.save_undo(buf, *cpos);
                let end = line_end(buf, *cpos);
                self.yank(&buf[*cpos..end], false);
                buf.drain(*cpos..end);
                clamp_normal(buf, cpos);
                self.reset_pending();
                Action::Consumed
            }
            'C' => {
                self.save_undo(buf, *cpos);
                let end = line_end(buf, *cpos);
                self.yank(&buf[*cpos..end], false);
                buf.drain(*cpos..end);
                self.enter_insert_mode();
                Action::Consumed
            }
            'Y' => {
                let (start, end) = current_line_range(buf, *cpos);
                self.yank(&buf[start..end], true);
                self.reset_pending();
                Action::Consumed
            }

            // ── Direct edits ────────────────────────────────────────────
            'x' => {
                let n = self.take_count();
                if !buf.is_empty() && *cpos < buf.len() {
                    self.save_undo(buf, *cpos);
                    let end = advance_chars(buf, *cpos, n);
                    self.yank(&buf[*cpos..end], false);
                    buf.drain(*cpos..end);
                    clamp_normal(buf, cpos);
                }
                self.reset_pending();
                Action::Consumed
            }
            'X' => {
                let n = self.take_count();
                if *cpos > 0 {
                    self.save_undo(buf, *cpos);
                    let start = retreat_chars(buf, *cpos, n);
                    self.yank(&buf[start..*cpos], false);
                    buf.drain(start..*cpos);
                    *cpos = start;
                    clamp_normal(buf, cpos);
                }
                self.reset_pending();
                Action::Consumed
            }
            's' => {
                let n = self.take_count();
                self.save_undo(buf, *cpos);
                if !buf.is_empty() && *cpos < buf.len() {
                    let end = advance_chars(buf, *cpos, n);
                    self.yank(&buf[*cpos..end], false);
                    buf.drain(*cpos..end);
                }
                self.enter_insert_mode();
                Action::Consumed
            }
            'S' => {
                self.save_undo(buf, *cpos);
                let (start, end) = current_line_content_range(buf, *cpos);
                self.yank(&buf[start..end], false);
                buf.drain(start..end);
                *cpos = start;
                self.enter_insert_mode();
                Action::Consumed
            }
            'r' => {
                self.sub = SubState::WaitingR;
                Action::Consumed
            }
            '~' => {
                let n = self.take_count();
                if !buf.is_empty() && *cpos < buf.len() {
                    self.save_undo(buf, *cpos);
                    for _ in 0..n {
                        if *cpos >= buf.len() {
                            break;
                        }
                        let ch = buf[*cpos..].chars().next().unwrap();
                        let end = *cpos + ch.len_utf8();
                        let toggled: String = if ch.is_uppercase() {
                            ch.to_lowercase().collect()
                        } else {
                            ch.to_uppercase().collect()
                        };
                        buf.replace_range(*cpos..end, &toggled);
                        *cpos += toggled.len();
                    }
                    clamp_normal(buf, cpos);
                }
                self.reset_pending();
                Action::Consumed
            }
            'J' => {
                let after = &buf[*cpos..];
                if let Some(nl) = after.find('\n') {
                    self.save_undo(buf, *cpos);
                    let abs = *cpos + nl;
                    // Remove newline and leading whitespace on next line.
                    let mut end = abs + 1;
                    while end < buf.len() && buf.as_bytes()[end] == b' ' {
                        end += 1;
                    }
                    buf.replace_range(abs..end, " ");
                    *cpos = abs;
                }
                Action::Consumed
            }

            // ── Paste ───────────────────────────────────────────────────
            'p' => {
                if !self.register.is_empty() {
                    self.save_undo(buf, *cpos);
                    if self.register_linewise {
                        let eol = line_end(buf, *cpos);
                        let insert = format!("\n{}", self.register);
                        buf.insert_str(eol, &insert);
                        *cpos = eol + 1;
                        // Move to first non-blank.
                        *cpos += buf[*cpos..]
                            .bytes()
                            .take_while(|b| *b == b' ' || *b == b'\t')
                            .count();
                    } else {
                        let after = advance_chars(buf, *cpos, 1).min(buf.len());
                        buf.insert_str(after, &self.register);
                        *cpos = after;
                        clamp_normal(buf, cpos);
                    }
                }
                Action::Consumed
            }
            'P' => {
                if !self.register.is_empty() {
                    self.save_undo(buf, *cpos);
                    if self.register_linewise {
                        let sol = line_start(buf, *cpos);
                        let insert = format!("{}\n", self.register);
                        buf.insert_str(sol, &insert);
                        *cpos = sol;
                        *cpos += buf[*cpos..]
                            .bytes()
                            .take_while(|b| *b == b' ' || *b == b'\t')
                            .count();
                    } else {
                        buf.insert_str(*cpos, &self.register);
                        // Cursor on last char of pasted text.
                        let plen = self.register.len();
                        if plen > 0 {
                            *cpos += plen;
                            clamp_normal(buf, cpos);
                        }
                    }
                }
                Action::Consumed
            }

            // ── Undo / Redo ─────────────────────────────────────────────
            'u' => {
                self.undo(buf, cpos);
                Action::Consumed
            }

            // ── Enter insert mode ───────────────────────────────────────
            'i' => {
                self.take_count(); // discard
                self.enter_insert_mode();
                Action::Consumed
            }
            'I' => {
                self.take_count();
                *cpos = first_non_blank(buf, *cpos);
                self.enter_insert_mode();
                Action::Consumed
            }
            'a' => {
                self.take_count();
                if !buf.is_empty() && *cpos < buf.len() {
                    *cpos = advance_chars(buf, *cpos, 1);
                }
                self.enter_insert_mode();
                Action::Consumed
            }
            'A' => {
                self.take_count();
                *cpos = line_end(buf, *cpos);
                self.enter_insert_mode();
                Action::Consumed
            }
            'o' => {
                self.save_undo(buf, *cpos);
                let eol = line_end(buf, *cpos);
                buf.insert(eol, '\n');
                *cpos = eol + 1;
                self.enter_insert_mode();
                Action::Consumed
            }
            'O' => {
                self.save_undo(buf, *cpos);
                let sol = line_start(buf, *cpos);
                buf.insert(sol, '\n');
                *cpos = sol;
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
                    let mut pos = *cpos;
                    for _ in 0..n {
                        if let Some(p) = find_char(buf, pos, kind, ch) {
                            pos = p;
                        }
                    }
                    *cpos = pos;
                }
                self.reset_pending();
                Action::Consumed
            }
            ',' => {
                if let Some((kind, ch)) = self.last_find {
                    let n = self.take_count();
                    let rev = kind.reversed();
                    let mut pos = *cpos;
                    for _ in 0..n {
                        if let Some(p) = find_char(buf, pos, rev, ch) {
                            pos = p;
                        }
                    }
                    *cpos = pos;
                }
                self.reset_pending();
                Action::Consumed
            }

            // ── Wait-for-second-char ────────────────────────────────────
            'g' => {
                self.sub = SubState::WaitingG;
                Action::Consumed
            }

            // ── Motions ─────────────────────────────────────────────────
            'h' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = move_left(buf, *cpos);
                }
                Action::Consumed
            }
            'l' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = move_right_normal(buf, *cpos);
                }
                Action::Consumed
            }
            'j' => {
                let n = self.take_count();
                if buf.contains('\n') {
                    let new_pos = move_down(buf, *cpos);
                    if new_pos == *cpos && n <= 1 {
                        // Clipped at bottom — navigate history forward.
                        self.reset_pending();
                        return Action::HistoryNext;
                    }
                    *cpos = new_pos;
                    for _ in 1..n {
                        *cpos = move_down(buf, *cpos);
                    }
                    clamp_normal(buf, cpos);
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
                if buf.contains('\n') {
                    let new_pos = move_up(buf, *cpos);
                    if new_pos == *cpos && n <= 1 {
                        // Clipped at top — navigate history backward.
                        self.reset_pending();
                        return Action::HistoryPrev;
                    }
                    *cpos = new_pos;
                    for _ in 1..n {
                        *cpos = move_up(buf, *cpos);
                    }
                    clamp_normal(buf, cpos);
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
                    *cpos = word_forward_pos(buf, *cpos, CharClass::Word);
                }
                clamp_normal(buf, cpos);
                Action::Consumed
            }
            'W' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = word_forward_pos(buf, *cpos, CharClass::WORD);
                }
                clamp_normal(buf, cpos);
                Action::Consumed
            }
            'b' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = word_backward_pos(buf, *cpos, CharClass::Word);
                }
                Action::Consumed
            }
            'B' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = word_backward_pos(buf, *cpos, CharClass::WORD);
                }
                Action::Consumed
            }
            'e' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = word_end_pos(buf, *cpos, CharClass::Word);
                }
                clamp_normal(buf, cpos);
                Action::Consumed
            }
            'E' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = word_end_pos(buf, *cpos, CharClass::WORD);
                }
                clamp_normal(buf, cpos);
                Action::Consumed
            }
            '0' => {
                *cpos = line_start(buf, *cpos);
                self.reset_pending();
                Action::Consumed
            }
            '^' | '_' => {
                *cpos = first_non_blank(buf, *cpos);
                self.reset_pending();
                Action::Consumed
            }
            '$' => {
                let n = self.take_count();
                // n$ moves down n-1 lines then to end.
                for _ in 1..n {
                    *cpos = move_down(buf, *cpos);
                }
                *cpos = line_end_normal(buf, *cpos);
                Action::Consumed
            }
            'G' => {
                self.take_count();
                *cpos = buf.len();
                clamp_normal(buf, cpos);
                Action::Consumed
            }

            // Unknown — swallow it.
            _ => {
                self.reset_pending();
                Action::Consumed
            }
        }
    }

    // ── Sub-state handlers ──────────────────────────────────────────────

    fn handle_waiting_r(&mut self, key: KeyEvent, buf: &mut String, cpos: &mut usize) -> Action {
        self.sub = SubState::Ready;
        if let KeyCode::Char(c) = key.code {
            if !buf.is_empty() && *cpos < buf.len() {
                let n = self.take_count();
                self.save_undo(buf, *cpos);
                let mut pos = *cpos;
                for _ in 0..n {
                    if pos >= buf.len() {
                        break;
                    }
                    let old = buf[pos..].chars().next().unwrap();
                    let end = pos + old.len_utf8();
                    let replacement = if c == '\n' {
                        "\n".to_string()
                    } else {
                        c.to_string()
                    };
                    buf.replace_range(pos..end, &replacement);
                    pos += replacement.len();
                }
                clamp_normal(buf, cpos);
            }
        }
        self.reset_pending();
        Action::Consumed
    }

    fn handle_waiting_find(
        &mut self,
        key: KeyEvent,
        kind: FindKind,
        buf: &mut str,
        cpos: &mut usize,
    ) -> Action {
        self.sub = SubState::Ready;
        if let KeyCode::Char(ch) = key.code {
            let n = self.take_count();
            self.last_find = Some((kind, ch));
            let mut pos = *cpos;
            for _ in 0..n {
                if let Some(p) = find_char(buf, pos, kind, ch) {
                    pos = p;
                }
            }
            *cpos = pos;
        }
        self.reset_pending();
        Action::Consumed
    }

    fn handle_waiting_op_find(
        &mut self,
        key: KeyEvent,
        op: Op,
        kind: FindKind,
        buf: &mut String,
        cpos: &mut usize,
    ) -> Action {
        self.sub = SubState::Ready;
        if let KeyCode::Char(ch) = key.code {
            let n = self.effective_count();
            self.last_find = Some((kind, ch));
            let origin = *cpos;
            // For operators, always find the actual char position (Forward/Backward),
            // then adjust the range for till variants.
            let raw_kind = match kind {
                FindKind::ForwardTill => FindKind::Forward,
                FindKind::BackwardTill => FindKind::Backward,
                other => other,
            };
            let mut pos = origin;
            for _ in 0..n {
                if let Some(p) = find_char(buf, pos, raw_kind, ch) {
                    pos = p;
                }
            }
            if pos != origin {
                // f is inclusive (include target char), t excludes target char.
                let (start, end) = match kind {
                    FindKind::Forward => (*cpos, advance_chars(buf, pos, 1)),
                    FindKind::ForwardTill => (*cpos, pos),
                    FindKind::Backward => (pos, *cpos),
                    FindKind::BackwardTill => (advance_chars(buf, pos, 1), *cpos),
                };
                if start < end {
                    match op {
                        Op::Delete => {
                            self.save_undo(buf, *cpos);
                            self.yank(&buf[start..end], false);
                            buf.drain(start..end);
                            *cpos = start;
                            clamp_normal(buf, cpos);
                        }
                        Op::Change => {
                            self.save_undo(buf, *cpos);
                            self.yank(&buf[start..end], false);
                            buf.drain(start..end);
                            *cpos = start;
                            self.enter_insert_mode();
                            self.reset_counts();
                            return Action::Consumed;
                        }
                        Op::Yank => {
                            self.yank(&buf[start..end], false);
                            *cpos = start;
                        }
                    }
                }
            }
        }
        self.reset_pending();
        Action::Consumed
    }

    fn handle_waiting_g(&mut self, key: KeyEvent, buf: &mut str, cpos: &mut usize) -> Action {
        self.sub = SubState::Ready;
        if let KeyCode::Char('g') = key.code {
            // gg → start of buffer.
            if let Some(n) = self.count1.take() {
                // {n}gg → go to line n.
                *cpos = goto_line(buf, n.saturating_sub(1));
            } else {
                *cpos = 0;
            }
        }
        self.count1 = None;
        self.count2 = None;
        Action::Consumed
    }

    fn handle_waiting_textobj(
        &mut self,
        key: KeyEvent,
        op: Op,
        inner: bool,
        buf: &mut String,
        cpos: &mut usize,
    ) -> Action {
        self.sub = SubState::Ready;
        if let KeyCode::Char(c) = key.code {
            if let Some((start, end)) = text_object(buf, *cpos, inner, c) {
                let n = self.effective_count();
                // For text objects, count means repeat the object n times (expand).
                // Simplified: just use the single object range.
                let _ = n;
                match op {
                    Op::Delete => {
                        self.save_undo(buf, *cpos);
                        self.yank(&buf[start..end], false);
                        buf.drain(start..end);
                        *cpos = start;
                        clamp_normal(buf, cpos);
                    }
                    Op::Change => {
                        self.save_undo(buf, *cpos);
                        self.yank(&buf[start..end], false);
                        buf.drain(start..end);
                        *cpos = start;
                        self.enter_insert_mode();
                        self.reset_counts();
                        return Action::Consumed;
                    }
                    Op::Yank => {
                        self.yank(&buf[start..end], false);
                        *cpos = start;
                    }
                }
            }
        }
        self.reset_pending();
        Action::Consumed
    }

    /// Operator pending + a motion key.
    fn execute_op_motion(
        &mut self,
        key: KeyEvent,
        op: Op,
        buf: &mut String,
        cpos: &mut usize,
    ) -> Action {
        let n = self.effective_count();
        let origin = *cpos;

        // Resolve motion target.
        let target = match key.code {
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
                let mut p = origin;
                for _ in 0..n {
                    p = move_left(buf, p);
                }
                Some(p)
            }
            KeyCode::Char('l') | KeyCode::Right => {
                let mut p = origin;
                for _ in 0..n {
                    p = move_right_inclusive(buf, p);
                }
                Some(p)
            }
            KeyCode::Char('w') => {
                let mut p = origin;
                for _ in 0..n {
                    p = word_forward_pos(buf, p, CharClass::Word);
                }
                Some(p)
            }
            KeyCode::Char('W') => {
                let mut p = origin;
                for _ in 0..n {
                    p = word_forward_pos(buf, p, CharClass::WORD);
                }
                Some(p)
            }
            KeyCode::Char('b') => {
                let mut p = origin;
                for _ in 0..n {
                    p = word_backward_pos(buf, p, CharClass::Word);
                }
                Some(p)
            }
            KeyCode::Char('B') => {
                let mut p = origin;
                for _ in 0..n {
                    p = word_backward_pos(buf, p, CharClass::WORD);
                }
                Some(p)
            }
            KeyCode::Char('e') => {
                let mut p = origin;
                for _ in 0..n {
                    p = word_end_pos(buf, p, CharClass::Word);
                }
                // e is inclusive: include the char at target.
                Some(advance_chars(buf, p, 1))
            }
            KeyCode::Char('E') => {
                let mut p = origin;
                for _ in 0..n {
                    p = word_end_pos(buf, p, CharClass::WORD);
                }
                Some(advance_chars(buf, p, 1))
            }
            KeyCode::Char('0') => Some(line_start(buf, origin)),
            KeyCode::Char('^' | '_') => Some(first_non_blank(buf, origin)),
            KeyCode::Char('$') => Some(line_end(buf, origin)),
            KeyCode::Char('G') => Some(buf.len()),
            KeyCode::Char('g') => {
                // Wait for 'g' → gg.
                self.sub = SubState::WaitingG;
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
            KeyCode::Home => Some(line_start(buf, origin)),
            KeyCode::End => Some(line_end(buf, origin)),
            _ => None,
        };

        let Some(target) = target else {
            // Invalid motion — cancel.
            return Action::Consumed;
        };

        let (start, end) = if target < origin {
            (target, origin)
        } else {
            (origin, target)
        };

        if start == end {
            return Action::Consumed;
        }

        match op {
            Op::Delete => {
                self.save_undo(buf, *cpos);
                self.yank(&buf[start..end], false);
                buf.drain(start..end);
                *cpos = start;
                clamp_normal(buf, cpos);
            }
            Op::Change => {
                self.save_undo(buf, *cpos);
                self.yank(&buf[start..end], false);
                buf.drain(start..end);
                *cpos = start;
                self.enter_insert_mode();
                return Action::Consumed;
            }
            Op::Yank => {
                self.yank(&buf[start..end], false);
                *cpos = start;
            }
        }
        Action::Consumed
    }

    fn execute_linewise_op(&mut self, op: Op, buf: &mut String, cpos: &mut usize) -> Action {
        let n = self.effective_count();
        self.reset_counts();
        self.sub = SubState::Ready;

        let (mut start, _) = current_line_range(buf, *cpos);
        let mut end = start;
        let mut included_trailing_nl = false;
        for _ in 0..n {
            end = line_end(buf, end);
            if end < buf.len() {
                end += 1; // include the newline
                included_trailing_nl = true;
            }
        }
        // If deleting all content, don't leave a trailing newline.
        if start == 0 && end == buf.len() && !included_trailing_nl {
            // Deleting everything.
        } else if start > 0 && end == buf.len() && !included_trailing_nl {
            // Last line(s) with no trailing newline: remove preceding newline instead.
            start -= 1;
        }

        match op {
            Op::Delete => {
                self.save_undo(buf, *cpos);
                self.yank(&buf[start..end], true);
                buf.drain(start..end);
                *cpos = start.min(buf.len());
                if !buf.is_empty() && *cpos < buf.len() {
                    *cpos = first_non_blank_at(buf, *cpos);
                }
                clamp_normal(buf, cpos);
            }
            Op::Change => {
                self.save_undo(buf, *cpos);
                // cc: clear line content but keep the line itself.
                let (cs, ce) = current_line_content_range(buf, *cpos);
                self.yank(&buf[cs..ce], true);
                buf.drain(cs..ce);
                *cpos = cs;
                self.enter_insert_mode();
                return Action::Consumed;
            }
            Op::Yank => {
                self.yank(&buf[start..end], true);
                *cpos = line_start(buf, *cpos);
            }
        }
        Action::Consumed
    }

    // ── Mode transitions ────────────────────────────────────────────────

    fn enter_insert_mode(&mut self) {
        self.mode = ViMode::Insert;
        self.sub = SubState::Ready;
    }

    fn enter_normal(&mut self, buf: &str, cpos: &mut usize) {
        // Commit insert snapshot if any.
        self.commit_insert_snapshot();
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

    // ── Undo/redo ───────────────────────────────────────────────────────

    fn save_undo(&mut self, buf: &str, cpos: usize) {
        self.redo_stack.clear();
        self.undo_stack.push(UndoEntry {
            buf: buf.to_string(),
            cpos,
        });
    }

    fn undo(&mut self, buf: &mut String, cpos: &mut usize) {
        if let Some(entry) = self.undo_stack.pop() {
            self.redo_stack.push(UndoEntry {
                buf: buf.clone(),
                cpos: *cpos,
            });
            *buf = entry.buf;
            *cpos = entry.cpos;
            clamp_normal(buf, cpos);
        }
    }

    fn redo(&mut self, buf: &mut String, cpos: &mut usize) {
        if let Some(entry) = self.redo_stack.pop() {
            self.undo_stack.push(UndoEntry {
                buf: buf.clone(),
                cpos: *cpos,
            });
            *buf = entry.buf;
            *cpos = entry.cpos;
            clamp_normal(buf, cpos);
        }
    }

    fn commit_insert_snapshot(&mut self) {
        if let Some(snap) = self.insert_snapshot.take() {
            self.redo_stack.clear();
            self.undo_stack.push(snap);
        }
    }

    // ── Register ────────────────────────────────────────────────────────

    fn yank(&mut self, text: &str, linewise: bool) {
        self.register = text.to_string();
        self.register_linewise = linewise;
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

// ── Character classification ────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum CharClass {
    /// vim "word" boundaries: alphanumeric+underscore vs punctuation vs whitespace.
    Word,
    /// vim "WORD" boundaries: non-whitespace vs whitespace.
    #[allow(clippy::upper_case_acronyms)]
    WORD,
}

fn char_class(c: char, mode: CharClass) -> u8 {
    match mode {
        CharClass::Word => {
            if c.is_alphanumeric() || c == '_' {
                1
            } else if c.is_whitespace() {
                0
            } else {
                2
            }
        }
        CharClass::WORD => {
            if c.is_whitespace() {
                0
            } else {
                1
            }
        }
    }
}

// ── Motion helpers ──────────────────────────────────────────────────────────

fn move_left(buf: &str, cpos: usize) -> usize {
    if cpos == 0 {
        return 0;
    }
    let sol = line_start(buf, cpos);
    if cpos <= sol {
        return cpos; // Don't cross line boundary.
    }
    prev_char_boundary(buf, cpos)
}

/// Move right, staying within the current line and not landing on '\n'.
fn move_right_normal(buf: &str, cpos: usize) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let eol = line_end(buf, cpos);
    let last_on_line = if eol > line_start(buf, cpos) {
        prev_char_boundary(buf, eol)
    } else {
        eol // Empty line — stay put.
    };
    if cpos >= last_on_line {
        return cpos;
    }
    next_char_boundary(buf, cpos)
}

/// Move right inclusive (for operator motions on `l`).
fn move_right_inclusive(buf: &str, cpos: usize) -> usize {
    next_char_boundary(buf, cpos).min(buf.len())
}

fn word_forward_pos(buf: &str, cpos: usize, mode: CharClass) -> usize {
    let chars: Vec<(usize, char)> = buf[cpos..].char_indices().collect();
    if chars.is_empty() {
        return cpos;
    }
    let mut i = 0;
    let start_class = char_class(chars[0].1, mode);
    // Skip same class.
    while i < chars.len() && char_class(chars[i].1, mode) == start_class {
        i += 1;
    }
    // Skip whitespace.
    while i < chars.len() && char_class(chars[i].1, mode) == 0 {
        i += 1;
    }
    if i < chars.len() {
        cpos + chars[i].0
    } else {
        buf.len()
    }
}

fn word_backward_pos(buf: &str, cpos: usize, mode: CharClass) -> usize {
    if cpos == 0 {
        return 0;
    }
    let chars: Vec<(usize, char)> = buf[..cpos].char_indices().collect();
    if chars.is_empty() {
        return 0;
    }
    let mut i = chars.len() - 1;
    // Skip whitespace backward.
    while i > 0 && char_class(chars[i].1, mode) == 0 {
        i -= 1;
    }
    let target_class = char_class(chars[i].1, mode);
    // Skip same class backward.
    while i > 0 && char_class(chars[i - 1].1, mode) == target_class {
        i -= 1;
    }
    chars[i].0
}

fn word_end_pos(buf: &str, cpos: usize, mode: CharClass) -> usize {
    let next = next_char_boundary(buf, cpos);
    if next >= buf.len() {
        return cpos;
    }
    let chars: Vec<(usize, char)> = buf[next..].char_indices().collect();
    if chars.is_empty() {
        return cpos;
    }
    let mut i = 0;
    // Skip whitespace.
    while i < chars.len() && char_class(chars[i].1, mode) == 0 {
        i += 1;
    }
    if i >= chars.len() {
        return buf.len().saturating_sub(1);
    }
    let target_class = char_class(chars[i].1, mode);
    // Skip same class.
    while i + 1 < chars.len() && char_class(chars[i + 1].1, mode) == target_class {
        i += 1;
    }
    next + chars[i].0
}

fn line_start(buf: &str, cpos: usize) -> usize {
    buf[..cpos].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

fn line_end(buf: &str, cpos: usize) -> usize {
    cpos + buf[cpos..].find('\n').unwrap_or(buf.len() - cpos)
}

/// End of line for normal mode (on last char, not past it).
fn line_end_normal(buf: &str, cpos: usize) -> usize {
    let end = line_end(buf, cpos);
    if end > line_start(buf, cpos) {
        prev_char_boundary(buf, end)
    } else {
        end
    }
}

fn first_non_blank(buf: &str, cpos: usize) -> usize {
    first_non_blank_at(buf, line_start(buf, cpos))
}

fn first_non_blank_at(buf: &str, from: usize) -> usize {
    let eol = line_end(buf, from);
    let mut pos = from;
    while pos < eol {
        let c = buf[pos..].chars().next().unwrap();
        if c != ' ' && c != '\t' {
            break;
        }
        pos += c.len_utf8();
    }
    pos
}

/// Range of the full current line including trailing newline (for dd).
fn current_line_range(buf: &str, cpos: usize) -> (usize, usize) {
    let start = line_start(buf, cpos);
    let end = line_end(buf, cpos);
    (start, end)
}

/// Range of just the content of the current line (no newline) — for S/cc.
fn current_line_content_range(buf: &str, cpos: usize) -> (usize, usize) {
    let start = line_start(buf, cpos);
    let end = line_end(buf, cpos);
    (start, end)
}

fn goto_line(buf: &str, line_idx: usize) -> usize {
    let mut pos = 0;
    for _ in 0..line_idx {
        match buf[pos..].find('\n') {
            Some(i) => pos += i + 1,
            None => return pos,
        }
    }
    pos
}

fn move_down(buf: &str, cpos: usize) -> usize {
    let sol = line_start(buf, cpos);
    let col = cpos - sol;
    let eol = line_end(buf, cpos);
    if eol >= buf.len() {
        return cpos; // Already on last line.
    }
    let next_sol = eol + 1;
    let next_eol = line_end(buf, next_sol);
    let next_len = next_eol - next_sol;
    next_sol + col.min(next_len)
}

fn move_up(buf: &str, cpos: usize) -> usize {
    let sol = line_start(buf, cpos);
    if sol == 0 {
        return cpos; // Already on first line.
    }
    let col = cpos - sol;
    let prev_eol = sol - 1;
    let prev_sol = line_start(buf, prev_eol);
    let prev_len = prev_eol - prev_sol;
    prev_sol + col.min(prev_len)
}

// ── Find char on line ───────────────────────────────────────────────────────

fn find_char(buf: &str, cpos: usize, kind: FindKind, ch: char) -> Option<usize> {
    let sol = line_start(buf, cpos);
    let eol = line_end(buf, cpos);

    match kind {
        FindKind::Forward | FindKind::ForwardTill => {
            let start = next_char_boundary(buf, cpos);
            for (i, c) in buf[start..eol].char_indices() {
                if c == ch {
                    let pos = start + i;
                    return Some(match kind {
                        FindKind::ForwardTill => prev_char_boundary(buf, pos).max(cpos),
                        _ => pos,
                    });
                }
            }
            None
        }
        FindKind::Backward | FindKind::BackwardTill => {
            let search = &buf[sol..cpos];
            for (i, c) in search.char_indices().rev() {
                if c == ch {
                    let pos = sol + i;
                    return Some(match kind {
                        FindKind::BackwardTill => {
                            next_char_boundary(buf, pos).min(cpos)
                        }
                        _ => pos,
                    });
                }
            }
            None
        }
    }
}

// ── Text objects ────────────────────────────────────────────────────────────

fn text_object(buf: &str, cpos: usize, inner: bool, kind: char) -> Option<(usize, usize)> {
    match kind {
        'w' => text_object_word(buf, cpos, inner, CharClass::Word),
        'W' => text_object_word(buf, cpos, inner, CharClass::WORD),
        '"' | '\'' | '`' => text_object_quote(buf, cpos, inner, kind),
        '(' | ')' | 'b' => text_object_pair(buf, cpos, inner, '(', ')'),
        '[' | ']' => text_object_pair(buf, cpos, inner, '[', ']'),
        '{' | '}' | 'B' => text_object_pair(buf, cpos, inner, '{', '}'),
        '<' | '>' => text_object_pair(buf, cpos, inner, '<', '>'),
        _ => None,
    }
}

fn text_object_word(
    buf: &str,
    cpos: usize,
    inner: bool,
    mode: CharClass,
) -> Option<(usize, usize)> {
    if buf.is_empty() || cpos >= buf.len() {
        return None;
    }
    let chars: Vec<(usize, char)> = buf.char_indices().collect();
    // Find char index for cpos.
    let ci = chars.iter().position(|(i, _)| *i >= cpos)?;
    let cur_class = char_class(chars[ci].1, mode);

    // Expand backward over same class.
    let mut start = ci;
    while start > 0 && char_class(chars[start - 1].1, mode) == cur_class {
        start -= 1;
    }
    // Expand forward over same class.
    let mut end = ci;
    while end + 1 < chars.len() && char_class(chars[end + 1].1, mode) == cur_class {
        end += 1;
    }

    let byte_start = chars[start].0;
    let byte_end = if end + 1 < chars.len() {
        chars[end + 1].0
    } else {
        buf.len()
    };

    if inner {
        Some((byte_start, byte_end))
    } else {
        // "a word" includes trailing whitespace, or leading if no trailing.
        let mut a_end = byte_end;
        while a_end < buf.len() && buf[a_end..].starts_with([' ', '\t']) {
            a_end += 1;
        }
        if a_end > byte_end {
            Some((byte_start, a_end))
        } else {
            // No trailing whitespace — include leading.
            let mut a_start = byte_start;
            while a_start > 0
                && buf[..a_start].ends_with([' ', '\t'])
            {
                a_start -= 1;
            }
            Some((a_start, byte_end))
        }
    }
}

fn text_object_quote(
    buf: &str,
    cpos: usize,
    inner: bool,
    quote: char,
) -> Option<(usize, usize)> {
    // Find the opening quote before or at cpos, and closing quote after.
    let line_s = line_start(buf, cpos);
    let line_e = line_end(buf, cpos);
    let line = &buf[line_s..line_e];
    let rel = cpos - line_s;

    // Collect quote positions in the line.
    let positions: Vec<usize> = line
        .char_indices()
        .filter(|(_, c)| *c == quote)
        .map(|(i, _)| i)
        .collect();

    // Find a pair that contains rel.
    for pair in positions.chunks(2) {
        if pair.len() == 2 && pair[0] <= rel && rel <= pair[1] {
            let abs_open = line_s + pair[0];
            let abs_close = line_s + pair[1];
            return if inner {
                Some((abs_open + quote.len_utf8(), abs_close))
            } else {
                Some((abs_open, abs_close + quote.len_utf8()))
            };
        }
    }
    None
}

fn text_object_pair(
    buf: &str,
    cpos: usize,
    inner: bool,
    open: char,
    close: char,
) -> Option<(usize, usize)> {
    // Search backward for matching open.
    let mut depth = 0i32;
    let mut open_pos = None;
    for (i, c) in buf[..=cpos.min(buf.len().saturating_sub(1))]
        .char_indices()
        .rev()
    {
        if c == close && i != cpos {
            depth += 1;
        } else if c == open {
            if depth == 0 {
                open_pos = Some(i);
                break;
            }
            depth -= 1;
        }
    }
    let open_pos = open_pos?;

    // Search forward for matching close.
    depth = 0;
    let search_start = open_pos + open.len_utf8();
    for (i, c) in buf[search_start..].char_indices() {
        if c == open {
            depth += 1;
        } else if c == close {
            if depth == 0 {
                let close_pos = search_start + i;
                return if inner {
                    Some((open_pos + open.len_utf8(), close_pos))
                } else {
                    Some((open_pos, close_pos + close.len_utf8()))
                };
            }
            depth -= 1;
        }
    }
    None
}

// ── Byte boundary helpers ───────────────────────────────────────────────────

fn prev_char_boundary(s: &str, pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    let mut p = pos - 1;
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

fn next_char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    let mut p = pos + 1;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}

fn advance_chars(buf: &str, pos: usize, n: usize) -> usize {
    let mut p = pos;
    for _ in 0..n {
        if p >= buf.len() {
            break;
        }
        p = next_char_boundary(buf, p);
    }
    p
}

fn retreat_chars(buf: &str, pos: usize, n: usize) -> usize {
    let mut p = pos;
    for _ in 0..n {
        if p == 0 {
            break;
        }
        p = prev_char_boundary(buf, p);
    }
    p
}

/// Clamp cursor to valid normal-mode position (on a char, not past end).
/// Exception: if the buffer ends with '\n', `buf.len()` is valid — it
/// represents the cursor on the empty trailing line.
fn clamp_normal(buf: &str, cpos: &mut usize) {
    if buf.is_empty() {
        *cpos = 0;
    } else if *cpos >= buf.len() {
        if buf.ends_with('\n') {
            *cpos = buf.len();
        } else {
            *cpos = prev_char_boundary(buf, buf.len());
        }
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

    fn setup(text: &str) -> (Vim, String, usize) {
        let mut vim = Vim::new();
        vim.mode = ViMode::Normal;
        vim.sub = SubState::Ready;
        let buf = text.to_string();
        let cpos = 0;
        (vim, buf, cpos)
    }

    #[test]
    fn test_word_forward() {
        let (mut vim, mut buf, mut cpos) = setup("hello world foo");
        vim.handle_key(key('w'), &mut buf, &mut cpos);
        assert_eq!(cpos, 6);
        vim.handle_key(key('w'), &mut buf, &mut cpos);
        assert_eq!(cpos, 12);
    }

    #[test]
    fn test_word_backward() {
        let (mut vim, mut buf, mut cpos) = setup("hello world");
        cpos = 6;
        vim.handle_key(key('b'), &mut buf, &mut cpos);
        assert_eq!(cpos, 0);
    }

    #[test]
    fn test_word_end() {
        let (mut vim, mut buf, mut cpos) = setup("hello world");
        vim.handle_key(key('e'), &mut buf, &mut cpos);
        assert_eq!(cpos, 4);
    }

    #[test]
    fn test_delete_word() {
        let (mut vim, mut buf, mut cpos) = setup("hello world");
        vim.handle_key(key('d'), &mut buf, &mut cpos);
        vim.handle_key(key('w'), &mut buf, &mut cpos);
        assert_eq!(buf, "world");
        assert_eq!(cpos, 0);
    }

    #[test]
    fn test_delete_inner_word() {
        let (mut vim, mut buf, mut cpos) = setup("hello world");
        vim.handle_key(key('d'), &mut buf, &mut cpos);
        vim.handle_key(key('i'), &mut buf, &mut cpos);
        vim.handle_key(key('w'), &mut buf, &mut cpos);
        assert_eq!(buf, " world");
    }

    #[test]
    fn test_change_word() {
        let (mut vim, mut buf, mut cpos) = setup("hello world");
        vim.handle_key(key('c'), &mut buf, &mut cpos);
        vim.handle_key(key('w'), &mut buf, &mut cpos);
        assert_eq!(buf, "world");
        assert_eq!(vim.mode(), ViMode::Insert);
    }

    #[test]
    fn test_dd_single_line() {
        let (mut vim, mut buf, mut cpos) = setup("hello");
        vim.handle_key(key('d'), &mut buf, &mut cpos);
        vim.handle_key(key('d'), &mut buf, &mut cpos);
        assert_eq!(buf, "");
    }

    #[test]
    fn test_dd_multiline() {
        let (mut vim, mut buf, mut cpos) = setup("aaa\nbbb\nccc");
        cpos = 4; // on 'bbb'
        vim.handle_key(key('d'), &mut buf, &mut cpos);
        vim.handle_key(key('d'), &mut buf, &mut cpos);
        assert_eq!(buf, "aaa\nccc");
    }

    #[test]
    fn test_dd_middle_line_with_empty_neighbors() {
        // Three lines: empty, "foo", empty. Delete middle line.
        let (mut vim, mut buf, mut cpos) = setup("\nfoo\n");
        cpos = 1; // on 'f'
        vim.handle_key(key('d'), &mut buf, &mut cpos);
        vim.handle_key(key('d'), &mut buf, &mut cpos);
        assert_eq!(buf, "\n"); // Two empty lines remain.
    }

    #[test]
    fn test_undo_redo() {
        let (mut vim, mut buf, mut cpos) = setup("hello world");
        vim.handle_key(key('d'), &mut buf, &mut cpos);
        vim.handle_key(key('w'), &mut buf, &mut cpos);
        assert_eq!(buf, "world");
        vim.handle_key(key('u'), &mut buf, &mut cpos);
        assert_eq!(buf, "hello world");
        vim.handle_key(key_ctrl('r'), &mut buf, &mut cpos);
        assert_eq!(buf, "world");
    }

    #[test]
    fn test_count_motion() {
        let (mut vim, mut buf, mut cpos) = setup("one two three four");
        vim.handle_key(key('2'), &mut buf, &mut cpos);
        vim.handle_key(key('w'), &mut buf, &mut cpos);
        assert_eq!(cpos, 8); // start of "three"
    }

    #[test]
    fn test_count_delete() {
        let (mut vim, mut buf, mut cpos) = setup("one two three four");
        vim.handle_key(key('2'), &mut buf, &mut cpos);
        vim.handle_key(key('d'), &mut buf, &mut cpos);
        vim.handle_key(key('w'), &mut buf, &mut cpos);
        assert_eq!(buf, "three four");
    }

    #[test]
    fn test_find_char() {
        let (mut vim, mut buf, mut cpos) = setup("hello world");
        vim.handle_key(key('f'), &mut buf, &mut cpos);
        vim.handle_key(key('o'), &mut buf, &mut cpos);
        assert_eq!(cpos, 4);
        vim.handle_key(key(';'), &mut buf, &mut cpos);
        assert_eq!(cpos, 7);
        vim.handle_key(key(','), &mut buf, &mut cpos);
        assert_eq!(cpos, 4);
    }

    #[test]
    fn test_till_char() {
        let (mut vim, mut buf, mut cpos) = setup("hello world");
        vim.handle_key(key('t'), &mut buf, &mut cpos);
        vim.handle_key(key('o'), &mut buf, &mut cpos);
        assert_eq!(cpos, 3);
    }

    #[test]
    fn test_text_object_pair() {
        let (mut vim, mut buf, mut cpos) = setup("foo(bar)baz");
        cpos = 5; // on 'a' inside parens
        vim.handle_key(key('d'), &mut buf, &mut cpos);
        vim.handle_key(key('i'), &mut buf, &mut cpos);
        vim.handle_key(key('('), &mut buf, &mut cpos);
        assert_eq!(buf, "foo()baz");
    }

    #[test]
    fn test_text_object_quote() {
        let (mut vim, mut buf, mut cpos) = setup(r#"say "hello" end"#);
        cpos = 6; // on 'e' inside quotes
        vim.handle_key(key('d'), &mut buf, &mut cpos);
        vim.handle_key(key('i'), &mut buf, &mut cpos);
        vim.handle_key(key('"'), &mut buf, &mut cpos);
        assert_eq!(buf, r#"say "" end"#);
    }

    #[test]
    fn test_paste() {
        let (mut vim, mut buf, mut cpos) = setup("hello world");
        // Delete word, then paste.
        vim.handle_key(key('d'), &mut buf, &mut cpos);
        vim.handle_key(key('w'), &mut buf, &mut cpos);
        assert_eq!(buf, "world");
        vim.handle_key(key('p'), &mut buf, &mut cpos);
        assert_eq!(buf, "whello orld");
    }

    #[test]
    fn test_tilde() {
        let (mut vim, mut buf, mut cpos) = setup("hello");
        vim.handle_key(key('~'), &mut buf, &mut cpos);
        assert_eq!(buf, "Hello");
        assert_eq!(cpos, 1); // ~ advances cursor after toggling
    }

    #[test]
    fn test_replace() {
        let (mut vim, mut buf, mut cpos) = setup("hello");
        vim.handle_key(key('r'), &mut buf, &mut cpos);
        vim.handle_key(key('X'), &mut buf, &mut cpos);
        assert_eq!(buf, "Xello");
    }

    #[test]
    fn test_insert_ctrl_w() {
        let mut vim = Vim::new(); // starts in Insert
        let mut buf = "hello world".to_string();
        let mut cpos = buf.len();
        vim.handle_key(key_ctrl('w'), &mut buf, &mut cpos);
        assert_eq!(buf, "hello ");
    }

    #[test]
    fn test_line_movement() {
        let (mut vim, mut buf, mut cpos) = setup("aaa\nbbb\nccc");
        vim.handle_key(key('j'), &mut buf, &mut cpos);
        assert_eq!(cpos, 4); // start of 'bbb'
        vim.handle_key(key('j'), &mut buf, &mut cpos);
        assert_eq!(cpos, 8); // start of 'ccc'
        vim.handle_key(key('k'), &mut buf, &mut cpos);
        assert_eq!(cpos, 4);
    }

    #[test]
    fn test_open_line_and_navigate() {
        // Type 'o' to open line below, press Esc, then navigate with j/k.
        let (mut vim, mut buf, mut cpos) = setup("hello");
        // 'o' opens line below → buf = "hello\n", cpos = 6, insert mode.
        vim.handle_key(key('o'), &mut buf, &mut cpos);
        assert_eq!(buf, "hello\n");
        assert_eq!(cpos, 6);
        assert_eq!(vim.mode(), ViMode::Insert);

        // Esc → normal mode, cursor stays on empty trailing line.
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        vim.handle_key(esc, &mut buf, &mut cpos);
        assert_eq!(vim.mode(), ViMode::Normal);
        assert_eq!(cpos, 6); // On the empty second line.

        // 'k' should go up to "hello" line.
        vim.handle_key(key('k'), &mut buf, &mut cpos);
        assert_eq!(cpos, 0);

        // 'j' should go back down to the empty line.
        vim.handle_key(key('j'), &mut buf, &mut cpos);
        assert_eq!(cpos, 6);
    }

    #[test]
    fn test_esc_moves_cursor_back() {
        // In vim, Esc moves cursor left one position.
        let mut vim = Vim::new(); // starts in Insert
        let mut buf = "hello".to_string();
        let mut cpos = 5; // cursor at end
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        vim.handle_key(esc, &mut buf, &mut cpos);
        assert_eq!(cpos, 4); // Moved back one.
        assert_eq!(vim.mode(), ViMode::Normal);
    }

    #[test]
    fn test_esc_at_line_start_stays() {
        // Esc at start of line should not move further left.
        let mut vim = Vim::new();
        let mut buf = "hello".to_string();
        let mut cpos = 0;
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        vim.handle_key(esc, &mut buf, &mut cpos);
        assert_eq!(cpos, 0);
    }

    #[test]
    fn test_h_l_stay_within_line() {
        let (mut vim, mut buf, mut cpos) = setup("aa\nbb");
        // Move to end of first line.
        vim.handle_key(key('$'), &mut buf, &mut cpos);
        assert_eq!(cpos, 1); // On second 'a'.
        // 'l' should NOT cross to next line.
        vim.handle_key(key('l'), &mut buf, &mut cpos);
        assert_eq!(cpos, 1); // Still on 'a'.
        // Move to start of second line.
        vim.handle_key(key('j'), &mut buf, &mut cpos);
        vim.handle_key(key('0'), &mut buf, &mut cpos);
        assert_eq!(cpos, 3);
        // 'h' should NOT cross to previous line.
        vim.handle_key(key('h'), &mut buf, &mut cpos);
        assert_eq!(cpos, 3); // Still at start of "bb".
    }

    #[test]
    fn test_empty_buffer() {
        let (mut vim, mut buf, mut cpos) = setup("");
        // All of these should be no-ops on empty buffer.
        vim.handle_key(key('x'), &mut buf, &mut cpos);
        assert_eq!(buf, "");
        vim.handle_key(key('d'), &mut buf, &mut cpos);
        vim.handle_key(key('w'), &mut buf, &mut cpos);
        assert_eq!(buf, "");
    }

    #[test]
    fn test_gg() {
        let (mut vim, mut buf, mut cpos) = setup("aaa\nbbb\nccc");
        cpos = 8;
        vim.handle_key(key('g'), &mut buf, &mut cpos);
        vim.handle_key(key('g'), &mut buf, &mut cpos);
        assert_eq!(cpos, 0);
    }

    #[test]
    fn test_dollar_and_zero() {
        let (mut vim, mut buf, mut cpos) = setup("hello world");
        vim.handle_key(key('$'), &mut buf, &mut cpos);
        assert_eq!(cpos, 10); // last char 'd'
        vim.handle_key(key('0'), &mut buf, &mut cpos);
        assert_eq!(cpos, 0);
    }

    #[test]
    fn test_yank_paste() {
        let (mut vim, mut buf, mut cpos) = setup("hello world");
        vim.handle_key(key('y'), &mut buf, &mut cpos);
        vim.handle_key(key('w'), &mut buf, &mut cpos);
        vim.handle_key(key('$'), &mut buf, &mut cpos);
        vim.handle_key(key('p'), &mut buf, &mut cpos);
        assert_eq!(buf, "hello worldhello ");
    }
}
