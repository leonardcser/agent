//! Emacs-style kill ring with yank-pop support.
//!
//! Accumulates killed text with a bounded history and tracks the byte range
//! of the most recent yank so successive `yank_pop` calls can rotate through
//! earlier kills in place.

use std::time::{Duration, Instant};

const KILL_RING_MAX: usize = 32;

/// How long the post-yank highlight flash stays on screen. Matches
/// nvim's `vim.highlight.on_yank` default.
pub const YANK_FLASH_DURATION: Duration = Duration::from_millis(200);

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
    /// Timestamp of the most recent yank operation (set explicitly by
    /// `mark_yanked` after vim `y`-family ops complete). Used to drive
    /// a brief post-yank highlight flash on the source range. Cleared
    /// implicitly by the flash window expiring.
    last_yank_at: Option<Instant>,
    /// Exact text last pushed to the system clipboard from this kill
    /// ring. Paste sites compare `clipboard.read()` to this value to
    /// decide between "kill ring is authoritative" (preserves vim
    /// linewise flag + yank-pop history) and "clipboard was updated
    /// externally" (overwrite kill ring with the clipboard text).
    last_clipboard_write: Option<String>,
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
            last_yank_at: None,
            last_clipboard_write: None,
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
    /// Clears `last_yank_at` so a delete / change doesn't inherit a prior
    /// yank's flash window — yank-only sites must call `mark_yanked`
    /// after this to re-stamp the timestamp.
    pub fn set_with_source(&mut self, text: String, linewise: bool, start: usize, end: usize) {
        self.current = text;
        self.linewise = linewise;
        self.source_range = Some((start, end));
        self.last_yank_at = None;
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

    /// Mark the most recent kill as a *yank* (vs a delete / change).
    /// Drives the post-yank highlight flash. Vim yank operations call
    /// this immediately after `set_with_source`; delete / change do
    /// not, so the flash only fires on actual copies.
    pub fn mark_yanked(&mut self) {
        self.last_yank_at = Some(Instant::now());
    }

    /// Source range of the most recent yank if its flash window is
    /// still active. Renderers overlay selection-bg on this range to
    /// reproduce nvim's `vim.highlight.on_yank` effect.
    pub fn yank_flash_range(&self, now: Instant) -> Option<(usize, usize)> {
        let started = self.last_yank_at?;
        let range = self.source_range?;
        if now.duration_since(started) < YANK_FLASH_DURATION {
            Some(range)
        } else {
            None
        }
    }

    /// Earliest `Instant` at which the flash window expires, if a flash
    /// is currently active. The render loop uses this to keep ticking
    /// at frame rate while the flash is visible so it clears promptly.
    pub fn yank_flash_until(&self) -> Option<Instant> {
        self.last_yank_at.map(|t| t + YANK_FLASH_DURATION)
    }

    /// Record that we just pushed `text` to the system clipboard. The
    /// next paste compares `clipboard.read()` against this value: a
    /// match means our push is still current (trust kill ring's
    /// linewise flag); a mismatch means the clipboard was updated
    /// externally and the new text should overwrite the kill ring.
    pub fn record_clipboard_write(&mut self, text: String) {
        self.last_clipboard_write = Some(text);
    }

    pub fn last_clipboard_write(&self) -> Option<&str> {
        self.last_clipboard_write.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flash_range_active_only_after_mark_yanked() {
        let mut kr = KillRing::new();
        kr.set_with_source("hello".into(), false, 3, 8);
        // set_with_source alone (delete / change) must not flash.
        assert!(kr.yank_flash_range(Instant::now()).is_none());
        // Yank-only sites mark explicitly.
        kr.mark_yanked();
        assert_eq!(kr.yank_flash_range(Instant::now()), Some((3, 8)));
    }

    #[test]
    fn flash_range_expires_after_window() {
        let mut kr = KillRing::new();
        kr.set_with_source("x".into(), false, 0, 1);
        kr.mark_yanked();
        let later = Instant::now() + YANK_FLASH_DURATION + Duration::from_millis(50);
        assert!(kr.yank_flash_range(later).is_none());
    }

    #[test]
    fn delete_after_yank_clears_flash() {
        // Regression: a delete that piggybacks on `set_with_source`
        // must not inherit the prior yank's flash timestamp, or the
        // selection-bg would briefly paint over the deletion target.
        let mut kr = KillRing::new();
        kr.set_with_source("first".into(), false, 0, 5);
        kr.mark_yanked();
        assert!(kr.yank_flash_range(Instant::now()).is_some());
        // Subsequent delete-style update — no mark_yanked.
        kr.set_with_source("second".into(), false, 10, 16);
        assert!(kr.yank_flash_range(Instant::now()).is_none());
    }
}
