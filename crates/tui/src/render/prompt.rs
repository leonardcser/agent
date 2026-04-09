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
    /// Persisted scroll offset for multi-line input (vim-style viewport).
    pub input_scroll: usize,
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
            input_scroll: 0,
        }
    }
}
