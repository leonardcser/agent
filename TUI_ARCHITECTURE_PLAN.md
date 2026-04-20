# TUI Architecture — UI Framework Rewrite

## Vision

Build a **retained-mode TUI rendering framework** (`crates/ui/`) inspired by
Neovim's architecture but designed for Rust's ownership model. The framework
provides a cell grid, compositor, and component system where every visible
surface is a window or component that draws into a grid region.

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

## Design principles

### Everything is a window (except the status bar)

Every interactive surface in the UI is a **window** backed by a **buffer**:

- **Transcript** — split window, readonly buffer, block content projected in
- **Prompt** — split window, editable buffer with vim motions
- **All dialogs** — float windows (help, confirm, resume, rewind, etc.)
- **BtwBlock** — float window (not a custom overlay)
- **Notifications** — ephemeral float window with auto-dismiss
- **Completions** — float window anchored to cursor position
- **Lua floats** — float windows created by plugins

The **status bar** is the only non-window surface — it's a single-row
component with no buffer, no scroll, no cursor. Making it a window would
force an abstraction that doesn't fit.

### One float dialog pattern

All dialogs follow the same visual structure:

```
┌─ Title ────────────────────┐
│                             │  ← scrollable content (BufferView)
│                             │
│  1. Option A                │  ← optional footer (ListSelect)
│  2. Option B                │
│                  [hints]    │
└─────────────────────────────┘
```

Each dialog is a **configuration** of a single `FloatDialog` component, not
a separate implementation. The differences between dialogs are:

| Dialog      | Content                        | Footer              |
|-------------|--------------------------------|----------------------|
| Help        | Key binding table              | None (scroll only)   |
| Export      | 2 options                      | ListSelect           |
| Rewind      | Numbered turns                 | ListSelect           |
| Resume      | Filtered session list          | ListSelect + search  |
| Permissions | Section headers + entries      | ListSelect + delete  |
| Ps          | Process list                   | ListSelect + kill    |
| Agents list | Agent rows                     | ListSelect + detail  |
| Agent detail| Prompt + tool calls            | Scroll only          |
| Float (Lua) | Lines from Lua                 | Optional ListSelect  |
| Confirm     | Preview (diff/code/plan)       | ListSelect + textarea|
| Question    | Question text + options        | ListSelect + textarea|

### Shared rendering for diffs and code

Code diffs, syntax-highlighted files, and notebook previews render into
**buffers with highlights**. The same rendering code produces content for
both transcript blocks and confirm dialog previews. This means:

- Diffs in the transcript use the same code as diffs in confirm dialogs
- A confirm dialog's preview is an interactive buffer you can scroll through
- Lua plugins can create buffers with highlighted content using the same API

### Simplify question dialogs

The current question dialog has a complex tab system with `active_tab`,
`visited`, `answered`, `multi_toggles`, `other_areas`, `editing_other`.
Replace with a sequential wizard-style flow: one question per float,
advance to next on answer. Same visual pattern as every other dialog.

### Agent detail as separate float

The agents dialog currently has a two-mode design (list → detail with mode
switch). Replace with two separate floats: selecting an agent in the list
closes it and opens a detail float. Simpler state, no mode switching.

## Why not ratatui

We evaluated ratatui and decided against it:

- **Immediate mode vs retained.** Ratatui rebuilds the entire UI every frame.
  We want retained mode with dirty tracking.
- **No windows.** No concept of persistent viewports with cursor, scroll, focus.
- **No z-order.** Composites by render order only.
- **Abstraction clash.** Ratatui's `Buffer` = cell grid. Our `Buffer` = content model.

What we take: the cell grid concept as an intermediate rendering surface.

## Why a separate crate

- **Forces clean boundaries.** Can't import `protocol::Message` in `crates/ui/`.
- **Testable in isolation.** Unit-test grid, layout, components without an engine.
- **Reusable.** General TUI toolkit — not smelt-specific.
- **Makes the API surface explicit.** The `pub` items in `ui` *are* the API.

