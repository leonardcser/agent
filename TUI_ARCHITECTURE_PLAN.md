# TUI Architecture — UI Framework Rewrite

## Vision

Build a **general-purpose TUI framework** (`crates/ui/`) inspired by neovim's
buf/win/layout model. Every visible surface — transcript, prompt, dialogs,
notifications, completions, status bar — is a window backed by a buffer. The
framework knows nothing about agents, engines, or protocols. The `tui` crate
becomes a thin app shell that wires `ui` primitives to smelt-specific logic.

Both internal Rust code and Lua plugins talk through the same `ui::Api`. If the
API can't express something, the API is incomplete — fix the API, don't bypass
it.

## Why a separate crate

- **Forces clean boundaries.** If you can't import `protocol::Message` in
  `crates/ui/`, you can't accidentally couple UI to domain logic.
- **Testable in isolation.** Unit-test buffer ops, layout constraints, and
  rendering without spinning up an engine.
- **Reusable.** The framework is a general TUI toolkit — not smelt-specific.
- **Makes the API surface explicit.** The `pub` items in `ui` *are* the API.

## Core primitives

### Buffer

Content container. A sequence of lines with metadata.

```rust
pub struct Buffer {
    id: BufId,
    lines: Vec<String>,
    modifiable: bool,
    buftype: BufType,           // Normal, Nofile, Prompt, Scratch
    virtual_text: Vec<VirtualText>,  // ghost text, decorations
    marks: HashMap<String, Mark>,
    changedtick: u64,
}
```

Buffers exist independently of windows. Multiple windows can display the same
buffer. Creating a buffer does not display it.

### Window

Viewport into a buffer. Owns cursor, scroll, visual state.

```rust
pub struct Window {
    id: WinId,
    buf: BufId,
    config: WinConfig,
    cursor: Cursor,             // byte offset + selection anchor
    scroll: Scroll,             // top row, pinned state
    vim: Option<VimState>,
    kill_ring: KillRing,
    focusable: bool,
    zindex: u16,                // stacking order for floats
}

pub enum WinConfig {
    Split {
        region: Region,         // named slot in the layout tree
        gutters: Gutters,       // pad_left, pad_right, scrollbar
    },
    Float {
        relative: FloatRelative, // editor, cursor, win(parent)
        anchor: Anchor,          // NW, NE, SW, SE
        row: i32,
        col: i32,
        width: Constraint,       // fixed, pct, fill
        height: Constraint,
        border: Border,
        title: Option<String>,
        zindex: u16,
    },
}
```

### Layout

Region tree that positions split windows. Floats layer on top.

```
Root
├── Split(Vertical)
│   ├── Region("transcript", fill)
│   └── Region("prompt", min=1, max=50%)
├── FloatLayer (z-sorted)
│   ├── Float(dialog_win)
│   ├── Float(notification_win)
│   └── Float(completion_win)
└── Fixed("status", 1 row, bottom)
```

### Renderer

Diff-based terminal output. Each frame:
1. Compute layout → assign rects to all windows
2. For each window, render buffer content into its rect
3. Apply highlights, virtual text, decorations
4. Diff against previous frame → emit minimal SGR sequences
5. Flush inside synchronized update envelope

### Event dispatch

Input events route through the focus chain:

```
Key event
  → focused window's buffer-local keymap
  → focused window's window-local keymap
  → global keymap
  → fallback (insert char / motion / noop)
```

Mouse events hit-test the layout tree to find the target window.

## `ui` crate public API

```rust
// Buffer operations
ui::buf_create(opts) -> BufId
ui::buf_delete(buf)
ui::buf_get_lines(buf, start, end) -> Vec<String>
ui::buf_set_lines(buf, start, end, lines)
ui::buf_line_count(buf) -> usize
ui::buf_set_option(buf, key, value)
ui::buf_get_option(buf, key) -> Value
ui::buf_set_virtual_text(buf, line, chunks)
ui::buf_clear_virtual_text(buf, line)
ui::buf_set_mark(buf, name, pos)
ui::buf_get_mark(buf, name) -> Option<Mark>

// Window operations
ui::win_open(buf, config) -> WinId
ui::win_close(win)
ui::win_set_config(win, config)
ui::win_get_config(win) -> WinConfig
ui::win_set_cursor(win, pos)
ui::win_get_cursor(win) -> CursorPos
ui::win_set_scroll(win, top_row)
ui::win_get_scroll(win) -> u16
ui::win_set_option(win, key, value)
ui::win_get_buf(win) -> BufId
ui::win_set_buf(win, buf)
ui::win_list() -> Vec<WinId>
ui::win_get_current() -> WinId
ui::win_set_current(win)

// Layout
ui::layout_set(tree)            // define the region tree
ui::layout_get() -> LayoutTree
ui::layout_resize(w, h)        // terminal resize

// Rendering
ui::render(backend) -> Frame    // compute + emit a frame
ui::mark_dirty(win)             // schedule repaint
ui::redraw()                    // force full repaint

// Highlights
ui::hl_define(name, attrs)
ui::hl_buf_add(buf, hl_name, line, col_start, col_end)
ui::hl_buf_clear(buf, line_start, line_end)

// Keymaps
ui::keymap_set(scope, chord, action)   // scope: global | buf(id) | win(id)
ui::keymap_del(scope, chord)

// Events
ui::on(event, handler) -> SubId
ui::off(sub_id)
ui::emit(event, data)           // internal: fire event to subscribers
```

