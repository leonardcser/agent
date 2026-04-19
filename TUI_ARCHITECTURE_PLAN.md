# TUI Architecture Refactor Plan

## Goal

Reshape the TUI into a familiar editor-style architecture modeled on neovim — **buffers** (content + cursor + selection + buffer-local keymaps) inside **windows** (viewports with window-local keymaps and a layout rect), every interaction routed through a stable **public API** (`smelt::api::{buf, win, block, cmd, transcript, keymap, ui}`) — and expose that API to user configuration via **Lua bindings** (`mlua` + `~/.config/smelt/init.lua`).

The original code fought three problems at once: the content pane navigated a rendered projection instead of a model, coordinate systems / layout / freeze logic each lived in multiple places, and there was no shared vocabulary for "do this thing to that buffer/window." The nvim-style model + Lua FFI dissolves all three and yields an extensible platform.

## Why nvim vocabulary

- **Buffer = content** (text + undo + attachments + buffer-local keymap). Readonly or editable. Independent of on-screen presence.
- **Window = viewport onto a buffer** (rect + scroll + cursor + selection + optional vim state + window-local keymap). Multiple windows can show different buffers. Floating windows = windows with a non-docked rect.
- **Dialogs and completer are floating windows** — one primitive covers them all.
- **Keymaps layer** (`block → buffer → window → global`) so a transcript can have vim motions without the prompt inheriting them.
- **Public API** — internal code and Lua scripts share one surface. If the API expresses everything the app does, user Lua can extend anything the app can do.

## Guiding decisions