---

## Core architecture

### Cell Grid

2D array of `Cell { symbol, style }` between components and the terminal.
Components never emit escape sequences — they write cells to a grid region.
`GridSlice` is the Rust ownership adaptation: a borrowed rectangular view.

### Component

Retained rendering unit. Each UI surface implements `Component`:
- `draw()` — writes cells into its grid slice
- `handle_key()` — returns Consumed, Ignored, or Action(string)
- `is_dirty()` / `mark_dirty()` / `mark_clean()` — dirty tracking
- `cursor()` — cursor position if focused

### Compositor

Manages the component tree, orchestrates rendering, diffs frames.
Each frame: resolve layout → draw dirty components → diff grids → emit SGR.

### Buffer (content model)

Lines + highlights + marks + virtual text. Buffers are the data model —
components read buffers and write cells to the grid.

### Window

Viewport into a buffer with cursor, scroll, visual state. Windows are
components that render their buffer content into the grid.

### FloatDialog

Reusable component that composes BufferView + optional ListSelect + optional
TextInput. All dialogs are configurations of this component. Handles:
- Border + title chrome
- Scrollable content area (buffer view)
- Optional selectable footer (list select)
- Optional inline text input
- Common keys: scroll, dismiss (Esc), confirm (Enter)

### Layout

Region tree that positions split windows. Floats layer on top via z-index.

### Event dispatch

Focus chain: focused component → parent → global keymap → fallback.
Mouse events hit-test the layout tree.

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

| Current (tui crate)           | New (ui crate)                                |
|-------------------------------|-----------------------------------------------|
| `Screen`                      | `Compositor` + `Grid`                         |
| `RenderOut` / `Frame`         | `Grid` + diff engine in `Compositor`          |
| `Dialog` trait (9 impls)      | `FloatDialog` component (one impl, N configs) |
| `FloatDialog` (Lua)           | `FloatDialog` component                       |
| `ConfirmDialog` (985 lines)   | `FloatDialog` with preview buffer + ListSelect|
| `HelpDialog`                  | `FloatDialog` with keybindings buffer         |
| `QuestionDialog` (tabs)       | Sequential `FloatDialog` per question         |
| `AgentsDialog` (2-mode)       | List `FloatDialog` + Detail `FloatDialog`     |
| `BtwBlock` (custom overlay)   | `FloatDialog` with question content           |
| `Notification`                | Ephemeral float window                        |
| `Completer` (custom popup)    | Float window anchored to cursor               |
| `InputState`                  | Prompt split window                           |
| `TranscriptWindow`            | Transcript split window                       |
| `CmdlineState` / status line  | `StatusBar` component                         |
| `LayoutState`                 | `Layout` tree + compositor                    |
| `StyleState`                  | `Style` on cells + diff engine                |
| `DisplayBlock` / paint        | Buffer content + highlights → grid cells      |
| `ListState` (shared helper)   | `ListSelect` component                        |
| `TextArea` (shared helper)    | `TextInput` component                         |
| `BlockHistory`                | Managed by tui, projected into transcript buf |
| `ConfirmPreview` (5 variants) | Diff/code rendered into buffer with highlights|
| `Vim`                         | `Vim` (already in ui crate)                   |

## What stays in `tui`

- `App` struct, event loop, agent management
- Engine communication (`EngineHandle`, `UiCommand`, `EngineEvent`)
- `BlockHistory` + `StreamParser` + block rendering pipeline
- Session persistence
- Lua runtime + API bindings (calls through `ui::*`)
- Permission system
- Commands (slash commands are app-level, not framework-level)
- Terminal setup/teardown (raw mode, alternate screen, etc.)
- Dialog-specific logic (what content to show, what actions mean)

The `tui` crate calls `ui.render(&mut writer)` each frame. The block
rendering pipeline writes output into ui buffers. Dialog opening creates
a `FloatDialog` with appropriate content and footer configuration.

