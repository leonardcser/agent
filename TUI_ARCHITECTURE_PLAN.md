# TUI Architecture Refactor Plan

## Goal

Reshape the TUI into a familiar editor-style architecture modeled on neovim: **buffers** (content + cursor + selection + buffer-local keymaps) sit inside **windows** (viewports with window-local keymaps and a layout rect), and every interaction — internal or external — goes through a **public API** (`smelt::api::{buf, win, ui, cmd, keymap}`) that is stable enough to expose to plugins later. The current code fights three problems at once: the content pane navigates a rendered projection instead of a model, coordinate systems / layout / freeze logic each live in multiple places, and there is no shared vocabulary for "do this thing to that buffer/window." The nvim-style model dissolves all three.

## Why nvim terminology

- **Buffer = content** (text + cursor + selection + optional vim + buffer-local keymap). Can be readonly or editable. Lives independently of whether it is on screen.
- **Window = viewport** onto one buffer (rect + scroll + window-local keymap). Multiple windows can show different buffers. Floating windows are just windows with a non-docked rect and a z-order.
- **Dialog and completer are floating windows** over the same primitive. One less abstraction to maintain.
- **Keymaps layer** — buffer → window → global — so a transcript buffer can have vim motions without the prompt buffer inheriting them.
- **Public API** — the same surface that internal code uses is the surface plugins will use. If the API can express everything the app does, plugins can extend anything the app can do.

## Guiding decisions

