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
  We want retained mode with grid diffing (no dirty flags).
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

### Compositor (inside Ui)

Internal to `Ui`. Manages the component tree, orchestrates rendering,
diffs frames. Each frame: resolve layout → draw components → diff
grids → emit SGR. The tui crate never touches the compositor directly
— it calls `ui.render()`, `ui.handle_key()`, `ui.handle_mouse()`,
`ui.win_open_float()`.

**Event routing is z-ordered.** `handle_key` walks focused → parent
→ global keymap. `handle_mouse` hit-tests top-down against layer
rects: the topmost layer whose rect contains the event consumes it.
Clicks, drags, and wheel all go through the same routing — wheel over
a float scrolls the float, not the window beneath.

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
- **Selection** — anchor position, visual mode. Rendered generically
  by the window's own draw path (reverse-video overlay on the grid
  slice), not by per-surface code.
- **Vim state** — mode (normal/visual/visual-line), operator pending
- **Kill ring** — yank history (per-window)
- **Keybindings** — handled via the window, not the buffer
- **Tail follow** — `tail_follow: bool`. When true and the buffer
  grows, scroll advances so the last row stays visible. Any cursor
  motion off the last row clears the flag; motion back (or `G`) sets
  it. Default false; transcript windows set it to true. Generic —
  not transcript-specific.
- **Modifiable** — mirrors `buffer.modifiable`; surfaced on the window
  so the keymap layer can gate insert mode without reaching into the
  buffer.

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
- Solid background fill across the dialog rect (no transcript bleed-through)
- Scrollable content area (buffer view)
- Optional selectable footer (list select)
- Optional inline text input
- Common keys: scroll, dismiss (Esc), confirm (Enter)

**Placement is a config parameter, not hard-coded.** Dialogs pick a
`Placement` variant: `Centered`, `DockBottom { above_status,
full_width }`, `AnchorCursor`, or `AnchorRect`. Built-in dialogs dock
at the bottom above the status bar and span full terminal width;
completer anchors to the prompt cursor; Lua floats default to
centered. Position and sizing live in the config so new dialog types
don't fork layout code.

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
| Transcript tail-follow | `ui::Window::tail_follow` | Generic property; transcript sets true by default |
| Prompt editable text | `ui::Buffer` | Editable buffer, same type as transcript |
| Prompt cursor/scroll/selection/vim | `ui::Window` | Same window behavior as transcript |
| Prompt chrome (notification bar, top/bottom bars, queued rows) | Separate compositor float layers | Each is a window with a buffer, stacked above the prompt input window |
| Buffer modifiability | `ui::Buffer::modifiable` + mirrored on `ui::Window` | Gates insert mode uniformly |
| Selection rendering | `ui::Window` / `WindowView::draw` | Reverse-video overlay painted by window, not per-surface |
| Status bar segments | `StatusBar` component | Set at event time, not recomputed per frame |
| Dialog content | `ui::Buffer` per dialog | Written when dialog opens or content changes |
| Dialog semantic state | App/domain layer (`BuiltinFloat` enum) | Confirm choices, question answers, etc. |
| Dialog rendering/layout | `FloatDialog` component + `Placement` config | Chrome and placement are framework; behavior is app |
| Dialog background | `FloatDialog::draw` | Solid fill across dialog rect |
| Mouse z-order | `Compositor::handle_mouse` | Topmost layer at hit point consumes event |
| Completer | Compositor float layer (`AnchorCursor`) | Same path as any other float |
| Cmdline | Compositor float layer | Anchored at bottom |
| Notifications | Ephemeral compositor float layer | Not part of prompt |
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

// Rendering (called by tui — compositor is internal)
ui.render<W: Write>(w) -> io::Result<()>
ui.render_with(base_components, cursor_override, w)  // transitional
ui.force_redraw()
ui.resize(w, h)

// Event dispatch (compositor routes to focused float)
ui.handle_key(key, mods) -> KeyResult
ui.handle_mouse(event) -> bool
ui.focused_float() -> Option<WinId>
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
App updates buffer content  (buf_set_lines, win_open_float, etc.)
    │
    ▼
render tick
    │
    ▼
Ui (owns compositor internally)
├── Transcript Window  — reads from readonly buffer, draws into grid
├── Prompt Window      — reads from editable buffer, draws into grid
├── StatusBar          — 1-row component (segments set at event time)
└─��� Float windows      — auto-created FloatDialog layers
    │
    ▼
