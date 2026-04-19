# TUI Architecture Refactor Plan

## Goal

Reshape the TUI into a familiar editor-style architecture modeled on neovim — **buffers** (content + cursor + selection + buffer-local keymaps) inside **windows** (viewports with window-local keymaps and a layout rect), every interaction routed through a stable **public API** (`smelt::api::{buf, win, block, cmd, transcript, keymap, ui, status}`) — and expose that API to user configuration via **Lua bindings** (`mlua` + `~/.config/smelt/init.lua`). The status line is a dedicated **one-row docked UI region** driven by structured status items, not a fake editable buffer window.

The original code fought three problems at once: the content pane navigated a rendered projection instead of a model, coordinate systems / layout / freeze logic each lived in multiple places, and there was no shared vocabulary for "do this thing to that buffer/window." The nvim-style model + Lua FFI dissolves all three and yields an extensible platform.

## Why nvim vocabulary

- **Buffer = content** (text + undo + attachments + buffer-local keymap). Readonly or editable. Independent of on-screen presence.
- **Window = viewport onto a buffer** (rect + scroll + cursor + selection + optional vim state + window-local keymap). Multiple windows can show different buffers. Floating windows = windows with a non-docked rect.
- **Dialogs and completer are floating windows** — one primitive covers them all.
- **Keymaps layer** (`block → buffer → window → global`) so a transcript can have vim motions without the prompt inheriting them.
- **Public API** — internal code and Lua scripts share one surface. If the API expresses everything the app does, user Lua can extend anything the app can do.
- **Status line = one-row docked UI region, not a normal editor window.** It participates in layout and hit-testing like a docked window, but it is driven by a structured status-item model rather than buffer/cursor/selection semantics.

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
    status_rect:     Rect,                         // exactly one row; top or bottom dock
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

// smelt::api::status — one-row docked status region
set_provider(StatusProvider)
clear_provider()
invalidate()
default_items(StatusContext) -> Vec<StatusItem>

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
- **Status bar direction**: keep the status line exactly one row high; treat it as a docked UI region/role in layout, but keep width fitting, priority dropping, and truncation in Rust. Lua supplies structured status items (`text`, `priority`, `align`, `truncatable`, optional style/tag), not raw terminal drawing.
- **Streaming-safe input routing**: `handle_event_idle` + `handle_event_running` share one `dispatch_common` preamble (mouse, pane chord, cmdline, content-focus routing, overlay keys). Dispatch priority documented inline. Focus switch, click, drag-select all work mid-stream.
- **Nvim-style cmdline**: `:` opens a command line in the status bar from any window (normal mode only). `CmdlineState` in `render/cmdline.rs` owns editing + rendering. Enter executes via `run_command` (`:` → `/` normalization). Cmdline module is self-contained: state, key handling, and rendering in one file.
- **`CursorOwner` consolidation**: single enum set per frame determines which component draws the soft cursor. Replaces scattered `cmdline.is_none()` / `focused && last_app_focus == X` guards at each draw site.
- **Prompt prediction cursor gated on focus** — no more double-cursor when focus is on the transcript or cmdline.
- **Curswant seeded on click** (both prompt and transcript), and on `refocus`.
- **Stage 3 command bus scaffold**: `api::cmd::run` is the single entry point; `Outcome = CommandAction` is the stable result shape. Registry lookup still matches legacy `App::handle_command` — migration is mechanical.

## Known bugs (remaining)

- **`j`/`k` locked on resume until first click** — intermittent; likely cpos/curswant/vim-state seeding order across `refocus` + `mount` + `sync_from_cpos`. Hasn't reproduced against the new `ViewportGeom`; deferred as a follow-up audit.
- **Selection flickers when scrolled up during streaming** — when viewport is pinned and buffer grows, selection highlight flickers. Root cause: `cpos` is a byte offset into display text that changes each frame during streaming.

---

# Completed phases (summary)

## Phase A — Paradigm cleanup ✅

