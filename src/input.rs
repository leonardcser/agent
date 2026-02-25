use crate::config;
use crate::render::{self, Screen};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    terminal, ExecutableCommand,
};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct History {
    entries: Vec<String>,
    cursor: usize,
    draft: String,
    path: PathBuf,
}

const RECORD_SEP: char = '\x1e';

impl History {
    pub fn load() -> Self {
        let path = config::state_dir().join("history");
        let entries = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .split(RECORD_SEP)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect::<Vec<_>>();
        let cursor = entries.len();
        Self { entries, cursor, draft: String::new(), path }
    }

    pub fn push(&mut self, entry: String) {
        if !entry.is_empty() && self.entries.last().map_or(true, |last| *last != entry) {
            self.entries.push(entry.clone());
            self.append_to_file(&entry);
        }
        self.reset();
    }

    fn append_to_file(&self, entry: &str) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = write!(f, "{}{}", entry, RECORD_SEP);
        }
    }

    fn reset(&mut self) {
        self.cursor = self.entries.len();
        self.draft.clear();
    }

    pub fn up(&mut self, current_buf: &str) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        if self.cursor == self.entries.len() {
            self.draft = current_buf.to_string();
        }
        if self.cursor > 0 {
            self.cursor -= 1;
            Some(&self.entries[self.cursor])
        } else {
            None
        }
    }

    pub fn down(&mut self, _current_buf: &str) -> Option<&str> {
        if self.cursor >= self.entries.len() {
            return None;
        }
        self.cursor += 1;
        if self.cursor == self.entries.len() {
            Some(&self.draft)
        } else {
            Some(&self.entries[self.cursor])
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Normal,
    Apply,
}

impl Mode {
    pub fn toggle(self) -> Self {
        match self {
            Mode::Normal => Mode::Apply,
            Mode::Apply => Mode::Normal,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Apply => "apply",
        }
    }
}

/// Char count up to byte index.
pub fn char_pos(s: &str, byte_idx: usize) -> usize {
    s[..byte_idx].chars().count()
}

/// Byte index of the nth char.
pub fn byte_of_char(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len())
}

