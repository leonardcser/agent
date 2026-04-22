#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GutterSide {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowGutters {
    pub pad_left: u16,
    pub scrollbar: Option<GutterSide>,
}

pub const TRANSCRIPT_GUTTERS: WindowGutters = WindowGutters {
    pad_left: 1,
    scrollbar: Some(GutterSide::Right),
};

impl WindowGutters {
    pub fn scrollbar_width(&self) -> u16 {
        if self.scrollbar.is_some() {
            1
        } else {
            0
        }
    }

    pub fn layer_width(&self, term_width: u16) -> u16 {
        term_width.saturating_sub(self.pad_left)
    }

    pub fn content_width(&self, term_width: u16) -> u16 {
        self.layer_width(term_width)
            .saturating_sub(self.scrollbar_width())
    }
}