- Nvim-style names throughout: `Buffer`, `Window`, `TranscriptWindow`, `PromptWindow`, `curswant`, `cursor`, `api::buf::*`, `api::win::*`.
- Every state mutation goes through `smelt::api`. No direct `input.buf = …`, no direct `screen.active_* = …`. The API is the only door — Lua sees exactly what internal code sees.
- `BufId` / `WinId` / `BlockId` are stable opaque handles; Lua holds them by value. Mutation does not invalidate handles.
- Keep `BlockHistory`, `Vim`, content-addressed block layout caching. They are the good parts.
- **Cursor lives on the window, not the buffer.** `Window.cursor` is the canonical position (byte offset into its buffer's text). Display row/col is *derived* each frame from cursor + scroll + snapshot. Same buffer in two windows = two independent cursors.
- `TranscriptSnapshot` is the canonical derived view for the transcript window — cursor motion, selection, yank, click-hit-testing, scrollbar all read from it.
- **Top-relative coordinates everywhere.** Scroll = rows from the top of the transcript; cursor = absolute `(row, col)` in the snapshot. Bottom-relative math deletes itself.
- **Viewport pin, not freeze.** When the user is scrolled up, drag-selecting, or in vim visual, pin the top row to a fixed transcript row. New streaming output flows below off-screen; visible rows do not shift. Implemented by growing `scroll_offset` against a tracked total, not by skipping the paint.
- **Window-level gutters.** Left/right padding, scrollbar column, and any future number / sign / fold column are **window properties** rendered *around* the content rect. `content_rect = window.rect - gutters`. Cursor columns live in content-rect coords (cursor cannot enter a gutter — nvim's `numbercol` semantics). Clicks in gutters route to the gutter widget (scrollbar) or snap into content.
- **Span-level `SpanMeta` for in-block decorations** — diff markers, quote bars, tool-call indent. Cells carry `selectable: bool` + `copy_as: Option<String>`. Soft-wrap unwrap-on-copy uses per-row `logical_line: u32` so wrapped visual rows of one source line don't get `\n` inserted on copy.
- The `Window` trait describes only the shared surface. `PromptWindow` and `TranscriptWindow` stay structurally different where they need to be.
- Don't flatten `BlockHistory` into plain text. Snapshot *projects* it.
- One render path for normal mode. Scrollback `\r\n` stays only for headless fallback.
- **Alt-buffer is the only interactive rendering model.** Any code defending the old immutable-scrollback paradigm is dead weight; Phase A scrubs it.

## Target shape

```rust
struct State {
    buffers:        SlotMap<BufId, Buffer>,
    windows:        SlotMap<WinId, Window>,
    transcript:     Transcript,                    // owns the transcript buffer id
    current_window: WinId,
    layout:         LayoutState,                   // recomputed per frame
    keymap:         GlobalKeymap,
    commands:       CommandRegistry,
    autocmds:       AutocmdRegistry,               // Lua-observable events
    ui:             UiState,
    lua:            LuaRuntime,                    // mlua; holds init.lua env
}

struct Buffer {
    kind:        BufferKind,                       // Prompt | Transcript | Scratch | Completer | Dialog
    text:        String,
    undo:        UndoHistory,
    attachments: Vec<AttachmentId>,
    keymap:      Keymap,                           // buffer-local
    readonly:    bool,
    // No cursor, no selection, no vim — those live on the window.
}

struct Window {
    buffer:           BufId,
    rect:             WindowRect,                  // Dock(Region) | Float { rect, z, anchor }
    gutters:          WindowGutters,
    scroll_top_row:   u16,                         // top-relative; content-rect rows
    cursor:           usize,                       // byte offset into buffer.text
    selection_anchor: Option<usize>,
    vim:              Option<Vim>,
    kill_ring:        KillRing,
    keymap:           Keymap,                      // window-local
    focused_block:    Option<BlockId>,             // drives block-local keymap layer
    role:             WindowRole,                  // Prompt | Transcript | Completer | Dialog | StatusLine
}

struct WindowGutters {
    pad_left:   u16,
    pad_right:  u16,
    scrollbar:  Option<Side>,                      // Left | Right | None
    // Future: numbercol_width, signcol_width, foldcol_width
}

struct Transcript {
    blocks:    Vec<BlockId>,                       // one storage; no separate "live"/"committed" split
    history:   BlockStore,                         // BlockId → Block
    buffer:    BufId,
    viewport:  ViewportGeom,                       // shared geometry module (see Phase B)
}

struct Block {
    content:    BlockContent,
    status:     Status,                            // Streaming | Done
    view_state: ViewState,                         // Expanded | Collapsed | TrimmedHead | TrimmedTail
    keymap:     Keymap,                            // block-local
}

struct TranscriptSnapshot {                        // width-keyed; cached
    width:         u16,
    rows:          Vec<DisplayRow>,
    logical:       String,                          // copy-text projection
    cell_to_logical: Vec<Vec<Option<usize>>>,
    logical_to_cell: Vec<(u16, u16)>,
    block_of_row:  Vec<BlockId>,                   // O(1) row → owning block
    row_of_block:  HashMap<BlockId, Range<u16>>,
}

struct LayoutState {
    windows:         Vec<(WinId, Rect)>,           // z-ordered; floats on top
    transcript_rect: Rect,
    prompt_rect:     Rect,
    status_rect:     Rect,
}
```

## Public API (`smelt::api`)

One surface for internal callers, Lua scripts, and future out-of-process plugins.

```rust
// smelt::api::buf — text + undo + attachments
get_text / set_text / insert / delete
cursor / set_cursor / selection / yank
set_keymap

// smelt::api::win — viewport, per-window cursor, scroll, selection
list / current / set_current
buffer / rect / gutters / set_gutters
scroll / set_scroll_top / set_cursor
selection / extend_selection_to / clear_selection
set_keymap / focus_block

// smelt::api::block — per-block interactive model
status / set_status
view_state / set_view_state
append_text / rewrite / invoke(BlockAction)
push_streaming(block) -> BlockId
streaming_ids -> Vec<BlockId>
set_keymap

// smelt::api::transcript — transcript-level ops
snapshot(width) -> &TranscriptSnapshot
blocks / push / truncate
streaming_ids
clear

// smelt::api::cmd — user + plugin commands
register(name, handler)
run(line)                                          // ":quit", "/export file", "gg"
unregister(name)

// smelt::api::keymap — global bindings
set_global(chord, Action)

// smelt::api::ui — floats + notifications + mode
open_floating(FloatingSpec) -> WinId
open_completer(spec, anchor) -> WinId
close_window(WinId)
notify(msg) / notify_error(msg)
set_mode(Mode)

// smelt::api::autocmd — events (Lua consumers)
on(event, handler) -> AutocmdId
off(AutocmdId)
emit(event, payload)                               // internal code triggers these
```

Lookup order when a key fires: **block → buffer → window → global**. Matches nvim's `vim.keymap.set` + `buffer = N`.

`Action` is a small enum — `Cmd(String)`, `Motion(Motion)`, `Callback(LuaRef | RustFn)`. Commands go through `api::cmd::run`, so `":quit"` from Lua, from a keybind, or typed at the prompt all land in the same handler.

### Autocmd events (what Lua can subscribe to)

| Event           | Payload                               | When                                          |
| --------------- | ------------------------------------- | --------------------------------------------- |
| `key`           | `{ key, mods, buf, win }`             | Before dispatch                               |
| `cmd_pre`       | `{ name, args }`                      | Before command handler runs                   |
| `cmd_post`      | `{ name, args, result }`              | After command handler returns                 |
| `buf_enter`     | `{ buf, win }`                        | Focus moves to a buffer                       |
| `buf_leave`     | `{ buf, win }`                        | Focus leaves a buffer                         |
| `win_enter`     | `{ win }`                             | Window focus switch                           |
| `block_create`  | `{ block, kind }`                     | New block pushed                              |
| `block_change`  | `{ block, field }`                    | `rewrite` / `set_status` / `set_view_state`   |
| `stream_start`  | `{ block, kind }`                     | Status flipped to `Streaming`                 |
| `stream_end`    | `{ block, kind }`                     | Status flipped to `Done`                      |
| `selection`     | `{ win, start, end }`                 | Selection changes                             |
| `mode_change`   | `{ old, new }`                        | Agent mode / vim mode / app focus transitions |
| `resize`        | `{ width, height }`                   | Terminal resize                               |

## Shipped inventory

What's done so far, cross-referenced with the phases below.

- **Nvim-style renames** (Phase A prerequisite, done): `TextBuffer → Buffer`, `Pane → Window`, `ContentPane → TranscriptWindow`, `PaneId → WinId`, `pane` module → `window`, `content_pane` field → `transcript_window`.
- **`Window` trait filled out**: `cursor() / set_cursor()`, `scroll_top() / set_scroll_top()`, `selection() / clear_selection()`, shared by `TranscriptWindow` + `InputState`. `DerefMut for InputState` dropped.
- **`smelt::api` surface**: `buf::{get_text, replace}`, `win::{cursor, set_cursor, selection, clear_selection, scroll_top, set_scroll_top}`, `cmd::run`, `block::{view_state, set_view_state, status, set_status, push_streaming, rewrite, streaming_ids}`.
- **Unified `Viewport` type** (C8 shipped): `Viewport` in `render/region.rs` replaces `TranscriptRegion` and `InputRegion`. Every scrollable buffer records a single `Viewport { top_row, rows, content_width, total_rows, scroll_offset, scrollbar }` after paint. `ScrollbarGeom` lives inside `Viewport.scrollbar`. `Screen::transcript_viewport()` and `Screen::input_viewport()` expose them. Mouse hit-testing uses `Viewport::hit() → ViewportHit::{Content, Scrollbar}`. Adding a new scrollable region (dialog body, file preview) = record one more `Viewport`.
- **WindowCursor** (one type for prompt + transcript) carries `anchor` + `curswant`; `move_vertical(buf, cpos, delta)` is the single vertical-motion entry point.
- **Block primitives for mutable blocks (Stage 3.5 pre-req + core)**:
  - `BlockId` decoupled from content — monotonic per-session handle (cross-session cache sharing via `content_hash` field on `LayoutKey`).
  - `ViewState` + `Status` enums + per-block sparse maps.
  - `layout_block` honors `view_state` (Collapsed / TrimmedHead / TrimmedTail post-processing with elision marker).
  - `push_streaming`, `rewrite`, `streaming_block_ids` on `BlockHistory` + `Screen` + `api::block`.
  - `is_live()` centralizes "streaming blocks invisible in main paint path" across `render` / `paint_viewport` / `viewport_text` / `total_rows`.
- **Auto-pin when scrolled up**: transcript viewport pins on `scroll_offset > 0`, not just on selection/drag. Streaming rows grow below off-screen; visible content stays stable. Pin delta uses fresh `full_transcript_text.len()` each tick (no more stale `region.total_rows`).
- **Bottom-anchor transcript**: `paint_viewport` + `viewport_text` prepend leading blank rows when `total < viewport_rows` so the last content row sits at the viewport bottom — matches the cursor math and the `scroll_offset == 0 = stuck to bottom` convention.
- **Status bar consolidation**: buffer-agnostic `StatusPosition { line, col, scroll_pct }` pushed by `App::compute_status_position`; right-aligned via `align_right: bool` on `StatusSpan` + `cursor::MoveToColumn`; status bar carved out from click region (row `h-1` swallows mouse events).
- **Streaming-safe input routing**: `handle_event_idle` + `handle_event_running` share one `dispatch_common` preamble (mouse, pane chord, cmdline, content-focus routing, overlay keys). Dispatch priority documented inline. Focus switch, click, drag-select all work mid-stream.
- **Nvim-style cmdline**: `:` opens a command line in the status bar from any window (normal mode only). `CmdlineState` in `render/cmdline.rs` owns editing + rendering. Enter executes via `run_command` (`:` → `/` normalization). Cmdline module is self-contained: state, key handling, and rendering in one file.
- **`CursorOwner` consolidation**: single enum set per frame determines which component draws the soft cursor. Replaces scattered `cmdline.is_none()` / `focused && last_app_focus == X` guards at each draw site.
- **Prompt prediction cursor gated on focus** — no more double-cursor when focus is on the transcript or cmdline.
- **Curswant seeded on click** (both prompt and transcript), and on `refocus`.
- **Stage 3 command bus scaffold**: `api::cmd::run` is the single entry point; `Outcome = CommandAction` is the stable result shape. Registry lookup still matches legacy `App::handle_command` — migration is mechanical.

## Known bugs to fix during Phase B

- **`j`/`k` locked on resume until first click** — intermittent; likely cpos/curswant/vim-state seeding split across `refocus` + `mount` + `sync_from_cpos`.
- **Click hit-test offset inverted on short buffers** — `position_content_cursor_from_hit` doesn't account for the bottom-anchor leading blanks added in `paint_viewport`. Clicking top of viewport maps to wrong line.

Both fall out of Phase B's `ViewportGeom` centralization.

---

# Phased plan

Five phases. Each is shippable. Each phase lists what it **deletes** alongside what it adds.

## Phase A — Paradigm cleanup: purge terminal-scrollback-era code

The scrollback-commit rendering model is dead. Alt-buffer repaints every frame; there is no terminal-side scrollback that can drift. Every abstraction that defended the old invariants is dead weight and has to go before anything else. This phase deletes, it doesn't add.

**Status**
- ✅ **A2** (PurgeRedraw): fully shipped.
- ✅ **A3** (commit/flush/scroll-mode vocabulary): `BlockHistory::render`, `flushed`-counter gating, `last_block_rows`, `suppress_leading_gap`, `pending_head_skip`, `scroll_up`, scroll-mode `newline` branch — all deleted. `paint_viewport` is the only block painter.
- ✅ **A4** (obsolete tests): shipped.
- ✅ **A5**: `draw_frame` fully deleted. `tick_dialog` → `draw_viewport_dialog_frame` (alt-buffer, reserves `dialog_height + 1 gap + 1 status` at the bottom of the viewport). `draw_prompt` + the one unit-test caller route through `draw_viewport_frame`.
- ✅ **A1**: every stream now flows through streaming blocks via `push` + `rewrite` + `set_status(Done)`. `active_exec` + `active_thinking` + `active_text` (including the table and in-code-line cases) + `active_tools` + `active_agents` all ship as `Block::Exec` / `Block::Thinking` / `Block::Text` / `Block::CodeLine` / `Block::ToolCall` / `Block::Agent` in history. `Element::ActiveTool` and `render_tool`'s overlay callers are deleted; `render_ephemeral_into` is now just the aggregate thinking-summary widget (kept intentionally — it's a summary, not a stream). `has_ephemeral` gates solely on `active_thinking && !show_thinking`.

### A1 — Atomic `active_*` → `Streaming` blocks cutover

Do all five `active_*` migrations in **one** coherent patch, not per-variant dual-write.

- Remove `active_text`, `active_thinking`, `active_tools`, `active_exec`, `active_agents` from `Screen`.
- At stream start: `api::block::push_streaming(block)` → hold the returned `BlockId`.
- On chunks: `api::block::rewrite(id, new_block)` or `api::block::append_text(id, chunk)`.
- On commit: `api::block::set_status(id, Done)`.
- Delete `Screen::render_ephemeral_into`.
- Delete `extra_lines` parameter from `paint_viewport`, `viewport_text`, `total_rows`.
- Delete `has_ephemeral()`.
- `Transcript::streaming_ids()` replaces all `self.active_*.is_some() / iter()` readers.
- `is_live()` stops skipping Streaming blocks in paint — Streaming = normal-painted, just with a "live" style flag.

**Deletes**: `active_*` fields, `render_ephemeral_into`, `has_ephemeral`, `extra_lines`, "live = overlay, committed = history" mental model, dual-storage for one logical stream.

### A2 — Remove `Ctrl+L` purge/redraw debounce

- `Action::PurgeRedraw`, `App::purge_redraw_debounced`, `App::last_purge_redraw`, `PURGE_REDRAW_DEBOUNCE` all go.
- `Screen::redraw`'s `Clear::All` goes.
- Ctrl+L handler collapses to `screen.mark_dirty()`.

**Deletes**: ~100 lines of anti-tearing gymnastics that the alt buffer makes irrelevant.

### A3 — Drop "commit" vocabulary + scrollback newlining

- Rename / inline `commit_exec`, `commit_active_tools`, `flush`, `flushed` counter, `last_block_rows`.
- `scroll_newline` vs `overlay_newline` bifurcation in `RenderOut` — overlay is the only path that ships; scrollback newline retained only for headless fallback under `cfg(test)` or a flag.
- `RenderOut::row: Option<u16>` "overlay vs scroll mode" branch collapses — always overlay in interactive mode.
- `pending_head_skip`, `suppress_leading_gap`, `BlockHistory::has_unflushed` — all artifacts of partial-commit rendering. Delete.

**Deletes**: `BlockHistory::flushed`, `last_block_rows`, `pending_head_skip`, `suppress_leading_gap`, scroll-mode `RenderOut` branches, `commit_*` naming.

### A4 — Delete obsolete tests

- Scrollback-integrity "no duplicate row" suite: invariants meaningless under alt-buffer.
- Overlay-edge-cases committed-history resize tests: assertion shape is wrong for the model.
- Any test that asserts "line X appears exactly once in the combined paint stream."
- 9 already removed; audit for more.

**Deletes**: tests defending dead invariants.

### A5 — Clean up `draw_frame` vs `draw_viewport_frame`

- `draw_viewport_frame` becomes the only normal-mode entry. `draw_frame` retained only under `#[cfg(test)]` + headless-capture.
- One `if is_dialog` branch collapses.

**Deletes**: normal-mode dual-paint path, ~200 lines.

**Phase A done when**: `Screen` has no `active_*`, no `render_ephemeral_into`, no `extra_lines`, no `PurgeRedraw`, no "flush" counter, no scroll-mode newlining in normal paint. Every block renders through one path; every stream mutates blocks in place.

---

## Phase B — Centralize viewport/cursor/scroll geometry

**Status**
- ✅ **B1**: `render/viewport.rs` ships `ViewportGeom` with `max_scroll` / `clamped_scroll` / `skip_from_top` / `leading_blanks` / `row_of_line` / `line_of_row` / `stuck_to_bottom` / `apply_growth`.
- ✅ **B2**: 13 unit tests — boundary matrix plus a parametric `matrix_row_line_roundtrip` that exhaustively validates `row_of_line ∘ line_of_row = id` across every (total, scroll) combination the plan calls out.
- ✅ **B3**: `BlockHistory::paint_viewport`, `BlockHistory::viewport_text`, `Screen::draw_viewport_frame` (scrollbar + cursor sites), `Screen::draw_viewport_dialog_frame`, `TranscriptWindow::reanchor_to_visible_row`, `App::content_visual_range`, and `App::position_content_cursor_from_hit` all read from `ViewportGeom`. Remaining bottom-relative cursor_line math is now a pure row-index convention — not viewport geometry — and stays on the window.
- ⚠️ **B4**: short-buffer click-offset bug fixed — `position_content_cursor_from_hit` now snaps leading-blank clicks to the first content line (regression test in `viewport::leading_blank_click_snaps_to_first_line`). The j/k-locked-on-resume bug is intermittent and hasn't been reproduced against the new geom; deferred as a follow-up audit.

---

## Phase C — API surface lockdown

**Status**
- ✅ **C6 shape**: `DisplaySpan` carries `SpanMeta { selectable, copy_as }`. Every span construction defaults to `selectable: true, copy_as: None`; gutters and decorations can flip them off without the copy path having to re-parse layout structure.
- ✅ **C9** (core): `api::intent::PaneIntent` (Scroll / MoveCursor / BeginSelection / ExtendSelection / YankSelection) ships as the wheel-and-mouse semantic intent. `scroll_prompt_by_lines` uses direct `cursor.move_vertical` instead of synthetic `KeyEvent` construction — no more looping `handle_event(Key(Up/Down))` for mouse-wheel scroll.
- ✅ **C10**: `api::VERSION = "1"` — public API generation marker for user scripts to branch on.
- ✅ **C3 shape**: `WindowGutters { pad_left, pad_right, scrollbar: Option<GutterSide> }` ships as a type; `content_width(window_width)` subtracts reservations.
- ✅ **C1**: `Transcript` extracted to `render/transcript.rs` — owns `BlockHistory` + all `active_*` streaming state. `Screen` delegates through thin forwarding calls. Scrollback-era vocabulary renamed: `commit_active_tools` → `finalize_active_tools`, `flush_blocks` → `finish_turn`, `render_pending_blocks` → `mark_blocks_dirty`, `commit_exec` → `finalize_exec`.
- ✅ **C2** (core): `TranscriptSnapshot` with `rows`, `block_of_row`, `row_of_block` cached on `Transcript` via generation counter. `full_transcript_text` and viewport text both read from the snapshot. `BlockHistory::full_text` and `viewport_text` deleted. `total_rows` in draw paths replaced with `snapshot.rows.len()`. Remaining: wire `block_of_row` into mouse dispatch, add `copy_range` / `snap_to_selectable` / `cell_to_logical` for full C2 completion.
- ✅ **C3 wire-up**: `WindowGutters` wired into `TranscriptWindow` + `Screen`. `paint_viewport` takes `pad_left` and applies it per-block via `paint_line(…, pad_left)` — `Block::User` gets `pad_left=0` (edge-to-edge bg with inner padding), all other blocks get the window's `pad_left`. Scrollbar column derived from `gutters.scrollbar` side. Content width uses `gutters.content_width(width)`. Block renderers stripped of legacy 1-space indent (moved to window gutter).
- ✅ **C4** (core): `dispatch_block_key` wired before vim/novim dispatch in `handle_event_app_history`. `focused_block_id` derives block from cursor position via snapshot's `block_of_row`. ToolCall blocks bind `e` to toggle expand/collapse. Remaining: add more block-scoped bindings (re-run, yank output), generalize to non-ToolCall blocks.
- ✅ **C5** (cmdline): nvim-style `:` command line renders inside the status bar row. `CmdlineState` in `render/cmdline.rs` owns buffer + cursor + editing + rendering + history. `:` opens from any window in normal mode (gated on not in insert mode). Enter executes through `run_command` (`:` normalizes to `/`). Esc/Ctrl-C dismisses. Up/Down recall previous commands. Ctrl-W deletes word backward. Ctrl-U clears line. Persistent history survives across open/close cycles. Remaining: tab completion, completer + dialogs as floating windows (defers to `WindowRect::Float` in C8).
- ✅ **C7** (cursor ownership): `CursorOwner` enum (`Prompt | Transcript | Cmdline | Dialog | None`) set once per frame via `refresh_cursor_owner`. All cursor draw sites check `cursor_owner` instead of independently testing focus + mode flags. Scrollbar now unified via `Viewport` (C8). Remaining: unified selection renderer.
- ✅ **C8** (core): `LayoutState` with `Rect`, `LayoutInput`, `HitRegion` in `render/layout.rs`. `compute()` produces docked transcript + prompt rects and optional `DialogLayout`. Computed per frame, stored on `Screen.layout`. Mouse dispatch uses `layout.hit_test()`. `Viewport` replaces `TranscriptRegion` + `InputRegion` — both transcript and prompt record a `Viewport` after paint; mouse click/drag/scrollbar all route through `Viewport::hit()`. `LayoutState` is lean (just rects); scrollbar state lives on the buffer's `Viewport`. `WindowRect::Float` and `FloatEntry` scaffolded; `push_float` registers z-ordered float rects; `hit_test` walks floats (highest z first) before docked regions. Remaining: wire completer and dialogs to use float layout.

---

## Phase D — Lua bindings (mlua)

**Status — all six sub-phases shipped as scaffolding.**

- ✅ **D1**: `mlua` 0.11 with the `lua54` + `vendored` features. `crate::lua::LuaRuntime::new()` constructs the runtime, registers the `smelt` global, and loads `$XDG_CONFIG_HOME/smelt/init.lua` (or `~/.config/smelt/init.lua`). Missing configs are not errors; load errors surface via `load_error`.
- ✅ **D2**: `smelt.api.version`, `smelt.api.cmd.register`, `smelt.notify`, `smelt.keymap`, `smelt.on`, `smelt.defer` live in `register_api`.
- ✅ **D3**: `AutocmdEvent::{BlockDone, CmdPre, CmdPost, StreamStart, StreamEnd}` + `LuaRuntime::emit(event)` iterate handlers; errors are captured per-handler and don't stop the dispatch.
- ✅ **D4**: `run_command`, `run_keymap`, `has_command`, `command_names` let the Rust dispatcher check for Lua bindings before falling back to built-in handlers.
- ✅ **D5**: `smelt.defer(ms, fn)` posts to `pending_timers`; `LuaRuntime::tick_timers` fires due handlers on each app tick. Pending-ops queue is the same `pending_notifications` / `lua_errors` channel.
- ✅ **D6**: every call path is wrapped so Lua errors land in `lua_errors`; the app drains them via `drain_errors` and surfaces the first as a notification. `load_error` captures `init.lua` syntax failures.
- ✅ **Wire-up into `App`**: `App::lua` is a directly-owned `LuaRuntime`. `run_command` emits `cmd_pre`, routes through the Lua registry if a name is bound, falls through to the built-in `handle_command`, then emits `cmd_post` and drains notifications/errors. `start_agent` / `finish_turn` emit `stream_start` / `stream_end`. The app loop pumps `tick_timers`, drains newly finished blocks (→ `block_done` autocmd), and surfaces pending notifications + errors every iteration.

## Phase E — Dogfood

- ✅ **E3**: `docs/lua-examples/` ships `leader.lua`, `block_summarizer.lua`, `per_project.lua`, and a README explaining the surface.
- ⬜ **E1**: porting the `/rewind`, `/export`, `/help`, `/model`, `/compact` handlers to ship as `smelt.api.cmd.register` in a default init.lua depends on the D wire-up above landing.
- ⬜ **E2**: default keymap-config-as-Lua likewise depends on the wire-up.

Four cursor/scroll bugs this session all had the same shape: the mapping `(viewport_rows, total, scroll_offset) ↔ (row_on_screen, line_in_buffer)` was reinvented at each call site. Centralize.

### B1 — New `render/viewport.rs` module

```rust
pub struct ViewportGeom {
    pub total:          u16,                       // rows in the flattened buffer
    pub viewport_rows:  u16,                       // visible rows
    pub scroll_offset:  u16,                       // rows from the bottom (alt-buffer convention)
    pub leading_blanks: u16,                       // viewport.saturating_sub(total), for bottom-anchoring
}

impl ViewportGeom {
    pub fn max_scroll(&self) -> u16;
    pub fn clamped_scroll(&self) -> u16;
    pub fn skip_from_top(&self) -> u16;            // lines to skip before painting
    pub fn row_of_line(&self, line_idx: u16) -> Option<u16>;   // None if offscreen
    pub fn line_of_row(&self, row: u16) -> Option<u16>;        // None if in leading blank
    pub fn cursor_row(&self, cursor_line: u16) -> u16;         // bottom-anchored
    pub fn apply_growth(&mut self, delta: u16);                // pin math
    pub fn stuck_to_bottom(&self) -> bool;
}
```

- Every paint path + every hit-test + every cursor placement consumes this.
- No more open-coded `viewport_rows.saturating_sub(1 + line)` or `total.saturating_sub(viewport).saturating_sub(scroll)`.
- `apply_growth` replaces the `pinned_last_total` delta dance.

### B2 — Unit tests lock down the invariants

Matrix: total ∈ {0, 1, viewport-1, viewport, viewport+1, 2·viewport}, scroll ∈ {0, 1, max-1, max, max+1}, line ∈ {0, total-1, total}. Every cell of the matrix has an expected `row_of_line` / `line_of_row` / `cursor_row`.

### B3 — Migrate call sites

- `TranscriptWindow::sync_from_cpos` / `visible_cpos` / `mount` / `reanchor_to_visible_row` — read from `ViewportGeom`.
- `BlockHistory::paint_viewport` / `viewport_text` — consume the geom's `leading_blanks` + `skip_from_top`.
- `events.rs::position_content_cursor_from_hit` — `geom.line_of_row(rel_row)` instead of `skip + rel_row`.
- `app/events.rs::sync_transcript_pin` / `apply_pin` — `geom.apply_growth(delta)`.

### B4 — Fix the two known cursor bugs as a side effect

- **j/k locked on resume**: trace through `refocus` → `mount` → `sync_from_cpos` with geom in place. Curswant + cpos seed order becomes one call into the geom.
- **Click offset inverted on short buffers**: `line_of_row(row)` accounts for `leading_blanks` automatically.

**Deletes**: open-coded viewport math in 6+ call sites; `pinned_last_total` as a magic number; the implicit "bottom-relative" convention (becomes an explicit type).

**Phase B done when**: `grep "viewport_rows.saturating_sub" crates/tui/src` returns nothing outside `viewport.rs`. Both known cursor bugs fixed by unit test.

---

## Phase C — API surface lockdown

With paradigm cleanup done and geometry stable, the remaining surface is what Lua will bind to. Make it clean and final *before* shipping Lua.

### C1 — Extract `Transcript` from `Screen`

- `Transcript` owns `BlockStore` + `Vec<BlockId>` + the transcript `BufId` + a `TranscriptSnapshot` cache.
- `Screen` becomes pure UI chrome: prompt paint, status bar, dialog layout, frame composition.
- `Transcript` is unit-testable without `Screen` — snapshot tests, view_state tests, streaming tests all run in microseconds with no TTY harness.
- Engine event handlers call `api::block::*` / `api::transcript::*` instead of `self.screen.append_text` etc.

**Deletes**: `Screen` as domain owner; direct `self.screen.active_*` was already gone in Phase A; now `self.screen.push_text` goes too.

### C2 — `TranscriptSnapshot` + top-relative coords + `SpanMeta`

```rust
struct SpanMeta {
    selectable: bool,
    copy_as:    Option<String>,                    // None = emit char; Some("") = skip; Some(s) = substitute
    action:     Option<BlockAction>,               // clickable cell
}

struct DisplayCell { ch: char, style: Style, meta: SpanMeta }
struct DisplayRow  { cells: Vec<DisplayCell>, logical_line: u32 }

struct TranscriptSnapshot {
    width:            u16,
    rows:             Vec<DisplayRow>,
    logical:          String,
    cell_to_logical:  Vec<Vec<Option<usize>>>,
    logical_to_cell:  Vec<(u16, u16)>,
    block_of_row:     Vec<BlockId>,
    row_of_block:     HashMap<BlockId, Range<u16>>,
}
```

- `Transcript::snapshot(width)` — cached, invalidated on width or block mutation.
- `TranscriptWindow` navigates snapshot `(row, col)` coords; vim line-motions use `logical` via the mapping.
- Top-relative: `scroll_top_row: u16` (top of viewport is this snapshot row). "Stuck to bottom" is `scroll_top_row == snapshot.rows.len() - viewport_rows`.
- `snapshot.copy_range(sel_start, sel_end) -> String` is the single copy primitive.
- `snapshot.snap_to_selectable((row, col))` for cursor placement.
- `snapshot.block_of_row[row]` for O(1) "which block owns this row" — mouse dispatch reads this directly.

**Deletes**: `full_transcript_text`, `viewport_text`, `last_viewport_text`, `rows.join("\n")` inside `TranscriptWindow::mount`, bottom-relative `scroll_offset` (becomes derived), all three coord conversions, viewport-relative visual-selection remap in `events.rs`.

### C3 — Window-level gutters (task #26)

- `WindowGutters { pad_left, pad_right, scrollbar: Option<Side> }` on every `Window`.
- `content_rect(window) = window.rect - gutters`.
- Cursor / selection / click-hit-test / snapshot rendering all in content-rect coords.
- Gutter painting is the renderer's job; buffer code never thinks about gutters.
- Scrollbar column fits inside `pad_right` (or `pad_left` if scrollbar = Left).
- Future `numbercol_width`, `signcol_width`, `foldcol_width` slot in without call-site changes.

### C4 — Keymap layering (block → buffer → window → global)

- Each buffer, window, and block has its own `Keymap`. Lookup in order.
- Transcript buffer ships with vim motions at buffer scope (`h/j/k/l`, `v`, `y`, `gg`, `G`). Prompt buffer ships with editor bindings.
- Transcript window ships with scroll bindings (`Ctrl-U/D/F/B/Y/E`) at window scope.
- Focused block can override keys (e.g. tool block binds `e` to expand, `r` to re-run).
- `Ctrl-W` window navigation, `Ctrl-L` redraw, `Ctrl-C` interrupt at global scope.

**Deletes**: hardcoded `if window is content then route through vim` branches; `KeyAction::Cmd` vs `Action::Motion` divergence at dispatch (both through one chain).

### C5 — Completer + dialogs as floating windows

- `Completer` keeps its fuzzy engine, renders into a floating window. Two mount sites (above prompt, over status).
- Floats don't shift dock rects — overlay only.
- Dialogs become floating windows with `modal = true`; dispatcher routes to top modal first.
- `api::ui::open_floating(spec)` / `open_completer(spec, anchor)` are the entry points.

**Deletes**: `Dialog` trait as a distinct concept; per-completer render code; "dialog intercepts events" special case.

### C6 — Selectable regions + unwrap-on-copy

- Left/right viewport padding → `selectable: false, copy_as: Some("")`.
- Soft-wrap continuation: shared `logical_line`, no `\n` between on copy.
- Hard newlines: different `logical_line`, `\n` on copy.
- Diff gutter `+/-/ `, quote bar `│`, tool-call indent, line-number column → `selectable: false` with appropriate `copy_as`.
- `snapshot.copy_range()` walks cells emitting `copy_as` or `ch` or nothing.

**Deletes**: bare `copy_to_clipboard(&buf[s..e])` in `copy_content_selection_and_clear`; selection-range translation through `rows.join("\n")`.

### C7 — Unified selection / cursor / scrollbar renderers

- One `Selection` view on a window (vim visual if present, else shift anchor).
- `render::cursor(&dyn Window, &Snapshot, &LayoutState)` and `render::scrollbar(&dyn Window, &LayoutState)` — shared.
- `api::win::extend_selection_to(pos)` replaces the `extend_content_selection_to` / `extend_prompt_selection_to` split.

**Deletes**: duplicated selection priority; per-window scrollbar paint; separate `draw_soft_cursor` call sites.

### C8 — Layout primitive (`LayoutState` + `Viewport` + `WindowRect`)

- `LayoutState` owns rects only — no scrollbar state. `Viewport` in `region.rs` is the buffer-level primitive: each scrollable buffer records `Viewport { top_row, rows, content_width, total_rows, scroll_offset, scrollbar }` after paint. `Viewport::hit()` classifies mouse events into `ViewportHit::{Content, Scrollbar}`. Any future scrollable region (dialog body, file preview) adds one more `Viewport` — no layout changes needed.
- `WindowRect::{Dock(Region), Float { rect, z, anchor }}` for floating windows (remaining).
- Compositor produces `LayoutState` per frame from rects + dock priorities.
- Mouse handlers read `LayoutState` (z-ordered hit-test walks floats first).

**Deletes**: `TranscriptRegion`, `InputRegion`, `TranscriptHit` (replaced by `Viewport` + `ViewportHit`); `Screen::input_region()` tuple method; `Screen::input_scrollbar()` method.

### C9 — Semantic intents for mouse/wheel

- `PaneIntent::{Scroll, MoveCursor, BeginSelection, ExtendSelection, YankSelection}`.
- Wheel handlers call `api::win::scroll` directly; no synthetic `KeyCode::Char('j')`.

**Deletes**: `Buffer::press_n`; `scroll_prompt_by_lines`'s synthetic-key loop.

### C10 — Freeze + document `api::*`

- Freeze public signatures. Every `pub fn` in `api::` gets a doc comment that will render as user-facing docs.
- `smelt.api.version = "1"` constant. Future breaking changes bump it; Lua can branch.
- One-page API reference generated from doc comments.

**Phase C done when**: `Transcript` is unit-testable without `Screen`; `State` is unit-testable without a terminal; no coord-system conversion; every mutation goes through `api::*`; one selection rule, one cursor renderer, one scrollbar renderer; one-page API doc exists.

---

## Phase D — Lua bindings (mlua)

`api::*` is stable. Wire it to Lua.

### D1 — mlua bootstrap

- Add `mlua` crate (Lua 5.4; feature `send` if multi-thread, `vendored` for hermetic build).
- `LuaRuntime` lives on `State`. Loads `~/.config/smelt/init.lua` at startup.
- Errors in `init.lua` surface as a notification + deferred dialog; app keeps running with default config.
- Lua sandbox: no `io.*`, no `os.execute`, no `package.loadlib` (configurable — default locked down).

### D2 — Shim `api::*` as Lua tables

One file (`lua/bindings.rs`), one `register_fn!` call per Rust `pub fn`. Mechanical.

```lua
-- ~/.config/smelt/init.lua example
smelt.keymap("n", "<C-g>", function()
  smelt.api.win.set_cursor(0, 0)                   -- gg equivalent
end)

smelt.cmd.register("double_compact", function(args)
  smelt.api.cmd.run("compact")
  smelt.api.cmd.run("compact")
end)

smelt.on("block_change", function(ev)
  if ev.field == "status" and ev.status == "Done" then
    smelt.notify("block " .. ev.block .. " finished")
  end
end)
```

### D3 — Autocmd dispatch

- `AutocmdRegistry { events: HashMap<EventKind, Vec<(AutocmdId, LuaRef)>> }`.
- Emission points in Rust (`api::autocmd::emit(event, payload)`) at every `*_change` / `*_enter` / `*_leave` / `stream_start` / `stream_end` / `key` / `cmd_pre` / `cmd_post`.
- Dispatch is synchronous; Lua errors caught, logged, next handler runs.
- Event table documented (see list above).

### D4 — User-command + keymap registration from Lua

- `smelt.cmd.register(name, fn)` → calls `api::cmd::register` with a Rust-side adapter that calls `lua.call(ref, args)`.
- `smelt.keymap(mode, chord, fn_or_cmd)` → calls `api::keymap::set_global` (or `set_buffer` / `set_window` with scope arg).
- Handlers live as `LuaRef`; dropped when user re-sources config or calls `unregister`.

### D5 — Re-entrancy + event loop integration

- Lua callbacks run on the main thread, synchronously with the dispatching event.
- Mutations inside callbacks (e.g. `api::buf::insert` from `on("key", ...)`) are queued as pending ops, applied after the dispatching handler returns — prevents mid-event state corruption.
- No async in v1. Scheduled callbacks use a simple `smelt.defer(ms, fn)` that posts to the tick loop.

### D6 — Lua error surfacing UX

- `init.lua` syntax errors → startup notification + dialog with the traceback. App runs with default config.
- Runtime errors in callbacks → logged to `~/.cache/smelt/lua.log` + notification. Handler is marked dead so it doesn't spam on repeat.
- `:lua <expr>` command for live evaluation (debug helper).

**Deletes**: nothing — this phase is purely additive.

**Phase D done when**: `init.lua` can register user commands, bind keys, subscribe to events, and drive mutations through the API. Errors don't crash.

---

## Phase E — Dogfood

Validate the API by porting existing features *through* it.

### E1 — Move slash commands to Lua

- `/rewind`, `/export`, `/help`, `/model`, `/compact` etc. register via `smelt.cmd.register` in a shipped `init.lua` that defaults to the current behavior.
- Users can override by redefining in their own config.
- Exposes gaps: any command that can't be expressed through `api::*` is a Phase C bug report.

### E2 — Built-in keybinds become Lua

- Default keymap config shipped as Lua. Users can rebind or disable without touching Rust.
- Block keymaps (tool expand/collapse/re-run) via `smelt.api.block.set_keymap`.

### E3 — Example plugins

- Ship one or two example plugins under `docs/lua-examples/`:
  - Vim-style custom leader keymap
  - Block summarizer (collapse long tool output by default)
  - Per-project config (load `smelt.lua` from `$PWD/.smelt/` if present)

**Phase E done when**: any existing user-visible feature is reimplementable as pure Lua. Gaps surfaced here close Phase C.

---

## Explicit non-goals

- **Flattening `BlockHistory` into plain text.** Structure is load-bearing.
- **Unifying prompt and transcript under one monolithic window type.** Shared trait + per-role structs.
- **One big-bang rewrite.** Each phase ships.
- **Plugin loader / manifest / sandboxing beyond mlua's defaults.** Lua scripts live in `~/.config/smelt/`; no registry.
- **Async Lua.** v1 is sync-only; coroutines possible later.
- **Deleting `draw_frame` entirely.** Headless mode still needs scrollback output.
- **Forcing structural symmetry between windows.**
- **Moving freeze into the model.** Freeze is "don't repaint"; model is always current.

## Success criteria

- Adding a new window type is: implement `Window`, register keymaps via `api::win::set_keymap`. No changes to `State`, `Screen`, or dispatch core.
- Adding a new completer use-case: build a `CompleterSpec`, call `api::ui::open_completer`. No new render code.
- Adding a new command: `api::cmd::register(name, handler)` — reachable from keybinds, `:cmd`, and Lua.
- Adding a non-selectable UI element (gutter, decoration, line number): `selectable: false` on the span. Nothing else changes.
- Adding a Lua hook into any existing feature: one `smelt.on(event, fn)` call. No Rust changes.
- No function converts between bottom-relative and top-relative coordinates.
- No mutation bypasses `api::*`.
- `Transcript`, `State`, every primitive is unit-testable without a terminal.
- Viewport/cursor math has a unit test matrix that covers the full space of `(total, viewport, scroll, line)`.
- `smelt.api.*` surface documents to one page and is stable across minor versions.
- Users can redefine every default keybind and built-in slash command in Lua without touching Rust.

## Recommended sequencing

**A → B → C → D → E.** Two checkpoints make sense as release candidates:

- **After Phase B**: paradigm clean, math stable. Code is much smaller and correct-by-construction. Materially better system on its own even if Lua never lands.
- **After Phase C**: API stable, documented. First private alpha of `api::*` possible here for out-of-process bindings.
- **After Phase D**: Lua ships.
- **After Phase E**: Lua ecosystem opens.

Phases are not strictly blocking — e.g. some Phase C items (gutters, completer-as-float) can start while Phase B's geom module is being tested. Anything that doesn't touch the viewport/cursor math can parallelize freely.