/// Read a line of input with the prompt box UI.
pub fn read_input(screen: &mut Screen, mode: &mut Mode, history: &mut History, initial_buf: String, initial_cursor: usize) -> Option<String> {
    let mut buf = initial_buf;
    let mut cpos = initial_cursor.min(buf.len()); // cursor byte position
    let mut out = io::stdout();
    let mut width = render::term_width();

    terminal::enable_raw_mode().ok()?;
    let _ = out.execute(cursor::Hide);

    screen.render(&buf, char_pos(&buf, cpos), *mode, width, &[]);
    let _ = out.execute(cursor::Show);

    let mut resize_pending = false;

    loop {
        let ev = if resize_pending {
            match event::poll(Duration::from_millis(150)) {
                Ok(true) => match event::read() {
                    Ok(ev) => ev,
                    Err(_) => continue,
                },
                _ => {
                    // Resize settled — clear and redraw.
                    resize_pending = false;
                    let _ = out.execute(cursor::Hide);
                    screen.redraw_all();
                    screen.render(&buf, char_pos(&buf, cpos), *mode, width, &[]);
                    let _ = out.execute(cursor::Show);
                    continue;
                }
            }
        } else {
            match event::read() {
                Ok(ev) => ev,
                Err(_) => continue,
            }
        };

        let mut redraw = true;

        match ev {
            Event::Key(KeyEvent { code: KeyCode::BackTab, .. }) => {
                *mode = mode.toggle();
            }
            Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
                if buf.trim().is_empty() {
                    redraw = false;
                } else {
                    let _ = out.execute(cursor::Hide);
                    screen.erase_prompt();
                    let _ = out.execute(cursor::Show);
                    let _ = out.flush();
                    break;
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char('j'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                buf.insert(cpos, '\n');
                cpos += 1;
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char('c' | 'd'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                let _ = out.execute(cursor::Hide);
                screen.erase_prompt();
                let _ = out.execute(cursor::Show);
                let _ = out.flush();
                terminal::disable_raw_mode().ok();
                return None;
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char('a'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                // Move to start of current line
                let before = &buf[..cpos];
                cpos = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char('e'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                // Move to end of current line
                let after = &buf[cpos..];
                cpos += after.find('\n').unwrap_or(after.len());
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                ..
            }) if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT => {
                buf.insert(cpos, c);
                cpos += c.len_utf8();
            }
            Event::Key(KeyEvent { code: KeyCode::Backspace, .. }) => {
                if cpos > 0 {
                    // Find previous char boundary
                    let prev = buf[..cpos].char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
                    buf.drain(prev..cpos);
                    cpos = prev;
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Left, .. }) => {
                if cpos > 0 {
                    let cp = char_pos(&buf, cpos);
                    cpos = byte_of_char(&buf, cp - 1);
                }
                    }
            Event::Key(KeyEvent { code: KeyCode::Right, .. }) => {
                if cpos < buf.len() {
                    let cp = char_pos(&buf, cpos);
                    cpos = byte_of_char(&buf, cp + 1);
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Up, .. }) => {
                if let Some(entry) = history.up(&buf) {
                    buf = entry.to_string();
                    cpos = buf.len();
                } else {
                    redraw = false;
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Down, .. }) => {
                if let Some(entry) = history.down(&buf) {
                    buf = entry.to_string();
                    cpos = buf.len();
                } else {
                    redraw = false;
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Home, .. }) => {
                let before = &buf[..cpos];
                cpos = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
            }
            Event::Key(KeyEvent { code: KeyCode::End, .. }) => {
                let after = &buf[cpos..];
                cpos += after.find('\n').unwrap_or(after.len());
            }
            Event::Resize(w, _) => {
                width = w as usize;
                screen.erase_prompt();
                resize_pending = true;
                redraw = false;
            }
            _ => {
                redraw = false;
            }
        }

        if redraw {
            let _ = out.execute(cursor::Hide);
            screen.render(&buf, char_pos(&buf, cpos), *mode, width, &[]);
            let _ = out.execute(cursor::Show);
        }
    }

    terminal::disable_raw_mode().ok();
    Some(buf)
}

/// Handle a terminal key event during agent processing.
/// Returns (needs_redraw, cancel).
pub fn handle_term_event(
    ev: Event,
    pre_buf: &mut String,
    pre_cursor: &mut usize,
    mode: &mut Mode,
    last_esc: &mut Option<Instant>,
    queued: &mut Vec<String>,
) -> (bool, bool) {
    match ev {
        Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => {
            // If there are queued messages, single Escape unqueues them back into pre_buf
            if !queued.is_empty() {
                let mut combined = queued.join("\n");
                if !pre_buf.is_empty() {
                    combined.push('\n');
                    combined.push_str(pre_buf);
                }
                *pre_buf = combined;
                *pre_cursor = pre_buf.len();
                queued.clear();
                *last_esc = None;
                return (true, false);
            }
            // Otherwise, double Escape cancels
            if let Some(prev) = *last_esc {
                if prev.elapsed() < Duration::from_millis(500) {
                    return (false, true);
                }
            }
            *last_esc = Some(Instant::now());
            (false, false)
        }
        Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
            let text = pre_buf.trim().to_string();
            if !text.is_empty() {
                queued.push(text);
                pre_buf.clear();
                *pre_cursor = 0;
                (true, false)
            } else {
                (false, false)
            }
        }
        Event::Key(KeyEvent { code: KeyCode::BackTab, .. }) => {
            *mode = mode.toggle();
            (true, false)
        }
        Event::Key(KeyEvent {
            code: KeyCode::Char('c' | 'd'),
            modifiers: KeyModifiers::CONTROL,
            ..
        }) => (false, true),
        Event::Key(KeyEvent {
            code: KeyCode::Char('j'),
            modifiers: KeyModifiers::CONTROL,
            ..
        }) => {
            pre_buf.insert(*pre_cursor, '\n');
            *pre_cursor += 1;
            (true, false)
        }
        Event::Key(KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::CONTROL,
            ..
        }) => {
            let before = &pre_buf[..*pre_cursor];
            *pre_cursor = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
            (true, false)
        }
        Event::Key(KeyEvent {
            code: KeyCode::Char('e'),
            modifiers: KeyModifiers::CONTROL,
            ..
        }) => {
            let after = &pre_buf[*pre_cursor..];
            *pre_cursor += after.find('\n').unwrap_or(after.len());
            (true, false)
        }
        Event::Key(KeyEvent { code: KeyCode::Char(c), modifiers, .. })
            if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT =>
        {
            pre_buf.insert(*pre_cursor, c);
            *pre_cursor += c.len_utf8();
            (true, false)
        }
        Event::Key(KeyEvent { code: KeyCode::Backspace, .. }) => {
            if *pre_cursor > 0 {
                let prev = pre_buf[..*pre_cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                pre_buf.drain(prev..*pre_cursor);
                *pre_cursor = prev;
            }
            (true, false)
        }
        Event::Key(KeyEvent { code: KeyCode::Left, .. }) => {
            if *pre_cursor > 0 {
                let cp = char_pos(pre_buf, *pre_cursor);
                *pre_cursor = byte_of_char(pre_buf, cp - 1);
                (true, false)
            } else {
                (false, false)
            }
        }
        Event::Key(KeyEvent { code: KeyCode::Right, .. }) => {
            if *pre_cursor < pre_buf.len() {
                let cp = char_pos(pre_buf, *pre_cursor);
                *pre_cursor = byte_of_char(pre_buf, cp + 1);
                (true, false)
            } else {
                (false, false)
            }
        }
        Event::Key(KeyEvent { code: KeyCode::Home, .. }) => {
            let before = &pre_buf[..*pre_cursor];
            *pre_cursor = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
            (true, false)
        }
        Event::Key(KeyEvent { code: KeyCode::End, .. }) => {
            let after = &pre_buf[*pre_cursor..];
            *pre_cursor += after.find('\n').unwrap_or(after.len());
            (true, false)
        }
        Event::Resize(_, _) => (true, false),
        _ => (false, false),
    }
}