Purged terminal-scrollback-era code. All sub-phases shipped:
- **A1**: All streams flow through streaming blocks (`push` + `rewrite` + `set_status(Done)`). The `active_*` fields on `Transcript` are **stream parsers** (accumulate chars, detect paragraph/code-block/table boundaries, call `history.rewrite()`), not a dual-storage overlay. `render_ephemeral_into` is the thinking-summary widget (a synthesized cross-block aggregate, intentionally kept as an overlay). `has_ephemeral` gates on `active_thinking && !show_thinking`.
- **A2**: `PurgeRedraw` debounce deleted.
- **A3**: commit/flush vocabulary purged. `paint_viewport` is the only block painter.
- **A4**: Obsolete scrollback tests deleted.
- **A5**: `draw_frame` deleted. `draw_viewport_frame` is the only normal-mode entry.
- **A6**: Dirty tracking simplified to `dirty: bool` (always full repaint). Paint split: `paint_transcript` / `paint_transcript_cursor` / `paint_prompt_region`. Stale overlay fix: `paint_viewport` always blank-fills.

## Phase B — Viewport geometry ✅

`ViewportGeom` centralized in `render/viewport.rs`. 13 unit tests. All call sites migrated. Short-buffer click bug fixed.

## Phase C — API surface lockdown (mostly ✅)

- ✅ C1 (Transcript extraction), C2 (TranscriptSnapshot + SpanMeta), C3 (WindowGutters), C4 (block keymap), C5 (cmdline), C6 (selectable spans), C7 (CursorOwner), C8 (LayoutState + Viewport + floats), C9 (PaneIntent), C10 (api::VERSION).
- Remaining: C5 tab completion, C7 unified selection renderer, C8 completer-as-float.

## Phase D — Lua bindings ✅

D1–D6 shipped. `mlua` runtime, `smelt.api.*` surface, autocmd dispatch, user commands + keymaps, `smelt.defer`, error surfacing.

## Phase E — Dogfood (partial)

- ✅ E3: `docs/lua-examples/` with example scripts.
- Remaining: E1 (port slash commands to Lua), E2 (default keybinds as Lua).

---

# Pending UX items

## Completer polish

- ✅ Reversed order, residue fix, ctrl+j/k direction, cmdline tab-activation, prompt `/` prefix.
- ⬜ Compute prompt completer indent from `/` visual column (hardcoded to 2).

## Known bugs

- **`j`/`k` locked on resume** — intermittent, deferred.

---

# Completed work (recent)

## Viewport top-anchoring ✅

Content starts at row 0 with trailing blanks below (was bottom-anchored with leading blanks). `ViewportGeom`, `paint_viewport`, `TranscriptSnapshot`, and cursor math all updated. `leading_blanks()` → `trailing_blanks()`, `visible_content_rows()` added.

## Pin fix — signed delta ✅

`apply_pin` now uses signed arithmetic (`i32`) to handle both content growth AND shrinkage. When streaming markdown changes shape (e.g., `**bold` → rendered `bold` changes wrapping), total row count fluctuates. The old `saturating_sub` only handled growth; scroll_offset now adjusts both ways.

## Logical nav buffer (Approach B) ✅

Vim motions operate on a content-only buffer — non-selectable gutter chars (`│ `, padding, line numbers) are stripped. The cursor inherently stays on selectable content without post-motion snapping.

**Architecture:**
- `TranscriptSnapshot::nav_rows()` — selectable display chars only (no `copy_as` substitutions)
- `nav_col_to_display_col()` / `display_col_to_nav_col()` — per-row column mapping between nav and display coordinate systems
- `nav_byte_to_row_col()` / `copy_nav_byte_range()` — byte-level mapping + copy with `copy_as` applied
- `Screen::full_transcript_nav_text()` — committed nav rows + ephemeral selectable chars
- All navigation callers switched: `handle_content_novim_key`, `move_content_cursor_by_lines`, `handle_content_vim_key`, `content_visual_range`, `compute_status_position`, `position_content_cursor_from_hit`, `copy_content_selection_and_clear`, `refocus_content`, scrollbar reanchor
- Cursor rendering maps nav col → display col via `last_viewport_lines` span metadata
- Selection painting maps nav cols → display cols in `ContentVisualRange`
- Click handling maps display col → nav col via `display_col_to_nav_col`
- Copy operations go through `copy_nav_byte_range` which applies `copy_as` substitutions

