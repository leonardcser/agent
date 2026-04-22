use crate::layout::Rect;
use crossterm::style::Color;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub crossedout: bool,
}

impl Style {
    pub fn fg(color: Color) -> Self {
        Self {
            fg: Some(color),
            ..Default::default()
        }
    }

    pub fn bg(color: Color) -> Self {
        Self {
            bg: Some(color),
            ..Default::default()
        }
    }

    pub fn bold() -> Self {
        Self {
            bold: true,
            ..Default::default()
        }
    }

    pub fn dim() -> Self {
        Self {
            dim: true,
            ..Default::default()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub symbol: char,
    pub style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            symbol: ' ',
            style: Style::default(),
        }
    }
}

pub struct Grid {
    cells: Vec<Cell>,
    width: u16,
    height: u16,
}

impl Grid {
    pub fn new(width: u16, height: u16) -> Self {
        let len = width as usize * height as usize;
        Self {
            cells: vec![Cell::default(); len],
            width,
            height,
        }
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.cells
            .resize(width as usize * height as usize, Cell::default());
        self.clear_all();
    }

    pub fn cell(&self, x: u16, y: u16) -> &Cell {
        &self.cells[self.idx(x, y)]
    }

    pub fn cell_mut(&mut self, x: u16, y: u16) -> &mut Cell {
        let idx = self.idx(x, y);
        &mut self.cells[idx]
    }

    pub fn set(&mut self, x: u16, y: u16, symbol: char, style: Style) {
        use unicode_width::UnicodeWidthChar;
        if x < self.width && y < self.height {
            let idx = self.idx(x, y);
            self.cells[idx] = Cell { symbol, style };
            // Wide-char invariant: the cell immediately after a width-2
            // glyph holds a `\0` continuation marker so the flush and
            // diff paths can skip it (the terminal renders the wide
            // glyph across both visual cells on its own). Marker
            // inherits the wide char's style so inclusion in a styled
            // pill stays visually consistent.
            if UnicodeWidthChar::width(symbol).unwrap_or(1) == 2 && x + 1 < self.width {
                let cont = self.idx(x + 1, y);
                self.cells[cont] = Cell {
                    symbol: '\0',
                    style,
                };
            }
        }
    }

    pub fn put_str(&mut self, x: u16, y: u16, text: &str, style: Style) {
        use unicode_width::UnicodeWidthChar;

        if y >= self.height {
            return;
        }
        let mut col = x;
        for ch in text.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(1).max(1) as u16;
            if col + cw > self.width {
                break;
            }
            let idx = self.idx(col, y);
            self.cells[idx] = Cell { symbol: ch, style };
            col += cw;
        }
    }

    pub fn fill(&mut self, area: Rect, symbol: char, style: Style) {
        for row in area.top..area.bottom().min(self.height) {
            for col in area.left..area.right().min(self.width) {
                let idx = self.idx(col, row);
                self.cells[idx] = Cell { symbol, style };
            }
        }
    }

    pub fn clear(&mut self, area: Rect) {
        self.fill(area, ' ', Style::default());
    }

    pub fn clear_all(&mut self) {
        for cell in &mut self.cells {
            *cell = Cell::default();
        }
    }

    pub fn slice_mut(&mut self, area: Rect) -> GridSlice<'_> {
        let area = Rect::new(
            area.top.min(self.height),
            area.left.min(self.width),
            area.width.min(self.width.saturating_sub(area.left)),
            area.height.min(self.height.saturating_sub(area.top)),
        );
        GridSlice { grid: self, area }
    }

    pub fn diff<'a>(&'a self, prev: &'a Grid) -> impl Iterator<Item = CellUpdate<'a>> {
        self.cells.iter().enumerate().filter_map(move |(i, cell)| {
            // Wide-char continuation cells (`\0` sentinel) are never
            // flushed — the preceding wide glyph paints both visual
            // columns. Emitting them would either overwrite the
            // continuation (clobbering the glyph) or desync the
            // terminal cursor.
            if cell.symbol == '\0' {
                return None;
            }
            let prev_cell = prev.cells.get(i)?;
            if cell != prev_cell {
                let x = (i % self.width as usize) as u16;
                let y = (i / self.width as usize) as u16;
                Some(CellUpdate { x, y, cell })
            } else {
                None
            }
        })
    }

    pub fn swap_with(&mut self, other: &mut Grid) {
        std::mem::swap(&mut self.cells, &mut other.cells);
        std::mem::swap(&mut self.width, &mut other.width);
        std::mem::swap(&mut self.height, &mut other.height);
    }

    fn idx(&self, x: u16, y: u16) -> usize {
        y as usize * self.width as usize + x as usize
    }
}