## Mapping existing concepts

| Current (tui crate)         | New (ui crate)                              |
|-----------------------------|---------------------------------------------|
| `Screen`                    | `ui::Ui` (owns all bufs, wins, layout)      |
| `InputState`                | Split window (prompt region) + buffer       |
| `TranscriptWindow`          | Split window (transcript region) + buffer   |
| `Dialog` trait              | Float window + buffer                       |
| `FloatDialog`               | Float window + buffer + footer widget       |
| `BtwBlock`                  | Float window (Lua plugin)                   |
| `Notification`              | Ephemeral float window                      |
| `Completer`                 | Float window anchored to cursor             |
| `CmdlineState`              | Status bar window (special region)          |
| `LayoutState`               | `ui::Layout`                                |
| `RenderOut` / `Frame`       | `ui::Renderer`                              |
| `StyleState`                | `ui::Style` + highlight groups              |
| `DisplayBlock` / paint      | Buffer content + highlight overlays         |
| `BlockHistory`              | Managed by tui, projected into transcript buf |
| `Vim`                       | `ui::VimState` (moves into ui crate)        |
| `WindowCursor`              | `ui::Cursor`                                |
| `KillRing`                  | `ui::KillRing`                              |

## What stays in `tui`

- `App` struct, event loop, agent management
- Engine communication (`EngineHandle`, `UiCommand`, `EngineEvent`)
- `BlockHistory` + `StreamParser` + block rendering pipeline
- Session persistence
- Lua runtime + API bindings (calls through `ui::*`)
- Permission system
- Commands (slash commands are app-level, not framework-level)

The `tui` crate *uses* the `ui` framework to create windows, set content, and
render frames. The rendering pipeline (Block → DisplayBlock → lines) stays in
`tui` but writes its output into ui buffers instead of directly to the terminal.

---

# Implementation phases

Each phase produces a working, compilable system. No phase breaks existing
functionality — new abstractions wrap the old, then the old is deleted once
all callers migrate.

## Phase 0: Create the `ui` crate with core types

**Goal:** Establish the crate boundary and define the type vocabulary.

- Create `crates/ui/Cargo.toml` (deps: crossterm, unicode-width)
- Define: `BufId`, `WinId` (newtype u64 with slotmap-style generational IDs)
- Define: `Buffer` struct (lines, modifiable, buftype, changedtick, virtual_text)
- Define: `Window` struct (buf, config, cursor, scroll, focusable, zindex)
- Define: `WinConfig` enum (Split, Float) with all config fields
- Define: `Constraint` (Fixed, Pct, Fill), `Anchor`, `Border`, `FloatRelative`
- Define: `Gutters` struct
- Define: `Cursor` (byte offset, selection anchor, curswant)
- Define: `Scroll` (top_row, pinned)
- Define: `BufType` enum (Normal, Nofile, Prompt, Scratch)
- Define: `VirtualText` struct (line, col, chunks, hl_group)
- Define: `Mark` struct (line, col)
- `Ui` struct: `bufs: HashMap<BufId, Buffer>`, `wins: HashMap<WinId, Window>`,
  `current_win: WinId`, `next_buf_id`, `next_win_id`
- Implement: `buf_create`, `buf_delete`, `buf_get_lines`, `buf_set_lines`,
  `buf_line_count`
- Implement: `win_open`, `win_close`, `win_get_buf`, `win_set_buf`,
  `win_list`, `win_get_current`, `win_set_current`
- Unit tests for buffer CRUD and window lifecycle
- Wire into workspace `Cargo.toml`
- `tui` does NOT depend on `ui` yet — this is just the foundation

## Phase 1: Move text primitives into `ui`

**Goal:** The existing `Buffer` (text + undo) and vim module migrate to `ui`.

- Move `Buffer` (the text type from `tui/src/buffer.rs` or equivalent) into
  `ui::buffer`, adapting it to the new `Buffer` struct
