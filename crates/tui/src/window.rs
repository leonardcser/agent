#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GutterSide {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowGutters {
    pub pad_left: u16,
    pub pad_right: u16,
    pub scrollbar: Option<GutterSide>,
}

impl WindowGutters {
    pub fn total(&self) -> u16 {
        self.pad_left + self.pad_right
    }

    pub fn content_width(&self, window_width: u16) -> u16 {
        window_width.saturating_sub(self.total())
    }
}
