use crate::component::{Component, DrawContext, KeyResult};
use crate::flush::flush_diff;
use crate::grid::Grid;
use crate::layout::Rect;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::QueueableCommand;
use std::io::Write;

struct Layer {
    id: String,
    component: Box<dyn Component>,
    rect: Rect,
    zindex: u16,
    /// On `Down(Left)` inside this layer's rect, mark it focused
    /// before dispatching the event to it.
    focus_on_click: bool,
    /// On `Down(Left)` inside this layer's rect, bump its zindex
    /// above its current siblings so it rises to the top.
    raise_on_click: bool,
}

/// Per-layer interaction policy passed to `Compositor::add_with_opts`.
/// Defaults to focus + raise on click, which is what every interactive
/// float (Dialog, Picker focused-mode) wants. Notifications and
/// status overlays should opt out with `focus_on_click = false` and
/// `raise_on_click = false`.
#[derive(Clone, Copy, Debug)]
pub struct LayerOpts {
    pub focus_on_click: bool,
    pub raise_on_click: bool,
}

impl Default for LayerOpts {
    fn default() -> Self {
        Self {
            focus_on_click: true,
            raise_on_click: true,
        }
    }
}

pub struct Compositor {
    current: Grid,
    previous: Grid,
    layers: Vec<Layer>,
    focused: Option<String>,
    width: u16,
    height: u16,
    force_redraw: bool,
}

