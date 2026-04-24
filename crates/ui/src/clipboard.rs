//! System clipboard abstraction.
//!
//! Vim and emacs yank/paste sites need read/write access to the system
//! clipboard without pulling subprocess plumbing into the `ui` crate.
//! Callers in the outer crate provide a `Clipboard` implementation;
//! `VimContext` takes it by `&mut dyn Clipboard`.

/// Read/write handle on the system clipboard.
pub trait Clipboard {
    /// Read the current clipboard text, if any. `None` on failure or
    /// if the clipboard holds non-text data.
    fn read(&mut self) -> Option<String>;

    /// Write `text` to the clipboard. Errors are surfaced as strings
    /// so the UI crate stays free of platform-specific error types.
    fn write(&mut self, text: &str) -> Result<(), String>;
}

/// No-op clipboard for tests and contexts where the system clipboard
/// is unavailable. `read` always returns `None`, `write` is a no-op.
pub struct NullClipboard;

impl Clipboard for NullClipboard {
    fn read(&mut self) -> Option<String> {
        None
    }
    fn write(&mut self, _text: &str) -> Result<(), String> {
        Ok(())
    }
}
