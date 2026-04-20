# TUI Architecture — UI Framework Rewrite

## Vision

Build a **retained-mode TUI rendering framework** (`crates/ui/`) inspired by
Neovim's architecture but designed for Rust's ownership model. The framework
provides a cell grid, compositor, and component system where every visible
surface — transcript, prompt, dialogs, notifications, completions, status bar —
is a component that draws into a grid region.

Three-layer architecture:

```
engine (core logic, no UI)
    ↕
ui (framework: grid, compositor, components, buffers, windows, layout)
    ↕
tui (terminal I/O: crossterm, event loop, app shell, Lua runtime)
```

The `ui` crate knows nothing about agents, engines, or protocols. The `tui`
crate is a thin app shell that wires `ui` primitives to smelt-specific logic
and handles terminal I/O. Both internal Rust code and Lua plugins talk through
the same `ui` API.

## Why not ratatui

We evaluated ratatui and decided against it:

- **Immediate mode vs retained.** Ratatui rebuilds the entire UI every frame.
  We want retained mode — components persist state, mark dirty, and only
  redraw when something changes.
- **No windows.** Ratatui has no concept of persistent viewports with cursor,
  scroll, and focus. We need first-class windows.
- **No z-order.** Ratatui composites by render order only. We need explicit
  float layering with z-index.
- **Abstraction clash.** Ratatui's `Buffer` is a cell grid (render target).
  Our `Buffer` is a content model (lines + highlights + marks). Same name,
  fundamentally different concepts.
- **We already have RenderOut.** Our `StyleState` + SGR diff engine is more
  sophisticated than ratatui's cell-level diffing.

What we take from ratatui: the cell grid concept as an intermediate rendering
surface between components and the terminal. That's ~200 lines.

## Why a separate crate

- **Forces clean boundaries.** Can't import `protocol::Message` in `crates/ui/`.
- **Testable in isolation.** Unit-test grid, layout, components without an engine.
- **Reusable.** The framework is a general TUI toolkit — not smelt-specific.
- **Makes the API surface explicit.** The `pub` items in `ui` *are* the API.

---

## Core architecture

### Cell Grid

The **rendering primitive**. A 2D array of `Cell { symbol, style }` that sits
between components and the terminal. Components never emit escape sequences —
they write cells to a grid region.

```rust
pub struct Cell {
    pub symbol: char,
    pub style: Style,
}

pub struct Grid {
    cells: Vec<Cell>,
    width: u16,
    height: u16,
}

pub struct GridSlice<'a> {
    grid: &'a mut Grid,
    area: Rect,
}
```

`GridSlice` is the Rust ownership adaptation: a borrowed rectangular view into
the main grid. Only one component writes to any region at a time.

### Component

The **rendering unit**. Each UI surface implements `Component`:

```rust
pub trait Component {
    fn draw(&self, area: Rect, grid: &mut GridSlice, ctx: &DrawContext);
    fn handle_key(&mut self, key: KeyEvent) -> KeyResult;
    fn handle_mouse(&mut self, event: MouseEvent) -> bool { false }
    fn cursor(&self) -> Option<CursorPosition> { None }
    fn is_dirty(&self) -> bool;
    fn mark_dirty(&mut self);
    fn mark_clean(&mut self);
}
```

Components are **retained** — they own their state, persist across frames, and
only redraw when dirty. The framework tracks dirty flags and skips clean
components.

### Compositor

Manages the component tree, orchestrates rendering, and diffs frames:

```rust
pub struct Compositor {
    grid: Grid,
    prev_grid: Grid,
    layers: Vec<Layer>,      // split components, then floats by z-order
}
```

Each frame:
1. Resolve layout → assign `Rect` to each component
2. For each dirty component: `component.draw(rect, &mut grid_slice, ctx)`
3. Diff `grid` vs `prev_grid` → emit only changed cells
4. Swap grids

The compositor replaces the current `RenderOut` direct-write pattern. All
terminal output flows through: component → grid → diff → terminal.

### Buffer (content model)

Unchanged from current design. Lines + highlights + marks + virtual text.
Buffers are the **data model** — they hold content. Components read buffers
and write cells to the grid.

### Window

Viewport into a buffer. Owns cursor, scroll, visual state. A window IS a
component — it implements `Component` by rendering its buffer content into
the grid.

```rust
pub struct Window {
    id: WinId,
    buf: BufId,
    config: WinConfig,
    cursor: Cursor,
    scroll: Scroll,
    dirty: bool,
    // ... vim, kill_ring, focusable
}

impl Component for Window {
    fn draw(&self, area: Rect, grid: &mut GridSlice, ctx: &DrawContext) {
        // Render buffer lines + highlights + virtual text + border
    }
}
```

### Layout

