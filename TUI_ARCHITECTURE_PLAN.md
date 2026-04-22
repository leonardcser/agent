# TUI Architecture — UI Framework Rewrite

Living document. Reorganized 2026-04-22 to fold in the FFI-unification
direction. Historical narrative preserved below "Completed work".

## North star

Two commitments drive every remaining change:

1. **One rendering path.** The compositor (`ui::Ui`) owns every pixel. No
   parallel ANSI-emitting layer, no cached "last seen" state inside `app`,
   no dirty flags outside the grid diff. Everything visible is a window
   registered with the compositor.

2. **FFI = internal API.** Lua plugins call the same functions Rust uses.
   `smelt.api.ui.*` is a thin userdata wrapper over `ui::Ui` — no translation
   layer, no schema parsing, no stringly-typed action tokens. Behavior is
   expressed as callbacks (`ui::Callback`), not as strings the host matches
   on.

Everything below follows from these two.

## Implementation instructions

These directives govern how this plan is executed. They override defaults.

- **Stop at friction.** When abstractions don't fit, stop and talk. Present
  options, explain trade-offs, ask for a decision. Don't push through
  ambiguity. Cost of pausing is low; cost of wrong abstraction is high.

- **The plan evolves.** Living roadmap, not contract. When implementation
  reveals the plan is wrong, fix the plan, then keep going.

- **Correct abstractions matter most.** Not "get it done" — "get it right."
  Take inspiration from Neovim (buffers, windows, compositor, event
  dispatch) but adapt to Rust's ownership model.

- **No dead code annotations.** Never add `#[allow(dead_code)]`. Use it,
  remove it, or leave the compiler warning as a tracking marker.

- **Format, lint, test at the end of each logical commit.**
  `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings &&
  cargo test --workspace`. Update the plan, then commit.

- **Atomic rewrites over incremental scaffolding.** Some refactors
  cannot be split into a chain of always-green small commits without
  inserting intermediary shims (parallel trait impls, stringly-typed
  bridges, "kept for now" stubs) that get deleted a commit later. Do
  not add that scaffolding. If the refactor is genuinely atomic —
  e.g. deleting `DialogState` alongside all 9 dialog conversions —
  land it as one commit. A larger diff that leaves the tree
  *correct* is preferable to a chain of small diffs that leave the
  tree *working via temporary duplication*. Many small steps can
  still be done internally and reviewed as one patch; what matters
  is that the committed history reflects real architectural moves,
  not transient compromises.

- **Unit of commit = unit of architectural change.** A commit
  answers "what moved to its final home in this change?" A dialog
  migration that deletes its `DialogState` impl, registers its
  `Callbacks`, and removes the host-side string matching is one
  commit. A migration that adds Callbacks alongside a still-present
  `DialogState` is incomplete — don't ship it.

- **No throwaway work.** Every step is a subset of the final state,
  not a detour. If you find yourself writing code you know you'll
  delete in the next commit, stop and roll that deletion into the
  current commit.

- **Present multiple approaches.** Include the bold option (clean rewrite).
  Let the user choose.

### Testing interactive TUI changes via tmux

Smelt is a full-screen TUI; `cargo test` only covers unit-testable logic.
For anything visual (dialog rendering, layout, selection, prompt shifts),
drive the real binary in a tmux pane and capture the screen.

1. **Target pane = split inside `smelt:4`** (the worktree window). **Never**
   run the binary in the pane you're typing into, and never launch a new
   window — split the worktree window.
2. Create a side pane once and keep its `%id`:
   ```bash
   tmux split-window -h -t smelt:4 -c <worktree-path> -P -F '#{pane_id}'
   ```
3. Pre-build (`cargo build --quiet`), then launch the compiled binary in
   the side pane so compile output doesn't pollute the captured screen:
   ```bash
   tmux send-keys -t %ID './target/debug/smelt' Enter
   ```
4. Drive with `tmux send-keys`; inspect with
   `tmux capture-pane -t %ID -p | tail -N`.
5. For panel content debugging, avoid `eprintln!` (crossterm raw mode
   swallows stderr). Write to `/tmp/smelt-draw.log` via `OpenOptions`.
   Remove the writes before committing.

### UI conventions

- **Dialog titles are lowercase.** `resume (workspace):`, `permissions`,
  `help`. Uppercase is reserved for proper nouns.
- **Meta columns are dim, content is normal.** Resume list: size + time
  columns use `SpanStyle::dim()`, title keeps default fg. Selection retints
  the whole row fg to accent.
- **Selection = fg-accent on the cursor row.** No bg fill, no cursor glyph,
  no layout shift. Single mechanism across every list panel.
- **Gap above the hints row.** Every dialog reserves one blank row between
  panel content and hints.

## Current state

**Phase 1 + 2 of the rendering keystone — shipped.** Cumulative delta:
**−3147 lines** across 4 commits (`9b25449`, `66002e8`, `b22ceef`,
`53d0ebd`).

Deleted from `render::Screen`:
- Status bar path: `render_status_line`, `queue_status_line`, cached
  `last_mode` / `last_vim_*` / `last_status_position`.
- Dialog path: `dialog_row`, `queue_dialog_gap`, `clear_dialog_area`,
  `set_dialog_open`, `set_constrain_dialog`, `dialog_open`,
  `constrain_dialog`, `CursorOwner::Dialog`.
- Legacy draw path: `draw_prompt`, `draw_viewport_frame`,
  `draw_prompt_sections` (startup-only, never called once dialogs
  migrated).