---

# Implementation phases

Each phase produces a working, compilable system. No phase breaks existing
functionality.

## Phase 0–2: Foundation (DONE)

Core types, text primitives, layout engine:
- `crates/ui/` with `BufId`, `WinId`, `Buffer`, `Window`, `Ui`
- Text primitives: `EditBuffer`, `Vim`, `KillRing`, `Cursor`, `Undo`
- Layout: `LayoutTree`, constraint solver, float resolution
- Buffer highlights: `Span`, `SpanStyle`, per-line styled content

## Phase 3–5: Grid + Components + FloatDialog (DONE)

Cell grid, compositor, primitive components, and unified dialog:
- `Grid`, `Cell`, `Style`, `GridSlice` — cell-level rendering surface
- `flush_diff()` — SGR emission from grid diffs
- `Component` trait — `draw()`, `handle_key()`, `cursor()`
  (no dirty flags — compositor always draws all layers, grid diff
  handles change detection at the cell level)
- `Compositor` — manages layers, orchestrates render, focus routing
- `BufferView` — renders buffer content with highlights and borders
- `ListSelect` — selectable list with indicators and navigation
- `TextInput` — single-line text editor with cursor
- `StatusBar` — left/right segmented status line
- `FloatDialog` — unified dialog composing BufferView + optional
  ListSelect footer + optional TextInput. All dialogs will be
  configurations of this single component. Supports border/title
  chrome, content/footer/input/hints layout, Tab focus cycling,
  vim-style scroll keys, and action-based key results
  (`select:N`, `dismiss`, `submit:text`).

## Phase 6: Compositor takes over rendering

**Goal:** Replace `RenderOut` + `Frame` with compositor as sole frame
renderer. Every visible surface becomes a `Component` drawing into a
`GridSlice`. No intermediate adapters or legacy bridges.

### Architecture

```
Compositor (full screen grid, owns the frame)
├── TranscriptView    — split, renders DisplayLines → grid cells
├── PromptView        — split, renders input + ghost text → grid cells
├── StatusBar         — 1-row component (already built in ui crate)
└── FloatDialog(s)    — float layers (already built in ui crate)
```

### Data flow

Components don't hold references to app state — they hold snapshots.
Before each frame, the app pushes fresh data into components:

```
app computes viewport → transcript_view.set_lines(visible_lines)
app reads input state → prompt_view.set_input(text, cursor, ghost)
app reads status data → status_bar.set_left/set_right(segments)
                      → compositor.render(&mut stdout)
```

### Implementation steps

