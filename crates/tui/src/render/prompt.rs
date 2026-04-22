pub(super) struct PromptState {
    /// Persisted scroll offset for multi-line input (vim-style viewport).
    pub input_scroll: usize,
    /// Buffer viewport for the input text area, recorded after paint.
    pub viewport: Option<super::region::Viewport>,
}

impl PromptState {
    pub fn new() -> Self {
        Self {
            input_scroll: 0,
            viewport: None,
        }
    }
}
