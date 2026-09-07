use super::geometry::Rect;
use compact_str::CompactString;
use smelt_style::cell_width;
pub use smelt_style::style::{Color, Style};

/// Convert `Color` to crossterm's `Color` at the SGR-emit boundary.
/// A free function because the orphan rule prevents a `From` impl.
pub(crate) fn to_crossterm_color(c: Color) -> crossterm::style::Color {
    use crossterm::style::Color as X;
    match c {
        Color::Reset => X::Reset,
        Color::Black => X::Black,
        Color::DarkGrey => X::DarkGrey,
        Color::Red => X::Red,
        Color::DarkRed => X::DarkRed,
        Color::Green => X::Green,
        Color::DarkGreen => X::DarkGreen,
        Color::Yellow => X::Yellow,
        Color::DarkYellow => X::DarkYellow,
        Color::Blue => X::Blue,
        Color::DarkBlue => X::DarkBlue,
        Color::Magenta => X::Magenta,
        Color::DarkMagenta => X::DarkMagenta,
        Color::Cyan => X::Cyan,
        Color::DarkCyan => X::DarkCyan,
        Color::White => X::White,
        Color::Grey => X::Grey,
        Color::Rgb { r, g, b } => X::Rgb { r, g, b },
        Color::AnsiValue(v) => X::AnsiValue(v),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

pub fn display_width(text: &str) -> u16 {
    cell_width::text_width_u16(text)
}

pub fn truncate_width(text: &str, max_width: u16) -> String {
    let mut out = String::new();
    write_text(0, max_width, text, |_, grapheme| out.push_str(grapheme));
    out
}

fn symbol_width(symbol: &str) -> u16 {
    cell_width::text_width_u16(symbol)
}

fn write_text(mut col: u16, limit: u16, text: &str, mut write: impl FnMut(u16, &str)) -> u16 {
    for grapheme in cell_width::graphemes(text) {
        let width = symbol_width(grapheme);
        if col.saturating_add(width) > limit {
            break;
        }
        write(col, grapheme);
        col = col.saturating_add(width);
    }
    col.min(limit)
}

fn write_line(
    mut col: u16,
    limit: u16,
    line: &crate::line::Line<'_>,
    mut write: impl FnMut(u16, &str, Style),
) -> u16 {
    let capacity = line.spans.iter().map(|span| span.text.len()).sum();
    let mut text = String::with_capacity(capacity);
    let mut span_ends = Vec::with_capacity(line.spans.len());
    for span in &line.spans {
        text.push_str(span.text.as_ref());
        span_ends.push((text.len(), span.style));
    }

    let mut span_index = 0;
    for (byte, grapheme) in cell_width::grapheme_indices(&text) {
        while span_ends
            .get(span_index)
            .is_some_and(|(end, _)| byte >= *end)
        {
            span_index += 1;
        }
        let width = symbol_width(grapheme);
        if col.saturating_add(width) > limit {
            break;
        }
        let style = span_ends
            .get(span_index)
            .map(|(_, style)| *style)
            .unwrap_or_default();
        write(col, grapheme, style);
        col = col.saturating_add(width);
    }
    col.min(limit)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellSymbol(CompactString);

impl CellSymbol {
    const CONTINUATION: &'static str = "\0";

    pub(crate) fn new(symbol: &str) -> Self {
        Self(CompactString::from(symbol))
    }

    fn continuation() -> Self {
        Self::new(Self::CONTINUATION)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn is_continuation(&self) -> bool {
        self.as_str() == Self::CONTINUATION
    }

    pub fn width(&self) -> u16 {
        if self.is_continuation() {
            0
        } else {
            symbol_width(self.as_str())
        }
    }

    fn push_str(&mut self, suffix: &str) {
        self.0.push_str(suffix);
    }
}

impl PartialEq<char> for CellSymbol {
    fn eq(&self, other: &char) -> bool {
        let mut chars = self.as_str().chars();
        chars.next() == Some(*other) && chars.next().is_none()
    }
}

impl PartialEq<&str> for CellSymbol {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub symbol: CellSymbol,
    pub style: Style,
}

impl Cell {
    fn new(symbol: &str, style: Style) -> Self {
        Self {
            symbol: CellSymbol::new(symbol),
            style,
        }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self::new(" ", Style::default())
    }
}

#[derive(Clone)]
pub struct Grid {
    cells: Vec<Cell>,
    width: u16,
    height: u16,
    terminal_cursor_position: Option<(u16, u16)>,
}

impl Grid {
    pub fn new(width: u16, height: u16) -> Self {
        let len = width as usize * height as usize;
        Self {
            cells: vec![Cell::default(); len],
            width,
            height,
            terminal_cursor_position: None,
        }
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    /// Position where the hidden terminal cursor should be restored after this
    /// frame. Terminal emulators use it to anchor input-method preedit text even
    /// though the application paints its own visible cursor into the grid.
    pub fn terminal_cursor_position(&self) -> Option<(u16, u16)> {
        self.terminal_cursor_position
    }

    /// Set the hidden terminal cursor's screen position for this frame.
    pub fn set_terminal_cursor_position(&mut self, x: u16, y: u16) {
        if x < self.width && y < self.height {
            self.terminal_cursor_position = Some((x, y));
        }
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

    /// Mutable cell access; returns `None` when out of bounds.
    ///
    /// Escape hatch that **bypasses** the wide-char continuation invariant
    /// upheld by `set` / `put_char` / `fill`. Callers must own the entire
    /// region they write into and re-establish the invariant themselves. For
    /// any normal painting, prefer `set` / `put_str` / `put_char` / `fill`.
    pub fn cell_mut(&mut self, x: u16, y: u16) -> Option<&mut Cell> {
        if x < self.width && y < self.height {
            let idx = self.idx(x, y);
            Some(&mut self.cells[idx])
        } else {
            None
        }
    }

    pub fn set(&mut self, x: u16, y: u16, symbol: char, style: Style) {
        let mut buf = [0; 4];
        self.set_symbol(x, y, symbol.encode_utf8(&mut buf), style);
    }

    pub fn set_symbol(&mut self, x: u16, y: u16, symbol: &str, style: Style) {
        if y >= self.height {
            return;
        }
        let mut graphemes = cell_width::graphemes(symbol);
        let Some(first) = graphemes.next() else {
            return;
        };
        if graphemes.next().is_none() {
            self.write_symbol_clipped(x, y, first, style, self.width);
        } else {
            write_text(x, self.width, symbol, |col, grapheme| {
                self.write_symbol_clipped(col, y, grapheme, style, self.width)
            });
        }
    }

    fn write_symbol_clipped(
        &mut self,
        x: u16,
        y: u16,
        symbol: &str,
        style: Style,
        right_limit: u16,
    ) {
        if symbol_width(symbol) == 0 {
            self.append_zero_width(x, y, symbol, right_limit);
        } else {
            self.write_cell_clipped(x, y, Cell::new(symbol, style), right_limit);
        }
    }

    fn append_zero_width(&mut self, x: u16, y: u16, symbol: &str, right_limit: u16) {
        if x == 0 || x > self.width || y >= self.height {
            return;
        }
        let mut lead_x = x - 1;
        if self.cell(lead_x, y).symbol.is_continuation() {
            let Some(previous) = lead_x.checked_sub(1) else {
                return;
            };
            lead_x = previous;
        }
        let idx = self.idx(lead_x, y);
        if self.cells[idx].symbol.is_continuation() {
            return;
        }

        let mut combined = self.cells[idx].symbol.clone();
        combined.push_str(symbol);
        let style = self.cells[idx].style;
        self.write_cell_clipped(lead_x, y, Cell::new(combined.as_str(), style), right_limit);
    }

    /// Single mutation entry point. Every cell write - `set`, `put_char`,
    /// `fill`, and their slice equivalents - funnels through here so the
    /// wide-char continuation invariant is upheld in exactly one place:
    ///
    /// 1. A stored wide char at `(x, y)` implies `(x+1, y).symbol == '\0'`.
    /// 2. `(x, y).symbol == '\0'` implies `(x-1, y)` holds a wide char.
    /// 3. A wide char that cannot fit before the active right boundary is
    ///    stored as a styled space, never as a half-wide glyph.
    ///
    /// Three transitions need extra bookkeeping to keep the invariant:
    ///
    /// - Overwriting the LEADING half of a wide char (the new cell is
    ///   narrow): clear the stale `\0` at `x+1` to a plain space so the
    ///   diff doesn't skip it and the previous frame's right-half doesn't
    ///   leak through.
    /// - Overwriting a CONTINUATION cell (was `\0`): break the now-orphaned
    ///   wide glyph at `x-1` to a plain space; otherwise the diff still
    ///   thinks the wide is intact.
    /// - Writing a wide char where `x+1` was itself the leading half of
    ///   another wide char: clear that displaced wide's continuation at
    ///   `x+2` so we don't leave a half-wide hanging.
    ///
    /// Cleared cells inherit the new cell's style so a styled background
    /// stays visually contiguous across the now-narrow run.
    fn write_cell_clipped(&mut self, x: u16, y: u16, new_cell: Cell, right_limit: u16) {
        if x >= self.width || y >= self.height {
            return;
        }
        let right_limit = right_limit.min(self.width);
        let new_cell = if new_cell.symbol.width() == 2 && x + 1 >= right_limit {
            Cell::new(" ", new_cell.style)
        } else {
            new_cell
        };
        let new_width = new_cell.symbol.width();
        let new_style = new_cell.style;
        let idx = self.idx(x, y);
        let old_width = self.cells[idx].symbol.width();
        let old_was_continuation = self.cells[idx].symbol.is_continuation();

        if x + 1 < self.width && old_width == 2 {
            let cont = self.idx(x + 1, y);
            if self.cells[cont].symbol.is_continuation() {
                self.cells[cont] = Cell::new(" ", new_style);
            }
        }
        if old_was_continuation && x > 0 {
            let lead = self.idx(x - 1, y);
            if self.cells[lead].symbol.width() == 2 {
                self.cells[lead] = Cell::new(" ", new_style);
            }
        }

        self.cells[idx] = new_cell;

        if new_width == 2 && x + 1 < self.width {
            let cont = self.idx(x + 1, y);
            let displaced_width = self.cells[cont].symbol.width();
            if displaced_width == 2 && x + 2 < self.width {
                let dispcont = self.idx(x + 2, y);
                if self.cells[dispcont].symbol.is_continuation() {
                    self.cells[dispcont] = Cell::new(" ", new_style);
                }
            }
            self.cells[cont] = Cell {
                symbol: CellSymbol::continuation(),
                style: new_style,
            };
        }
    }

    pub fn put_str(&mut self, x: u16, y: u16, text: &str, style: Style) -> u16 {
        if y >= self.height {
            return x.min(self.width);
        }
        write_text(x, self.width, text, |col, grapheme| {
            self.write_symbol_clipped(col, y, grapheme, style, self.width)
        })
    }

    /// Overwrites `symbol` and `style.fg`; preserves the existing cell's
    /// `bg` and text attributes. Use for fg-only painting over a filled background.
    pub fn put_char(&mut self, x: u16, y: u16, symbol: char, fg: Color) {
        self.put_char_clipped(x, y, symbol, fg, self.width);
    }

    fn put_char_clipped(&mut self, x: u16, y: u16, symbol: char, fg: Color, right_limit: u16) {
        let mut buf = [0; 4];
        self.put_symbol_clipped(x, y, symbol.encode_utf8(&mut buf), fg, right_limit);
    }

    fn put_symbol_clipped(&mut self, x: u16, y: u16, symbol: &str, fg: Color, right_limit: u16) {
        if x >= self.width || y >= self.height {
            return;
        }
        if symbol_width(symbol) == 0 {
            self.append_zero_width(x, y, symbol, right_limit);
            return;
        }
        let idx = self.idx(x, y);
        let mut style = self.cells[idx].style;
        style.fg = Some(fg);
        self.write_cell_clipped(x, y, Cell::new(symbol, style), right_limit);
    }

    /// String form of [`Grid::put_char`]: overwrites symbol + fg, preserves bg and attrs.
    pub fn put_str_fg(&mut self, x: u16, y: u16, text: &str, fg: Color) -> u16 {
        if y >= self.height {
            return x.min(self.width);
        }
        write_text(x, self.width, text, |col, grapheme| {
            self.put_symbol_clipped(col, y, grapheme, fg, self.width)
        })
    }

    /// Paint a [`Line`](crate::line::Line) of styled spans at `(x, y)`, clipping at the right edge.
    /// Graphemes that cross span boundaries use the style of their first scalar.
    pub fn put_line(&mut self, x: u16, y: u16, line: &crate::line::Line<'_>) -> u16 {
        if y >= self.height {
            return x.min(self.width);
        }
        write_line(x, self.width, line, |col, grapheme, style| {
            self.write_symbol_clipped(col, y, grapheme, style, self.width)
        })
    }

    pub fn fill(&mut self, area: Rect, symbol: char, style: Style) {
        let mut buf = [0; 4];
        let symbol = symbol.encode_utf8(&mut buf);
        let symbol = if symbol_width(symbol) == 0 {
            " "
        } else {
            symbol
        };
        let new_cell = Cell::new(symbol, style);
        let right = area.right().min(self.width);
        let width = new_cell.symbol.width();
        for row in area.top..area.bottom().min(self.height) {
            let mut col = area.left;
            while col < right {
                self.write_cell_clipped(col, row, new_cell.clone(), right);
                let step = if width == 2 && col.saturating_add(1) < right {
                    2
                } else {
                    1
                };
                col = col.saturating_add(step);
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
        self.terminal_cursor_position = None;
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
            // Skip wide-char continuation cells (`\0`); the preceding wide
            // glyph already covers both visual columns on the terminal.
            if cell.symbol.is_continuation() {
                return None;
            }
            let prev_cell = prev.cells.get(i)?;
            if cell != prev_cell {
                let x = (i % self.width as usize) as u16;
                let y = (i / self.width as usize) as u16;
                Some(CellUpdate {
                    x,
                    y,
                    row_width: self.width,
                    cell,
                })
            } else {
                None
            }
        })
    }

    pub fn swap_with(&mut self, other: &mut Grid) {
        std::mem::swap(&mut self.cells, &mut other.cells);
        std::mem::swap(&mut self.width, &mut other.width);
        std::mem::swap(&mut self.height, &mut other.height);
        std::mem::swap(
            &mut self.terminal_cursor_position,
            &mut other.terminal_cursor_position,
        );
    }

    fn idx(&self, x: u16, y: u16) -> usize {
        y as usize * self.width as usize + x as usize
    }
}

pub struct CellUpdate<'a> {
    pub x: u16,
    pub y: u16,
    pub row_width: u16,
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

    /// Absolute rect this slice covers in the underlying grid.
    pub fn grid_rect(&self) -> Rect {
        self.area
    }

    /// Set the hidden terminal cursor using slice-local coordinates.
    pub fn set_terminal_cursor_position(&mut self, x: u16, y: u16) {
        if x < self.area.width && y < self.area.height {
            self.grid.set_terminal_cursor_position(
                self.area.left.saturating_add(x),
                self.area.top.saturating_add(y),
            );
        }
    }

    pub fn to_grid_rect(&self, rect: Rect) -> Rect {
        rect.to_grid(self.area)
    }

    pub fn to_local_rect(&self, rect: Rect) -> Rect {
        rect.to_local(self.area)
    }

    pub fn slice_mut(&mut self, area: Rect) -> GridSlice<'_> {
        let parent = Rect::new(0, 0, self.area.width, self.area.height);
        let clipped = area.clip_to(parent);
        let area = Rect::new(
            self.area.top.saturating_add(clipped.top),
            self.area.left.saturating_add(clipped.left),
            clipped.width,
            clipped.height,
        );
        GridSlice {
            grid: self.grid,
            area,
        }
    }

    pub fn set(&mut self, x: u16, y: u16, symbol: char, style: Style) {
        let mut buf = [0; 4];
        self.set_symbol(x, y, symbol.encode_utf8(&mut buf), style);
    }

    pub fn set_symbol(&mut self, x: u16, y: u16, symbol: &str, style: Style) {
        if x >= self.area.width || y >= self.area.height {
            return;
        }
        let mut graphemes = cell_width::graphemes(symbol);
        let Some(first) = graphemes.next() else {
            return;
        };
        if graphemes.next().is_none() {
            self.grid.write_symbol_clipped(
                self.area.left + x,
                self.area.top + y,
                first,
                style,
                self.area.right(),
            );
        } else {
            self.write_str_at(x, y, symbol, None, style);
        }
    }

    /// Read a cell at slice-local coords; returns default `Cell` when out of bounds.
    pub fn cell(&self, x: u16, y: u16) -> Cell {
        if x < self.area.width && y < self.area.height {
            self.grid
                .cell(self.area.left + x, self.area.top + y)
                .clone()
        } else {
            Cell::default()
        }
    }

    /// Mutable cell access at slice-local coords. See [`Grid::cell_mut`] for caveats.
    pub fn cell_mut(&mut self, x: u16, y: u16) -> Option<&mut Cell> {
        if x < self.area.width && y < self.area.height {
            self.grid.cell_mut(self.area.left + x, self.area.top + y)
        } else {
            None
        }
    }

    pub fn put_str(&mut self, x: u16, y: u16, text: &str, style: Style) -> u16 {
        self.write_str_at(x, y, text, None, style)
    }

    fn write_str_at(
        &mut self,
        x: u16,
        y: u16,
        text: &str,
        max_width: Option<u16>,
        style: Style,
    ) -> u16 {
        if y >= self.area.height {
            return x.min(self.area.width);
        }
        let abs_y = self.area.top + y;
        let limit = x
            .saturating_add(max_width.unwrap_or(u16::MAX))
            .min(self.area.width);
        let abs_left = self.area.left;
        let right = self.area.right();
        write_text(x, limit, text, |col, grapheme| {
            self.grid
                .write_symbol_clipped(abs_left + col, abs_y, grapheme, style, right)
        })
    }

    pub fn put_padded(&mut self, x: u16, y: u16, width: u16, text: &str, style: Style) -> u16 {
        if y >= self.area.height {
            return x.min(self.area.width);
        }
        let end = x.saturating_add(width).min(self.area.width);
        let mut col = self
            .write_str_at(x, y, text, Some(end.saturating_sub(x)), style)
            .min(end);
        while col < end {
            self.set(col, y, ' ', style);
            col = col.saturating_add(1);
        }
        end
    }

    pub fn put_str_aligned(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        text: &str,
        align: TextAlign,
        style: Style,
    ) -> u16 {
        if y >= self.area.height {
            return x.min(self.area.width);
        }
        let width = width.min(self.area.width.saturating_sub(x));
        let text_width = display_width(text).min(width);
        let col = match align {
            TextAlign::Left => x,
            TextAlign::Center => x.saturating_add(width.saturating_sub(text_width) / 2),
            TextAlign::Right => x.saturating_add(width.saturating_sub(text_width)),
        };
        self.put_str(col, y, text, style)
    }

    pub fn fill_row(&mut self, y: u16, style: Style) {
        self.fill(Rect::new(y, 0, self.area.width, 1), ' ', style);
    }

    pub fn rule_h(&mut self, y: u16, style: Style) {
        self.rule_h_range(0, y, self.area.width, style);
    }

    pub fn rule_v(&mut self, x: u16, style: Style) {
        self.rule_v_range(x, 0, self.area.height, style);
    }

    pub fn rule_h_range(&mut self, x: u16, y: u16, width: u16, style: Style) {
        if y >= self.area.height || x >= self.area.width {
            return;
        }
        let end = x.saturating_add(width).min(self.area.width);
        for col in x..end {
            self.set(col, y, '─', style);
        }
    }

    pub fn rule_v_range(&mut self, x: u16, y: u16, height: u16, style: Style) {
        if x >= self.area.width || y >= self.area.height {
            return;
        }
        let end = y.saturating_add(height).min(self.area.height);
        for row in y..end {
            self.set(x, row, '│', style);
        }
    }

    /// Slice-local [`Grid::put_char`]: overwrites symbol + fg, preserves bg and attrs.
    pub fn put_char(&mut self, x: u16, y: u16, symbol: char, fg: Color) {
        if x < self.area.width && y < self.area.height {
            self.grid.put_char_clipped(
                self.area.left + x,
                self.area.top + y,
                symbol,
                fg,
                self.area.right(),
            );
        }
    }

    /// Slice-local [`Grid::put_line`]: paint styled graphemes left-to-right.
    pub fn put_line(&mut self, x: u16, y: u16, line: &crate::line::Line<'_>) -> u16 {
        if y >= self.area.height {
            return x.min(self.area.width);
        }
        let abs_y = self.area.top + y;
        let abs_left = self.area.left;
        let right = self.area.right();
        write_line(x, self.area.width, line, |col, grapheme, style| {
            self.grid
                .write_symbol_clipped(abs_left + col, abs_y, grapheme, style, right)
        })
    }

    /// Slice-local [`Grid::put_str_fg`]: overwrites symbol + fg per char, preserves bg and attrs.
    pub fn put_str_fg(&mut self, x: u16, y: u16, text: &str, fg: Color) -> u16 {
        if y >= self.area.height {
            return x.min(self.area.width);
        }
        let abs_y = self.area.top + y;
        let abs_left = self.area.left;
        let right = self.area.right();
        write_text(x, self.area.width, text, |col, grapheme| {
            self.grid
                .put_symbol_clipped(abs_left + col, abs_y, grapheme, fg, right)
        })
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
    fn private_use_icons_are_narrow_cells() {
        let mut grid = Grid::new(4, 1);
        grid.set(0, 0, '\u{e6b2}', Style::default());

        assert_eq!(cell_width::char_width('\u{e6b2}'), 1);
        assert_eq!(grid.cell(0, 0).symbol, '\u{e6b2}');
        assert_eq!(grid.cell(1, 0).symbol, ' ');
    }

    #[test]
    fn put_str_uses_terminal_width_for_decomposed_accents() {
        let mut grid = Grid::new(80, 1);
        let text = "Bestellung 10500 besta\u{308}tigt.pdf";

        let end = grid.put_str(0, 0, text, Style::default());

        assert_eq!(end, display_width(text));
    }

    #[test]
    fn set_symbol_preserves_invariants_for_multiple_graphemes() {
        let mut grid = Grid::new(5, 1);

        grid.set_symbol(0, 0, "a👩\u{200d}💻b", Style::default());

        assert_eq!(grid.cell(0, 0).symbol, "a");
        assert_eq!(grid.cell(1, 0).symbol, "👩\u{200d}💻");
        assert!(grid.cell(2, 0).symbol.is_continuation());
        assert_eq!(grid.cell(3, 0).symbol, "b");
    }

    #[test]
    fn scalar_writes_repair_continuations_when_combining_changes_width() {
        let mut grid = Grid::new(4, 1);

        grid.set(0, 0, '9', Style::default());
        grid.set(1, 0, '\u{fe0f}', Style::default());

        assert_eq!(grid.cell(0, 0).symbol, "9\u{fe0f}");
        assert!(grid.cell(1, 0).symbol.is_continuation());

        grid.set(0, 0, '⌚', Style::default());
        assert!(grid.cell(1, 0).symbol.is_continuation());
        grid.set(2, 0, '\u{fe0e}', Style::default());

        assert_eq!(grid.cell(0, 0).symbol, "⌚\u{fe0e}");
        assert_eq!(grid.cell(1, 0).symbol, ' ');
    }

    #[test]
    fn put_line_keeps_graphemes_split_across_style_spans_atomic() {
        use crate::line::{Line, Span};

        for spans in [
            vec![Span::raw("e"), Span::styled("\u{301}", Style::new().bold())],
            vec![
                Span::raw("👩"),
                Span::styled("\u{200d}", Style::new().bold()),
                Span::raw("💻"),
            ],
            vec![
                Span::raw("9"),
                Span::styled("\u{fe0f}", Style::new().bold()),
            ],
        ] {
            let line = Line::from_spans(spans);
            let mut grid = Grid::new(8, 1);

            let end = grid.put_line(0, 0, &line);

            assert_eq!(end, line.width());
            let text: String = line.spans.iter().map(|span| span.text.as_ref()).collect();
            assert_eq!(grid.cell(0, 0).symbol, text.as_str());
            if end == 2 {
                assert!(grid.cell(1, 0).symbol.is_continuation());
            }
        }
    }

    #[test]
    fn set_and_read_cell() {
        let mut grid = Grid::new(10, 5);
        let style = Style::new().fg(Color::Red);
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
        let style = Style::new().bg(Color::Blue);
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
    fn slice_put_str_returns_clipped_end_column() {
        let mut grid = Grid::new(6, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 6, 1));
        let end = slice.put_str(4, 0, "abc", Style::default());
        assert_eq!(end, 6);
        assert_eq!(grid.cell(4, 0).symbol, 'a');
        assert_eq!(grid.cell(5, 0).symbol, 'b');
    }

    #[test]
    fn slice_put_str_counts_wide_chars() {
        let mut grid = Grid::new(6, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 6, 1));
        let end = slice.put_str(1, 0, "a語b", Style::default());
        assert_eq!(end, 5);
        assert_eq!(grid.cell(1, 0).symbol, 'a');
        assert_eq!(grid.cell(2, 0).symbol, '語');
        assert_eq!(grid.cell(3, 0).symbol, '\0');
        assert_eq!(grid.cell(4, 0).symbol, 'b');
    }

    #[test]
    fn slice_put_padded_fills_remaining_width() {
        let mut grid = Grid::new(8, 1);
        let style = Style::new().bg(Color::Blue);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 8, 1));
        let end = slice.put_padded(1, 0, 4, "hi", style);
        assert_eq!(end, 5);
        assert_eq!(grid.cell(1, 0).symbol, 'h');
        assert_eq!(grid.cell(2, 0).symbol, 'i');
        assert_eq!(grid.cell(3, 0).symbol, ' ');
        assert_eq!(grid.cell(4, 0).style.bg, Some(Color::Blue));
    }

    #[test]
    fn slice_put_padded_does_not_split_wide_chars() {
        let mut grid = Grid::new(4, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 4, 1));
        let end = slice.put_padded(0, 0, 2, "a語", Style::default());
        assert_eq!(end, 2);
        assert_eq!(grid.cell(0, 0).symbol, 'a');
        assert_eq!(grid.cell(1, 0).symbol, ' ');
        assert_eq!(grid.cell(2, 0).symbol, ' ');
    }

    #[test]
    fn slice_put_str_aligned_places_text() {
        let mut grid = Grid::new(12, 3);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 12, 3));
        slice.put_str_aligned(2, 0, 8, "ab", TextAlign::Left, Style::default());
        slice.put_str_aligned(2, 1, 8, "ab", TextAlign::Center, Style::default());
        slice.put_str_aligned(2, 2, 8, "ab", TextAlign::Right, Style::default());
        assert_eq!(grid.cell(2, 0).symbol, 'a');
        assert_eq!(grid.cell(5, 1).symbol, 'a');
        assert_eq!(grid.cell(8, 2).symbol, 'a');
    }

    #[test]
    fn slice_put_str_aligned_ignores_rows_outside_slice() {
        let mut grid = Grid::new(8, 2);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 8, 2));
        let end = slice.put_str_aligned(2, 9, 4, "nope", TextAlign::Left, Style::default());
        assert_eq!(end, 2);
        assert_eq!(grid.cell(2, 0).symbol, ' ');
    }

    #[test]
    fn slice_rules_clip_safely() {
        let mut grid = Grid::new(4, 3);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 4, 3));
        slice.rule_h_range(2, 1, 99, Style::default());
        slice.rule_v_range(3, 1, 99, Style::default());
        slice.rule_h(9, Style::default());
        slice.rule_v(9, Style::default());
        assert_eq!(grid.cell(2, 1).symbol, '─');
        assert_eq!(grid.cell(3, 1).symbol, '│');
        assert_eq!(grid.cell(3, 2).symbol, '│');
    }

    #[test]
    fn slice_mut_is_relative_to_parent() {
        let mut grid = Grid::new(20, 10);
        let mut parent = grid.slice_mut(Rect::new(2, 5, 10, 5));
        {
            let mut child = parent.slice_mut(Rect::new(1, 2, 3, 2));
            assert_eq!(child.grid_rect(), Rect::new(3, 7, 3, 2));
            child.set(0, 0, 'x', Style::default());
        }
        assert_eq!(grid.cell(7, 3).symbol, 'x');
    }

    #[test]
    fn slice_mut_clips_fully_outside_child_to_empty() {
        let mut grid = Grid::new(20, 10);
        let mut parent = grid.slice_mut(Rect::new(2, 5, 10, 5));
        let child = parent.slice_mut(Rect::new(99, 99, 3, 2));
        assert_eq!(child.grid_rect(), Rect::new(7, 15, 0, 0));
    }

    #[test]
    fn truncate_width_respects_display_width() {
        assert_eq!(display_width("a語b"), 4);
        assert_eq!(truncate_width("a語b", 3), "a語");
        assert_eq!(truncate_width("a語b", 2), "a");
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
    fn put_char_preserves_bg_and_attrs() {
        let mut grid = Grid::new(10, 3);
        let base = Style::new().fg(Color::Yellow).bg(Color::Blue).bold();
        grid.set(2, 1, '#', base);
        grid.put_char(2, 1, 'X', Color::Red);
        let cell = grid.cell(2, 1);
        assert_eq!(cell.symbol, 'X');
        assert_eq!(cell.style.fg, Some(Color::Red));
        assert_eq!(cell.style.bg, Some(Color::Blue));
        assert!(cell.style.bold);
    }

    #[test]
    fn put_char_on_empty_cell_leaves_bg_none() {
        let mut grid = Grid::new(5, 2);
        grid.put_char(0, 0, 'A', Color::Green);
        let cell = grid.cell(0, 0);
        assert_eq!(cell.symbol, 'A');
        assert_eq!(cell.style.fg, Some(Color::Green));
        assert_eq!(cell.style.bg, None);
    }

    #[test]
    fn put_line_paints_spans_with_their_styles() {
        use crate::line::{Line, Span};
        let mut grid = Grid::new(15, 1);
        let red = Style::new().fg(Color::Red);
        let line = Line::from_spans([Span::raw("ab"), Span::styled("CD", red), Span::raw("ef")]);
        grid.put_line(1, 0, &line);
        assert_eq!(grid.cell(1, 0).symbol, 'a');
        assert_eq!(grid.cell(1, 0).style.fg, None);
        assert_eq!(grid.cell(3, 0).symbol, 'C');
        assert_eq!(grid.cell(3, 0).style.fg, Some(Color::Red));
        assert_eq!(grid.cell(5, 0).symbol, 'e');
        assert_eq!(grid.cell(5, 0).style.fg, None);
    }

    #[test]
    fn slice_put_line_clips_at_right_edge() {
        use crate::line::{Line, Span};
        let mut grid = Grid::new(10, 1);
        {
            let mut slice = grid.slice_mut(Rect::new(0, 2, 5, 1));
            slice.put_line(0, 0, &Line::from_spans([Span::raw("abcdefgh")]));
        }
        assert_eq!(grid.cell(2, 0).symbol, 'a');
        assert_eq!(grid.cell(6, 0).symbol, 'e');
        assert_eq!(grid.cell(7, 0).symbol, ' ');
    }

    #[test]
    fn slice_put_str_fg_preserves_bg() {
        let mut grid = Grid::new(10, 2);
        grid.fill(Rect::new(0, 0, 6, 1), ' ', Style::new().bg(Color::Cyan));
        {
            let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
            slice.put_str_fg(1, 0, "hi", Color::Red);
        }
        assert_eq!(grid.cell(1, 0).symbol, 'h');
        assert_eq!(grid.cell(1, 0).style.fg, Some(Color::Red));
        assert_eq!(grid.cell(1, 0).style.bg, Some(Color::Cyan));
        assert_eq!(grid.cell(2, 0).style.bg, Some(Color::Cyan));
    }

    #[test]
    fn slice_terminal_cursor_position_uses_screen_coordinates_and_clears_with_the_frame() {
        let mut grid = Grid::new(8, 4);
        grid.slice_mut(Rect::new(1, 2, 4, 2))
            .set_terminal_cursor_position(1, 1);
        assert_eq!(grid.terminal_cursor_position(), Some((3, 2)));

        grid.clear_all();
        assert_eq!(grid.terminal_cursor_position(), None);
    }

    #[test]
    fn swap_grids() {
        let mut a = Grid::new(5, 3);
        let mut b = Grid::new(5, 3);
        a.set(0, 0, 'A', Style::default());
        a.set_terminal_cursor_position(1, 1);
        b.set(0, 0, 'B', Style::default());
        b.set_terminal_cursor_position(2, 2);
        a.swap_with(&mut b);
        assert_eq!(a.cell(0, 0).symbol, 'B');
        assert_eq!(a.terminal_cursor_position(), Some((2, 2)));
        assert_eq!(b.cell(0, 0).symbol, 'A');
        assert_eq!(b.terminal_cursor_position(), Some((1, 1)));
    }

    // ── Wide chars ───────────────────────────────────────────────────────

    #[test]
    fn set_wide_char_marks_next_cell_as_continuation() {
        // CJK chars have display width 2. The continuation cell carries
        // '\0' so flush/diff can skip it.
        let mut grid = Grid::new(5, 1);
        grid.set(1, 0, '漢', Style::default());
        assert_eq!(grid.cell(1, 0).symbol, '漢');
        assert_eq!(grid.cell(2, 0).symbol, '\0');
    }

    #[test]
    fn put_str_lays_wide_chars_two_columns_apart() {
        let mut grid = Grid::new(10, 1);
        grid.put_str(0, 0, "a漢b", Style::default());
        assert_eq!(grid.cell(0, 0).symbol, 'a');
        assert_eq!(grid.cell(1, 0).symbol, '漢');
        assert_eq!(grid.cell(3, 0).symbol, 'b');
    }

    #[test]
    fn wide_char_continuation_is_marked_consistently_across_paths() {
        // Every path that writes a wide char must mark the next cell so
        // downstream diff/flush can skip it. Otherwise a diff against a
        // prev frame with non-empty content at the continuation slot
        // emits a spurious update that overwrites the wide char's right
        // half on the terminal.
        let via_set = {
            let mut g = Grid::new(5, 1);
            g.set(0, 0, '漢', Style::default());
            g
        };
        let via_put_str = {
            let mut g = Grid::new(5, 1);
            g.put_str(0, 0, "漢", Style::default());
            g
        };
        let via_put_char = {
            let mut g = Grid::new(5, 1);
            g.put_char(0, 0, '漢', Color::Reset);
            g
        };
        assert_eq!(via_set.cell(1, 0).symbol, via_put_str.cell(1, 0).symbol);
        assert_eq!(via_set.cell(1, 0).symbol, via_put_char.cell(1, 0).symbol);
    }

    #[test]
    fn diff_does_not_emit_update_for_cell_under_a_wide_char() {
        // Regression for the wide-char bug: if prev had a real char at
        // the continuation column and curr paints a wide char that covers
        // it, diff must not yield an update for the continuation column -
        // otherwise flush_diff overwrites the right half of the wide char.
        let mut prev = Grid::new(5, 1);
        prev.set(1, 0, 'X', Style::default());
        let mut curr = Grid::new(5, 1);
        curr.put_str(0, 0, "漢", Style::default());
        let updates: Vec<_> = curr.diff(&prev).collect();
        let cols: Vec<u16> = updates.iter().map(|u| u.x).collect();
        assert_eq!(
            cols,
            vec![0],
            "expected one update at the wide char's column only; got {cols:?}"
        );
    }

    #[test]
    fn slice_put_str_lays_wide_chars_two_columns_apart() {
        let mut grid = Grid::new(10, 1);
        {
            let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
            slice.put_str(0, 0, "a漢b", Style::default());
        }
        assert_eq!(grid.cell(0, 0).symbol, 'a');
        assert_eq!(grid.cell(1, 0).symbol, '漢');
        assert_eq!(grid.cell(3, 0).symbol, 'b');
    }

    #[test]
    fn overwriting_wide_char_with_narrow_clears_continuation_marker() {
        // Within a single frame, an earlier paint can leave a wide char at
        // col N (marking col N+1 as '\0'), and a later paint can overwrite
        // col N with a narrow char. The narrow overwrite must clear the
        // stale continuation marker at col N+1 - otherwise the next diff
        // skips that cell (because it filters '\0'), and the terminal keeps
        // showing whatever was at col N+1 in the previous frame.
        let mut grid = Grid::new(5, 1);
        grid.set(0, 0, '漢', Style::default());
        assert_eq!(grid.cell(1, 0).symbol, '\0');
        grid.set(0, 0, 'A', Style::default());
        assert_ne!(
            grid.cell(1, 0).symbol,
            '\0',
            "narrow overwrite must clear stale continuation marker"
        );
    }

    #[test]
    fn overwriting_wide_continuation_with_narrow_clears_leading_wide_glyph() {
        // Symmetric case: a previous paint wrote a wide char at col N (so
        // col N+1 is '\0'). A later paint writes a narrow char at col N+1.
        // The wide char at col N is now orphaned - its visual right half
        // is occupied by the new narrow char. The leading slot must lose
        // its wide glyph so flush/diff don't try to render a 2-cell-wide
        // char that's been broken.
        let mut grid = Grid::new(5, 1);
        grid.set(0, 0, '漢', Style::default());
        assert_eq!(grid.cell(0, 0).symbol, '漢');
        assert_eq!(grid.cell(1, 0).symbol, '\0');
        grid.set(1, 0, 'A', Style::default());
        assert_ne!(
            grid.cell(0, 0).symbol,
            '漢',
            "writing to a wide char's continuation must break the leading glyph"
        );
    }

    #[test]
    fn diff_clears_terminal_when_wide_is_overwritten_with_narrow_in_same_frame() {
        // End-to-end shape of the leftover-cell bug.
        //
        // Frame N (prev): writes a wide char at col 0. Terminal ends up
        // with `漢` covering cols 0+1.
        //
        // Frame N+1 (curr): some paint writes a wide char at col 0 (e.g.
        // a span or virt text paint), then a later paint overwrites col 0
        // with a narrow char (e.g. a cursor or overlay). The diff must
        // still emit an update for col 1 so the terminal clears the right
        // half of the previous wide char - otherwise that half lingers.
        let mut prev = Grid::new(5, 1);
        prev.set(0, 0, '漢', Style::default());

        let mut curr = Grid::new(5, 1);
        curr.set(0, 0, '漢', Style::default()); // first paint: wide
        curr.set(0, 0, 'A', Style::default()); // second paint: narrow overwrite

        let updates: Vec<_> = curr.diff(&prev).collect();
        let cols: Vec<u16> = updates.iter().map(|u| u.x).collect();
        assert!(
            cols.contains(&1),
            "expected diff to clear the orphaned continuation at col 1; got {cols:?}"
        );
    }

    /// Assert the two grid-wide wide-char invariants:
    ///   I1. `(x, y).symbol == '\0'` only at the cell immediately right of a wide.
    ///   I2. A wide char at `(x, y)` with `x+1 < width` implies `(x+1, y).symbol == '\0'`.
    fn assert_grid_invariants(grid: &Grid) {
        for y in 0..grid.height() {
            for x in 0..grid.width() {
                let cell = grid.cell(x, y);
                if cell.symbol == '\0' {
                    assert!(x > 0, "continuation at column 0 has no leading cell");
                    let lead = &grid.cell(x - 1, y).symbol;
                    assert_eq!(
                        lead.width(),
                        2,
                        "orphaned continuation at ({x}, {y}); leading cell symbol is {lead:?}"
                    );
                }
                if cell.symbol.width() == 2 {
                    assert!(
                        x + 1 < grid.width(),
                        "wide char at ({x}, {y}) cannot fit a continuation cell"
                    );
                    let cont = &grid.cell(x + 1, y).symbol;
                    assert_eq!(
                        cont, &'\0',
                        "wide char at ({x}, {y}) is missing continuation; got {cont:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn wide_char_at_right_edge_paints_space() {
        let mut grid = Grid::new(2, 1);
        let style = Style::new().fg(Color::Red);
        grid.set(1, 0, '漢', style);

        assert_eq!(grid.cell(1, 0).symbol, ' ');
        assert_eq!(grid.cell(1, 0).style.fg, Some(Color::Red));
        assert_grid_invariants(&grid);
    }

    #[test]
    fn slice_wide_char_at_right_edge_paints_space_inside_slice() {
        let mut grid = Grid::new(4, 1);
        let style = Style::new().fg(Color::Red);
        {
            let mut slice = grid.slice_mut(Rect::new(0, 1, 2, 1));
            slice.set(1, 0, '漢', style);
        }

        assert_eq!(grid.cell(2, 0).symbol, ' ');
        assert_eq!(grid.cell(2, 0).style.fg, Some(Color::Red));
        assert_eq!(grid.cell(3, 0).symbol, ' ');
        assert_ne!(grid.cell(3, 0).symbol, '\0');
        assert_grid_invariants(&grid);
    }

    #[test]
    fn slice_put_char_wide_at_right_edge_paints_space_inside_slice() {
        let mut grid = Grid::new(4, 1);
        {
            let mut slice = grid.slice_mut(Rect::new(0, 1, 2, 1));
            slice.put_char(1, 0, '漢', Color::Red);
        }

        assert_eq!(grid.cell(2, 0).symbol, ' ');
        assert_eq!(grid.cell(2, 0).style.fg, Some(Color::Red));
        assert_eq!(grid.cell(3, 0).symbol, ' ');
        assert_grid_invariants(&grid);
    }

    #[test]
    fn slice_put_str_wide_at_right_edge_does_not_spill_outside_slice() {
        let mut grid = Grid::new(4, 1);
        {
            let mut slice = grid.slice_mut(Rect::new(0, 1, 2, 1));
            let end = slice.put_str(1, 0, "漢", Style::default());
            assert_eq!(end, 1);
        }

        assert_eq!(grid.cell(2, 0).symbol, ' ');
        assert_eq!(grid.cell(3, 0).symbol, ' ');
        assert_grid_invariants(&grid);
    }

    #[test]
    fn fill_wide_char_at_rect_edge_does_not_spill_outside_rect() {
        let mut grid = Grid::new(4, 1);
        let style = Style::new().bg(Color::Blue);
        grid.fill(Rect::new(0, 1, 1, 1), '漢', style);

        assert_eq!(grid.cell(1, 0).symbol, ' ');
        assert_eq!(grid.cell(1, 0).style.bg, Some(Color::Blue));
        assert_eq!(grid.cell(2, 0).symbol, ' ');
        assert_ne!(grid.cell(2, 0).style.bg, Some(Color::Blue));
        assert_grid_invariants(&grid);
    }

    #[test]
    fn fill_tiles_wide_chars_by_cell_width() {
        let mut grid = Grid::new(5, 1);
        let style = Style::new().bg(Color::Blue);
        grid.fill(Rect::new(0, 0, 5, 1), '漢', style);

        assert_eq!(grid.cell(0, 0).symbol, '漢');
        assert_eq!(grid.cell(1, 0).symbol, '\0');
        assert_eq!(grid.cell(2, 0).symbol, '漢');
        assert_eq!(grid.cell(3, 0).symbol, '\0');
        assert_eq!(grid.cell(4, 0).symbol, ' ');
        for x in 0..5 {
            assert_eq!(grid.cell(x, 0).style.bg, Some(Color::Blue));
        }
        assert_grid_invariants(&grid);
    }

    #[test]
    fn overwriting_continuation_with_right_edge_wide_clears_old_wide() {
        let mut grid = Grid::new(2, 1);
        grid.set(0, 0, '漢', Style::default());
        grid.set(1, 0, '漢', Style::default());

        assert_eq!(grid.cell(0, 0).symbol, ' ');
        assert_eq!(grid.cell(1, 0).symbol, ' ');
        assert_grid_invariants(&grid);
    }

    #[test]
    fn put_char_narrow_over_wide_keeps_invariant() {
        let mut grid = Grid::new(5, 1);
        grid.set(0, 0, '漢', Style::default());
        grid.put_char(0, 0, 'A', Color::Red);
        assert_grid_invariants(&grid);
        assert_eq!(grid.cell(0, 0).symbol, 'A');
        assert_ne!(grid.cell(1, 0).symbol, '\0');
    }

    #[test]
    fn put_char_on_continuation_breaks_leading_wide() {
        let mut grid = Grid::new(5, 1);
        grid.set(0, 0, '漢', Style::default());
        grid.put_char(1, 0, 'B', Color::Green);
        assert_grid_invariants(&grid);
        assert_ne!(grid.cell(0, 0).symbol, '漢');
        assert_eq!(grid.cell(1, 0).symbol, 'B');
    }

    #[test]
    fn fill_partially_overlapping_a_wide_char_keeps_invariant() {
        // Wide at col 3 (continuation at col 4). Fill clobbers cols 0..4
        // inclusive of the leading wide but not its continuation.
        let mut grid = Grid::new(6, 1);
        grid.set(3, 0, '漢', Style::default());
        grid.fill(Rect::new(0, 0, 4, 1), '#', Style::default());
        assert_grid_invariants(&grid);
        assert_eq!(grid.cell(3, 0).symbol, '#');
        assert_ne!(grid.cell(4, 0).symbol, '\0');
    }

    #[test]
    fn fill_starting_on_a_continuation_breaks_the_leading_wide() {
        // Wide at col 0 (continuation at col 1). Fill clobbers cols 1..3
        // - leaves the leading slot at col 0 untouched. The leading wide
        // must be broken because its right half is gone.
        let mut grid = Grid::new(5, 1);
        grid.set(0, 0, '漢', Style::default());
        grid.fill(Rect::new(0, 1, 2, 1), '#', Style::default());
        assert_grid_invariants(&grid);
        assert_ne!(grid.cell(0, 0).symbol, '漢');
        assert_eq!(grid.cell(1, 0).symbol, '#');
    }

    #[test]
    fn writing_wide_over_a_wide_displaces_the_old_continuation() {
        // Two adjacent wide chars: `漢` at col 0 (cont at 1), `字` at col 2 (cont at 3).
        // Writing a new wide at col 1 displaces the first, lands on the second's leading.
        // After the write, the new wide owns cols 1+2; col 0 should be broken (its
        // continuation got reassigned) and col 3 should no longer be a stale '\0'.
        let mut grid = Grid::new(6, 1);
        grid.set(0, 0, '漢', Style::default());
        grid.set(2, 0, '字', Style::default());
        grid.set(1, 0, '日', Style::default());
        assert_grid_invariants(&grid);
        assert_eq!(grid.cell(1, 0).symbol, '日');
        assert_eq!(grid.cell(2, 0).symbol, '\0');
        // Col 0 lost its right half, so the wide glyph there must be broken.
        assert_ne!(grid.cell(0, 0).symbol, '漢');
        // Col 3 was the displaced wide's continuation - it must be cleared.
        assert_ne!(grid.cell(3, 0).symbol, '\0');
    }

    #[test]
    fn clear_all_holds_invariant_after_arbitrary_paint() {
        let mut grid = Grid::new(8, 2);
        grid.set(0, 0, '漢', Style::default());
        grid.set(3, 0, '字', Style::default());
        grid.put_str(0, 1, "a漢b", Style::default());
        grid.clear_all();
        assert_grid_invariants(&grid);
        for y in 0..2 {
            for x in 0..8 {
                assert_eq!(grid.cell(x, y).symbol, ' ');
            }
        }
    }

    #[test]
    fn diff_emits_continuation_clear_when_wide_was_overwritten() {
        // The reported "leftover after dialog close" scenario, end-to-end:
        // prev frame painted a wide char; curr frame overwrites it with a
        // narrow before flushing. The continuation cell must end up in the
        // diff stream so the terminal clears the right half.
        let mut prev = Grid::new(6, 1);
        prev.set(2, 0, '漢', Style::default());
        let mut curr = Grid::new(6, 1);
        curr.set(2, 0, '漢', Style::default());
        curr.set(2, 0, 'A', Style::default());
        let updates: Vec<_> = curr.diff(&prev).collect();
        let cols: Vec<u16> = updates.iter().map(|u| u.x).collect();
        assert!(
            cols.contains(&3),
            "expected diff to clear orphaned continuation at col 3; got {cols:?}"
        );
        assert_grid_invariants(&curr);
    }

    #[test]
    fn slice_fill_partial_wide_overlap_keeps_invariant() {
        let mut grid = Grid::new(10, 1);
        grid.set(4, 0, '漢', Style::default());
        {
            let mut slice = grid.slice_mut(Rect::new(0, 0, 5, 1));
            slice.fill(Rect::new(0, 0, 5, 1), '#', Style::default());
        }
        assert_grid_invariants(&grid);
    }

    #[test]
    fn put_str_fg_over_wide_keeps_invariant() {
        let mut grid = Grid::new(6, 1);
        grid.set(1, 0, '漢', Style::default());
        grid.put_str_fg(0, 0, "abc", Color::Red);
        assert_grid_invariants(&grid);
        assert_eq!(grid.cell(0, 0).symbol, 'a');
        assert_eq!(grid.cell(1, 0).symbol, 'b');
        assert_eq!(grid.cell(2, 0).symbol, 'c');
    }

    #[test]
    fn put_str_breaks_when_wide_char_would_overflow() {
        // 4-wide grid, write "ab漢": the wide char would land at col 2 and
        // would need col 3 too - fits. Try "abc漢" in width 4: 'c' at 2,
        // wide char needs 3+4 → overflows, breaks before writing.
        let mut grid = Grid::new(4, 1);
        grid.put_str(0, 0, "abc漢", Style::default());
        assert_eq!(grid.cell(0, 0).symbol, 'a');
        assert_eq!(grid.cell(1, 0).symbol, 'b');
        assert_eq!(grid.cell(2, 0).symbol, 'c');
        // Position 3 was never written.
        assert_eq!(grid.cell(3, 0).symbol, ' ');
    }

    // ── Diff over styles ─────────────────────────────────────────────────

    #[test]
    fn diff_picks_up_style_only_change() {
        let mut prev = Grid::new(5, 1);
        prev.set(0, 0, 'X', Style::default());
        let mut curr = Grid::new(5, 1);
        curr.set(0, 0, 'X', Style::new().fg(Color::Red));
        let updates: Vec<_> = curr.diff(&prev).collect();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].cell.style.fg, Some(Color::Red));
    }

    // ── Bounds ───────────────────────────────────────────────────────────

    #[test]
    fn set_out_of_bounds_is_silent_noop() {
        let mut grid = Grid::new(3, 2);
        grid.set(99, 99, 'X', Style::default());
        // No panic; nothing changed.
        assert_eq!(grid.cell(0, 0).symbol, ' ');
        assert_eq!(grid.cell(2, 1).symbol, ' ');
    }

    #[test]
    fn put_str_skips_when_y_out_of_bounds() {
        let mut grid = Grid::new(5, 2);
        grid.put_str(0, 99, "hello", Style::default());
        // First row unchanged.
        assert_eq!(grid.cell(0, 0).symbol, ' ');
    }
}