- Move `UndoHistory` into `ui::undo`
- Move `KillRing` into `ui::kill_ring`
- Move `Cursor` / `WindowCursor` into `ui::cursor`
- Move `Vim` state machine + motions + text objects into `ui::vim`
- `tui` adds `ui` as a dependency; existing code imports from `ui::` instead
  of local modules
- All existing tests continue to pass — this is a pure code-move

## Phase 2: Layout engine in `ui`

**Goal:** Replace the hardcoded layout (transcript rect + prompt rect + status row)
with a constraint-based region tree.

- Define `LayoutTree` (recursive: `Split(dir, children)` | `Leaf(name, constraint)`)
- Define `LayoutResult`: resolved `HashMap<String, Rect>` + float placements
- Implement constraint solver: fixed sizes first, then distribute remaining
  space among fill/pct regions
- Float positioning: resolve anchor + relative to parent rect
- Port existing `LayoutState::compute()` logic to use the new solver
- `tui` calls `ui::layout_set(tree)` at startup, `ui::layout_resize(w, h)` on
  terminal resize
- Unit tests: fixed + pct + fill combinations, float anchoring, edge cases
  (terminal too small, zero-height regions)

## Phase 3: Window manager

**Goal:** `Ui` struct manages the window tree. All window CRUD goes through
the API. Focus, z-ordering, and hit-testing are centralized.

- `Ui::open_split(buf, region_name, opts) -> WinId`
- `Ui::open_float(buf, float_config) -> WinId`
- `Ui::close(win)`
- `Ui::set_current(win)` — focus management
- `Ui::win_at(row, col) -> Option<WinId>` — hit-testing from layout
- `Ui::floats_z_ordered() -> Vec<WinId>` — for rendering back-to-front
- Float stacking: higher zindex on top, most recently opened wins ties
- `tui` creates transcript + prompt windows at startup via `Ui` API
- Dialog open/close goes through `Ui::open_float` / `Ui::close`

## Phase 4: Migrate dialogs to float windows

**Goal:** Kill the `Dialog` trait. Every dialog becomes a float window backed
by a buffer.

- Define `ui::FloatWidget` trait (optional): provides `handle_key` +
  `render_into_buffer` for stateful float content. This is what dialog
  implementations become — they render into their buffer instead of directly
  to RenderOut.
- Migrate one dialog at a time (start with simplest: `HelpDialog`):
  - Create a buffer with help text
  - Open a float window with title + border
  - Key handling: the float's buffer-local keymap handles dismiss/nav
  - Delete the old `HelpDialog` struct
- Migrate remaining dialogs: Export → Rewind → Resume → Permissions → Ps →
  Agents → Confirm → Question
- `FloatDialog` (the Lua-driven generic float) is already a float — just
  rewire it to use `Ui::open_float` instead of the ad-hoc `pending_float_ops`
- Delete: `Dialog` trait, `DialogResult` enum, `ListState`, `active_dialog`
  local variable, `open_dialog` / `finalize_dialog_close` methods
- All dialog rendering now goes through the normal window render path

## Phase 5: Migrate prompt and transcript

**Goal:** The two main panes become proper `ui` windows.

- **Prompt:** Create a prompt buffer (modifiable, buftype=Prompt) + split
  window in the "prompt" region. `InputState` becomes a thin wrapper that
  delegates text ops to `ui::buf_*` and cursor/vim to the window.
- **Transcript:** Create a transcript buffer (readonly, buftype=Nofile) +
  split window in the "transcript" region. The block rendering pipeline
  writes its output into this buffer's lines. `TranscriptSnapshot` moves
  to `ui` as the buffer's cached derived view.
- Ghost text (input predictions) → `buf_set_virtual_text` on the prompt buffer
- Selection, yank, vim motions all go through `ui` window methods
- `tui` no longer owns any window or buffer state directly — it's all in `Ui`

## Phase 6: Rendering engine in `ui`

**Goal:** The framework owns the full render pipeline. `tui` calls
`ui.render()` and gets a frame.

- Move `RenderOut`, `Frame`, `PooledBufWriter` into `ui::render`
- Move `StyleState`, SGR diff emission into `ui::style`
- Move `paint_line`, `apply_style` into `ui::paint`
- The render loop: for each window (split, then floats by z-order):
  - Clip to window rect
  - Render buffer content (lines + highlights + virtual text)
  - Render window chrome (border, title, scrollbar, gutters)
- Diff against previous frame buffer (cell grid) → emit only changed cells
- `tui` provides a `ContentRenderer` callback for the transcript buffer
  (block rendering pipeline produces styled lines — framework doesn't know
  about blocks, but accepts pre-rendered content)
- Move `theme.rs` into `ui` as the default theme with customization hooks
- Move `highlight.rs` (syntect integration) — or keep in `tui` and inject
  highlighted content into buffers