- Cascading dead code: `render_notification`, `render_stash`,
  `render_queued`, `paint_transcript`, `paint_transcript_cursor`,
  `paint_prompt_region`, `paint_visual_range`, `paint_completer_float`,
  entire `completions.rs`, `BlockHistory::paint_viewport`, `paint_line`,
  `apply_style`, `PAD_SPACES`, `render_status_spans`, `draw_bar`,
  `Scrollbar::paint_column`, `render_styled_chars`,
  `LayoutState::{push_float, floats, term_width, gap}`,
  `HitRegion::Completer`, `PromptState::prev_prompt_ui_rows`,
  `RenderOut::{init_cursor, move_to}`, `WorkingState::{pause, resume,
  paused_at, is_paused}`.
- Tests pinned to legacy paint: `tests/harness/`, `tests/status_bar.rs`.

Global chord layer landed (`1cf2960`): `dispatch_terminal_event` routes
`Shift+Tab` / `Ctrl+T` / `Ctrl+L` before float/cmdline/prompt dispatch.
Status bar reads from `self.mode` (source of truth) instead of the
deleted `last_mode` cache — B1 and B2 resolved.

**Still on the legacy side:**
- Prompt input isn't a compositor window yet — painted through the
  remaining Screen infrastructure (task #11).
- Notification / queued / stash layers are bespoke; should be plain
  windows (task #9).
- A shrunken `render::Screen` persists to coordinate transcript paint +
  prompt paint during migration; deletion is the last gate (task #15).

## Dispatch: three parallel systems today

All three must collapse into one.

1. **`ui::KeyResult::Action(String)`** — widgets emit stringly-typed
   tokens (`"select:N"`, `"submit"`, `"shortcut:X"`, `"dismiss"`). Host
   matches on prefixes.
2. **`app::dialogs::DialogState` trait** — per-builtin state struct stored
   in `App::float_states: HashMap<WinId, Box<dyn DialogState>>`. Methods:
   `handle_key`, `on_action(&str)`, `on_select(idx)`, `on_dismiss`, `tick`.
3. **`app::dialogs::lua_dialog::LuaDialog`** — parses a Lua table
   `{title, panels=[{kind, …}]}` into `PanelSpec`s, keeps
   `OptionEntry { action: String, on_select: Option<RegistryKey> }`, builds
   a `{action, option_index, inputs}` result table that the coroutine
   resumes on. String-matching all the way down.

**The clean path is already built.** `crates/ui/src/callback.rs` defines:
- `Callbacks` — per-window registry keyed by `(WinId, KeyBind)` or
  `(WinId, WinEvent)`.
- `WinEvent::{Open, Close, FocusGained, FocusLost, SelectionChanged,
  Submit, TextChanged, Dismiss}`.
- `Payload::{None, Key, Selection, Text}`.
- `CallbackResult::{Consumed, Pass}`, `CallbackCtx`.
- `Callback::{Rust(FnMut), Lua(LuaHandle)}` — unified dispatch.

Doc comment: "this is the single behavior mechanism." Zero production
callers today — only tests exercise it.

## Two Lua APIs today

- `smelt.api.win.open_float(buf, opts)` — neovim-primitive style, used by
  `btw.lua`. Build a buffer, open a window, register key/event callbacks.
  1:1 with `Ui::win_open_float` + `Ui::win_set_keymap` + `Ui::win_on_event`.
- `smelt.api.dialog.open({title, panels, …})` — declarative schema, used
  by `plan_mode.lua`. Rust parses it into `PanelSpec`, loses information
  (focusable, collapse_when_empty, separator_style, pad_left, PanelHeight
  variants), re-emits string actions, the plugin re-matches.

Only the first survives. The second collapses into `ui.dialog_open` with
userdata constructors.

## The full rewrite

**Scope**: collapse the three parallel dispatch systems, migrate the
last of rendering off `render::Screen`, and unify the Lua FFI with the
internal API. All done as atomic commits that each delete what they
replace — no intermediary shims, no "works for now" scaffolding.

**Ordering**: phases are read top-to-bottom. Within a phase, the listed
commits are ordered such that each commit is *independently landable*
without leaving the tree in a split-architecture state. Some phases are
a single commit because they can't be split without intermediaries.

### Foundation: the typed effect op enum

**Decision (2026-04-22): option (a) — typed effect ops.** Rust and Lua
callbacks are identical because both push typed ops into the same
channel. No `&mut App` reentrance, no parallel dispatch paths, no
stringly-typed matching.

**Decision (2026-04-22, refined for Phase B):**
- *(1a) Shared channel*: `AppOp` lives in `tui` (it references tui-only
  types). `ui::CallbackCtx.actions: Vec<String>` stays structurally for
  ui-level compatibility but tui's Rust closures ignore it — they
  capture a clone of `Arc<Mutex<AppOps>>` (same channel Lua already
  uses) and push `AppOp` directly. One drain path, symmetric Rust/Lua.
- *(2a) Widgets stay pure*: `ui::KeyResult::Action(String)` stays as
  the *internal* widget→container protocol inside `ui`. The Dialog /
  Window container translates `Action("select:N")` /
  `Action("dismiss")` into `dispatch_event(WinEvent::Submit/Dismiss,
  Payload::…)` so widgets don't need access to `Callbacks` or a
  `WinId`. The host-side (tui) string matching on `Action(...)` is
  what gets deleted.

Every dialog effect that today lives inside a `DialogState::on_select`
body becomes a variant on `AppOp`. The reducer drains ops each tick.

```rust
// crates/tui/src/app/ops.rs (new)
pub enum AppOp {
    // Session
    LoadSession(String),            // Resume::on_select
    DeleteSession(String),          // Resume Delete key
    RewindTo(usize),                // Rewind::on_select
    // Permissions
    ApproveTool { scope: ApprovalScope, reason: Option<String> },
    DenyTool   { reason: Option<String> },
    AddPermissionRule { scope, pattern, decision },
    RemovePermissionRule { id },    // Permissions::delete
    // Agents
    OpenAgentDetail(AgentId),
    KillAgent(AgentId),
    // Mode / model
    SetMode(Mode),
    SetReasoning(ReasoningLevel),
    // UI / notification
    CloseFloat(WinId),
    Notify(String),
    NotifyError(String),
    // Export
    Export(ExportKind),
    // ... ~30–50 total across all dialogs
}
```

**Why this works for both Rust and Lua.** A Rust-side Resume callback
pushes `AppOp::LoadSession(id)`. A Lua-side `plan_mode` callback pushes
`AppOp::ApproveTool { scope }`. The reducer doesn't know or care which
language wrote it. The ops ARE the narrow App→dialog surface. The ops
ARE the Lua plugin API.

### Phase A — AppOp foundation (1 commit)

**A1 · Rename `PendingOp` → `AppOp`, relocate to `app/ops.rs`.** The
existing `lua::PendingOp` enum already plays this role for Lua-side
ops and `App::apply_ops` already drains it. Rename, move to its own
module, update the ~66 callsites. The `LuaShared.ops: Vec<AppOp>`
channel stays — Lua continues to push through it.

This commit doesn't add new variants. Phase B adds them as dialog
conversions need each one (Rust forbids unused enum variants without
`#[allow(dead_code)]`, which the plan forbids). The inventory from
the pre-Phase-A research pass stays in commit-message / plan notes
as reference material for Phase B.

**Note on `CallbackCtx.actions`**: Phase A does NOT touch
`ui::CallbackCtx.actions: Vec<String>`. `AppOp` references tui-only
types (`ApprovalScope`, `Mode`, `ResolvedSettings`, `CustomCommand`,
`PermissionEntry`) and can't live in `ui`. Phase B wires Rust closures
to capture `Arc<Mutex<AppOps>>` (the same channel Lua uses) and push
`AppOp` directly — `CallbackCtx.actions` stays as a ui-level field
but tui code doesn't write to it.

### Phase B — Dispatch unification **(done 2026-04-22)**

Landed as a series of small commits rather than one atomic big-bang.
Each commit left the tree building + tests green. `DialogState` and
`Callbacks` coexisted at the *codebase* level during the transition
but never at the *per-dialog* level — each dialog belonged to exactly
one system at a time. No shims, no forwarding.

Shipped commits:

- **B.0 · Infrastructure.** `ui::Ui::handle_key_with_actions`
  auto-translates widget `KeyResult::Action` strings (`"select:N"`,
  `"submit"`, `"submit:T"`, `"dismiss"`) into `WinEvent` dispatches
  when the target window has a callback registered for that event.
  Added `WinEvent::Tick`, `Ui::dispatch_tick`, per-window key
  fallback (for Resume's typed-into-filter pattern).
- **B.1..B.6 · Per-dialog migrations.** Help/Export/Ps, then
  Rewind/Permissions, Resume, Question, Agents, Confirm. Each commit
  deleted one dialog's `DialogState` impl and replaced it with
  `Rc<RefCell<State>>`-captured closures registered via
  `win_on_event` / `win_set_keymap`. Confirm added the
  `blocking_wins: HashSet<WinId>` path as a replacement for
  `DialogState::blocks_agent`.
- **B.7 · LuaDialog.** Migrated the Lua-driven dialog path
  (`smelt.api.dialog.open`) onto the same Callbacks+AppOp pipeline.
  New `AppOp::ResolveLuaDialog` carries the `on_select` RegistryKey
  from the callback into the reducer by *moving* it out of the
  dialog state. Simplified `OptionList::handle_key` to emit
  `select:N` (and move the cursor) on shortcut match, deleting the
  `shortcut:X` action string plus the shortcut lookup code that
  consumed it.
- **B.final · Delete `DialogState` infrastructure.** With every
  dialog on Callbacks+AppOp, deleted: `DialogState` trait,
  `ActionResult` enum, `App::float_states` HashMap,
  `handle_float_action`, `intercept_float_key`, `tick_focused_float`,
  and the legacy `close_float` branch. `focused_float_blocks_agent`
  now reads `blocking_wins` only. Host-side `KeyResult::Action`
  matching in `events.rs` is gone — the focused-float key path is
  now just `ui.handle_key(...)` + `apply_lua_ops()`.
- **B.rename · `BackgroundAsk` → `EngineAsk`.** Moved `AuxiliaryTask`
  from `engine` to `protocol` (single source of truth; `engine` and
  `tui::config` now re-export). Renamed `UiCommand::BackgroundAsk` →
  `UiCommand::EngineAsk`, `EngineEvent::BackgroundAskResponse` →
  `EngineAskResponse`, `AppOp::BackgroundAsk` → `AppOp::EngineAsk`.
  Replaced `task: Option<String>` with a typed
  `task: AuxiliaryTask` (serde-default `Btw`); deleted the silent
  `_ => AuxiliaryTask::Btw` fallback in the engine — unknown task
  strings from Lua now error explicitly.

Net: ~−250 LOC and one uniform dispatch path for every float window
in the app.


### Phase C — Rendering: kill Screen (2–3 commits)

**C1 · Notification + queued + stash + status metadata → compositor.**
Single commit. These share the notification / status routing surface;
splitting leaves parallel paths.
- Notification becomes a compositor float (`focusable=false`, ephemeral).
- Queued messages become a compositor float above the prompt.
- Stash indicator becomes a compositor float / single-row layer.
- Status bar segments read directly from App fields. Delete
  `Screen::{show_tokens, show_cost, show_slug, show_thinking, show_tps,
  task_label, custom_status_items, model_label, reasoning_effort,
  context_tokens, context_window, session_cost_usd, notification,
  running_procs, running_agents, pending_dialog, has_scrollback}` —
  move each to App or delete if already duplicated.

**C2 · Cmdline as float window.** Self-contained commit. Cmdline
becomes a single-panel `Input` dialog, focusable, `DockBottom`.

**C3 · Prompt + Transcript as real `ui::Window`s + Screen deletion.**
The big atomic commit:
- `InputState` holds `win_id: WinId`; the `Window` lives in `ui.wins`.
  All 122 sites of `self.input.win.{cpos, edit_buf, win_cursor,
  kill_ring}` migrate to `self.ui.win_mut(self.input.win_id).unwrap().
  <field>`.
- `transcript_window` likewise becomes a `WinId` pointing into
  `ui.wins`.
- Route prompt keys through `ui.handle_key()` when focused (uses
  `Callbacks` registry populated at startup — typing keys, submit,
  Ctrl+S stash, etc.).
- Delete `render::Screen` entirely. `BlockHistory`, `StreamParser`,
  `TranscriptProjection`, `WorkingState`, `CursorOwner`, `layout` move
  to App.
- Delete `dirty` flag + `needs_draw` / `mark_dirty` / `mark_clean`.
  Compositor grid-diff is the sole change-detection mechanism.
- Delete `last_viewport_text` / `last_viewport_lines` /
  `last_transcript_viewport` / `transcript_gutters` caches (read from
  live Window state).

Splitting this is counterproductive — the Screen struct is the glue
holding all these fields together, and every partial extraction leaves
a shrunken-but-still-alive Screen with progressively weirder invariants.
One atomic move.

Expected LOC: net −800..−1200 lines.

### Phase D — Lua FFI unification (1 commit)

**D1 · PanelSpec + widgets exposed to Lua, plugins rewritten,
`lua_dialog.rs` deleted, `LuaTask` suspension reshaped.** Atomic
because partial exposure (e.g. expose PanelSpec but keep the old
schema parser) leaves two APIs for the same thing.

Inside:
1. Userdata constructors: `ui.panel_content(buf, height)`,
   `ui.panel_list(buf, height)`, `ui.panel_widget(widget, height)`,
   `ui.option_list(options)`, `ui.text_input(opts)`. Builder methods
   for focusable/separator/pad_left/collapse_when_empty. `PanelHeight`
   variants exposed verbatim.
2. Rewrite `plan_mode.lua` on `ui.dialog_open(float_cfg, dialog_cfg,
   panels)` with callbacks pushing `AppOp::{ApproveTool, DenyTool}`
   via `smelt.api.agent.approve/deny` wrappers.
3. Delete `crates/tui/src/app/dialogs/lua_dialog.rs` (~360 lines).
4. Delete `smelt.api.dialog.open` schema parser and
   `{action, option_index, inputs}` result table.
5. Reshape `LuaTask::OpenDialog` → `TaskWait::AwaitEvent { win, event }`
   returning `Payload`. Lua side: `ui.await_event(win, "submit"):
   as_selection()`.
6. `btw.lua` verified unchanged (already on primitives).

Expected LOC: net −600..−800 lines.

### Phase E — UX polish (3 small commits, can each stand alone)

**E1 · `Placement::FitContent { max: HalfScreen | FullScreen }`** +
Rewind/Resume/Permissions migrations. Fix Fill-vs-Fit in
`resolve_panel_rects` so List panels with `Fit` scroll internally past
the cap. (B4)

**E2 · `Compositor::hit_test` + mouse routing to focused float +
scrollbar click-drag.** (B6 + B7) Single commit — mouse routing
enables both.

**E3 · TextInput as the blessed input widget** — Confirm's reason,
Resume's filter, Agents' search all use the same `TextInput` with
identical cursor / vim / keymap behavior. (B8)

---

**Total estimated net change across all phases**: **−2500..−3500
lines** after Phase A's +200–300.

**Atomic commit boundary = no intermediary scaffolding exists**. At no
point between commits does the tree contain parallel systems with "one
being migrated". Every commit closes a chapter.

## Open UX bugs (fall out of the above)

- **B4 — dialog height convention.** Rewind uses `Fixed(14 max)`, Resume
  uses `Pct(60)`, neither reflects content. Introduce
  `Placement::FitContent { max: HalfScreen | FullScreen }`. Fix
  Fill-vs-Fit in `resolve_panel_rects` so a List panel with
  `PanelHeight::Fit` scrolls internally past the cap instead of being
  over-allocated rows. Rewind/Resume → `FitContent { max: HalfScreen }`;
  Permissions → `FitContent { max: FullScreen }`. Falls out of step 5.
- **B5 — transcript status under float.** Status row disappears when a
  float docks bottom; should layer above the float's gutter. Fixes
  itself once the status bar is a top-level compositor layer rather than
  painted-by-Screen.
- **B6 — mouse wheel routing to floats** (task #7). Currently scrolls
  transcript even when a float is focused. Add `Compositor::hit_test
  (col, row) -> Option<WinId>` and route wheel events to topmost layer.
- **B7 — scrollbar click-drag** (task #14). Visual only today; hook up
  drag in the compositor's mouse handler. Ties into B6 — mouse-routing-
  to-float lands first.
- **(dead code)** Legacy cleanup sweep — every `#[allow(dead_code)]` is
  either (a) abandoned migration, (b) legitimate seam with TODO, or (c)
  obsolete and deletable. Each commit audits as it goes.

## Design principles

### Two primitives: Buffer and Window

The entire UI model rests on two concepts, same as Neovim.

**Buffer** = content + metadata. Lines, highlights, decorations, marks,
virtual text, modifiable flag. Buffers know nothing about display —
they're just data.

**Window** = viewport into a buffer. Cursor, scroll, selection, vim
state, keybindings, mouse handling. The transcript window and prompt
window get the same vim motions, selection, yank, mouse handling,
scroll — that's all window behavior. Only difference: the buffer's
`modifiable` flag (gates insert mode and text mutations).

No separate "transcript navigation state" or "prompt surface state."
Just windows looking at buffers.

### Window vs Component

- **`ui::Component`** — anything that draws into a grid. Gets a rect,
  paints into a `GridSlice`. Handles keys (returns `KeyResult`).
  Components are what the compositor stacks as layers.
- **`ui::Window`** — buffer viewer. Owns `BufId`, cursor, scroll offset,
  vim state, selection, kill ring. A Component wraps a Window to paint
  its buffer.

**Not every Component needs a Window.** Statusline, bars, separators are
pure decoration.

| Question | Window or Component? |
|---|---|
| User puts cursor in it? | Window |
| User selects text + copies? | Window |
| Pure decoration (bars, labels)? | Component |
| Fixed-height, app-driven? | Component |

**Focusable flag.** A `Window` can opt out of the focus cycle via
`focusable: bool` on its float config. Modeled on Neovim's
`WinConfig.focusable`: `<C-w>w` skips non-focusable floats, cursor
can't roam into them. Splits are always focusable.

**Chrome split.**

| Surface | Kind | Focusable? |
|---|---|---|
| Transcript | Window (split) | yes |
| Prompt input | Window (split) | yes |
| Normal dialogs (resume, agents, rewind, etc.) | Window (float) | yes |
| Confirm / Question dialogs | Window (float) | yes |
| Completer (fuzzy finder) | Window (float) | **no** — matches nvim-cmp |
| Notification | Window (float) | **no** |
| Queued messages | Window (float) | **no** |
| Top / bottom prompt bars | Component | n/a |
| Status bar | Component | n/a |
| Stash indicator | Component | n/a |

### Layout stack vs focus graph

- **Layout** — where a surface is on screen. All windows *and* components
  participate.
- **Focus graph** — which windows `<C-w>` cycles through. Only focusable
  windows.

A window can be in the layout without being in the focus graph
(notification, completer). A component is always in the layout, never
in the focus graph.

### Dialogs are stacks of panels, panels are windows

A dialog is a compositor float containing a vertical stack of panels.
Every panel is a real `ui::Window` backed by a `ui::Buffer`. Cursor,
scroll, vim, selection, kill ring, mouse routing — all for free.

```
────────────────────────────────────────   ← top rule (─, accent)
 edit_file: src/foo.rs                      ← title (Content, Fixed)
  making a small diff                       ← summary (Content, Fit)
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌   ← dashed ╌ separator
   12  │ fn foo() {                         ← preview (Content, Fill)
   13- │     old_line();                      vim + selection + scrollbar
   13+ │     new_line();                    
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
 Allow edits?                               ← action prompt
  1. Yes                                    ← options (List, Fit)
  2. Yes + always                            mouse click, wheel scroll
  3. No                                      LineDecoration for selection
  type message here…                        ← msg input (Input, Fit)
 ENTER confirm · m add msg · j/k · ESC      ← hints (StatusBar, Fixed 1)
```

**Chrome** (drawn by `Dialog`, not panels):
- Top rule: accent-colored `─` across the rect.
- Dashed `╌` separators between panels (per-panel config).
- Hints row: `StatusBar` component at bottom.
- Background: solid fill across dialog rect.
- No side or bottom edges. Terminal's bottom rows *become* the dialog.

**Placement** on `FloatConfig`:
```rust
pub enum Placement {
    DockBottom { above_rows: u16, full_width: bool },
    Centered { width: Constraint, height: Constraint },
    AnchorCursor { width, height },
    Manual { anchor, row, col, width, height },
}
```
Built-in dialogs default to `DockBottom { above_rows: 1, full_width: true }`.
Completer uses `AnchorCursor`. Lua floats default to `Centered`.

### Panels

```rust
pub struct DialogPanel {
    pub win: WinId,
    pub kind: PanelKind,
    pub height: PanelHeight,
    pub separator_above: Option<SeparatorStyle>,
}

pub enum PanelKind {
    Content,                         // readonly, vim + select + scroll
    List { multi: bool },            // cursor line = selected
    Input { multiline: bool },       // editable
}

pub enum PanelHeight { Fixed(u16), Fit, Fill }

pub enum PanelContent {
    Buffer(BufId),                   // 6+ migrated dialogs use this
    Widget(Box<dyn PanelWidget>),    // TextInput, OptionList, TabBar
}
```

**Selection rendering**: List panel reads its window's cursor line and
applies `LineDecoration::fill_bg` (same mechanism as the transcript).
**Scrolling**: each panel draws its own scrollbar from `WindowViewport`
/ `ScrollbarState`. **Focus**: `Dialog` owns focused panel index; Tab
cycles forward, Shift-Tab back; mouse click focuses hit panel.

| Dialog | Panels (top → bottom) |
|---|---|
| help | title, keybinding-list |
| rewind | title, turn-list |
| export | title, options-list |
| resume | title, search-input, session-list |
| permissions | title, entries-list (section headers via LineDecoration) |
| ps | title, process-list |
| agents list | title, agent-list |
| agents detail | title, prompt-content, tool-calls-content |
| confirm (bash) | title+body-content, options-list, msg-input (when used) |
| confirm (edit) | title, summary, preview-content, action, options-list, msg-input |
| question | title, question-text, options-list, msg-input |
| completer | suggestion-list |
| cmdline | prompt-input |
| notification | text-content |

### Shared rendering for diffs and code

Diff `-`/`+` coloring, bash syntax, search-match highlights all project
into `ui::Buffer` as `Span` / `SpanStyle` + per-line `LineDecoration`.
Same rendering the transcript uses. Lua plugins get the same highlight
API.

**The pipeline exists.** Preview renderers (`print_inline_diff`,
`render_notebook_preview`, `print_syntax_file`,
`BashHighlighter::print_line`, `render_markdown_inner`) all write to a
`LayoutSink` trait. `SpanCollector: LayoutSink` accumulates into a
`DisplayBlock`. `render/to_buffer.rs::render_into_buffer` projects that
into a `ui::Buffer`.

Confirm preview migration was **not** a renderer rewrite — just:
1. Create `SpanCollector`.
2. Call existing renderer against it.
3. Project into a fresh `ui::Buffer`.
4. Plant the buffer, open dialog with `PanelSpec::content(preview_buf,
   PanelHeight::Fill)`.

### Widgets inside panels

Custom UX (tabs, multi-select with chord keys, preview + reason textarea)
uses `PanelContent::Widget`:

```rust
pub trait PanelWidget {
    fn prepare(&mut self, rect: Rect, ctx: &DrawContext) {}
    fn draw(&self, rect: Rect, grid: &mut GridSlice<'_>, ctx: &DrawContext);
    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult;
    fn cursor(&self) -> Option<(u16, u16)> { None }
    fn as_any_mut(&mut self) -> &mut dyn Any;
}
```

**Shipped widgets** (`crates/ui/src/widgets/`):
- `TextInput` — wraps `Window` + `Buffer`, reuses kill ring / vim / undo.
- `OptionList` — single- or multi-select; checkbox glyph; chord-key map.
- `TabBar` — single-row label strip.

Previews are **not** widgets — they're buffer-backed `Content` panels
populated once at open time via the legacy renderer → `SpanCollector` →
`DisplayBlock` → `Buffer` pipeline.

### Escape hatch: bare Component floats

`Ui::add_float_component(rect, zindex, Box<dyn Component>)` registers a
raw compositor layer — no chrome, no panels. For image viewers,
mini-editors, dataviz. Most floats still use `Dialog` + widgets.

### LuaTask runtime — one suspend mechanism

Every "imperative-wants-to-wait" Lua API has the same root need: suspend
the plugin, drive Rust-side work, resume with a result.

**Design: coroutine-driven tasks via `mlua::Thread`.** Handler runs
inside a Thread. Yielding API yields a typed request, Rust handles it,
Rust resumes with the answer. Plugin code reads synchronously.

After step 8 of the arc:
```rust
pub enum LuaYield {
    AwaitEvent { win: WinId, event: WinEvent },  // returns Payload
    Sleep(Duration),
    EngineAsk(AskRequest),   // future
}
```

`LuaTaskRuntime::drive` runs each frame: resumes tasks whose waits are
satisfied, collects new yields, dispatches, re-parks.

Lua side:
```lua
smelt.task(function()
  local win = ui.dialog_open(float_cfg, dialog_cfg, {
    ui.panel_list(list_buf, "fill"),
  })
  local idx = ui.await_event(win, "submit"):as_selection()
  ui.win_close(win)
  -- act on idx
end)
```

Yielding from a non-task context is a Lua error with a clear message.

**What collapses:**
- `PendingOp::ResolveToolResult`, the `callbacks: HashMap<u64, LuaHandle>`
  registry, and `smelt.api.tools.resolve` — deleted.
- Declarative `confirm = {...}` block — deleted.
- `smelt.defer(ms, fn)` / `smelt.api.engine.ask(..., on_response=fn)`
  kept as thin wrappers; ID-plumbing disappears internally.

### Theme access from Lua

Plugins must not hardcode ansi. `smelt.api.theme` exposes read/write for
every role, snapshot for paint-pass consistency, and terminal brightness
detection. Color shape: `{ ansi = u8 } | { rgb = {r,g,b} } | { preset =
"sage" }`.

## Why not ratatui

- **Immediate mode vs retained.** Ratatui rebuilds every frame. We want
  grid diffing.
- **No windows.** No persistent viewports with cursor, scroll, focus.
- **No z-order.** Composites by render order only.
- **Abstraction clash.** Ratatui's `Buffer` = cell grid. Our `Buffer` =
  content model.

What we take: the cell grid concept as intermediate rendering surface.

## Why a separate crate

- Forces clean boundaries. Can't import `protocol::Message` in `ui`.
- Testable in isolation.
- Reusable — general TUI toolkit.
- Makes API surface explicit. `pub` items in `ui` *are* the API.

## Core architecture

### Cell Grid

2D array of `Cell { symbol, style }`. Components never emit escape
sequences — they write cells to a grid region. `GridSlice` is the
borrowed rectangular view.

### Component

- `draw()` — writes cells into grid slice.
- `handle_key()` — returns `Consumed | Ignored | Action(String)`. Arc
  step 3 retires `Action(String)` in favor of callbacks.
- `cursor()` — returns `Option<CursorInfo>` (position + optional
  `CursorStyle { glyph, style }`).

### Compositor (inside Ui)

Manages the component tree, orchestrates rendering, diffs frames. Each
frame: resolve layout → draw → diff grids → emit SGR. `tui` never
touches compositor directly — calls `ui.render()`, `ui.handle_key()`,
`ui.handle_mouse()`, `ui.win_open_float()`.

**Event routing is z-ordered.** `handle_key` walks focused → parent →
global. `handle_mouse` hit-tests top-down. Wheel over a float scrolls
the float.

### Buffer

Lines + highlights + marks + virtual text + per-line decoration +
modifiable flag. Per-line `LineDecoration` supports gutter backgrounds,
fill backgrounds, soft-wrap markers. Most buffers don't use it; transcript
and diff previews do.

### Window

Viewport into a buffer. Owns:
- **Cursor** — position, curswant.
- **Scroll** — top_row, pinned flag.
- **Selection** — anchor, visual mode. Painted generically by the
  window's own draw path.
- **Vim state** — mode, operator pending.
- **Kill ring** — per-window yank history.
- **Keybindings** — via callbacks registry.
- **Tail follow** — `tail_follow: bool`. Generic; transcript sets true
  by default.
- **Modifiable** — mirrors buffer; gates insert mode.

Both transcript and prompt are windows.

## Canonical ownership

| Concern | Owner |
|---|---|
| Transcript content | `ui::Buffer` (projected at event time) |
| Transcript cursor/scroll/selection/vim | `ui::Window` |
| Transcript tail-follow | `ui::Window::tail_follow` |
| Prompt editable text | `ui::Buffer` |
| Prompt cursor/scroll/selection/vim | `ui::Window` |
| Buffer modifiability | `ui::Buffer::modifiable` mirrored on Window |
| Selection rendering | `ui::Window` / `WindowView::draw` |
| Dialog content | `ui::Buffer` per panel or widget-owned state |
| Dialog semantic state | `App.float_states` (arc: → closures via Callbacks) |
| Dialog rendering/layout | `ui::Dialog` component + `Placement` config |
| Dialog background | `ui::Dialog::draw` (solid fill) |
| Mouse z-order | `Compositor::handle_mouse` |
| Completer | Float Window + Dialog, `focusable=false`, AnchorCursor |
| Notifications | Float Window, `focusable=false`, ephemeral |
| Status bar | `StatusBar` component (segments set at event time) |
| Cmdline | Float Window (single-panel Input, focusable) |
| Block history + layout cache | `tui::BlockHistory` (projects into buffer) |

## `ui` crate public API

```rust
// Buffer
ui.buf_create(opts) -> BufId
ui.buf_delete(buf)
ui.buf_set_lines(buf, start, end, lines)
ui.buf_get_lines / buf_line_count
ui.buf_set_virtual_text / buf_set_mark

// Window
ui.win_open_split(buf, config) -> WinId
ui.win_open_float(buf, config) -> WinId
ui.win_close(win)
ui.win_set_config / win_set_cursor / win_set_scroll
ui.win_list / win_get_current / win_set_current

// Dialog (panels + chrome)
ui.dialog_open(float_cfg, dialog_cfg, panels) -> Option<WinId>
ui.dialog_mut(win) -> Option<&mut Dialog>

// Callbacks (arc step 3 makes this the only behavior mechanism)
ui.win_set_keymap(win, key, Callback)
ui.win_on_event(win, event, Callback)
ui.clear_callbacks(win)

// Highlight / layout / rendering
ui.hl_buf_add / hl_buf_clear
ui.layout_set / layout_resize
ui.render<W: Write>(w)
ui.handle_key(key, mods) -> KeyResult
ui.handle_mouse(event) -> bool
ui.focused_float() -> Option<WinId>
ui.dispatch_event(win, event, payload, lua_invoke)
```

# Completed work

Historical phases preserved for context on why things are shaped the way
they are. Detailed narrative in commit history.

## Phase 0–2: Foundation

Core types, text primitives, layout engine. `crates/ui/` with `BufId`,
`WinId`, `Buffer`, `Window`, `Ui`. `EditBuffer`, `Vim`, `KillRing`,
`Cursor`, `Undo`. `LayoutTree`, constraint solver, float resolution.
Buffer highlights: `Span`, `SpanStyle`, per-line styled content.

## Phase 3–5: Grid + Components + FloatDialog

`Grid`, `Cell`, `Style`, `GridSlice`. `flush_diff()` SGR emission.
`Component` trait (no dirty flags — compositor always draws all layers).
`Compositor`. `BufferView`, `ListSelect` (retired), `TextInput` (retired),
`StatusBar`, `FloatDialog` (rewritten as `Dialog`).

## Phase 6: Buffer/window rendering model

Goal: windows pull from buffers; app updates buffers at event time; render
loop is just `compositor.render()`.

Sub-steps (all done except where noted):
- **6a** — btw removed from Screen ✅
- **6b** — Compositor merged into Ui ✅. `win_open_float()` creates
  window AND compositor layer.
- **6c** — Lua ops wired to Ui ✅. PendingOps are `BufCreate`,
  `BufSetLines`, `WinOpenFloat`, `WinClose`, `WinUpdate`.
- **6d** — Action dispatch ✅. Float keys route through
  `handle_float_action()`.
- **6f** — Real compositor layers ✅. Transcript, prompt, status bar
  are compositor layers (not borrowed "base" components).
- **6g** — Generic cursor overlay ✅. `Component::cursor()` returns
  `Option<CursorInfo>`.
- **6g.1** — Shared viewport + selection state ✅.
- **6g.2** — Shared `WindowView` ✅.
- **6h** — Eliminate nav text ✅. All Window coords in display-text
  space.
- **6i** — Prompt rendering through Buffer ✅.
- **6j** — Unified WindowView ✅.

## Phase 9: Seam elimination (most of it done)

Step 9 merged the previous "migrate dialogs" + "delete legacy" into one
coherent arc because splitting them left two render engines coexisting.

- **9.1** — New `Dialog` + `DialogPanel` framework ✅. Retired
  `FloatDialog`, `ListSelect`, `TextInput`,
  `FloatDialogConfig::{hint_left, hint_right, footer_height}`.
- **9.2** — `Placement` on `FloatConfig` ✅.
- **9.3** — `Compositor::handle_mouse` with z-order hit-testing —
  pending (task #7, ties into B6/B7).
- **9.4** — Unified keymap/event behavior model ✅. DialogState trait +
  `float_states` HashMap landed; callback registry landed but unused in
  production. Arc step 3 retires DialogState in favor of the callback
  registry.
- **9.5** — Migrate final three dialogs (Confirm, Question, Agents) to
  unified model ✅.
- **9.5b** — Implementation order for Confirm/Question migration ✅.
  Foundations: `blocks_agent()`, `focusable: bool`, `PanelContent` +
  `PanelWidget`. Widgets: `TextInput`, `OptionList`. Projection helper:
  `render_into_buffer`. Item 11a (LuaTask runtime + theme API + drop
  `plugin_confirm`): **(i)** LuaTask runtime ✅, **(ii)** Theme API ✅,
  **(iii)** Tool execute as task ✅, **(iv)** `smelt.api.dialog.open`
  yield ✅, **(v)** Delete `plugin_confirm` + migrate `plan_mode` —
  **in progress**, superseded by arc step 4.
- **9.6** — Migrate overlays (completer, cmdline, notification, queued)
  to dialogs — pending (task #9).
- **9.7** — Delete legacy rendering — partially complete. `trait Dialog`,
  `DialogResult`, `active_dialog`, `Frame`, `RenderOut`, `paint_line`
  all gone. Remaining: `Screen` struct itself.
- **9.8** — Bug fixes on unified path — ongoing.
- **9.9** — `tail_follow` as `ui::Window` property — pending (task #13).
- **9.10** — Delete `Screen` — pending (task #15).

## Phases 7 & 8 (upcoming)

- **Phase 7** — Event dispatch generalization (keymap scopes beyond
  window-local, vim operator-pending state machine beyond what 9.3
  lands).
- **Phase 8** — Complete `smelt.api.buf/win/ui` surface. After arc steps
  5–8 land, this is mostly API polish — the primitive set is already
  1:1 with Rust.

## Non-goals

- **Using ratatui.** Abstraction mismatch too large.
- **Plugin registry.** Lua scripts in `~/.config/smelt/`.
- **Remote UI protocol.** Local terminal only.
- **Async Lua.** Sync-only; coroutine tasks handle "wait" cases.
- **Full nvim compatibility.** Borrow the model, not the API.
- **Immediate mode.** Retained with grid diffing.

## Progress log

- **2026-04-22** — Phase A landed: `lua::PendingOp` → `app::ops::AppOp`
  (new module). ~60 callsites renamed; `LuaOps.ops: Vec<AppOp>` +
  `App::apply_ops(Vec<AppOp>)` stay as the one drain/reducer pair.
  Build/clippy/tests green.
- **2026-04-22** — task #11 commit 2 landed: prompt input Buffer now
  persistent in `ui.bufs` with stable BufId owned by App. Tests green.
- **2026-04-22** — task #11 commit 1 landed: dead `PromptState` fields
  + `move_cursor_past_prompt` + `erase_prompt` deleted. All
  `erase_prompt()` callsites swapped to `mark_dirty()`. Tests green.
- **2026-04-22** — decision locked: typed `AppOp` effect enum is the
  unified Rust+Lua callback channel. Arc is now 9 steps starting with
  op-enum design.
- **2026-04-22** — plan reorganized around FFI-unification arc.
- **2026-04-22** — keystone phase 1+2 complete (−3147 lines across 4
  commits).
- **2026-04-21** — global chord layer landed (Shift+Tab, Ctrl+T, Ctrl+L).
- **2026-04-21** — status bar source-of-truth fix (B1/B2 resolved).
- **2026-04-21** — Confirm migration + legacy dialog infra deletion.
- **2026-04-21** — Agents dialog split into two DialogStates.
- Earlier — see git history for Phase 0–6 landing.