impl Compositor {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            current: Grid::new(width, height),
            previous: Grid::new(width, height),
            layers: Vec::new(),
            focused: None,
            width,
            height,
            force_redraw: true,
        }
    }

    pub fn add(
        &mut self,
        id: impl Into<String>,
        component: Box<dyn Component>,
        rect: Rect,
        zindex: u16,
    ) {
        self.add_with_opts(id, component, rect, zindex, LayerOpts::default());
    }

    pub fn add_with_opts(
        &mut self,
        id: impl Into<String>,
        component: Box<dyn Component>,
        rect: Rect,
        zindex: u16,
        opts: LayerOpts,
    ) {
        let id = id.into();
        self.layers.push(Layer {
            id,
            component,
            rect,
            zindex,
            focus_on_click: opts.focus_on_click,
            raise_on_click: opts.raise_on_click,
        });
        self.sort_layers();
    }

    pub fn remove(&mut self, id: &str) -> Option<Box<dyn Component>> {
        if let Some(pos) = self.layers.iter().position(|l| l.id == id) {
            let layer = self.layers.remove(pos);
            if self.focused.as_deref() == Some(id) {
                self.focused = None;
            }
            self.force_redraw = true;
            Some(layer.component)
        } else {
            None
        }
    }

    pub fn set_rect(&mut self, id: &str, rect: Rect) {
        if let Some(layer) = self.layers.iter_mut().find(|l| l.id == id) {
            layer.rect = rect;
        }
    }

    pub fn set_zindex(&mut self, id: &str, zindex: u16) {
        if let Some(layer) = self.layers.iter_mut().find(|l| l.id == id) {
            if layer.zindex != zindex {
                layer.zindex = zindex;
                self.sort_layers();
                self.force_redraw = true;
            }
        }
    }

    pub fn focus(&mut self, id: impl Into<String>) {
        self.focused = Some(id.into());
    }

    pub fn focused(&self) -> Option<&str> {
        self.focused.as_deref()
    }

    pub fn component(&self, id: &str) -> Option<&dyn Component> {
        self.layers
            .iter()
            .find(|l| l.id == id)
            .map(|l| l.component.as_ref())
    }

    pub fn component_mut(&mut self, id: &str) -> Option<&mut dyn Component> {
        for layer in &mut self.layers {
            if layer.id == id {
                return Some(&mut *layer.component);
            }
        }
        None
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.current.resize(width, height);
        self.previous.resize(width, height);
        self.force_redraw = true;
    }

    pub fn render<W: Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.current.clear_all();

        let focused_id = self.focused.clone();

        for layer in &mut self.layers {
            let ctx = DrawContext {
                terminal_width: self.width,
                terminal_height: self.height,
                focused: focused_id.as_deref() == Some(&layer.id),
            };
            layer.component.prepare(layer.rect, &ctx);
        }

        for layer in &self.layers {
            let ctx = DrawContext {
                terminal_width: self.width,
                terminal_height: self.height,
                focused: focused_id.as_deref() == Some(&layer.id),
            };
            let mut slice = self.current.slice_mut(layer.rect);
            layer.component.draw(layer.rect, &mut slice, &ctx);
        }

        // Paint block cursors from focused layer into the grid (before flush).
        let cursor_info = focused_id.as_deref().and_then(|fid| {
            self.layers
                .iter()
                .find(|l| l.id == fid)
                .and_then(|l| l.component.cursor().map(|ci| (l.rect, ci)))
        });
        let hardware_cursor = cursor_info.as_ref().and_then(|(rect, ci)| {
            let abs_x = rect.left + ci.col;
            let abs_y = rect.top + ci.row;
            if let Some(ref cs) = ci.style {
                self.current.set(abs_x, abs_y, cs.glyph, cs.style);
                None
            } else {
                Some((abs_x, abs_y))
            }
        });

        w.queue(BeginSynchronizedUpdate)?;

        if self.force_redraw {
            flush_full(&self.current, w)?;
        } else {
            flush_diff(w, self.current.diff(&self.previous))?;
        }

        if let Some((x, y)) = hardware_cursor {
            w.queue(crossterm::cursor::Show)?;
            w.queue(crossterm::cursor::MoveTo(x, y))?;
        } else {
            w.queue(crossterm::cursor::Hide)?;
        }

        w.queue(EndSynchronizedUpdate)?;
        w.flush()?;

        self.current.swap_with(&mut self.previous);
        self.force_redraw = false;

        Ok(())
    }

    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult {
        if let Some(ref focused_id) = self.focused {
            if let Some(layer) = self.layers.iter_mut().find(|l| &l.id == focused_id) {
                let result = layer.component.handle_key(code, mods);
                if result != KeyResult::Ignored {
                    return result;
                }
            }
        }
        KeyResult::Ignored
    }

    /// Route a mouse event to the topmost layer whose rect contains the
    /// pointer. Returns `None` when no layer is hit (caller falls back
    /// to its own routing — transcript/prompt for App). On `Down(Left)`,
    /// applies per-layer `focus_on_click` and `raise_on_click` policy
    /// before the event reaches the component, so a click both selects
    /// the layer and raises it within its z-band. Returns the id of
    /// the dispatched-to layer so the caller can fan out widget actions
    /// to that layer's callbacks.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> Option<(String, KeyResult)> {
        let row = event.row;
        let col = event.column;
        let id = self
            .layers
            .iter()
            .rev()
            .find(|l| l.rect.contains(row, col))
            .map(|l| l.id.clone())?;

        if let MouseEventKind::Down(MouseButton::Left) = event.kind {
            // Snapshot policy before mutating; iter_mut + later lookups
            // would conflict with the borrow checker otherwise.
            let (focus, raise) = self
                .layers
                .iter()
                .find(|l| l.id == id)
                .map(|l| (l.focus_on_click, l.raise_on_click))
                .unwrap_or((false, false));
            if focus {
                self.focused = Some(id.clone());
            }
            if raise {
                let max_z = self.layers.iter().map(|l| l.zindex).max().unwrap_or(0);
                let target_z = max_z.saturating_add(1);
                if let Some(layer) = self.layers.iter_mut().find(|l| l.id == id) {
                    if layer.zindex != target_z {
                        layer.zindex = target_z;
                        self.sort_layers();
                        self.force_redraw = true;
                    }
                }
            }
        }

        let layer = self.layers.iter_mut().find(|l| l.id == id)?;
        let result = layer.component.handle_mouse(event);
        Some((id, result))
    }

    /// Dispatch a mouse event directly to a known layer, bypassing
    /// hit-testing. Used during drag capture: after a layer returns
    /// `KeyResult::Capture` on its `Down`, App routes subsequent
    /// `Drag` / `Up` events here so the gesture continues even when
    /// the pointer leaves the layer's rect.
    pub fn handle_mouse_to(&mut self, id: &str, event: MouseEvent) -> Option<KeyResult> {
        let layer = self.layers.iter_mut().find(|l| l.id == id)?;
        Some(layer.component.handle_mouse(event))
    }

    pub fn force_redraw(&mut self) {
        self.force_redraw = true;
    }

    pub fn layer_ids(&self) -> Vec<&str> {
        self.layers.iter().map(|l| l.id.as_str()).collect()
    }

    /// Return the id of the topmost layer whose rect contains (row, col),
    /// or `None` if the cell is not covered by any layer. "Topmost" =
    /// highest zindex since `sort_layers` orders ascending and we
    /// iterate in reverse.
    pub fn hit_test(&self, row: u16, col: u16) -> Option<&str> {
        self.layers
            .iter()
            .rev()
            .find(|l| l.rect.contains(row, col))
            .map(|l| l.id.as_str())
    }

    fn sort_layers(&mut self) {
        self.layers.sort_by_key(|l| l.zindex);
    }
}