Grid diff → terminal
```

`win_open_float()` both creates the window in the registry AND adds
the visual component to the compositor. One call, one system. Whether
called from Rust or Lua, the path is identical.

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

**Step 6: Unified window system + btw as plugin** — merge the
compositor into `Ui` so that `win_open_float()` is a single call
that creates the buffer, window, AND compositor layer. Neovim model:
one system, one owner. Whether Rust or Lua opens a float, the path
is identical. Then prove it by making `/btw` a pure Lua plugin.

Sub-steps:

6a. **Remove btw from Screen** ✅ — delete `BtwBlock`, all btw
    methods, btw rendering, btw handling. Pure deletion (feature was
    broken — `set_btw` was never called).

6b. **Merge Compositor into Ui** — `Ui` absorbs the `Compositor`.
    `win_open_float()` automatically creates a `FloatDialog`
    component as a compositor layer backed by the window's buffer.
    `win_close()` removes it. `buf_set_all_lines()` syncs the
    float's visual content automatically. Key dispatch goes through
    `ui.handle_key()` → compositor → returns `KeyResult`. Rendering
    goes through `ui.render()` → compositor.

    This eliminates the split between Ui (registry) and Compositor
    (rendering). They become one system, like Neovim's window manager.
    The tui crate passes external base components (transcript view,
    prompt view, status bar) to `ui.render_with()` until those are
    migrated to real windows (Steps 4–5 above made them windows but
    they still render through transitional views).

    Delete: `App.compositor` field (replaced by `App.ui` owning it),
    direct compositor calls from tui code.

6c. **Wire Lua ops to Ui** — Lua PendingOps become `BufCreate`,
    `BufSetLines`, `WinOpenFloat`, `WinClose`, `WinUpdate`.
    `apply_ops` calls `self.ui.buf_create()`, `self.ui.win_open_float()`,
    etc. — same API as Rust code would use. Delete `FloatOp`,
    `drain_float_ops`, `pending_float_ops`, `render::FloatDialog`
    (legacy Dialog-trait float).

6d. **Action dispatch** — `Ui.handle_key()` returns
    `KeyResult::Action("dismiss")` or `KeyResult::Action("select:N")`.
    App maps these to Lua callbacks (or Rust handlers for built-in
    dialogs). Generic — no Lua knowledge in Ui, no caller knowledge
    in Ui.

6e. **btw.lua** — rewrite to use generic `smelt.api.buf/win` API:
    `buf.create()` → `win.open_float(buf, {title, border, hints})`
    → `engine.ask({on_response = set_lines})`. Zero btw-specific
    Rust code. This proves the architecture.

**Step 7: Status bar event-driven** ✅ — `status_data.rs` deleted.
`App::refresh_status_bar()` builds status segments directly from App/Screen
state (no intermediate `StatusInput`/`StatusOutput` structs). `spans_to_segments`
moved to `status.rs`. Screen getters (`last_vim_enabled`, `last_vim_mode`,
`last_status_position`) removed; `refresh_status_bar` computes vim/position
inline and syncs to Screen for the legacy render path.

**Step 8: Hollow out Screen** (deferred) — Screen's fields are all read
by its own legacy render methods (`render_status_line`, `draw_prompt_sections`).
Moving them out adds indirection until those methods are deleted. This step
is folded into Steps 9–10: as each dialog migrates, its legacy render
dependencies are removed, and Screen fields can move to App.

**Step 9: Seam elimination — one render path, one input path**

This step merges the previous Steps 9 and 10. Splitting "migrate
dialogs" from "delete legacy" left the codebase with two render engines
running side-by-side — the compositor for normal frames and six
migrated floats; the legacy `Frame` / `RenderOut` / `Screen::
draw_viewport_dialog_frame` path for the last three dialogs, the
completer, the cmdline, the notification overlay, and the status bar
during dialog mode. Every live bug on this branch (transcript
selection gone, click off-by-one, prompt shifts on newline, completer
invisible, wheel-over-dialog scrolls transcript underneath, dialog bg
transparent, dialogs top-anchored) lives on that seam. Deleting the
seam is a prerequisite for the bug fixes, not a cleanup that follows
them.

This step ends when `App::run` calls exactly one thing per tick:
`self.ui.render()`. No `active_dialog`, no `render_dialog`/
`render_normal` fork, no `Frame`, no `RenderOut`.

**Step 9.1 — Migrate the final three dialogs to `FloatDialog`.**
Order: Confirm (heaviest, ~985 lines, preview buffer + ListSelect +
TextInput), Question (sequential `FloatDialog` per question — kill
the tab/`visited`/`answered`/`multi_toggles` state machine), Agents
(two separate `FloatDialog`s — list, then detail; no mode switch).
Built-in dialog state lives in `BuiltinFloat` enum variants;
`intercept_float_key()` handles any per-dialog keys (e.g. dd-chord,
search) before falling through to `FloatDialog::handle_key`.

**Step 9.2 — Migrate overlays to compositor float layers.**
Completer (anchored to prompt cursor), cmdline (anchored to bottom),
notification (ephemeral float above prompt). Each is a `ui::Window`
with a `ui::Buffer`, opened via `ui.win_open_float()` — same path as
any dialog. Deletes `paint_completer_float`, `draw_prompt_sections`
completer/cmdline branches, `Screen::cmdline` overlay drawing.

**Step 9.3 — Add mouse routing.** `Compositor::handle_mouse(event)`
hit-tests layers top-down; the topmost layer whose rect contains the
point consumes the event (click, drag, wheel). Background windows
only receive the event if no float covers the point. Fixes
"wheel-over-dialog scrolls transcript" and "click in Resume list
doesn't select". `app/events.rs` stops hand-routing mouse events to
`Content`/`Prompt`.

**Step 9.4 — Add `Placement` to `FloatDialog` config.**
```rust
pub enum Placement {
    Centered,
    DockBottom { above_status: bool, full_width: bool },
    AnchorCursor,       // completer
    AnchorRect(Rect),   // explicit position
}
```
Default: `Centered`. Built-in dialogs (Resume, Permissions, Ps, etc.)
use `DockBottom { above_status: true, full_width: true }`. Completer
uses `AnchorCursor`. `FloatDialog::draw` fills its rect with a solid
background style before drawing components (fixes transparent
dialogs).

**Step 9.5 — Delete legacy rendering.** After 9.1–9.4 land, the
following have no callers and get deleted in a single pass:
- `trait Dialog`, `DialogResult`, `ListState`, `TextArea`
- `Frame`, `RenderOut`, `StyleState`, `paint_line`
- `Screen::draw_viewport_frame`, `draw_viewport_dialog_frame`,
  `draw_prompt_sections`, `draw_prompt`, `queue_status_line`,
  `queue_dialog_gap`, `paint_completer_float`
- `active_dialog`, `open_dialog`, `finalize_dialog_close`
- `render_dialog` / `render_normal` split
- All files in `render/dialogs/*.rs`
- `prompt_data.rs` (its layout role moves to a generic stacked-
  layout helper on the prompt window chain — not a prompt-specific
  struct)

**Step 9.6 — Bug fixes on the unified path.** Each collapses to a
small, localized change once the seam is gone:
- **Selection** — `WindowView::draw` reads `window.selection_range()`
  and paints a generic reverse-video overlay into its grid slice.
  Dead `paint_visual_range`/`paint_transcript_cursor` in `screen.rs`
  go away with `Screen`. The `_visual` discard at `events.rs:1171`
  disappears (the range no longer needs to be threaded by hand).
- **Prompt shift on newline** — prompt window's layer rect is
  bottom-anchored; height = `clamp(content_rows, 1..=max)`. Chrome
  (notification, queued, top/bottom bars) stacks as separate layers
  above it. Adding a line grows the prompt upward, doesn't shift it.
- **Click off-by-one** — `Viewport::hit` is the single authoritative
  coord translator. Every other `pad_left` subtraction goes away.
- **Scrollbar center-on-click** — `apply_scrollbar_drag` subtracts
  `thumb_size / 2` on a click outside the current thumb; drags
  inside the thumb preserve their grab offset.

**Step 9.7 — `tail_follow` as a `ui::Window` property.**
```rust
pub struct Window {
    // ...
    pub tail_follow: bool,
    pub modifiable: bool,   // buffer-level, surfaced on window
    // ...
}
```
Transcript defaults to `tail_follow = true`. Any cursor motion that
moves off the last row clears the flag; motion back to the last row
(or explicit `G`) sets it. `TranscriptProjection` consults the flag
when new streaming content arrives: if set, advance scroll so the
last row stays visible; otherwise leave scroll alone. Fresh-session
resume initializes the transcript cursor on the last row, so
`tail_follow` is true until the user scrolls.

**Step 9.8 — Delete `Screen`.** With the legacy render path gone,
Screen's remaining fields (`transcript`, `parser`, `prompt`,
`working`, `notification`, `cmdline`, metadata) move to `App` or to
the buffer projection that owns their display. No more `Screen` type.

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

- Step 5: Prompt window ✅ — `InputState` now wraps a `ui::Window`
  (`input.win`) instead of owning separate buffer/cpos/vim/cursor/
  kill_ring fields. All window state lives on the `ui::Window`;
  InputState is the prompt-specific side-car (completer, menu,
  history, attachments). `Deref<Target = EditBuffer>` still works.

- Step 6a: Remove btw from Screen ✅ — deleted `BtwBlock`,
  all btw methods from Screen, btw rendering from `prompt_data.rs`
  and `draw_prompt_sections`, btw field from `PromptInput`.

- Step 6b: Merge Compositor into Ui ✅ — `Ui` owns the compositor.
  `win_open_float()` creates both window AND FloatDialog layer.
  `win_close()` removes both. tui never touches compositor directly.
- Step 6c: Wire Lua ops to Ui ✅ — PendingOps are `BufCreate`,
  `BufSetLines`, `WinOpenFloat`, `WinClose`, `WinUpdate`. Deleted
  `FloatOp`, `drain_float_ops`, `pending_float_ops`.
- Step 6d: Action dispatch ✅ — compositor float keys route through
  `handle_float_action()`. `dismiss` → Lua callback + close.
  `select:N` → Lua callback. Deleted legacy `render::FloatDialog`,
  `FloatSelect`, `FloatDismiss`.

- Step 6f: Real compositor layers ✅ — transcript, prompt, and status
  bar are now registered as real compositor layers (not borrowed "base"
  components). Deleted `render_with` / `cursor_override` pattern from
  Compositor and Ui. Layer rects set each frame via
  `ui.set_layer_rect()`. Focus synced from `AppFocus` via
  `ui.focus_layer()`. New Ui methods: `add_layer`, `set_layer_rect`,
  `focus_layer`, `layer_mut<T>`, `render` (no base params).

- Step 6g: Generic cursor overlay ✅ — `Component::cursor()` now
  returns `Option<CursorInfo>` instead of `Option<(u16, u16)>`.
  `CursorInfo` carries position + optional `CursorStyle { glyph, style }`
  for block cursors. Compositor paints block cursors into the grid
  before flush; hardware cursors use terminal escape sequences.
  Removed manual cursor painting from TranscriptView and PromptView
  `draw()`. Deleted SoftCursor → CursorInfo conversion in `set_cursor`.

- Step 6g.1: Shared viewport + selection state ✅ — non-vim transcript
  selection now anchors through `ui::Window::win_cursor`, matching the
  prompt's selection path instead of keeping transcript-only anchor
  state. Scrollbar and hit-test geometry moved into
  `ui::WindowViewport` / `ui::ScrollbarState` in `crates/ui/`, and
  `prompt_data.rs` no longer owns scrollbar geometry. Transitional
  `PromptView` / `TranscriptView` now consume generic viewport state
  rather than pane-specific scrollbar fields.

- Step 6g.2: Shared `WindowView` ✅ — transcript and prompt now use the
  same `render::window_view::WindowView` component. Buffer-backed
  transcript rendering and row-backed prompt rendering both go through
  one scrollbar/cursor/viewport implementation, which removes the
  duplicated `PromptView` / `TranscriptView` behavior and leaves only
  one transitional surface to delete in Step 6j.

- Step 6i.1: Prompt input projected into `ui::Buffer` ✅ —
  `compute_prompt()` now splits prompt chrome from the editable input
  region. The input area is projected into a buffer with highlights,
  while bars / notifications / queued rows stay as chrome rows.
  App wiring now renders prompt chrome and prompt input as separate
  `WindowView` layers so the input path is buffer-backed like the
  transcript.

- Step 6i.2: Prompt layout ownership clarified ✅ — prompt chrome keeps
  owning the full prompt rect, while the buffer-backed input is an
  overlay sub-viewport inside it. That preserves the existing prompt
  layout contract and keeps mouse hit-testing / scrollbar geometry tied
  to the generic window viewport instead of inventing a second prompt
  layout model.

**Step 6h: Eliminate nav text** ✅ — Switched all Window coordinates
to display-text space. Replaced `full_transcript_nav_text()` with
`full_transcript_display_text()` everywhere (vim motions, selection,
copy, click, scroll). Added `snap_transcript_cursor()` helper that
calls `snap_cpos_to_selectable()` after every motion. Copy operations
use `copy_display_range()` which delegates to `copy_byte_range()`.
Removed `nav_col_to_display_col` from screen.rs. Nav-text functions
still exist in transcript.rs but have no callers from events.rs —
will be deleted once prompt migration (6i) is complete.

**Step 6i: Prompt rendering through Buffer** — Replace the
PromptRow/StyledSegment pipeline with Buffer + BufferView rendering.
`compute_prompt()` syncs input text to a `ui::Buffer` with highlights
and decorations each frame (same projection pattern as transcript).
Chrome (notification bar, top/bottom bars, queued messages) drawn
at the app level around the BufferView. Delete PromptRow,
StyledSegment, most of prompt_data.rs, PromptView.

**Step 6j: Unified WindowView** — Both transcript and prompt
surfaces render through BufferView + optional scrollbar. Delete
TranscriptView. One component type for all buffer-backed surfaces.

### Seam check (2026-04-21)

Re-audit after Step 6h/6i/6j landed. The compositor path handles the
"normal" frame and six migrated floats. A parallel legacy path
(`Frame`, `RenderOut`, `StyleState`, `Screen::draw_viewport_dialog_frame`,
`draw_prompt_sections`, `paint_completer_float`, `queue_status_line`)
is still live for:
- Three unmigrated dialogs (Confirm, Question, Agents) via
  `trait Dialog` + `active_dialog`.
- Completer popup (only drawn from `draw_prompt_sections`, which is
  only reached from the legacy prompt path).
- Cmdline, notification overlays.
- Status bar queueing during dialog-active frames (dual-write with
  the new `StatusBar` component).

Both engines drift: transcript selection, completer visibility,
click coord offset, prompt shift on newline, wheel-over-dialog
scrolling the transcript, transparent dialog bg, top-anchored
dialogs — all regressions land on this seam. Step 9 below is the
dedicated seam-elimination phase.

Next: Step 9 (seam elimination).

## Phase 7: Event dispatch

**Goal:** Input routing through component tree is framework-level.

- Compositor manages focus stack (z-index ordered for floats)
- `handle_key()` walks: focused → parent → global keymap
- `handle_mouse()` hit-tests layout → route to target
- Keymap system: buffer-local, window-local, global scopes
- Vim integration: vim state on windows, framework-level handling

Note: basic compositor key dispatch for floats lands in Step 6c.
`handle_mouse` with z-order hit-testing lands in Step 9.3 (required
to fix "wheel-over-dialog scrolls transcript"). Phase 7 generalizes
the remaining event plumbing (keymap scopes, mouse event types
beyond click/drag/wheel, vim operator-pending state machine).

## Phase 8: Lua bindings — remaining operations

**Goal:** Complete the `smelt.api.buf/win` surface beyond floats.

Step 6b lands the core bridge (buf.create, win.open_float,
buf.set_lines). This phase adds the remaining operations:
- `buf.set_highlights`, `buf.add_virtual_text`, `buf.set_mark`
- `win.set_cursor`, `win.get_cursor`, `win.set_scroll`
- Port `predict.lua`, `plan_mode.lua` to clean API
- Any remaining Lua plugins that bypass the `Ui` registry

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
Phase 6 (buffer/window model + Lua float bridge + btw plugin)
    │
    ▼
Step 9 (seam elimination — one render path, one input path,
        final dialog migrations, legacy deletion, bug fixes
        on the unified path)
    │
    ▼
Phase 7 (event dispatch — generalize keymap + vim operator
         state beyond what Step 9.3 lands)
    │
    ▼
Phase 8 (Lua bindings — remaining buf/win operations)
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
- **Immediate mode.** We are retained mode with grid diffing.

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