**Coordinate system boundaries:**
| Boundary | Mapping |
|---|---|
| Cursor rendering | nav col → display col (via DisplayLine spans) |
| Selection painting | nav cols → display cols (via snapshot) |
| Click-to-position | display col → nav col (via snapshot) |
| Copy/yank | nav byte range → display (row, col) → `copy_range` |

## Selection highlight respects selectability ✅

`paint_visual_range` now skips non-selectable spans — their original appearance is preserved. Each selectable span gets its own `move_to` so the highlight jumps over non-selectable gaps (gutters, padding, borders keep their normal look within multi-line or visual-line selections).

## Thinking block streaming fixes ✅

- Suppressed committed thinking block during streaming (ephemeral overlay handles it)
- Fixed gap suppression for 0-row blocks
- Fixed thinking summary padding: `" │ "` → `"│ "` (removed leading space)
- Fixed content doubling in `render_ephemeral_into`

---

# Remaining work

## R0 — Tool block copy refinements ✅

- ✅ **Soft-wrapped bash commands copy without injected newlines.** `print_tool_line` sets `source_text(summary)` on the first line and marks wrap-continuation segments via `mark_soft_wrap_continuation()`. Copy yields the original unwrapped command.
- ✅ **Execution time is non-selectable.** `print_dim_non_selectable()` helper prints time/timeout strings with `selectable: false` on tool call headers, non-bash tool lines, agent block headers, and agent sub-tool entries.
- ✅ **`copy_range` tracks `source_text_emitted`.** Soft-wrapped continuation rows are only skipped when `source_text` was actually used on the parent row. Previously, any fully-covered soft-wrapped row was unconditionally skipped — this caused partial selections (e.g., starting mid-row after a selectable prefix like `⏺ bash `) to silently lose continuation row content.
- ✅ **`is_soft_wrap` flag corrected in `print_tool_line`.** Was using `first_wrap_seg` (first segment of wrapped line) inverted as `!first_wrap_seg[idx]` — incorrectly marked real newlines in multi-line commands as soft-wrap continuations. Fixed to track `si > 0` directly.
- ✅ **`all_selectable_covered` replaces `full_row`.** `copy_range` now checks whether all selectable cells fall within the selection range, not just `c_start == 0 && c_end == cells.len()`. This lets `source_text` work correctly when non-selectable gutters sit outside the nav-mapped selection range.

## Behavioral test coverage ✅

Targeted tests for critical behaviors — each test documents a contract that must survive refactoring.

**`window.rs` — pin signed delta (6 tests):**
- `apply_pin` handles growth, shrinkage, clamps to zero, consecutive mixed deltas, noop when unpinned, unpin stops tracking

**`transcript.rs` — copy pipeline + nav buffer (7 tests):**
- Soft-wrapped rows without `source_text` still emit content (not silently dropped)
- `all_selectable_covered` with mixed selectable/non-selectable cells + trailing time suffix
- 3-row soft-wrap with `source_text` correctly coalesces
- `source_text_emitted` resets across logical line boundaries
- Partial nav selection (skipping selectable prefix) preserves continuation rows
- Full nav selection uses `source_text` and skips continuations
- `nav_col ↔ display_col` roundtrip with interleaved non-selectable gaps

**`blocks.rs` — thinking summary + tool layout (8 tests):**
- `thinking_summary`: bold title extraction, fallback to "thinking", blank line skipping, empty content, reject empty bold `****`
- `layout_block` for bash tool: `source_text` set on first line, `soft_wrapped` on continuation lines
- Multi-line bash command (real newlines): second line is NOT marked `soft_wrapped`
- Elapsed time suffix rendered in a non-selectable span

## Scrollbar click/thumb alignment fix ✅

**Problem:** Scrollbar rendering and click-to-scroll used asymmetric integer rounding — rendering truncated (`scroll * max_thumb / max_scroll`) while click rounded (`thumb * max_scroll + max_thumb/2) / max_thumb`). This caused the thumb to jump when clicked in the middle of the bar.