fn flush_full<W: Write>(grid: &Grid, w: &mut W) -> std::io::Result<()> {
    use crate::grid::Style;
    use crossterm::cursor::MoveTo;
    use crossterm::style::{
        Attribute, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    };
    use unicode_width::UnicodeWidthChar;

    let mut current_style = Style::default();
    for y in 0..grid.height() {
        w.queue(MoveTo(0, y))?;
        let mut terminal_col: u16 = 0;
        let mut x = 0u16;
        while x < grid.width() {
            let cell = grid.cell(x, y);
            // `\0` marks the continuation half of a preceding wide
            // char. If the path through the row is aligned it should
            // have been skipped; if we somehow land on one, paint a
            // space so the cursor stays in sync instead of emitting a
            // literal NUL.
            let symbol = if cell.symbol == '\0' {
                ' '
            } else {
                cell.symbol
            };
            let cw = UnicodeWidthChar::width(symbol).unwrap_or(1).max(1) as u16;

            // Wide char whose second cell would fall past the terminal edge:
            // emit a space instead so the terminal doesn't wrap.
            let (sym, emit_w) = if terminal_col + cw > grid.width() {
                (' ', 1u16)
            } else {
                (symbol, cw)
            };

            if cell.style != current_style {
                w.queue(SetAttribute(Attribute::Reset))?;
                w.queue(ResetColor)?;
                if let Some(fg) = cell.style.fg {
                    w.queue(SetForegroundColor(fg))?;
                }
                if let Some(bg) = cell.style.bg {
                    w.queue(SetBackgroundColor(bg))?;
                }
                if cell.style.bold {
                    w.queue(SetAttribute(Attribute::Bold))?;
                }
                if cell.style.dim {
                    w.queue(SetAttribute(Attribute::Dim))?;
                }
                if cell.style.italic {
                    w.queue(SetAttribute(Attribute::Italic))?;
                }
                if cell.style.underline {
                    w.queue(SetAttribute(Attribute::Underlined))?;
                }
                if cell.style.crossedout {
                    w.queue(SetAttribute(Attribute::CrossedOut))?;
                }
                current_style = cell.style;
            }
            let mut buf = [0u8; 4];
            let s = sym.encode_utf8(&mut buf);
            w.queue(Print(s.to_string()))?;

            terminal_col += emit_w;
            // Advance grid by emit_w so wide chars consume their
            // continuation cell — the grid allocates 1 slot per char,
            // so the compositor must skip the next column to stay in
            // sync with the terminal's visual width.
            x += emit_w;
        }
    }
    w.queue(SetAttribute(Attribute::Reset))?;
    w.queue(ResetColor)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{GridSlice, Style};

    struct TestComponent {
        text: String,
    }

    impl TestComponent {
        fn new(text: &str) -> Self {
            Self {
                text: text.to_string(),
            }
        }
    }

    impl Component for TestComponent {
        fn draw(&self, _area: Rect, grid: &mut GridSlice<'_>, _ctx: &DrawContext) {
            grid.put_str(0, 0, &self.text, Style::default());
        }

        fn handle_key(&mut self, _code: KeyCode, _mods: KeyModifiers) -> KeyResult {
            KeyResult::Ignored
        }
    }

    #[test]
    fn add_and_render_component() {
        let mut comp = Compositor::new(20, 5);
        comp.add(
            "test",
            Box::new(TestComponent::new("hello")),
            Rect::new(0, 0, 20, 1),
            0,
        );
        let mut out = Vec::new();
        comp.render(&mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("hello"));
    }

    #[test]
    fn remove_component() {
        let mut comp = Compositor::new(20, 5);
        comp.add(
            "a",
            Box::new(TestComponent::new("aaa")),
            Rect::new(0, 0, 10, 1),
            0,
        );
        comp.add(
            "b",
            Box::new(TestComponent::new("bbb")),
            Rect::new(1, 0, 10, 1),
            0,
        );
        assert!(comp.remove("a").is_some());
        assert_eq!(comp.layer_ids(), vec!["b"]);
    }

    #[test]
    fn z_order_respected() {
        let mut comp = Compositor::new(20, 5);
        comp.add(
            "back",
            Box::new(TestComponent::new("BACK")),
            Rect::new(0, 0, 10, 1),
            0,
        );
        comp.add(
            "front",
            Box::new(TestComponent::new("FRONT")),
            Rect::new(0, 0, 10, 1),
            10,
        );
        assert_eq!(comp.layer_ids(), vec!["back", "front"]);
    }

    #[test]
    fn hit_test_returns_topmost_layer_under_cell() {
        let mut comp = Compositor::new(40, 10);
        comp.add(
            "back",
            Box::new(TestComponent::new("")),
            Rect::new(0, 0, 40, 10),
            0,
        );
        comp.add(
            "front",
            Box::new(TestComponent::new("")),
            Rect::new(3, 5, 10, 4),
            5,
        );
        // Inside the front rect → front wins.
        assert_eq!(comp.hit_test(4, 6), Some("front"));
        // Outside the front rect but inside back → back.
        assert_eq!(comp.hit_test(8, 0), Some("back"));
        // Outside both.
        assert_eq!(comp.hit_test(9, 39), Some("back"));
        assert_eq!(comp.hit_test(10, 0), None);
    }

    #[test]
    fn focus_routes_keys() {
        let mut comp = Compositor::new(20, 5);

        struct ConsumeAll;
        impl Component for ConsumeAll {
            fn draw(&self, _: Rect, _: &mut GridSlice<'_>, _: &DrawContext) {}
            fn handle_key(&mut self, _: KeyCode, _: KeyModifiers) -> KeyResult {
                KeyResult::Consumed
            }
        }

        comp.add("a", Box::new(ConsumeAll), Rect::new(0, 0, 10, 1), 0);
        assert_eq!(
            comp.handle_key(KeyCode::Char('x'), KeyModifiers::NONE),
            KeyResult::Ignored
        );
        comp.focus("a");
        assert_eq!(
            comp.handle_key(KeyCode::Char('x'), KeyModifiers::NONE),
            KeyResult::Consumed
        );
    }

    #[test]
    fn resize_triggers_force_redraw() {
        let mut comp = Compositor::new(20, 5);
        comp.add(
            "a",
            Box::new(TestComponent::new("hi")),
            Rect::new(0, 0, 10, 1),
            0,
        );
        let mut out = Vec::new();
        comp.render(&mut out).unwrap();
        assert!(!comp.force_redraw);
        comp.resize(40, 10);
        assert!(comp.force_redraw);
    }

    #[test]
    fn all_components_drawn_every_frame() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;

        struct CountingComponent {
            draw_count: Arc<AtomicU32>,
        }
        impl Component for CountingComponent {
            fn draw(&self, _: Rect, _: &mut GridSlice<'_>, _: &DrawContext) {
                self.draw_count.fetch_add(1, Ordering::Relaxed);
            }
            fn handle_key(&mut self, _: KeyCode, _: KeyModifiers) -> KeyResult {
                KeyResult::Ignored
            }
        }

        let count = Arc::new(AtomicU32::new(0));
        let mut comp = Compositor::new(20, 5);
        comp.add(
            "a",
            Box::new(CountingComponent {
                draw_count: count.clone(),
            }),
            Rect::new(0, 0, 10, 1),
            0,
        );

        let mut out = Vec::new();
        comp.render(&mut out).unwrap();
        assert_eq!(count.load(Ordering::Relaxed), 1);

        let mut out = Vec::new();
        comp.render(&mut out).unwrap();
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn cursor_position_from_focused() {
        use crate::component::CursorInfo;

        struct CursorComp;
        impl Component for CursorComp {
            fn draw(&self, _: Rect, _: &mut GridSlice<'_>, _: &DrawContext) {}
            fn handle_key(&mut self, _: KeyCode, _: KeyModifiers) -> KeyResult {
                KeyResult::Ignored
            }
            fn cursor(&self) -> Option<CursorInfo> {
                Some(CursorInfo::hardware(3, 1))
            }
        }

        let mut comp = Compositor::new(20, 10);
        comp.add("edit", Box::new(CursorComp), Rect::new(5, 10, 10, 3), 0);
        comp.focus("edit");
        let mut out = Vec::new();
        comp.render(&mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\x1b[?25h"));
    }
}