pub struct CellUpdate<'a> {
    pub x: u16,
    pub y: u16,
    pub cell: &'a Cell,
}

pub struct GridSlice<'a> {
    grid: &'a mut Grid,
    area: Rect,
}

impl<'a> GridSlice<'a> {
    pub fn width(&self) -> u16 {
        self.area.width
    }

    pub fn height(&self) -> u16 {
        self.area.height
    }

    pub fn area(&self) -> Rect {
        self.area
    }

    pub fn set(&mut self, x: u16, y: u16, symbol: char, style: Style) {
        if x < self.area.width && y < self.area.height {
            self.grid
                .set(self.area.left + x, self.area.top + y, symbol, style);
        }
    }

    /// Read a cell from the underlying grid at slice-local coords.
    /// Returns the default `Cell` when out of bounds.
    pub fn cell(&self, x: u16, y: u16) -> Cell {
        if x < self.area.width && y < self.area.height {
            *self.grid.cell(self.area.left + x, self.area.top + y)
        } else {
            Cell::default()
        }
    }

    /// Change the style of a cell in place, preserving its symbol.
    /// Used to overlay effects (selection highlight, cursor tint).
    pub fn set_style(&mut self, x: u16, y: u16, style: Style) {
        if x < self.area.width && y < self.area.height {
            let abs_x = self.area.left + x;
            let abs_y = self.area.top + y;
            self.grid.cell_mut(abs_x, abs_y).style = style;
        }
    }

    pub fn put_str(&mut self, x: u16, y: u16, text: &str, style: Style) {
        if y >= self.area.height {
            return;
        }
        let abs_y = self.area.top + y;
        for (col, ch) in (x..).zip(text.chars()) {
            if col >= self.area.width {
                break;
            }
            self.grid.set(self.area.left + col, abs_y, ch, style);
        }
    }

    pub fn fill(&mut self, area: Rect, symbol: char, style: Style) {
        let abs = Rect::new(
            self.area.top + area.top,
            self.area.left + area.left,
            area.width.min(self.area.width.saturating_sub(area.left)),
            area.height.min(self.area.height.saturating_sub(area.top)),
        );
        self.grid.fill(abs, symbol, style);
    }

    pub fn clear(&mut self) {
        self.grid.clear(self.area);
    }