**Fix:** Added matching `+ max_scroll / 2` rounding to the thumb position formula in `Scrollbar::new`. Both directions now use the same rounding, so render → click → re-render is idempotent for every scroll position.

**Files:** `render/scrollbar.rs` (formula fix), `render/region.rs` (exhaustive roundtrip test across multiple viewport sizes).

## Double-Esc rewind dialog restored ✅

**Problem:** The double-Esc rewind dialog was lost during worktree branch refactoring. A premature `return EventOutcome::Noop` in `handle_esc_key` prevented the rewind path from executing.

**Fix:** Restored the missing logic in `app/events.rs`: after the compaction cancel check returns, the handler now calls `user_turns()`, erases the prompt, and opens `RewindDialog`. If vim was in insert mode before the first Esc, `restore_vim_insert` is set so the dialog restores insert mode on cancel.

## R1 — Raw-copy pipeline ✅

**Problem:** Copying from the transcript yields the rendered display — `─` instead of `---`, no `**bold**` markers, soft-wrapped lines get `\n` injected, heading `#` prefixes stripped, blockquote `>` stripped, etc. Copy should produce the raw source markdown.

**Architecture (Option B — source text per display row):** Each `DisplayLine` carries an optional `source_text: String` — the raw source line it was rendered from. During `render_markdown_inner`, the first visual row of each source line stores the full source line; soft-wrap continuations leave it `None`. The `TranscriptSnapshot` propagates `source_text` per row. `copy_range` uses source text for fully-selected rows (emitting raw markdown) and falls back to cell-based `SpanMeta.copy_as` for partially-selected rows.

**Two complementary mechanisms:**

| Mechanism | Scope | Used when |
|---|---|---|
| `DisplayLine.source_text` | Full source line | Full-row selection — emits raw markdown (bold markers, heading `#`, blockquote `>`, etc.) |
| `SpanMeta.copy_as` | Per-cell substitution | Partial-row selection — restores `>` prefix on blockquotes, `---` on HR cells |

**What's implemented:**

- `DisplayLine.source_text: Option<String>` — set by `render_markdown_inner` on the first segment of each source line
- `DisplayLine.soft_wrapped: bool` — set on continuation segments after word-wrap
- `LayoutSink::set_source_text()` / `mark_soft_wrap_continuation()` — trait methods + SpanCollector impl
- `TranscriptSnapshot.source_text: Vec<Option<String>>` — propagated from display lines during snapshot build
- `TranscriptSnapshot.soft_wrapped: Vec<bool>` — propagated similarly
- `copy_range` logic:
  - Full row + has source_text → emit source_text (raw markdown)
  - Full row + soft_wrapped → skip (source already emitted by first segment)
  - Partial row → cell-based copy with SpanMeta.copy_as fallback
- Cell-level `copy_as` annotations: HR (`---`), blockquote (`> ` prefix)
- Tests: `copy_range_uses_source_text_for_full_rows`, `copy_range_partial_row_ignores_source_text`, `copy_range_soft_wrapped_rows_coalesce`

**Files touched:**
- `render/display.rs` — `source_text` + `soft_wrapped` on `DisplayLine`
- `render/layout_out.rs` — `set_source_text` + `mark_soft_wrap_continuation` on `LayoutSink` + `SpanCollector`
- `render/blocks.rs` — `set_source_text(lines[i])` in `render_markdown_inner`, `copy_as` on HR + blockquote, `mark_soft_wrap_continuation` on wrap segments
- `render/transcript.rs` — `source_text` + `soft_wrapped` on snapshot, `copy_range` source-text dispatch

## R2 — Extract StreamParser from Transcript ✅

**Problem:** `Transcript` mixed two concerns: (1) domain state (block store + snapshot cache) and (2) stream parsing (`active_thinking`, `active_text`, `active_tools`, `active_agents`, `stream_exec_id`). The streaming fields are transient input adapter state; the block store + snapshot are persistent domain state.

**Architecture:** `StreamParser` is a standalone struct owning all streaming state. Every method takes `&mut BlockHistory` for block mutations. `Screen` owns both as peers:

