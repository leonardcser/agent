pub(super) struct PromptState {
    pub drawn: bool,
    pub dirty: bool,
    pub prev_rows: u16,
    /// Just the prompt UI rows from the previous frame (input +
    /// queued + notifications + status bar), excluding ephemeral
    /// overlay rows and the gap above. Used by `draw_frame` as the
    /// bottom-of-viewport reserve for the overlay so it doesn't paint
    /// over the prompt.
    pub prev_prompt_ui_rows: u16,
    /// Where the next frame starts drawing. Updated at the end of every
    /// `draw_frame` call (always fresh). On first frame or after clear,
    /// falls back to `cursor::position()` once.
    pub anchor_row: Option<u16>,
    /// Computed each frame inside `draw_frame`, exposed via `dialog_row()`
    /// getter for the app loop.
    pub prev_dialog_row: Option<u16>,
    /// Dialog height from the last full layout pass. When the dialog
    /// height changes, the early-exit path must be skipped to
    /// recompute scroll and placement.
    pub prev_dialog_height: u16,
    /// Whether the last dialog-mode frame drew a 1-row gap between the
    /// content above and the dialog.  When the gap was drawn and the
    /// dialog scrolled the content into scrollback, the gap lands as a
    /// trailing blank in scrollback — the next block render must
    /// suppress its own leading gap to avoid duplication.  Fullscreen
    /// dialogs omit the gap, so no suppression is needed.
    pub prev_dialog_gap: u16,
    /// Persisted scroll offset for multi-line input (vim-style viewport).
    pub input_scroll: usize,
    /// Screen position `(col, row)` of the software block cursor from
    /// the last prompt frame. Used to erase it on exit.
    pub soft_cursor: Option<(u16, u16)>,
    /// Buffer viewport for the input text area, recorded after paint.
    pub viewport: Option<super::region::Viewport>,
}

impl PromptState {
    pub fn new() -> Self {
        Self {
            drawn: false,
            dirty: true,
            prev_rows: 0,
            prev_prompt_ui_rows: 0,
            anchor_row: None,
            prev_dialog_row: None,
            prev_dialog_height: 0,
            prev_dialog_gap: 0,
            input_scroll: 0,
            soft_cursor: None,
            viewport: None,
        }
    }
}