- Rename to nvim vocabulary: `TextBuffer` → `Buffer`, `Pane` → `Window`, `ContentPane` → `TranscriptWindow`, prompt-pane wrapper → `PromptWindow`. The mental model already matches; naming should too.
- Every state mutation goes through `smelt::api`. No direct `self.input.buf = …`, no direct `self.screen.active_text = …`. The API is the only door.
- `BufId` / `WinId` are stable opaque handles (not references) so plugins survive mutations.
- Keep `BlockHistory`, `Vim`, and content-addressed block layout caching. They are the good parts.
- **Cursor lives on the window, not the buffer.** Like nvim: `Window.cursor` is the canonical cursor position (byte offset into its buffer's text). Display position (which screen row/col it lands on) is *derived* on each render from cursor + scroll + snapshot — never stored. Same buffer shown in two windows = two independent cursors. Buffers are pure content; they don't own cursor, selection, or vim state.
- `TranscriptSnapshot` is the canonical derived view for the transcript window — cursor motion, selection, yank, click-hit-testing, scrollbar all read from it.
- **Top-relative coordinates everywhere.** Scroll = rows from the top of the transcript; cursor = absolute (row, col) in the snapshot. Bottom-relative math deletes itself.
- **Viewport pin, not freeze.** When a selection / drag is active we *pin the top row* of the viewport to a fixed transcript row. New agent output flows into scrollback below the pin; the visible rows do not shift. The renderer always repaints — pinning is achieved by adjusting `scroll_offset` against the growing transcript, not by skipping the paint. When the pin releases, scroll returns to its previous behavior (stuck-to-bottom if that's where it was).
- **Window-level gutters.** Left/right padding, scrollbar column, and any future number / sign / fold column are **window properties** — rendered *around* the content rect, never inside it. The window's `content_rect` is `window_rect - gutters`. Cursor columns are expressed in content-rect coordinates, so the cursor cannot enter a gutter (same as nvim's `numbercol`). Clicks in a gutter don't position a cursor; they either route to the gutter's widget (scrollbar) or snap into the content.
- **Span-level `SpanMeta` is for decorations *inside* blocks** — diff `+/-` markers, quote bars, tool-call indent when present inside a text block. Cells carry `selectable: bool` + `copy_as: Option<String>`. Soft-wrap reflow for unwrap-on-copy uses a per-row `logical_line: u32` marker on the snapshot so consecutive visual rows of one logical source line don't get `\n` inserted between them on copy.
- The `Window` trait describes only the shared surface (buffer, viewport, keymap, rect). `PromptWindow` and `TranscriptWindow` stay structurally different where they need to be — no forced symmetry.
- Don't flatten `BlockHistory` into plain text. Snapshot *projects* it.
- One render path for normal mode. Scrollback `\r\n` stays only for dialogs-as-overlays fallback and headless.

## Target shape

```rust
struct State {                             // pure app state; API operates here
    buffers:   SlotMap<BufId, Buffer>,
    windows:   SlotMap<WinId, Window>,
    transcript: Transcript,                // owns the transcript buffer id
    current_window: WinId,
    layout:    LayoutState,                // recomputed per frame
    keymap:    GlobalKeymap,
    commands:  CommandRegistry,
    ui:        UiState,                    // mode, notifications, theme, toggles
}

struct Buffer {
    kind:     BufferKind,                  // Prompt | Transcript | Scratch | Completer | Dialog
    text:     String,
    undo:     UndoHistory,
    attachments: Vec<AttachmentId>,
    keymap:   Keymap,                      // buffer-local
    readonly: bool,                        // checked by windows before applying edits
    // No cursor, no selection, no vim state — those live on the window.
}

struct Window {
    buffer:   BufId,
    rect:     WindowRect,                  // Dock(Region) | Float { rect, z, anchor }
    gutters:  WindowGutters,               // padding + scrollbar + future numbercol/signcol
    scroll_top_row: u16,                   // top-relative; content-rect rows
    cursor:   usize,                       // canonical cursor: byte offset into buffer.text
    selection_anchor: Option<usize>,       // per-window non-vim selection anchor
    vim:      Option<Vim>,                 // per-window vim state (mode, visual_anchor, curswant)
    kill_ring: KillRing,                   // per-window for now; could consolidate later
    keymap:   Keymap,                      // window-local
    role:     WindowRole,                  // Prompt | Transcript | Completer | Dialog | StatusLine
}

// Display position (which screen row/col the cursor paints at) is
// never stored — it is derived each frame as
// `snapshot.cell_for_buffer_pos(window.cursor) - window.scroll_top_row`.
// This is the nvim model: cursor is a window property in buffer-logical
// coordinates; display coords are a projection.

struct WindowGutters {
    pad_left:       u16,                   // blank columns on the left of content
    pad_right:      u16,                   // blank columns on the right (scrollbar fits here)
    scrollbar:      Option<Side>,          // Left | Right | None; occupies one column of its side's padding
    // Future: numbercol_width, signcol_width, foldcol_width
}

// `content_rect(window) = window.rect - gutters`. Cursor, selection,
// click-hit-testing, and buffer rendering all live in content-rect coords.
// Gutter painting is the renderer's job, not the buffer's.

struct Transcript {
    history: BlockHistory,
    active:  ActiveStreams,
    buffer:  BufId,                        // the transcript buffer
}

struct TranscriptSnapshot {                // width-keyed; cached
    width:  u16,
    rows:   Vec<RenderedRow>,
    flat:   String,
    row_of: Vec<usize>,                    // flat byte offset → row idx
    offset_of: Vec<usize>,                 // row idx → flat byte offset
}

struct LayoutState {                       // compositor output; input reads this
    windows: Vec<(WinId, Rect)>,           // z-ordered; floats on top
    transcript_rect: Rect,
    prompt_rect:     Rect,
    prompt_input:    Rect,
    status_rect:     Rect,
}
```

## Public API (`smelt::api`)

The internal call pattern throughout the codebase becomes API calls. If plugins appear later, they use the same functions.

```rust
// smelt::api::buf
get_text(&State, BufId) -> &str
set_text(&mut State, BufId, String)
insert(&mut State, BufId, pos: usize, text: &str)
delete(&mut State, BufId, Range<usize>)
cursor(&State, BufId) -> usize
set_cursor(&mut State, BufId, pos: usize)
selection(&State, BufId) -> Option<Range<usize>>
yank(&mut State, BufId, Range<usize>)
set_keymap(&mut State, BufId, chord: &str, Action)

// smelt::api::win
list(&State) -> Vec<WinId>
current(&State) -> WinId
set_current(&mut State, WinId)
buffer(&State, WinId) -> BufId
rect(&State, WinId) -> Rect
scroll(&mut State, WinId, lines: i32)
set_cursor(&mut State, WinId, (row, col))
set_keymap(&mut State, WinId, chord: &str, Action)

// smelt::api::ui
open_floating(&mut State, FloatingSpec) -> WinId     // dialog, overlay, hover
open_completer(&mut State, CompleterSpec, Anchor) -> WinId
close_window(&mut State, WinId)
notify(&mut State, String)
set_mode(&mut State, Mode)

// smelt::api::cmd
register(&mut State, name: &str, Box<dyn Fn(&mut State, &[&str])>)
run(&mut State, line: &str)              // parses ":quit", ":compact", ":model ..."

// smelt::api::keymap
set_global(&mut State, chord: &str, Action)
```

Lookup order when a key fires: **buffer-local → window-local → global**. Matches nvim.

`Action` is a small enum — `Cmd(String)`, `Motion(Motion)`, `Callback(Box<dyn Fn(&mut State)>)`. Commands go through `api::cmd::run`, so `":quit"` from a plugin and the user typing `:quit` land in the same handler.

---

# Phased plan

Each stage is shippable. Each stage lists the code it **deletes**, not just what it adds.

## Stage 1 — `Buffer` + `Window` split, cursor on window [~5h]

**Replaces**: the `Buffer` type that mixed text, cursor, selection, vim state, and a `readonly` flag; the `Pane` naming that diverges from neovim convention; `Buffer.cpos` as the canonical cursor location.

- Rename `TextBuffer` → `Buffer`. Drop the `readonly` constructor pair in favor of a single `Buffer::new(BufferKind)` + an explicit `readonly: bool` property the *owning window* checks.
- Rename `Pane` → `Window`. `ContentPane` → `TranscriptWindow`. Prompt wrapper becomes `PromptWindow` (still richer than the trait — that's fine).
- **Move cursor, selection anchor, and vim state from `Buffer` to `Window`.** The cursor is a byte offset into `buffer.text`, owned by the window that's displaying the buffer. `VimContext` is built at call-sites from window + buffer borrows together.
- Viewport state (`scroll_top_row`) also lives on the window. Display row/col is computed on render, not stored.
- Buffers gain a `keymap: Keymap` field; windows gain one too. Both default-empty.
- `Buffer` now holds: `text`, `undo`, `attachments`, `keymap`, `readonly`, `kind`. Nothing else.

**Deletes**: `Buffer::cpos`, `Buffer::vim`, `Buffer::selection_anchor` as canonical state; `Buffer::writable()` / `Buffer::readonly()`; the readonly-snap-back branch inside `handle_vim_key` (window does this before dispatching); viewport fields on the buffer; stored `cursor_row`/`cursor_col` display coords.

**Done when**: `buffer.cursor` / `buffer.vim` compile errors everywhere; every cursor move goes through a window; display cursor position is a pure function of `(window.cursor, window.scroll_top_row, snapshot)`.

---

## Stage 2 — `Window` trait + `smelt::api::{buf, win}` [high payoff, ~6h]

**Replaces**: `Mutation` enum as the interface; direct `self.input.buf = …` writes; `Deref<Target = TextBuffer>` hiding mutation paths; per-pane ad-hoc method surfaces that only partially overlap.

- Introduce the `Window` trait: `buffer_id`, `rect`, `cursor`, `set_cursor`, `scroll`, `selection`, `keymap`, `keymap_mut`.
- Build `smelt::api::buf::*` and `smelt::api::win::*` as the single mutation surface. They enforce invariants: undo snapshots on text change, completer recompute for prompt buffers, selection clearing, attachment cleanup.
- Every direct field write in `app/events.rs` and `app/agent.rs` routes through the API. The four known bug sites (ghost-text accept, EscAction::Unqueue, agent cancel-unqueue, editor rewind) use `api::buf::set_text`.
- Remove `DerefMut<Target = TextBuffer>` from `InputState`. Keep `Deref` (read-only) for ergonomics during migration; drop it entirely by Stage 10.

**Deletes**: `Mutation` enum (superseded by typed API functions); `InputState::DerefMut`; four ad-hoc `buf =`/`cpos =` blocks; duplicated "combine queued + buf" logic across agent.rs and events.rs.

**Done when**: no app code writes buffer/cursor/selection fields directly; every mutation goes through `smelt::api`.

---

## Stage 3 — Command bus + `smelt::api::cmd` + global keymap [~3h]

**Replaces**: the ad-hoc `KeyAction` enum as the central dispatch; scattered command parsing (`/export`, `/compact`, `/quit`) across `commands.rs`; modal-only key routing that makes it hard to register new user actions.

- `CommandRegistry` + `smelt::api::cmd::{register, run}`. Commands are name → handler, invoked by `:quit`, `/quit` (legacy shim), or programmatically.
- Migrate existing `/commands` into registered handlers. Keep `/` as a UI affordance (prompt parsing); internally they go through `cmd::run`.
- `smelt::api::keymap::set_global(state, chord, Action::Cmd("quit"))` becomes how keys bind.
- Add lookup order: on a key, consult current buffer's keymap, then current window's keymap, then global.

**Deletes**: hand-rolled command matching in `commands.rs` (each branch becomes a registered handler); the split between `KeyAction` and command names (unified through `Action::Cmd`).

**Done when**: user-visible commands and internal handlers share one registry; keymaps can be bound at any of three layers; `:quit` and `/quit` and Ctrl-D all route through the same code.

---

## Stage 4 — Extract `Transcript` from `Screen` [~4h]

**Replaces**: `Screen` being both renderer and transcript model. `active_text`, `active_thinking`, `active_tools`, `active_agents`, `active_exec` move off `Screen`.

- `Transcript` owns `BlockHistory` + `ActiveStreams` + the transcript `BufId`.
- Public surface: `append_text`, `append_thinking`, `start_tool`, `finish_tool`, `append_exec_output`, `finish_exec`, `flush`, `snapshot(width)`.
- Transcript is stored on `State`. Callers (engine event handlers) mutate it via `smelt::api` wrappers or directly through transcript methods during this transition.
- `Screen` becomes a consumer — it reads `Transcript` + snapshot + `LayoutState` and paints.

**Deletes**: direct `self.screen.active_*` accesses; `Screen`'s role as domain owner; coupling that made `Screen` untestable without a terminal.

**Done when**: `Transcript` is unit-testable without `Screen`; `Screen`'s remaining state is layout/render-only.

---

## Stage 5 — `LayoutState` + `WindowRect::{Dock, Float}` primitive [~4h]

**Replaces**: implicit hit-testing via `Screen::input_region`, `prev_prompt_rows`, `last_scroll_offset`; the `viewport_rows_estimate` math; the fact that dialogs and completer each have bespoke layout code.

- Windows carry a `WindowRect`: `Dock(Region)` (transcript, prompt, status) or `Float { rect, z_order, anchor }` (completer, dialog, future overlays).
- The compositor produces a `LayoutState` per frame from window rects + dock priorities. Floating windows are laid out relative to an anchor (a rect or another window).
- `smelt::api::ui::open_floating(spec)` returns a `WinId` for a new floating window. `close_window(id)` removes it.
- Mouse handlers read `LayoutState` (z-ordered hit-test walks floats first, then docks).

**Deletes**: `Screen::input_region()` as a method; `Screen::prev_prompt_rows`; `Screen::last_scroll_offset`; `viewport_rows_estimate()`; bespoke dialog rect math.

**Done when**: every hit-test reads `LayoutState`; floats and docks are the same primitive with different rect kinds.

---

## Stage 6 — Completer and dialogs become floating windows [~4h]

**Replaces**: the `Completer` as a side-car on `InputState` with its own render; the `Dialog` trait as a separate surface. Both become floating windows with a buffer and a keymap.

- `Completer` keeps its fuzzy-match engine (the algorithm stays) but renders into a floating window. Two mount sites:
  - Above the prompt: `anchor = prompt_top`, direction `up` — for paths, aliases, `@file` refs.
  - Over the status bar: `anchor = status_top`, direction `up` — for `:command` and `/command` completion.
- Completer floats **do not shift content**: they overlay the existing layout without changing dock rects.
- Dialogs (`ConfirmDialog`, `QuestionDialog`, etc.) become floating windows with `modal = true`; the dispatcher routes keys to the top modal first.
- `smelt::api::ui::open_completer(spec, anchor)` and `api::ui::open_floating(spec)` are the entry points.

**Deletes**: `Dialog` trait as a distinct concept (becomes a floating-window role); per-completer render code in `render/completions.rs` (the layout happens via floating-window path; the widget stays); the special-case "dialog intercepts events" branch in `dispatch_terminal_event` (replaced by generic modal-float routing).

**Done when**: completer and dialogs share one primitive; opening a new overlay is an `api::ui::open_floating` call.

---

## Stage 7 — `TranscriptSnapshot` + top-relative coordinates + `SpanMeta` [biggest correctness win, ~8h]

**Replaces**: three ways of flattening the transcript (`full_transcript_text`, `viewport_text`, `last_viewport_text`, `rows.join("\n")` inside `TranscriptWindow::mount`); the bottom-relative `scroll_offset` / `cursor_line_from_bottom` / inverted-scrollbar math; the `view_top = total - viewport - scroll` conversion in three places; viewport-relative remapping of visual selection in `events.rs`.

The snapshot carries per-cell metadata from day one so Stage 8 (below) doesn't have to rebuild it:

```rust
struct SpanMeta {
    selectable: bool,             // cursor/selection can enter this cell
    copy_as: Option<String>,      // None = emit char; Some("") = emit nothing;
                                  // Some(s) = substitute on copy
}

struct DisplayCell {
    ch: char,
    style: Style,
    meta: SpanMeta,
}

struct DisplayRow {
    cells: Vec<DisplayCell>,
    logical_line: u32,            // rows sharing this value = soft-wrapped segments of one source line
}

struct TranscriptSnapshot {
    width: u16,
    rows: Vec<DisplayRow>,
    logical: String,              // copy-text projection: strips non-selectable cells, joins soft-wrapped rows
    cell_to_logical: Vec<Vec<Option<usize>>>,  // (row, col) → logical byte offset; None on non-selectable cells
    logical_to_cell: Vec<(u16, u16)>,          // logical byte offset → nearest display cell
}
```

- `Transcript::snapshot(width)` produces / retrieves a cached snapshot. Invalidated on width change or transcript mutation.
- `TranscriptWindow` navigates the snapshot in `(row, col)` display coordinates; vim operations that need line-based motion use `logical` via the mapping.
- Top-relative: `scroll_top_row: u16` (top of viewport = this row index in the snapshot); cursor is absolute `(row, col)` in snapshot coordinates. "Stuck to bottom" is `scroll_top_row == snapshot.rows.len() - viewport_rows`.
- Snapshot exposes `copy_range(sel_start, sel_end) -> String` as the single copy primitive — walks cells, emits `copy_as` or `ch` or nothing.
- Snapshot exposes `snap_to_selectable((row, col)) -> (row, col)` for cursor placement.

**Deletes**: `TranscriptWindow::mount`, `visible_cpos`, `sync_from_cpos`, `line_start_offsets`; `Screen::full_transcript_text`, `Screen::last_viewport_text`; `BlockHistory::viewport_text` as a public API; `content_visual_range` helper in events.rs; inverted-scrollbar arithmetic; all bottom-relative-to-top-relative conversions; the pin's `apply_pin` math gets simpler because `scroll_top_row` doesn't need adjustment on transcript growth.

**Done when**: no code path converts between coordinate systems; content cursor / selection / click / copy all read from one snapshot; the snapshot carries metadata slots for Stage 8.

---

## Stage 8 — Selectable regions + unwrap-on-copy [~3h]

**Replaces**: copying that includes viewport padding on the sides; copying soft-wrapped lines as multiple `\n`-separated strings; the "I selected a word but got the left padding too" UX papercuts.

Migrate existing renderers to emit proper `SpanMeta`:

- **Left and right viewport padding** → `selectable: false, copy_as: Some("")`. Click-to-position snaps to the nearest selectable cell on the same row; drag-select through padding is silent on copy.
- **Soft-wrap continuation**: when a logical line wraps across multiple rows, consecutive rows share the same `logical_line`; the snapshot's `logical` string does **not** insert `\n` between them. Copying a selection that spans the wrap gets the unwrapped text.
- **Hard newlines** (actual `\n` in source text): rows get different `logical_line` values; copying a selection that spans them gets `\n`.

Initial scope is exactly these three. Future work (future stage / one-liner edits):

- Diff `+/-/ ` gutter column → `selectable: false`
- Tool-call output indent → `selectable: false`
- Line-number column (if added) → `selectable: false`
- Quote-bar `│` in message blocks → `selectable: false, copy_as: Some("")`

Each is a single `SpanMeta` change at the emission point; no new code paths.

**Deletes**: the bare-string `copy_to_clipboard(&buf[s..e])` call in `copy_content_selection_and_clear` (replaced by `snapshot.copy_range(...)`); the selection-range translation that goes through `rows.join("\n")` to recover text.

**Done when**: selecting across the full width and copying produces only the real content, no padding; copying a soft-wrapped paragraph yields one line of text, not N.

---

## Stage 9 — Keymap layering (buffer-local + window-local + global) [~2h]

**Replaces**: the flat global keymap + context-based modal overrides; the scattered buffer-specific behavior inline in event handlers (vim normal-mode in prompt vs. content handled in different places).

- Each buffer and window has its own `Keymap`. Lookup: buffer → window → global.
- Transcript buffer ships with vim motion bindings (`h/j/k/l`, `v`, `y`, `gg`, `G`) registered at buffer scope. Prompt buffer ships with editor bindings.
- Transcript window ships with `Ctrl-U/D/F/B/Y/E` at window scope (scroll, not motion). Floating-window modal bindings (Esc, Enter) at window scope.
- Global scope: `Ctrl-W` window navigation, `Ctrl-L` redraw, `Ctrl-C` interrupt.

**Deletes**: hardcoded "if window is content then route through vim" branches in events.rs; the distinction between `KeyAction::Cmd` and `Action::Motion` at dispatch (both resolve through one keymap chain).

**Done when**: adding a new keybind for a specific buffer or window is one API call; no dispatch-time `if` checks which buffer is focused.

---

## Stage 10 — Selection/cursor/scrollbar renderers unified [~2h]

**Replaces**: duplicated selection-priority logic (`Buffer::selection_range` vs `Vim::visual_range`); per-window scrollbar paint code; separate `draw_soft_cursor` call sites.

- One `Selection` view on `Buffer` — returns vim visual if present, else shift anchor. Callers stop re-implementing priority.
- `render::cursor(&dyn Window, &Snapshot, &LayoutState)` and `render::scrollbar(&dyn Window, &LayoutState)` are shared. Both windows route through them.
- `TranscriptWindow::visual_range(&snapshot)` produces viewport-ready selection geometry; event code stops doing it.

**Deletes**: `content_visual_range` fragments in events.rs; `extend_content_selection_to` / `extend_prompt_selection_to` split (becomes one `api::win::extend_selection_to(pos)`); duplicate scrollbar paints.

**Done when**: one selection rule, one cursor renderer, one scrollbar renderer — parametrized by `&dyn Window`.

---

## Stage 11 — One render path + drop `Deref` [~2h]

**Replaces**: the parallel existence of `draw_frame` (scrollback-oriented) and `draw_viewport_frame` (fullscreen compositor) in normal mode.

- `draw_viewport_frame` is the only normal-mode path. `draw_frame` remains only for headless scrollback commit.
- Drop `Deref<Target = Buffer>` entirely from `InputState` — Stage 2 kept it for migration; now it goes.

**Deletes**: the normal-mode scrollback `\r\n` branch; `InputState::Deref`.

**Done when**: normal-mode has exactly one render entry; no hidden mutation pathway on the prompt.

---

## Stage 12 — Semantic intents for mouse/wheel [~2h]

**Replaces**: synthesizing `KeyCode::Char('j')` / `KeyCode::Up` to drive scrolling; the workaround where wheel in the prompt used `Up`/`Down` because `j`/`k` would type literal characters.

- `PaneIntent::{Scroll, MoveCursor, BeginSelection, ExtendSelection, YankSelection}` — small set covering non-keyboard input.
- `api::win::scroll` / `api::win::set_cursor` are the targets; wheel handlers call the API directly.
- Vim handles real keys; intents handle mouse/wheel/page.

**Deletes**: `Buffer::press_n`; `scroll_prompt_by_lines`'s synthetic-key loop.

**Done when**: no code synthesizes `KeyEvent` to express a non-keyboard action.

---

## What this plan explicitly rejects

- **Flattening `BlockHistory` into plain text.** Block structure is load-bearing for rendering.
- **Unifying prompt and content under one `KeyAction` enum.** Their semantics diverge; keymap layers give per-buffer/per-window divergence without ad-hoc `if` checks.
- **One big-bang rewrite.** Every stage is shippable. If we stop after Stage 8 we have a materially better system.
- **Plugin system in this refactor.** The API is *shaped* to support plugins; no plugin loader, manifest format, or sandboxing in scope.
- **Deleting the old scrollback path entirely.** Headless mode still needs it. Kill it from normal mode only (Stage 11).
- **Forcing prompt and transcript windows to be structurally identical.** The `Window` trait covers overlap; `PromptWindow` stays richer where it needs to be.
- **Moving freeze into the model.** Freeze is "don't repaint"; model is always current.

## Recommended order

Stages 1 → 12 in order. Two checkpoints are valid stopping points:

- **After Stage 3** — bugs fixed (undo skips, readonly confusion), public API surface exists, command dispatch unified. Minimal structural churn; shippable on its own.
- **After Stage 8** — coordinate drift bug class gone, transcript fully modeled with per-cell metadata, completer/dialogs unified as floating windows, padding-aware copy. Most architectural debt paid.

Stages 1–3 are the "foundations" — naming, invariants, API surface. Stages 4–8 are "model/view separation." Stages 9–12 are "polish and consistency."

## Success criteria

- Adding a new window type (file tree, agent list, inspector) is: implement `Window`, register keymaps via `api::win::set_keymap`. No changes to `State`, `Screen`, or dispatch core.
- Adding a new completer use-case is: build a `CompleterSpec`, call `api::ui::open_completer`. No new render code.
- Adding a new command is: `api::cmd::register(name, handler)`. It's reachable from both keybinds and `:cmd`.
- Adding a non-selectable UI element (gutter, decoration, line number, diff marker) requires no changes to cursor, selection, or copy logic — just `selectable: false` on the emitted spans.
- No function in the codebase converts between bottom-relative and top-relative coordinates.
- No mutation of any buffer bypasses `smelt::api`.
- `Transcript` is unit-testable without a terminal; `State` is unit-testable without `Screen`.
- Follow-up fixes stop clustering around cursor drift, selection mapping, scroll inversion, and prompt-vs-content handler forks.
- The surface of `smelt::api` is small enough to document on one page and stable enough to be a candidate plugin API.

## What the in-flight WIP already contributes

The uncommitted work lands the skeleton of Stages 1–2:
- `TextBuffer` split (will be renamed to `Buffer` and gain `BufferKind` in Stage 1).
- `Pane` trait + `Mutation` enum + `InputState::apply` (becomes `Window` trait + `api::buf::*`/`api::win::*` in Stage 2).
- Four direct-write bug sites fixed through `apply(Mutation::Replace)`.
- Viewport pin mechanism on `ContentPane` (replaces the failed "freeze" approach; stays; gets folded into Stage 7's top-relative scroll primitive).

Treat the WIP as a down payment on the naming, invariants, and mutation surface — then rename + reshape in Stage 1–2 proper.