```rust
struct Screen {
    transcript: Transcript,   // block store + snapshot cache
    parser: StreamParser,     // transient streaming state
    ...
}
```

Dependency is one-way: parser writes into `BlockHistory`, never reads the snapshot. `has_ephemeral` and `render_ephemeral_into` (thinking summary overlay) read `parser.active_thinking()` for accumulator state and `transcript.history` for committed blocks — the dual dependency is now explicit at the call site instead of hidden inside one struct.

**What's implemented:**
- `render/stream_parser.rs` — all streaming methods (thinking, text, code, table, tool, exec, agent lifecycles), `update_tool_state` helper
- `render/transcript.rs` — stripped to block store + snapshot cache (~600 lines removed)
- `render/screen.rs` — routes streaming calls through `self.parser.method(&mut self.transcript.history)`, `clear()` resets parser, `truncate_to()` clears parser tools/agents

## Remaining work — ordered by dependency

### T1. Unify transcript width/gutter authority ✅

Screen owns gutters, TranscriptWindow drops them. Dead `width` parameters removed from `full_transcript_text` / `full_transcript_nav_text`. `set_transcript_gutters` deleted (zero callers).

### T2. Top-relative scroll for transcript ✅

Transcript converted from bottom-relative `scroll_offset` to top-relative `scroll_top`. `ViewportGeom` simplified — `skip_from_top()` is identity. Scrollbar inversion removed. Pin holds `scroll_top` constant. 9 files touched, all tests pass.

### T3. Route all command execution through `run_command` ✅

Both `process_input` and `try_command_while_running` now call `run_command` so Lua overrides and autocmds fire consistently.

### T4. Transcript cursor in snapshot coordinates 🔧

**Problem:** `cpos` (byte offset into ephemeral joined nav buffer) is fragile: yank uses `buf.find(&raw)` which fails on duplicate text, `cpos` can land mid-codepoint during streaming, and the buffer is rebuilt on every key dispatch.

**Solution (incremental):**

**T4a. Fix yank path** ✅ — `KillRing.source_range` tracks the byte range vim yanked from. `handle_content_vim_key` reads `source_range()` instead of `buf.find(&raw)`. Eliminates the duplicate-text yank bug.

**T4b. Derive cpos from (row, col)** ✅ — Status bar uses `cursor_abs_row`/`cursor_col` directly (no byte offset). Visual range, copy, and block focus all use `compute_cpos()` from fresh rows instead of persisted `cpos`. Eliminates stale-offset and mid-codepoint hazards.

**T4c. Selection anchor in (row, col) space** ⬜ (deferred) — Switch `WindowCursor.anchor` from byte offset to `(row, col)` so selection survives transcript mutations during streaming. Low priority — the existing selection flicker during streaming is a known bug.

**Files:** `kill_ring.rs`, `vim/mod.rs`, `window.rs`, `cursor.rs`, `events.rs`, `screen.rs`.

### T5. YAML → Lua config migration ⬜

**Problem:** Config is YAML (`config.yaml`) but the plan wants nvim-style Lua config. Currently both exist independently — `init.lua` for keymaps/commands/autocmds, `config.yaml` for providers/settings/theme. The CLI `smelt auth` writes providers directly into YAML.

**Design:**
- `init.lua` becomes the single user config file for behavior: settings, theme, keymaps, commands.
- Providers move to a separate data file (`providers.yaml`) — CLI tools need to write them without parsing Lua, and providers are data (credentials + endpoints) not behavior.
- New Lua API: `smelt.settings({vim_mode = true, ...})`, `smelt.theme({accent = "blue"})`, `smelt.set_default_model("gpt-4o")`.
- Loading order: providers file → `init.lua` → CLI `--set` overrides.
- **TODO (discuss):** `smelt auth` currently mutates the entire config YAML. With the split, it writes only to the providers file — needs a migration path for existing users.

**Files:** `config.rs`, `lua.rs`, `engine/config_file.rs`, `setup.rs`.

### T6. Clean up dead scaffolding ✅ (partial)

