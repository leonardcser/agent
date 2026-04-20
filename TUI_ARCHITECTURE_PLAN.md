# TUI Architecture — UI Framework Rewrite

## Implementation instructions

These directives govern how this plan is executed. They override defaults.

### Process

- **Stop at friction.** When something is unclear, when abstractions don't
  fit, when you're unsure which direction to take — stop and talk to the
  user. Present options, explain trade-offs, ask for a decision. Don't
  push through ambiguity. The cost of pausing is low; the cost of building
  the wrong abstraction is high.

- **The plan evolves.** This document is a living roadmap, not a contract.
  As implementation proceeds, new insights will surface — things we didn't
  anticipate when writing the plan. That's expected and good. Don't force
  the code to match the plan when the plan is wrong. Update the plan to
  match reality, then keep going.

- **Correct abstractions matter most.** The goal is not "get it done" but
  "get the abstractions right." Take inspiration from Neovim's architecture
  (buffers, windows, compositor, event dispatch) but adapt to Rust's
  ownership model. When in doubt, study how Neovim solves the problem and
  translate the concept, not the implementation.

- **No dead code annotations.** Never add `#[allow(dead_code)]`. Either
  use the code, remove it, or leave the compiler warning visible as a
  tracking marker for future work. Pre-existing `#[allow(dead_code)]` from
  earlier phases should be removed too.