## Phase 7: Event dispatch in `ui`

**Goal:** Input routing is framework-level. The keymap chain (buffer → window
→ global) is managed by `ui`.

- `ui::handle_key(key, mods)` → walks focus chain, checks keymaps, returns
  `Action` or falls through
- `ui::handle_mouse(event)` → hit-test layout, route to target window
- `ui::handle_resize(w, h)` → relayout all windows
- Move keymap resolution (`keymap.rs`) into `ui`
- `tui` registers its keymaps via `ui::keymap_set` and handles `Action` results
- Vim integration: vim state lives on `ui::Window`, vim key handling is
  framework-level

## Phase 8: Lua bindings rewrite

**Goal:** Lua talks to `ui::*` directly. The `smelt.api.buf.*` / `win.*`
namespace maps 1:1 to the `ui` crate API.

- Rewrite `lua.rs` buf/win sections to call `ui::buf_*` / `ui::win_*`
- Remove `PendingOp::OpenFloat` / `UpdateFloat` / `CloseFloat` — Lua calls
  `ui` directly (through the snapshot/queue pattern)
- Remove `FloatOp` / `pending_float_ops` / `drain_float_ops`
- Port `btw.lua`, `predict.lua`, `plan_mode.lua` to use the clean API
- Full Lua API surface matches `ui` crate's pub interface

## Phase 9: Cleanup and polish

**Goal:** Delete everything the new framework replaces.

- Delete: old `Screen` rendering code, `Dialog` trait and all impls,
  `active_dialog` plumbing, `LayoutState::compute`, old `RenderOut` usage
- Delete: `BtwBlock`, `PluginConfirmSpec`, ad-hoc notification rendering
- Delete: `InputState` (replaced by prompt window), `TranscriptWindow`
  (replaced by transcript window)
- Audit all `pub` items in `ui` — hide anything that shouldn't be API
- Documentation: `docs/lua-api.md`, plugin authoring guide
- README update with UI framework capabilities
- Run the full test suite, fix regressions

---

# Dependency graph

```
Phase 0 (types)
    │
    ▼
Phase 1 (text primitives)
    │
    ▼
Phase 2 (layout engine)
    │
    ▼
Phase 3 (window manager)
    │
    ├──────────────────┐
    ▼                  ▼
Phase 4 (dialogs)   Phase 5 (prompt + transcript)
    │                  │
    └────────┬─────────┘
             ▼
Phase 6 (rendering)
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

Phases 4 and 5 can proceed in parallel once Phase 3 is done.

---

# Non-goals

- **Plugin registry / package manager.** Lua scripts live in `~/.config/smelt/`;
  bundled plugins ship in the binary.
- **Remote UI protocol (v1).** The framework renders to a local terminal.
  The API design accommodates a future msgpack-rpc layer but we don't build it.
- **Async Lua.** Sync-only; the snapshot/queue pattern avoids borrow issues.
- **Full nvim compatibility.** We borrow the conceptual model, not the exact
  API signatures. Our API should be clean for our use case.
- **Flattening BlockHistory.** Block structure is load-bearing for the rendering
  pipeline. The transcript buffer receives *projected* content from blocks.

---

# Completed work (prior phases)

All phases from the original plan (A through E, T1–T9, L1–L5.5) are complete.
See git history for details. Key outcomes:

- Alt-buffer rendering, top-relative coordinates, viewport pin
- `Window` trait, `Buffer`, `WindowCursor`, `Vim` state machine
- Block rendering pipeline with layout caching
- `TranscriptSnapshot` with nav buffer, selection, copy pipeline
- Lua runtime with `smelt.api.*` surface, autocmds, user commands, keymaps
- Plan mode extracted to Lua plugin
- `EngineSnapshot` / `PendingOp` snapshot/queue pattern
- Generalized callback registry, background LLM calls
- `FloatDialog` (interim — will be replaced by Phase 4)

## L6 interim work (buf/win float API)

Partial implementation of float API that bridges to the existing dialog system.
Will be superseded by Phase 3–4 but provides working Lua float support in the
meantime:

- `FloatDialog` implements `Dialog` trait (title, content, footer, scroll)
- `FloatOp` enum + `pending_float_ops` queue on `App`
- `drain_float_ops` bridges PendingOps to `active_dialog`
- `FloatSelect` / `FloatDismiss` dialog results fire Lua callbacks
- `as_any_mut()` on `Dialog` trait for downcast updates
- Lua APIs: `buf.create()`, `buf.set_lines()`, `win.open_float()`,
  `win.close()`, `win.set_title()`, `win.set_loading()`
- `tools.resolve()` for deferred plugin tool results
