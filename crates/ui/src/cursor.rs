#[derive(Clone, Debug, Default)]
pub struct Cursor {
    pub pos: usize,
    pub selection_anchor: Option<usize>,
    pub curswant: Option<u16>,
}

impl Cursor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        let lo = self.pos.min(anchor);
        let hi = self.pos.max(anchor);
        Some((lo, hi))
    }

    pub fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    pub fn start_selection(&mut self) {
        self.selection_anchor = Some(self.pos);
    }
}

#[derive(Clone, Debug, Default)]
pub struct Scroll {
    pub top_row: u16,
    pub pinned: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_range_ordered() {
        let mut c = Cursor::new();
        c.pos = 10;
        c.selection_anchor = Some(5);
        assert_eq!(c.selection_range(), Some((5, 10)));

        c.pos = 3;
        c.selection_anchor = Some(8);
        assert_eq!(c.selection_range(), Some((3, 8)));
    }

    #[test]
    fn no_selection() {
        let c = Cursor::new();
        assert_eq!(c.selection_range(), None);
    }
}