- ✅ Deleted `WinId` enum (zero usages)
- ✅ Deleted `api::intent::PaneIntent` (zero usages)
- ✅ Deleted `WindowRect` enum (never instantiated)
- ⬜ `LayoutState.gap` — still used in `screen.rs` and tests, not dead
- ⬜ `api::VERSION` — keep until Lua wiring (T8)

Can happen anytime — no dependencies.

### T7. Completer as floating window ✅ (partial)

Both prompt and cmdline completer paint paths now share `paint_completer_float()` — a single function that handles overlay positioning, drawing via `draw_completions()`, and float registration via `push_float`. Duplicated overlay code consolidated from ~35 lines × 2 sites → one 20-line helper.

Remaining: full `CompleterWindow` struct with its own rect/z-order (deferred until genuinely multiple completers exist).

### T8. Finish the public API model ✅

- `api::win::*` scroll semantics are honest (T2)
- `api::cmd::run` is the single entry point (T3)
- `api::block::*` fully wired for mutable blocks
- Handle-based (`WinId` / `BufId`) model deferred until genuinely multiple windows/buffers exist
- Lua shim → `api::*` routing deferred to T5 (config migration)

### T9. Unified selection renderer — kept separate ✅

Prompt uses per-char `SpanKind` walk with inline cursor rendering; transcript uses `DisplayLine` span walk with selectability gaps. Different data models make unification a complexity increase, not a simplification. Both share `theme::selection_bg()` for consistent highlight color.

### Remaining Phase C/D/E items

- ⬜ C5: cmdline tab completion
- ⬜ C11: status line provider-driven content (`api::status::set_provider`)
- ⬜ D7: Lua statusline providers (`smelt.statusline(fn)`)
- ⬜ E1: port slash commands to Lua (requires more API surface for dialog/state transitions)
- ⬜ E2: default keybinds as Lua (requires mode-aware keymap registration)

---

# Dependency graph

```
T1 (gutters)  ──→  T2 (top-relative scroll)  ──→  T3 (cmd unification)
     │                                                    │
     └──→ T7 (completer-as-float)                        ▼
                                                   T4 (snapshot cursor)
T6 (dead code) ← anytime                                 │
T5 (YAML→Lua) ← independent                              ▼
                                                   T8 (public API)  →  T9 (selection)
```

## Explicit non-goals

- **Flattening `BlockHistory` into plain text.** Structure is load-bearing.
- **Unifying prompt and transcript under one monolithic window type.** Shared trait + per-role structs.
- **One big-bang rewrite.** Each phase ships.
- **Plugin loader / manifest / sandboxing beyond mlua's defaults.** Lua scripts live in `~/.config/smelt/`; no registry.
- **Async Lua.** v1 is sync-only; coroutines possible later.
- **Deleting `draw_frame` entirely.** Headless mode still needs scrollback output.
- **Forcing structural symmetry between windows.**
- **Making the status line pretend to be a normal editable/selectable buffer window just to fit the editor model.** It is a docked UI region with its own structured content model.
- **Moving freeze into the model.** Freeze is "don't repaint"; model is always current.

## Success criteria

- Adding a new window type is: implement `Window`, register keymaps via `api::win::set_keymap`. No changes to `State`, `Screen`, or dispatch core.
- Adding a new completer use-case: build a `CompleterSpec`, call `api::ui::open_completer`. No new render code.
- Adding a new command: `api::cmd::register(name, handler)` — reachable from keybinds, `:cmd`, and Lua.
- Adding a non-selectable UI element (gutter, decoration, line number): `selectable: false` on the span. Nothing else changes.
- Adding a Lua hook into any existing feature: one `smelt.on(event, fn)` call. No Rust changes.
- Users can redefine the status line in Lua with structured items while Rust continues to own width fitting, priority dropping, and truncation.
- No function converts between bottom-relative and top-relative coordinates.
- No mutation bypasses `api::*`.
- `Transcript`, `State`, every primitive is unit-testable without a terminal.
- Viewport/cursor math has a unit test matrix that covers the full space of `(total, viewport, scroll, line)`.
- `smelt.api.*` surface documents to one page and is stable across minor versions.
- Users can redefine every default keybind and built-in slash command in Lua without touching Rust.