Region tree that positions split windows. Floats layer on top. Unchanged
from current design but now feeds into the compositor.

### Event dispatch

Input events route through the focus chain:

```
Key event
  → focused component (window/dialog/etc.)
  → parent component
  → global keymap
  → fallback
```

Mouse events hit-test the layout tree to find the target component.

---

## `ui` crate public API

```rust
// Buffer operations
ui.buf_create(opts) -> BufId
ui.buf_delete(buf)
ui.buf_get_lines(buf, start, end) -> &[String]
ui.buf_set_lines(buf, start, end, lines)
ui.buf_line_count(buf) -> usize
ui.buf_set_virtual_text(buf, line, chunks)
ui.buf_clear_virtual_text(buf, line)
ui.buf_set_mark(buf, name, pos)
ui.buf_get_mark(buf, name) -> Option<Mark>

// Window operations
ui.win_open_split(buf, config) -> WinId
ui.win_open_float(buf, config) -> WinId
ui.win_close(win)
ui.win_set_config(win, config)
ui.win_set_cursor(win, pos)
ui.win_get_cursor(win) -> CursorPos
ui.win_set_scroll(win, top_row)
ui.win_get_buf(win) -> BufId
ui.win_set_buf(win, buf)
ui.win_list() -> Vec<WinId>
ui.win_get_current() -> WinId
ui.win_set_current(win)

// Highlight
ui.hl_buf_add(buf, line, col_start, col_end, style)
ui.hl_buf_clear(buf, line_start, line_end)

// Layout
ui.layout_set(tree)
ui.layout_resize(w, h)

// Rendering (called by tui)
ui.render<W: Write>(w) -> io::Result<()>
ui.mark_dirty(win)
ui.force_redraw()

// Components
ui.register_component(id, Box<dyn Component>)
ui.remove_component(id)

// Event dispatch
ui.handle_key(key, mods) -> KeyResult
ui.handle_mouse(event) -> bool
```

## Mapping existing concepts

| Current (tui crate)         | New (ui crate)                              |
|-----------------------------|---------------------------------------------|
| `Screen`                    | `Compositor` + `Grid`                       |
| `RenderOut` / `Frame`       | `Grid` + diff engine in `Compositor`        |
| `Dialog` trait              | `Component` trait (float window)            |
| `FloatDialog`               | Float window component                      |
| `ConfirmDialog`             | Confirm component (float window)            |
| `HelpDialog`                | Help component (float window)               |
| `InputState`                | Prompt window component                     |
| `TranscriptWindow`          | Transcript window component                 |
| `BtwBlock`                  | Float window (Lua plugin)                   |
| `Notification`              | Ephemeral float component                   |
| `Completer`                 | Float component anchored to cursor          |
| `CmdlineState`              | Status bar component                        |
| `LayoutState`               | `Layout` tree + compositor                  |
| `StyleState`                | `Style` on cells + diff engine              |
| `DisplayBlock` / paint      | Buffer content + highlights → grid cells    |
| `BlockHistory`              | Managed by tui, projected into transcript   |
| `Vim`                       | `Vim` (already in ui crate)                 |
| `WindowCursor`              | `Cursor` (already in ui crate)              |
| `KillRing`                  | `KillRing` (already in ui crate)            |

## What stays in `tui`

- `App` struct, event loop, agent management
- Engine communication (`EngineHandle`, `UiCommand`, `EngineEvent`)
- `BlockHistory` + `StreamParser` + block rendering pipeline
- Session persistence
- Lua runtime + API bindings (calls through `ui::*`)
- Permission system
- Commands (slash commands are app-level, not framework-level)
- Terminal setup/teardown (raw mode, alternate screen, etc.)

The `tui` crate calls `ui.render(&mut writer)` each frame. The rendering
pipeline (Block → lines) stays in `tui` but writes output into ui buffers.
Components in ui handle the rest.

---

# Implementation phases

Each phase produces a working, compilable system. No phase breaks existing
functionality.

## Phase 0–2: Foundation (DONE)

Core types, text primitives, and layout engine are implemented:

- `crates/ui/` crate with `BufId`, `WinId`, `Buffer`, `Window`, `Ui` struct
- Text primitives moved: `EditBuffer`, `Vim`, `KillRing`, `Cursor`, `Undo`
- Layout engine: `LayoutTree`, constraint solver, `Rect`, float resolution
- Buffer highlights: `Span`, `SpanStyle`, per-line styled content
- Float renderer: `render_float()` with border + styled content
- `tui` depends on `ui`, re-exports moved types

## Phase 3: Cell Grid + Style

**Goal:** Build the cell grid — the intermediate rendering surface between
components and the terminal.

