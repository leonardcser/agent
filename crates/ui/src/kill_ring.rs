//! Emacs-style kill ring with yank-pop support.
//!
//! Accumulates killed text with a bounded history and tracks the byte range
//! of the most recent yank so successive `yank_pop` calls can rotate through
//! earlier kills in place.

const KILL_RING_MAX: usize = 32;

/// Emacs-style kill ring with yank-pop support.
///
/// Shared by the input buffer (emacs-style kills and yanks) and vim (registers
/// for `y`/`d`/`p`/`P`). The `linewise` flag lets vim distinguish line-wise
/// yanks (`Y`, `yy`, `dd`) from character-wise ones so `p`/`P` insert on a new
/// line rather than inline.
pub struct KillRing {
    current: String,
    /// Older kills, newest first.
    history: Vec<String>,
    /// Byte range of the last yank insertion, for yank-pop replacement.
    last_yank: Option<(usize, usize)>,
    pop_idx: usize,
    /// Whether the current kill was captured as a whole-line ("linewise") op.
    /// Needed for vim paste (`p`/`P`) to insert on a new line rather than
    /// inline.
    linewise: bool,
    /// Byte range in the *source* buffer the last kill was captured from.
    /// Used by the transcript yank path to map back to nav-row coordinates
    /// without the fragile `buf.find(&yanked_text)` search.
    source_range: Option<(usize, usize)>,
}

impl Default for KillRing {
    fn default() -> Self {
        Self::new()
    }
}

impl KillRing {
    pub fn new() -> Self {
        Self {
            current: String::new(),
            history: Vec::new(),
            last_yank: None,
            pop_idx: 0,
            linewise: false,
            source_range: None,
        }
    }

    /// Push a new kill, rotating the previous current into history.
    pub fn kill(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if !self.current.is_empty() {
            self.history.insert(0, std::mem::take(&mut self.current));
            if self.history.len() > KILL_RING_MAX {
                self.history.pop();
            }
        }
        self.current = text;
        self.last_yank = None;
        self.linewise = false;
        self.source_range = None;
    }

    /// Yank the current kill into `buf` at `cpos`. Returns new cpos.
    pub fn yank(&mut self, buf: &mut String, cpos: usize) -> Option<usize> {
        if self.current.is_empty() {
            return None;
        }
        buf.insert_str(cpos, &self.current);
        let end = cpos + self.current.len();
        self.last_yank = Some((cpos, end));
        self.pop_idx = 0;
        Some(end)
    }

    /// Replace the last yank with the next history entry. Returns new cpos.
    pub fn yank_pop(&mut self, buf: &mut String) -> Option<usize> {
        let (start, end) = self.last_yank?;
        if self.history.is_empty() {
            return None;
        }
        let text = &self.history[self.pop_idx % self.history.len()];
        let new_end = start + text.len();
        buf.replace_range(start..end, text);
        self.last_yank = Some((start, new_end));
        self.pop_idx = (self.pop_idx + 1) % self.history.len();
        Some(new_end)
    }

    /// Clear last-yank tracking (call on any non-yank editing action).
    pub fn clear_yank(&mut self) {
        self.last_yank = None;
    }

    /// Take the current kill text (for dialog sync).
    pub fn take(&mut self) -> String {
        self.linewise = false;
        self.source_range = None;
        std::mem::take(&mut self.current)
    }

    /// Set the current kill text (for dialog sync / emacs-style copy).
    /// Clears the linewise flag.
    pub fn set(&mut self, text: String) {
        self.current = text;
        self.linewise = false;
        self.source_range = None;
    }

    /// Set the current kill text along with an explicit linewise flag (used
    /// by vim yank operations).
    pub fn set_with_linewise(&mut self, text: String, linewise: bool) {
        self.current = text;
        self.linewise = linewise;
    }

    /// Set the current kill text with linewise flag and source byte range.
    pub fn set_with_source(&mut self, text: String, linewise: bool, start: usize, end: usize) {
        self.current = text;
        self.linewise = linewise;
        self.source_range = Some((start, end));
    }

    pub fn current(&self) -> &str {
        &self.current
    }

    pub fn is_linewise(&self) -> bool {
        self.linewise
    }

    pub fn source_range(&self) -> Option<(usize, usize)> {
        self.source_range
    }
}