- **Format, lint, test, commit as you go.** After each coherent change:
  `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.
  Then update this plan (mark progress, record decisions), then commit.
  Don't batch — small, clean commits that each pass CI.

- **No throwaway work.** Don't build intermediate abstractions that will
  be discarded in a later phase. If the final architecture needs X, build
  toward X directly, even if incrementally. Every step should be a subset
  of the final state, not a detour.

- **Present multiple approaches.** When solving a problem, present options
  with pros/cons. Include the bold option (what would a clean rewrite look
  like?). Let the user choose the direction.

---

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

### Two primitives: Buffer and Window

The entire UI model rests on two concepts, same as Neovim:

**Buffer** = content + metadata. Lines, highlights, decorations, marks,
virtual text, modifiable flag. Buffers know nothing about display —
they're just data. A buffer can be editable (prompt) or read-only
(transcript). Both use the same type.

**Window** = viewport into a buffer. Cursor, scroll, selection, vim
state, keybindings, mouse handling. Everything about how you interact
with content lives on the window — not the buffer, not the app, not
a separate navigation layer.

The transcript window and the prompt window get the same vim motions,
selection, yank, mouse handling, scroll — because that's all window
behavior. The only difference is the buffer's `modifiable` flag (which
gates insert mode and text mutations).

No separate "transcript navigation state" or "prompt surface state."
Just windows looking at buffers.

### Everything is a window (except the status bar)

Every interactive surface in the UI is a **window** backed by a **buffer**:

- **Transcript** — split window, readonly buffer, block content projected in
- **Prompt** — split window, editable buffer with vim motions
- **All dialogs** — float windows (help, confirm, resume, rewind, etc.)
- **BtwBlock** — float window (plugin-owned, not part of prompt)
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
a separate implementation. The visual chrome and layout are unified. Dialog-
specific behavior (confirm previews, question flow, agent detail) stays in
the app/domain layer — `FloatDialog` does not absorb all dialog semantics.

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
- `cursor()` — cursor position if focused

### Compositor

Manages the component tree, orchestrates rendering, diffs frames.
Each frame: resolve layout → draw components → diff grids → emit SGR.

### Buffer

Lines + highlights + marks + virtual text + per-line decoration +
modifiable flag. Buffers are the content model — windows read from
them during `draw()`. Buffers are updated at event time (keystrokes,
engine events, streaming), not at render time.

Per-line decoration (`LineDecoration`) supports gutter backgrounds, fill
backgrounds, and soft-wrap markers. This is optional metadata — most
buffers don't use it, but the transcript and diff previews do. Highlight
spans carry optional `SpanMeta` for selection/copy behavior.

### Window

Viewport into a buffer. Owns all interaction state:
- **Cursor** — position, curswant (for vertical motion memory)
- **Scroll** — top_row, pinned flag
- **Selection** — anchor position, visual mode
- **Vim state** — mode (normal/visual/visual-line), operator pending
- **Kill ring** — yank history (per-window)
- **Keybindings** — handled via the window, not the buffer

Windows are components. During `draw()`, a window reads its buffer's
content and renders into its grid slice. The app never pushes display
data into windows — windows pull from their buffers.

Both transcript and prompt are windows. The transcript window has a
read-only buffer (`modifiable: false`): same vim motions, visual
selection, yank, scroll, mouse — just no insert mode. The prompt
window has an editable buffer.

### Naming conventions

Components implement `draw()` — the compositor calls `draw()` on each.
No per-component-type render methods (`render_dialog`, `render_prompt`).
App has one `render()` entry point that calls `compositor.render()`.
Temporary internal helpers during migration are prefixed with the
surface they handle but will be deleted once all surfaces are components.

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

## Canonical ownership

Every piece of state has exactly one owner. No duplication.

| Concern | Owner | Notes |
|---|---|---|
| Transcript content | `ui::Buffer` | Projected from blocks at event time |
| Transcript cursor/scroll/selection/vim | `ui::Window` | All interaction state on window |
| Prompt editable text | `ui::Buffer` | Editable buffer, same type as transcript |
| Prompt cursor/scroll/selection/vim | `ui::Window` | Same window behavior as transcript |
| Prompt chrome (notification bar, top/bottom bars) | App layout | Not buffer content — layout around the window |
| Status bar segments | `StatusBar` component | Set at event time, not recomputed per frame |
| Dialog content | `ui::Buffer` per dialog | Written when dialog opens or content changes |
| Dialog semantic state | App/domain layer | Confirm choices, question answers, etc. |
| Dialog rendering/layout | `FloatDialog` component | Chrome is framework, behavior is app |
| Btw | Float/dialog | Plugin-owned surface, not part of prompt |
| Notifications | Ephemeral float or app state | Not part of prompt |
| Block history + layout cache | `tui::BlockHistory` | Projects into transcript buffer |

### What `Screen` currently owns vs. where it moves

`Screen` is the main piece of legacy architecture to hollow out. Its
current responsibilities and their final owners:

| Screen field | Final owner |
|---|---|
| `transcript` (BlockHistory) | Stays in tui, projects into `ui::Buffer` |
| `parser` (StreamParser) | Stays in tui app layer |
| `prompt` (PromptState) | `ui::Buffer` (editable) + app chrome state |
| `working` state | App layer |
| `notification` | Ephemeral float or app state |
| `btw` | Float/dialog (plugin-owned) |
| `last_viewport_text` | Read directly from buffer lines |
| `last_viewport_lines` | Read from buffer highlights/meta |
| `transcript_gutters` | Window config |
| `layout` | Compositor / layout tree |
| `cmdline` | StatusBar or dedicated component |
| Dialog flags | Compositor layer management |
| Status metadata (tokens, cost, model) | App state, pushed to StatusBar at event time |

Screen dies when all its fields have moved to their final owners.

### Deletion criteria for transitional modules

| Module | Dies when |
|---|---|
| `prompt_data.rs` | Prompt chrome set at event time, not computed per frame |
| `status_data.rs` | Status segments set at event time |
| `TranscriptView` | Transcript window reads buffer directly in draw() |
| `PromptView` | Prompt window is a real `ui::Window` |
| Old `Dialog` trait | All dialogs migrated to `FloatDialog` |
| `render/dialogs/*` | All dialog structs replaced |
| `render_normal` / `render_dialog` split | Dialogs are compositor layers |
| `Screen` | All state moved to buffers/windows/app |
| `tui::window::TranscriptWindow` | Merged into `ui::Window` |
| `tui::buffer::Buffer` (nav buffer) | Transcript buffer IS the nav buffer |

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
| `Screen`                      | Dies — state moves to buffers/windows/app     |
| `RenderOut` / `Frame`         | `Grid` + diff engine in `Compositor`          |
| `Dialog` trait (9 impls)      | `FloatDialog` component (one impl, N configs) |
| `FloatDialog` (Lua)           | `FloatDialog` component                       |
| `ConfirmDialog` (985 lines)   | `FloatDialog` with preview buffer + ListSelect|
| `HelpDialog`                  | `FloatDialog` with keybindings buffer         |
| `QuestionDialog` (tabs)       | Sequential `FloatDialog` per question         |
| `AgentsDialog` (2-mode)       | List `FloatDialog` + Detail `FloatDialog`     |
| `BtwBlock` (custom overlay)   | Float/dialog (plugin-owned)                   |
| `Notification`                | Ephemeral float window                        |
| `Completer` (custom popup)    | Float window anchored to cursor               |
| `InputState`                  | `ui::Window` (editable buffer)                |
| `TranscriptWindow`            | `ui::Window` (readonly buffer)                |
| `tui::buffer::Buffer`         | Merged into `ui::Buffer`                      |
| `CmdlineState` / status line  | `StatusBar` component                         |
| `LayoutState`                 | `Layout` tree + compositor                    |
| `StyleState`                  | `Style` on cells + diff engine                |
| `DisplayBlock` / paint        | Buffer content + highlights → grid cells      |
| `ListState` (shared helper)   | `ListSelect` component                        |
| `TextArea` (shared helper)    | `TextInput` component                         |
| `BlockHistory`                | Managed by tui, projected into transcript buf |
| `ConfirmPreview` (5 variants) | Diff/code rendered into buffer with highlights|
| `Vim` (tui)                   | Lives on `ui::Window`                         |

## What stays in `tui`

- `App` struct, event loop, agent management
- Engine communication (`EngineHandle`, `UiCommand`, `EngineEvent`)
- `BlockHistory` + `StreamParser` + block rendering pipeline
- `TranscriptProjection` (blocks → buffer, generation-gated)
- Session persistence
- Lua runtime + API bindings (calls through `ui::*`)
- Permission system
- Commands (slash commands are app-level, not framework-level)
- Terminal setup/teardown (raw mode, alternate screen, etc.)
- Dialog-specific behavior (what content to show, what actions mean)
- Prompt chrome layout (notification bar, top/bottom bars around window)

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

## Phase 6: Buffer/window rendering model (IN PROGRESS)

**Goal:** Windows pull from buffers. App updates buffers at event time.
The render loop is just `compositor.render()`. Replace `RenderOut` +
`Frame` + the push-based data extraction pipeline with the Neovim-style
buffer/window model.

### Architecture

```
event (key, engine, timer)
    │
    ▼
App updates buffer content
    │
    ▼
render tick
    │
    ▼
Compositor
├── Transcript Window  — reads from readonly buffer, draws into grid
├── Prompt Window      — reads from editable buffer, draws into grid
├── StatusBar          — 1-row component (segments set at event time)
└── FloatDialog(s)     — float layers reading from their buffers
    │
    ▼
Grid diff → terminal
```

### Data flow (pull model)

Events update buffers. Windows read from buffers during draw.
The app's render function is minimal:

```rust
fn render(&mut self) {
    self.compositor.render(&mut stdout);
}
```

No data extraction step. No pushing snapshots into views. Buffers hold
the truth; windows render from it. This is the model that makes Lua
plugins natural — `buf_set_lines()` updates a buffer, the window
redraws automatically on the next frame.

### Transition from current state

**Step 1: Clean up current state** ✅ — fix dead code, remove
`#[allow(dead_code)]`, get everything compiling clean. Rename
`tick_*` methods to `render_*`.

**Step 2: Enrich `ui::Buffer` with line decoration** ✅ — add
`LineDecoration` (gutter_bg, fill_bg, fill_right_margin, soft_wrapped)
and `SpanMeta` (selectable, copy_as) to the buffer model. Update
`BufferView` to render decorations.

**Step 3: Transcript buffer** ✅ — `TranscriptProjection` projects
blocks into a `ui::Buffer` (generation-gated). `TranscriptView`
reads from the buffer via `BufferView.sync_from_buffer()`.
Deleted: `collect_viewport`, `collect_transcript_data`, `paint_grid.rs`.

**Step 4: Transcript window** — make the transcript a real `ui::Window`.
Merge `tui::TranscriptWindow` state (cursor, scroll, selection, vim,
kill_ring) into `ui::Window`. The window reads from the projected
`ui::Buffer` during `draw()`. Delete `TranscriptView` (the window IS
the view). Delete `tui::buffer::Buffer` (the `ui::Buffer` IS the
nav buffer — vim motions operate on it directly). Delete
`last_viewport_text`, `last_viewport_lines` from Screen (read from
buffer instead).

**Step 5: Prompt window** — make the prompt a real `ui::Window` with
an editable buffer. `InputState`'s edit buffer becomes a `ui::Buffer`.
The prompt window handles key input, vim motions, cursor rendering.
Prompt chrome (notification bar, top/bottom bars) is app-level layout
around the window — not buffer content.

**Step 6: Btw as float** — move btw out of prompt composition into a
float/dialog. It becomes a `FloatDialog` opened by the app (and later
by a Lua plugin). Remove btw from Screen and prompt_data.

**Step 7: Status bar event-driven** — status bar segments are updated
when the underlying data changes (mode switch, new tokens, cost update),
not recomputed every frame. Delete `status_data.rs`.

**Step 8: Hollow out Screen** — as each piece moves to its final owner,
Screen shrinks. Data that was in Screen moves to buffers, windows, or
app state. Delete Screen when empty.

**Step 9: Migrate dialogs** — each of the 10 Dialog implementations
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

Also migrate: Notifications, Completions → float layers.

**Step 10: Delete legacy rendering** —
- `Dialog` trait, `DialogResult`, `ListState`, `TextArea`
- `Frame`, `RenderOut`, `StyleState` (SGR/style stack machinery)
- `paint_line` (legacy SGR paint path)
- `Screen::draw_viewport_frame`, `draw_viewport_dialog_frame`
- `active_dialog`, `open_dialog`, `finalize_dialog_close`
- All individual dialog structs in `render/dialogs/`
- `prompt_data.rs` (transitional compute module)
- `render_dialog`, `render_normal` (transitional dispatch methods)

### Current progress

Steps 1–4 complete:
- Step 1: Dead code cleanup, `tick_*` → `render_*` rename ✅
- Step 2: `ui::Buffer` enriched with `LineDecoration` and `SpanMeta` ✅
- Step 3: Transcript buffer ✅ — `TranscriptProjection` projects
  blocks into a `ui::Buffer` (generation-gated). `TranscriptView`
  reads from the buffer via `BufferView.sync_from_buffer()`.
  Deleted: `collect_viewport`, `collect_transcript_data`,
  `paint_grid.rs`.
- `BufId` and `Buffer::new` made public for cross-crate use ✅
- Step 4: Transcript window ✅ — merged `tui::TranscriptWindow` state
  and behavior into `ui::Window`. `ui::Window` now holds all
  interaction state (vim, kill_ring, win_cursor, selection, pin,
  scroll, cursor position). Deleted: `tui::window::Window` trait,
  `tui::window::TranscriptWindow`, `api::win` module,
  `impl Window for InputState`, `ui::cursor` module (absorbed).
  `WinId` constructor made public.

Next: Step 5 — prompt window (make the prompt a real `ui::Window`
with an editable buffer).

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
- Btw becomes a pure plugin (not engine-tied)

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
Phase 6 (buffer/window model: transcript + prompt + status + dialogs)
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