- Define `Cell` struct: `symbol: char`, `style: Style`
- Define `Style` struct: fg, bg, bold, dim, italic, underline, crossedout
  (maps to our existing `SpanStyle` but for cells, not spans)
- Define `Grid` struct: `Vec<Cell>`, width, height
- `Grid::new(w, h)` — fill with empty cells
- `Grid::cell(x, y) -> &Cell`, `Grid::cell_mut(x, y) -> &mut Cell`
- `Grid::set(x, y, symbol, style)` — write a single cell
- `Grid::print(x, y, text, style)` — write a string starting at (x, y),
  handling multi-width characters
- `Grid::fill(rect, symbol, style)` — fill a rectangular region
- `Grid::clear(rect)` — reset region to empty cells
- `GridSlice` — borrowed rectangular view for safe sub-region writes
- `Grid::slice_mut(rect) -> GridSlice` — borrow a sub-region
- `GridSlice` implements the same write methods as `Grid`, offset to its area
- Diff engine: `Grid::diff(prev) -> impl Iterator<Item = CellUpdate>`
  where `CellUpdate { x, y, cell }` — yields only changed cells
- SGR emission: convert `CellUpdate` stream to crossterm commands,
  minimizing attribute changes (reuse existing `StyleState` diff logic from
  `RenderOut::emit_diff`)
- Unit tests: write cells, diff grids, verify minimal update set

## Phase 4: Component trait + Compositor

**Goal:** Define the component contract and the compositor that orchestrates
rendering through the grid.

- Define `Component` trait:
  - `draw(&self, area: Rect, grid: &mut GridSlice, ctx: &DrawContext)`
  - `handle_key(&mut self, key: KeyEvent) -> KeyResult`
  - `is_dirty(&self) -> bool`
  - `mark_dirty(&mut self)` / `mark_clean(&mut self)`
  - `cursor(&self) -> Option<(u16, u16)>` — cursor position if focused
- Define `DrawContext`: terminal size, focused component id, theme
- Define `KeyResult`: `Consumed`, `Ignored`, `Action(String)`
- Define `Compositor` struct:
  - Owns `Grid` (current frame) + `Grid` (previous frame)
  - Manages ordered list of components with their assigned `Rect`
  - `render<W: Write>(w) -> io::Result<()>`:
    1. Resolve layout → assign rects
    2. For each dirty component: draw into grid slice
    3. Diff grids → emit SGR sequences
    4. Swap current/prev grids
    5. Mark all components clean
  - `handle_key(key) -> KeyResult` — route to focused component
  - `resize(w, h)` — resize grids, mark all dirty
- Window implements `Component`:
  - `draw()` renders buffer lines + highlights + border into grid slice
  - `handle_key()` delegates to vim/keymap
  - Dirty when buffer `changedtick` changes or scroll/cursor moves
- Unit tests: compositor with mock components, verify grid output

## Phase 5: Wire compositor into tui render loop

**Goal:** Replace the current `RenderOut` direct-write path with the grid-based
compositor. The existing rendering still works — this is the bridge.

- Add `Compositor` to `Ui` struct (or as a peer managed by `App`)
- Create a `LegacyBridge` component that wraps the current `Screen` rendering:
  - `draw()` calls the existing block paint pipeline but writes to grid
    instead of `RenderOut`
  - This lets the old rendering work through the new grid path
- `App::render_frame()` calls `ui.render(&mut writer)` which:
  1. Runs the compositor (layout → draw → diff → emit)
  2. Wraps output in synchronized update envelope
- Remove direct `RenderOut` usage from the main render path
- The `LegacyBridge` is temporary scaffolding — it gets deleted as individual
  components migrate in later phases
- Verify: all existing functionality works through the grid path

## Phase 6: Migrate dialogs to components

**Goal:** Kill the `Dialog` trait. Every dialog becomes a `Component` rendered
as a float window through the compositor.

- Build `DialogComponent` — a generic component for modal float windows:
  - Draws bordered float with title, scrollable content, optional footer
  - Handles common keys: scroll (up/down/page), dismiss (Esc), confirm (Enter)
  - Configurable via `DialogConfig`: title, border style, footer items, accent
- Migrate one dialog at a time (simplest first):
  - `HelpDialog` → `HelpComponent` (static content, scroll, dismiss)
  - `ExportDialog` → `ExportComponent` (list selection)
  - `FloatDialog` → Lua float becomes a generic `DialogComponent`
  - `ConfirmDialog` → `ConfirmComponent` (most complex: preview + selection)
  - Remaining: Resume, Rewind, Permissions, Ps, Agents, Question
- For each migration:
  - Create a component with a buffer + float window
  - Content goes into the buffer (styled with highlights)
  - Component draws buffer content into its grid slice
  - Key handling returns `KeyResult::Action("dismiss")` etc.
  - Delete the old `Dialog` impl