**Step 1: `paint_line_to_grid`** — pure function, no behavior change.
Converts a `DisplayLine` (spans + SpanStyle + ColorValue) into grid
cells by resolving theme colors to `ui::Style` and writing chars to
a `GridSlice` row. Lives in `tui` crate (bridges tui's DisplayLine
with ui's GridSlice). Same logic as `paint_line` but targets cells
instead of escape sequences.

**Step 2: `TranscriptView` component** — lives in `tui`, implements
`ui::Component`. Holds `Vec<DisplayLine>` + scrollbar state + cursor
state. `draw()` calls `paint_line_to_grid` per visible line, paints
scrollbar column, paints soft cursor if focused. The app does viewport
calculation (which blocks, skip count, clamped scroll) and pushes
visible lines before each frame.

**Step 3: `PromptView` component** — lives in `tui`, implements
`ui::Component`. Holds input text snapshot, cursor position, mode,
ghost text, queued commands. `draw()` renders input lines + ghost
text + prompt chrome into grid cells.

**Step 4: Wire compositor into `App`** — add `Compositor` to `App`.
`render_frame()` calls `compositor.render(&mut stdout)` instead of
`Frame::begin()` + `RenderOut`. Compositor handles synchronized
updates, grid diffing, cursor positioning. `Screen`'s draw methods
are replaced by component draw methods.

**Step 5: Migrate dialogs** — each of the 10 Dialog implementations
becomes a `FloatDialog` configuration added as a compositor layer.
Migration order (simplest first):
1. HelpDialog → FloatDialog with keybindings in buffer, no footer
2. ExportDialog → FloatDialog with 2-item ListSelect footer
3. RewindDialog → FloatDialog with turn list in ListSelect
4. FloatDialog (Lua) → FloatDialog with Lua content + optional footer
5. PermissionsDialog → FloatDialog with section content + ListSelect
6. PsDialog → FloatDialog with process list + ListSelect
7. ResumeDialog → FloatDialog with session list + search TextInput
8. AgentsDialog → Two FloatDialogs (list + detail)
9. QuestionDialog → Sequential FloatDialogs (one per question)
10. ConfirmDialog → FloatDialog with preview buffer + ListSelect + TextInput

Also migrate: BtwBlock, Notifications, Completions → float layers.

**Step 6: Delete legacy rendering** —
- `Dialog` trait, `DialogResult`, `ListState`, `TextArea`
- `Frame`, `RenderOut`, `StyleState` (SGR/style stack machinery)
- `paint_line` (replaced by `paint_line_to_grid`)
- `Screen::draw_viewport_frame`, `draw_viewport_dialog_frame`
- `active_dialog`, `open_dialog`, `finalize_dialog_close`
- All individual dialog structs in `render/dialogs/`

## Phase 7: Event dispatch

**Goal:** Input routing through component tree is framework-level.

- Compositor manages focus stack (z-index ordered for floats)
- `handle_key()` walks: focused → parent → global keymap
- `handle_mouse()` hit-tests layout → route to target
- Keymap system: buffer-local, window-local, global scopes
- Vim integration: vim state on windows, framework-level handling

## Phase 8: Lua bindings rewrite

**Goal:** Lua talks to `ui` directly. `smelt.api.buf/win` maps 1:1.

- Rewrite `lua.rs` buf/win sections to call ui API
- Lua creates buffers, opens float windows, sets content, adds highlights
- Remove `PendingOp::OpenFloat` / `UpdateFloat` / `CloseFloat`
- Port `btw.lua`, `predict.lua`, `plan_mode.lua` to clean API

## Phase 9: Cleanup and polish

**Goal:** Audit and finalize.

- Audit `pub` items in `ui` — hide internals
- Documentation: `docs/lua-api.md`, plugin authoring guide
- README update, full test suite pass

---

# Dependency graph

```
Phase 0–2 (DONE: types, text primitives, layout)
    │
    ▼
Phase 3–5 (DONE: grid, components, compositor, FloatDialog)
    │
    ▼
Phase 6 (compositor takes over: transcript + prompt + status + dialogs)
    │
    ▼
Phase 7 (event dispatch)
    │
    ▼
Phase 8 (Lua bindings)
    │
    ▼
Phase 9 (cleanup)
```

---

# Non-goals

- **Using ratatui.** Abstraction mismatch too large.
- **Plugin registry / package manager.** Lua scripts in `~/.config/smelt/`.
- **Remote UI protocol (v1).** Local terminal only.
- **Async Lua.** Sync-only; snapshot/queue pattern avoids borrow issues.
- **Full nvim compatibility.** We borrow the model, not the exact API.
- **Immediate mode.** We are retained mode with dirty tracking.

---

# Completed work

All prior phases (A–E, T1–T9, L1–L5.5) are complete. See git history.

Key outcomes:
- Alt-buffer rendering, top-relative coordinates, viewport pin
- Block rendering pipeline with layout caching
- Lua runtime with `smelt.api.*` surface, autocmds, user commands, keymaps
- `EngineSnapshot` / `PendingOp` snapshot/queue pattern

Phase 0–4 (ui crate):
- `crates/ui/` with core types, text primitives, layout engine
- Cell grid + style + SGR flush engine
- Component trait + Compositor (retained-mode rendering)
- BufferView, ListSelect, TextInput, StatusBar components