    pub fn sub_slice(&mut self, area: Rect) -> GridSlice<'_> {
        let abs = Rect::new(
            self.area.top + area.top.min(self.area.height),
            self.area.left + area.left.min(self.area.width),
            area.width.min(self.area.width.saturating_sub(area.left)),
            area.height.min(self.area.height.saturating_sub(area.top)),
        );
        GridSlice {
            grid: self.grid,
            area: abs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_grid_filled_with_spaces() {
        let grid = Grid::new(10, 5);
        assert_eq!(grid.width(), 10);
        assert_eq!(grid.height(), 5);
        assert_eq!(grid.cell(0, 0).symbol, ' ');
        assert_eq!(grid.cell(9, 4).symbol, ' ');
    }

    #[test]
    fn set_and_read_cell() {
        let mut grid = Grid::new(10, 5);
        let style = Style::fg(Color::Red);
        grid.set(3, 2, 'X', style);
        assert_eq!(grid.cell(3, 2).symbol, 'X');
        assert_eq!(grid.cell(3, 2).style.fg, Some(Color::Red));
    }

    #[test]
    fn put_str_writes_chars() {
        let mut grid = Grid::new(10, 5);
        grid.put_str(2, 1, "hello", Style::default());
        assert_eq!(grid.cell(2, 1).symbol, 'h');
        assert_eq!(grid.cell(3, 1).symbol, 'e');
        assert_eq!(grid.cell(6, 1).symbol, 'o');
        assert_eq!(grid.cell(7, 1).symbol, ' ');
    }

    #[test]
    fn put_str_clips_at_width() {
        let mut grid = Grid::new(5, 1);
        grid.put_str(3, 0, "hello", Style::default());
        assert_eq!(grid.cell(3, 0).symbol, 'h');
        assert_eq!(grid.cell(4, 0).symbol, 'e');
    }

    #[test]
    fn fill_region() {
        let mut grid = Grid::new(10, 5);
        let style = Style::bg(Color::Blue);
        grid.fill(Rect::new(1, 2, 3, 2), '#', style);
        assert_eq!(grid.cell(2, 1).symbol, '#');
        assert_eq!(grid.cell(4, 2).symbol, '#');
        assert_eq!(grid.cell(5, 1).symbol, ' ');
    }

    #[test]
    fn diff_yields_changed_cells() {
        let prev = Grid::new(5, 3);
        let mut curr = Grid::new(5, 3);
        curr.set(1, 0, 'A', Style::default());
        curr.set(3, 2, 'B', Style::default());

        let updates: Vec<_> = curr.diff(&prev).collect();
        assert_eq!(updates.len(), 2);
        assert_eq!((updates[0].x, updates[0].y), (1, 0));
        assert_eq!((updates[1].x, updates[1].y), (3, 2));
    }

    #[test]
    fn diff_empty_for_identical_grids() {
        let a = Grid::new(5, 3);
        let b = Grid::new(5, 3);
        assert_eq!(a.diff(&b).count(), 0);
    }

    #[test]
    fn slice_writes_offset_correctly() {
        let mut grid = Grid::new(20, 10);
        let area = Rect::new(2, 5, 10, 4);
        {
            let mut slice = grid.slice_mut(area);
            assert_eq!(slice.width(), 10);
            assert_eq!(slice.height(), 4);
            slice.set(0, 0, 'A', Style::default());
            slice.put_str(1, 1, "hi", Style::default());
        }
        assert_eq!(grid.cell(5, 2).symbol, 'A');
        assert_eq!(grid.cell(6, 3).symbol, 'h');
        assert_eq!(grid.cell(7, 3).symbol, 'i');
    }

    #[test]
    fn slice_clips_to_bounds() {
        let mut grid = Grid::new(10, 5);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 3, 2));
        slice.put_str(0, 0, "hello world", Style::default());
        assert_eq!(grid.cell(2, 0).symbol, 'l');
        assert_eq!(grid.cell(3, 0).symbol, ' ');
    }

    #[test]
    fn sub_slice_offsets_compound() {
        let mut grid = Grid::new(20, 10);
        let mut slice = grid.slice_mut(Rect::new(2, 3, 10, 6));
        {
            let mut sub = slice.sub_slice(Rect::new(1, 1, 4, 3));
            sub.set(0, 0, 'X', Style::default());
        }
        assert_eq!(grid.cell(4, 3).symbol, 'X');
    }

    #[test]
    fn resize_clears_grid() {
        let mut grid = Grid::new(5, 3);
        grid.set(2, 1, 'A', Style::default());
        grid.resize(10, 5);
        assert_eq!(grid.width(), 10);
        assert_eq!(grid.height(), 5);
        assert_eq!(grid.cell(2, 1).symbol, ' ');
    }

    #[test]
    fn swap_grids() {
        let mut a = Grid::new(5, 3);
        let mut b = Grid::new(5, 3);
        a.set(0, 0, 'A', Style::default());
        b.set(0, 0, 'B', Style::default());
        a.swap_with(&mut b);
        assert_eq!(a.cell(0, 0).symbol, 'B');
        assert_eq!(b.cell(0, 0).symbol, 'A');
    }
}