- Delete: `Dialog` trait, `DialogResult`, `ListState` (shared dialog helpers),
  `active_dialog`, `open_dialog`, `finalize_dialog_close`, `FloatOp`,
  `pending_float_ops`, `drain_float_ops`

## Phase 7: Migrate prompt and transcript

**Goal:** The two main panes become proper components.

- **Transcript component:**
  - Wraps the existing block rendering pipeline
  - `draw()` renders blocks → styled lines → grid cells
  - Scroll, selection, copy through the component
  - Block cache stays in `tui`, but output flows through grid
- **Prompt component:**
  - Text input with vim motions, undo, kill ring
  - `draw()` renders edit buffer content + cursor + ghost text → grid cells
  - Key handling: insert mode input, vim motions, completion trigger
- **Status bar component:**
  - Single-row component at bottom
  - Renders mode indicator, spinner, metrics, notifications
- Delete: `InputState`, `TranscriptWindow`, `Screen` struct, old layout code
- `tui` creates these components at startup and registers them with the compositor

## Phase 8: Event dispatch

**Goal:** Input routing through the component tree is framework-level.

- Compositor manages focus stack (ordered by z-index for floats)
- `handle_key()` walks focus chain: focused component → parent → global
- `handle_mouse()` hit-tests layout tree → route to target component
- Keymap system: buffer-local, window-local, global scopes
- `tui` registers its keymaps via the ui API
- Vim integration: vim state on windows, key handling is framework-level

## Phase 9: Lua bindings rewrite

**Goal:** Lua talks to `ui` directly. `smelt.api.buf.*` / `win.*` maps 1:1.

- Rewrite `lua.rs` buf/win sections to call ui API
- Lua can create buffers, open float windows, set content, add highlights
- Lua components: a Lua plugin can register a component that draws via
  buffer content (Lua writes lines + highlights, Rust renders grid cells)
- Remove `PendingOp::OpenFloat` / `UpdateFloat` / `CloseFloat`
- Port `btw.lua`, `predict.lua`, `plan_mode.lua` to clean API

## Phase 10: Cleanup and polish

**Goal:** Delete everything the new framework replaces.

- Delete: old `Screen` code, `RenderOut` direct usage, `LegacyBridge`
- Delete: `DisplayBlock` / `SpanCollector` / `paint_line` (replaced by grid)
- Audit `pub` items in `ui` — hide internals
- Documentation: `docs/lua-api.md`, plugin authoring guide
- README update
- Full test suite pass

---

# Dependency graph

```
Phase 0–2 (DONE: types, text primitives, layout)
    │
    ▼
Phase 3 (cell grid + style)
    │
    ▼
Phase 4 (component trait + compositor)
    │
    ▼
Phase 5 (wire into tui render loop)
    │
    ├──────────────────┐
    ▼                  ▼
Phase 6 (dialogs)   Phase 7 (prompt + transcript)
    │                  │
    └────────┬─────────┘
             ▼
Phase 8 (event dispatch)
             │
             ▼
Phase 9 (Lua bindings)
             │
             ▼
Phase 10 (cleanup)
```

Phases 6 and 7 can proceed in parallel once Phase 5 is done.

---

# Non-goals

- **Using ratatui.** We build our own — the abstraction mismatch is too large.
- **Plugin registry / package manager.** Lua scripts in `~/.config/smelt/`.
- **Remote UI protocol (v1).** Local terminal only. API accommodates future RPC.
- **Async Lua.** Sync-only; snapshot/queue pattern avoids borrow issues.
- **Full nvim compatibility.** We borrow the model, not the exact API.
- **Immediate mode.** We are retained mode with dirty tracking.

---

# Completed work

All prior phases (A–E, T1–T9, L1–L5.5) are complete. See git history.

Key outcomes:
- Alt-buffer rendering, top-relative coordinates, viewport pin
- `Window` trait, `Buffer`, `WindowCursor`, `Vim` state machine
- Block rendering pipeline with layout caching
- `TranscriptSnapshot` with nav buffer, selection, copy pipeline
- Lua runtime with `smelt.api.*` surface, autocmds, user commands, keymaps
- `EngineSnapshot` / `PendingOp` snapshot/queue pattern
- `FloatDialog` (interim — replaced by Phase 6)

Phase 0–2 (ui crate foundation):
- `crates/ui/` with `BufId`, `WinId`, `Buffer`, `Window`, `Ui`
- Text primitives: `EditBuffer`, `Vim`, `KillRing`, `Cursor`, `Undo`
- Layout: `LayoutTree`, constraint solver, float resolution
- Buffer highlights: `Span`, `SpanStyle`, per-line styled content
- Float renderer: `render_float()` (temporary — replaced by grid compositor)
