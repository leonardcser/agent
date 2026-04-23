# TUI Architecture — UI Framework Rewrite

Living document. Reorganized 2026-04-22 to fold in the FFI-unification
direction. Historical narrative preserved below "Completed work".

## North star

Three commitments drive every remaining change:

1. **One rendering path.** The compositor (`ui::Ui`) owns every pixel. No
   parallel ANSI-emitting layer, no cached "last seen" state inside `app`,
   no dirty flags outside the grid diff. Everything visible is a window
   registered with the compositor.

2. **FFI = internal API.** Lua plugins call the same functions Rust uses.
   `smelt.api.ui.*` is a thin userdata wrapper over `ui::Ui` — no translation
   layer, no schema parsing, no stringly-typed action tokens. Behavior is
   expressed as callbacks (`ui::Callback`), not as strings the host matches
   on.

3. **Rust core, Lua features.** Rust owns the pixel-pushing layer
   (compositor, buffers, windows, widgets, rendering) and the
   security-critical tools (bash, read, write, edit, glob, grep,
   session/agent lifecycle). Lua plugins own the *what*: which
   dialogs exist, which tools exist, which slash commands exist, and
   how their panels are composed. Same model as Neovim — minimal C
   core, everything user-facing is a plugin. A feature living in
   Rust that could live in Lua is a bug.

Everything below follows from these three.

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

- **Session boundaries are not scope boundaries.** Don't stop work or
  advise the user to "pick up next session" because a task feels big.
  If the refactor is multi-hour, break it down internally and keep
  going, committing meaningful green checkpoints. The only reasons to
  stop are (a) the user asks you to, or (b) a hard blocker that
  requires their input. "Needs more time" is not a blocker.

- **Format, lint, test at the end of each logical commit.**
  `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings &&
  cargo nextest run --workspace`. (Use `cargo-nextest` — it parallelises
  across cores and runs the full suite in seconds, vs. `cargo test`'s
  serial per-crate execution.) Update the plan, then commit.

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

Smelt is a full-screen TUI; `cargo nextest run` only covers unit-testable logic.
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
- `InputState.win: ui::Window` isn't a `WinId` into `ui.wins` yet —
  122 callsites still read `self.input.win.{cpos, edit_buf, …}`
  directly instead of through `ui.win_mut(id)`. Worth doing; no
  urgency — prompt already paints through the compositor, the inline
  `Window` just duplicates ownership.
- Notification / queued / stash layers are still painted as prompt
  chrome rows (`render/prompt_data.rs:69-82`), not compositor floats.
  Becomes worthwhile once a Lua plugin wants to own a notification.
- `render::Screen` struct is gone (commit `6cd800a`). `render/screen.rs`
  persists as 45 lines of standalone types (`TranscriptData`,
  `Notification`, `ContentVisualRange` etc.) — fine to leave, can
  fold into `app/transcript.rs` later.
- `Frame`, `TerminalBackend`, `StdioBackend`, `FramePrompt` deleted
  (commit `ef36101`).

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


### Phase B.cleanup — Consolidate seams exposed by Phase B

Phase B landed the unification but Phase B's *transition mechanisms*
left residue. Each sub-commit here deletes scaffolding or untangles
a seam. In order of smallest-impact-first:

- **B.cleanup.1 · `blocks_agent` on `FloatConfig`.** `App::blocking_wins:
  HashSet<WinId>` is runtime state that belongs on the float's config.
  Move to `FloatConfig.blocks_agent: bool`; derive
  `focused_float_blocks_agent` by looking up the focused float's
  config. Kills the per-dialog `blocking_wins.insert(win_id)` call
  and the matching `close_float` removal.
- **B.cleanup.2 · Confirm BackTab as keymap callback.** Delete
  `handle_confirm_backtab` and the early BackTab branch in
  `handle_event` that routes to it. Register BackTab directly on the
  Confirm dialog window via `win_set_keymap`; emit a new
  `AppOp::ToggleModeAndMaybeApprove { request_id, call_id, tool_name,
  args }` so the mode-check + approve-or-keep-open logic moves back
  into the reducer.
- **B.cleanup.3 · Agents list↔detail navigation.** Replace the
  `CloseFloat` + `RefreshAgentCounts` + `OpenAgentsList/Detail`
  three-op ping-pong with a single `AppOp::SwitchToAgentsList {
  selected }` / `SwitchToAgentsDetail { agent_id, parent_selected }`.
  Reducer owns the close-before-open sequence.
- **B.cleanup.4 · Fold `TurnState` onto `App`.** Move `agent:
  Option<TurnState>` from the `run()` local onto `App.agent`. Delete
  `pending_agent_cancel` and `pending_agent_clear_pending` bool
  flags plus the main-loop drain block. `apply_ops` mutates
  `self.agent` directly. Thread everywhere `agent: &mut
  Option<TurnState>` was a function argument.
- **B.cleanup.5 · Split `AppOp` into `DomainOp` + `UiOp` (shipped
  2026-04-23).** `AppOp` became a 2-variant wrapper `AppOp::{Ui,
  Domain}` carrying the two new enums. `UiOp` holds the pure
  compositor/buffer/window primitives + ephemeral UI chrome (Notify,
  NotifyError, CloseFloat, SetGhostText/ClearGhostText, BufCreate/
  BufSetLines/BufAddHighlight, WinOpenFloat/WinUpdate/WinClose).
  `DomainOp` holds app-state mutations, engine commands, session/
  agent/permission/process control, tool resolution. Reducer split
  into `apply_ui_op` + `apply_domain_op` so the dispatch reads like
  the partition. `OpsHandle::push<O: Into<AppOp>>` + `LuaOps::push
  <O: Into<AppOp>>` accept `UiOp` / `DomainOp` directly so call sites
  don't need explicit wrapping. New variants now declare intent at
  the type level — handlers decide which bucket they belong to.
- **B.cleanup.6 · `OpsHandle` rename + decouple from `LuaShared`.**
  `OpsHandle` wraps `Arc<LuaShared>` but nothing about it is
  Lua-specific. Move the op channel to its own `Arc<Mutex<OpQueue>>`;
  give Rust callbacks and the Lua runtime independent handles.
  Rename to `OpSender` / `OpReceiver`.
- **B.cleanup.7 · Unify Lua callback storage.** `LuaShared.callbacks:
  HashMap<u64, LuaHandle>` accessed via `fire_callback` is used by
  exactly one consumer now: `EngineAskResponse`. Either fold the
  continuation into an entry in `ui::Callbacks` keyed by a synthetic
  `WinId`, or replace the `u64` keyspace with something narrower.
  Goal: one Lua-callback surface, not two parallel ones.
- **B.cleanup.8 · Typed widget events (deferred).** Replace the
  `KeyResult::Action(String)` protocol with a typed `WidgetEvent`
  enum returned from `Component::handle_key`. Widgets stop
  formatting `"select:N"` / `"submit:T"`; `classify_widget_action`
  deletes itself. Larger touch; rebuild across every widget. Tracked
  here; execute after Phase C is on the table so the ui crate's
  public surface settles once.


### Phase C — Rendering: kill Screen (2–3 commits)

**C1 · Status metadata → App.** Single data-ownership commit. The
original plan bundled notification/queued/stash float conversion with
the status-field move, but an audit showed they're orthogonal:
queued-messages and stash aren't Screen fields (already on
App/InputState, passed into `compute_prompt` as params), and
notification-as-float requires prompt-relative placement — which
presupposes the prompt is a `ui::Window`. That's C3's job. So C1 is
just the data move.

- Move to App: `model_label`, `reasoning_effort`, `show_tokens`,
  `show_cost`, `show_slug`, `show_thinking`, `show_tps`,
  `context_tokens`, `context_window`, `session_cost_usd`, `task_label`,
  `custom_status_items`, `pending_dialog`, `running_procs`,
  `running_agents`, `has_scrollback`, `notification` (owner; rendering
  stays put).
- `BarInfo` construction reads from App fields directly.
- Delete the matching Screen setters/getters.
- Notification/queued/stash rendering paths unchanged — `PromptInput`
  still carries them by reference.

**C1.follow · Notification / queued / stash → compositor floats**
is folded into **C3** (see below) where prompt becomes a `ui::Window`
and floats can anchor relative to its rect.

**C2 · Cmdline as reusable `Cmdline` component.** Revised shape: the
cmdline stops being inline statusline-mode paint and becomes its own
**named component** — a focusable single-line text float. Mirrors
Neovim's pattern where the cmdline is a distinct entity, and its
completion popup is a separate non-focusable `Picker` anchored above
(our `pum` equivalent).

- Add `crates/ui/src/cmdline.rs` — `Cmdline` component implementing
  `Component`, wrapping a single-row Window + Buffer. Renders `:` prefix
  + inverse-video cursor cell. `Ui::cmdline_open(FloatConfig) -> WinId`,
  `cmdline_close(win)`.
- `App.cmdline` stops being a raw `CmdlineState`; becomes
  `Option<WinId>` into the open Cmdline component.
- Completion: when typing a `:` command, the Cmdline opens a `Picker`
  anchored just above itself with filtered results. Same Picker used by
  the prompt's `/` completer — one primitive, two callers, matching
  Neovim's `wildoptions+=pum`.
- Status bar returns to normal content while cmdline is active (the
  cmdline is a peer float, not a status-row overlay).

**C3 · Prompt + Transcript as real `ui::Window`s + Screen deletion.**
Original framing was "one atomic commit," but session-boundary
limits make this unworkable — and the practical experience shows
incremental green checkpoints shrink Screen without weird invariants.
Ongoing as a series of mechanical, test-green commits:

- C3 prep (done, 9dde0a9) — thread `show_thinking` as param, delete
  Screen mirror.
- C3 step 2 (done, 8b45af9) — delete `CursorOwner` enum,
  `last_app_focus`, `focused` mirror; compute inline.
- C3 step 3 (done, fb94530) — move `WorkingState` off Screen onto
  App.
- C3 step 4 (done, 35d01d9) — move `transcript_gutters` to App,
  inline `TRANSCRIPT_GUTTERS` const.
- C3 step 5 (done, 0976e75) — move `LayoutState` to App, refresh
  per-frame (fixes pre-existing staleness bug).
- C3 step 6 (done, e02c9f1) — move `PromptState` fields to App,
  delete `render::prompt` module.
- C3 step 7 (done, 0a03125) — route terminal size through
  `ui.terminal_size()`, delete `Screen::backend()` getter.
- C3 step 8 (done, 1ed4421) — move `last_transcript_viewport` to
  App; return via `TranscriptData`; thread viewport into
  `compute_transcript_cursor`.
- C3 step 9 (done, e31aba2) — delete unused `last_viewport_lines`
  field and `display_lines` projection.

**Migrate, don't wrap.** Each step deletes more than it adds. This
principle drove the keystone migration to completion.

**Shipped:**
- **✓ Delete `dirty` flag entirely.** Change detection is now
  `TranscriptProjection.generation` (work) + `ui::Compositor`
  grid-diff (paint). No manual bookkeeping.
- **✓ Migrate `transcript` / `parser` / `transcript_projection` /
  `last_viewport_text` onto App.** ~60 methods moved verbatim from
  `impl Screen` into a new `app/transcript.rs` (commit `6cd800a`,
  net −885 lines).
- **✓ Delete `Screen` struct, `backend: Box<dyn TerminalBackend>`,
  `Screen::clear`.** `Frame` + `TerminalBackend` + `StdioBackend` +
  `FramePrompt` deleted as unused (commit `ef36101`).

**Deferred (tracked, not blocking):**
- **InputState → WinId.** Replace `InputState.win: ui::Window` with
  `win_id: WinId` into `ui.wins`; migrate 122 callsites from
  `self.input.win.{cpos, edit_buf, win_cursor, kill_ring}` to
  `ui.win_mut(self.input.win_id)`. Route prompt keys through
  `ui.handle_key()` with the `Callbacks` registry.
- **Notification / queued-messages / stash → compositor floats**,
  anchored above the prompt window's rect. Becomes worthwhile once
  a Lua plugin wants plugin-owned notifications (Phase F).

**C3.picker · Rework completer onto a reusable `Picker` component.**
The landed Phase C3.completer (task #55) put the completer on a plain
`win_open_float` with hand-rolled paint (fill_bg selection, per-row
highlights, custom column math). Wrong direction: every future
selectable-list caller would need to duplicate that code. Rework:

- Add `crates/ui/src/picker.rs` — `Picker` component implementing
  `Component`. State: `items: Vec<PickerItem { label, description,
  prefix_hint }>`, `selected: usize`. Draw: two-column layout (label,
  right-padded to `max_label+2`, then dim description), accent-fg on
  `selected` row, no border, no background tint beyond theme `bar()`.
  `Ui::picker_open(FloatConfig, items, selected) -> WinId`,
  `picker_set_items(win, items, selected)`.
- Add `Placement::NearAnchor { row, col, auto_flip: true }` — picks
  above vs below based on available rows, like Neovim's
  `pum_compute_vertical_placement`.
- Rewrite `App::sync_completer_float` to drive a `Picker` instead of a
  bare float: map `InputState.completer` → `items` + `selected`,
  anchor at the prompt cursor.
- Delete `App::paint_completer_buffer` (~70 LOC of per-row highlight
  math) and `App.completer_buf` (Picker owns its own buffer).
- `win_open_float` already respects `focusable: false` after the fix
  in C3.completer — Picker inherits that contract.

Unlocks Phase C2 (Cmdline reuses Picker for `:`-completion), Phase F3
(`smelt.api.picker.open` is a one-line wrapper), and the remaining F2
Tier-1 commands (`/model`, `/theme`, `/color`, `/stats`, `/cost`)
which are all picker-based.

Actual delivered: net −982 lines on top of the prior phase-1-and-2
−3147 lines.

### Phase D — Neovim-model FFI completion

**Finding (2026-04-22 audit)**: all five primitives for building a
feature in Lua *already exist*. `smelt.api.tools.register` +
`smelt.api.dialog.open` + coroutine yields (`TaskWait::Dialog`) cover
(a) register a tool the LLM sees, (b) receive the invocation, (c)
open a UI dialog, (d) await the user's reply, (e) return the reply
as the tool's result. `plan_mode.lua`'s `exit_plan_mode` tool is a
working proof.

Phase D now reduces to four focused commits:

**D0 · Split task-runtime events off `AppOp`.** `AppOp::ResolveLuaDialog`
is a leaky abstraction — the reducer shouldn't know that "a compositor
dialog submitted" and "the Lua coroutine that opened it needs resuming"
are the same event. Introduce a task-runtime inbox owned by the Lua
module (`LuaShared.task_inbox: Mutex<Vec<TaskEvent>>`) with variants
like `DialogResolved { dialog_id, action, option_index, inputs,
on_select }` and `KeymapFired { callback_id, win_id, selected_index,
inputs }`. Rust callbacks in `lua_dialog.rs` push `TaskEvent`, *not*
`AppOp`. Main loop calls `lua.pump_task_events()` each tick; the Lua
runtime resolves its own parked tasks and fires its own on_press
callbacks. The reducer loses `ResolveLuaDialog` entirely. Prereq for
the callback-first keymap model (plugins write `on_press =
function(ctx) ... end` instead of dispatching on an action string),
which `/ps` + `/help` + every Tier-2 dialog needs.

**D1 · `execution_mode` on plugin tools.** Add
`ToolExecutionMode::{Concurrent, Sequential}` to `PluginToolDef`. The
engine today treats plugin tools as concurrent; a plugin tool that
opens a dialog and awaits user reply must block the LLM turn the
same way the Rust `ask_user_question` does (it routes through
`sequential_queue` — see `crates/engine/src/agent.rs:1215`). With
this field plumbed through `StartTurn`, the engine routes plugin
tools to the right queue. Unblocks Phase F1.

**D2 · Expose `PanelSpec` + widgets as userdata.** Today
`smelt.api.dialog.open` takes a Lua table and
`crates/tui/src/app/dialogs/lua_dialog.rs` (~290 lines) parses it
into `PanelSpec`s, losing shape along the way. Replace with userdata
constructors — `ui.panel_content(buf, height)`, `ui.option_list(items)`,
`ui.text_input(opts)` — so `dialog.open(float_cfg, dialog_cfg, panels)`
takes `PanelSpec` directly, no translation. Also exposes `WinId` as
Lua userdata and `win.set_keymap(id, key, lua_fn)` accepting Lua
functions (required for D3 below).

**D2a · `Callback::Lua` invocation** (shipped 2026-04-23).
`LuaRuntime::invoke_callback(handle, payload)` looks up the
registered mlua::Function under `shared.callbacks[handle.0]`,
builds a payload table (`{ index = 1-based }`, `{ text = … }`,
`{ code, mods }`, or empty for `Payload::None`), and calls the fn.
Errors are recorded via `record_error`. The two stub `lua_invoke`
closures in `events.rs::close_focused_non_blocking_float` and
`app/mod.rs` tick loop now call this directly. Zero behavior
change today since no Rust code registers `Callback::Lua`, but the
plumbing is the prereq for `smelt.api.win.set_keymap(id, key,
lua_fn)` in the D2b picker-first migration.

**D3 primitives · `ctx.panels` pull-read + `ctx.win`**
(shipped 2026-04-23). Prereq for the dialog port. When a
`Callback::Lua` fires, the ctx passed in now carries
`ctx.win` (the source WinId) plus `ctx.panels` — a live snapshot of
the dialog's panels at dispatch time. Each entry is
`{ kind = "content" | "list" | "input", selected = <1-based | nil>,
text = "…" }`. The snapshot is built Rust-side by
`Ui::snapshot_dialog_panels(win)` using new public accessors
(`Dialog::panel_kind_at`, `selected_index_at`, `panel_widget_selected`,
`panel_widget_text`, `panel_buf_at`) plus two default-impl methods on
`PanelWidget` (`selected_index`, `text_value`) that `OptionList` and
`TextInput` override. Empty slice for non-dialog windows — Lua code
reads `ctx.panels[i]` without special-casing. `lua_invoke`'s signature
grows to `FnMut(LuaHandle, WinId, &Payload, &[PanelSnapshot])`;
aliased as `ui::LuaInvoke` to keep call sites readable.
With this, a Lua runtime file can open a raw dialog, register
Enter/Esc keymaps, and on fire read the current selection + input
text to build the final result table and resume its parked task —
no Rust-side `lua_dialog.rs` glue required. Port + deletion is the
next commit.

**D2b primitives · External task yield + keymap + task-id API**
(shipped 2026-04-23). Additive foundation for Option 3 — no
deletions yet. Shipped:
- `TaskWait::External(u64)` + `Yield::External(u64)` +
  `TaskEvent::ExternalResolved { external_id, value }`, so a Lua
  coroutine can `coroutine.yield({__yield = "external", id = ...})`
  and park cleanly without the runtime knowing what intent it is
  waiting for. `LuaRuntime::pump_task_events` feeds the resume into
  `resolve_external`.
- `smelt.api.task.alloc()` mints an external id;
  `smelt.api.task.resume(id, value)` enqueues the resume from a
  keymap / callback. These are the two primitives every Lua runtime
  file will use.
- `smelt.api.win.set_keymap(win_id, key_str, lua_fn)` pushes
  `UiOp::WinBindLuaKeymap`; the reducer calls
  `ui.win_set_keymap(win, key, Callback::Lua(LuaHandle(id)))`.
  `parse_keybind` handles `"enter"` / `"esc"` / `"tab"` / `"bs"` /
  `"c-j"` / `"s-tab"` / single chars — same shape as the old
  `lua_dialog::parse_key` but returns `Option<KeyBind>`. Covered
  by `parse_keybind_handles_names_and_modifiers`.
With these in place, a runtime file can open a raw float, register
Lua keymaps, and resume its caller coroutine end-to-end without any
per-intent Rust glue. The D2b migration itself (port picker + delete
`lua_picker.rs`) is the next commit.

**D3 · Collapse intent glue into Lua runtime files.** After D2,
**delete `lua_dialog.rs` and `lua_picker.rs` entirely** — no
generic `lua_float.rs` successor, no `TaskDriveOutput::OpenDialog`/
`OpenPicker`, no `Yield::OpenDialog`/`OpenPicker`,
no `TaskWait::Dialog`/`Picker`. The blocking `picker.open` /
`dialog.open` ergonomic stays, but its implementation moves from
Rust glue into Lua runtime files (`runtime/lua/smelt/picker.lua`,
`runtime/lua/smelt/dialog.lua`, …). They open raw floats via the
primitive API, register Lua-function keymaps, and do
`coroutine.resume(co, result)` themselves — the coroutine dance
lives in one place each, shipped with smelt, never seen by plugin
authors.

Decision (2026-04-22): **Option 3 over Option 2.** Option 2
(a generic `lua_float.rs` with a `ResolvableFloat` trait per
component) still grows Rust ~10 LOC per new intent and bakes a
per-intent assumption into the TaskWait / TaskDriveOutput surface.
Option 3 ships the same plugin-author ergonomic
(`local r = picker.open(...)`) with **zero Rust cost per new intent**
— because the coroutine resume lives in the caller's language (Lua)
where it belongs. Matches Neovim's `vim.ui.select` model exactly.
Net deletion: ~800 LOC of Rust glue + ~50 LOC of Lua runtime sugar
added. The current session's `lua_picker.rs` is disposable — kept
until D2 lands, then replaced by `runtime/lua/smelt/picker.lua`
with no plugin-code changes.

After D3, the LuaTask runtime's remaining wait kinds are
`Ready` + `Sleep` only — every UI-blocking call becomes a raw
coroutine yield/resume pair owned by a Lua runtime file.

**D4 · Typed widget events.** **Shipped 2026-04-23.** Widgets now
return `KeyResult::Action(WidgetEvent)` where `WidgetEvent` is
`Submit | SubmitText(String) | Cancel | Dismiss | Select(usize) |
SelectDefault | TextChanged`. Replaces the stringly-typed
`Action(String)` dispatch that threaded through `OptionList`,
`TextInput`, `ListSelect`, `Dialog`, and `FloatDialog` — every emit
site and every intercept in dialog chrome is now a direct enum
match. `classify_widget_action` reduces to a single `match`; no more
`strip_prefix("select:")` / `format!("select:{idx}")` round-trips.

Expected LOC: net −400..−600.

### Phase E — UX polish (3 small commits, can each stand alone)

**E1 · `Placement::FitContent { max: HalfScreen | FullScreen }`** —
**shipped 2026-04-23.** `Ui::dialog_open` now pre-syncs the dialog's
panel `line_count`s before placement resolution so FitContent lands
at the right height on the first frame; `resolve_float_rects` queries
`Ui::natural_dialog_height(win_id)` via the compositor on each
render so live-updating dialogs (`/agents` on tick, `/resume` on
filter) shrink and grow with their content. All Lua-plugin dialogs
now use `fit_content(HalfScreen)` instead of the previous fixed
`Pct(60)` dock. List panels with `Fill` under FitContent scroll
internally past the cap — the existing bottom-clip logic in
`resolve_panel_rects` handles overflow correctly now that the area
is content-sized. (B4 resolved)

**E2 · `Compositor::hit_test` + mouse routing to focused float +
scrollbar click-drag.** Both halves shipped.

- **E2a** (shipped 2026-04-23) — `Compositor::hit_test(row, col)` +
  `Ui::float_at`; wheel over a float synthesises Up/Down into the
  float's keymap. (B6)
- **E2b** (shipped 2026-04-23) — Scrollbar click-drag inside dialog
  panels. `Dialog::{panel_at, panel_viewport, apply_panel_scrollbar_drag}`
  expose enough surface for the App to treat a dialog-panel
  scrollbar exactly like the transcript/prompt scrollbars: a
  `ScrollbarDragTarget::DialogPanel { win, panel }` variant on
  `App.drag_on_scrollbar` latches the gesture at mouse-down, and
  `apply_scrollbar_drag` dispatches on the variant so every
  subsequent drag tick reaches the right buffer. Click on the
  track jump-scrolls; click on the thumb starts a drag. (B7)

**E3 · TextInput as the blessed input widget** — Confirm's reason,
Resume's filter, Agents' search all use the same `TextInput` with
identical cursor / vim / keymap behavior. (B8)

### Phase F — Lua plugin sweep

With Phase D shipped, migrate features *out of Rust into Lua
plugins*. Rust shrinks to core (pixels + security); Lua owns the
composition layer. Neovim-model: `bash`/`edit_file`/`grep` stay
Rust, but `/resume`, `/agents`, `ask_user_question` become plugins.

The 2026-04-22 survey identified ~1750 LOC of candidate
dialogs/commands. Staged in three tiers:

**F1 · Keystone: `ask_user_question.lua`** (proves the pattern).

Decisions (2026-04-22):
- **Multi-question handling: iterate** — when the LLM sends N
  questions, the plugin opens one `dialog.open` per question in a
  loop. Simpler UX (one question at a time, no tabs), no new panel
  kind needed.
- **Answer wire format**: match the old JSON-object shape (question
  text as key) for LLM compatibility.
- **Auto-load**: hardcode `require('smelt.plugins.ask_user_question')`
  at Lua runtime init. Generalize to an autoload list later when
  more core plugins migrate.

Delete:
- `crates/engine/src/tools/ask_user_question.rs` (82 LOC)
- `crates/tui/src/app/dialogs/question.rs` (595 LOC)
- `crates/tui/src/render/dialogs/question.rs` (60 LOC, `parse_questions`)
- `AppOp::ResolveQuestion`, `SessionControl::NeedsAskQuestion`,
  `DeferredDialog::AskQuestion`, `resolve_question()`,
  `EngineEvent::RequestAnswer` / `UiCommand::QuestionAnswer`
  protocol pair.

Replace with ~40 LOC of `ask_user_question.lua` using
`smelt.api.tools.register` (with `execution_mode = "sequential"`
from D1) + `smelt.api.dialog.open`. Net **~−950 Rust LOC, +40 Lua**.
This one commit validates the entire Neovim-model direction.

**F1.5 · Unified command registry.** Prerequisite to F2. Today the
`/` completer merges four parallel sources: a hardcoded
`command_items()` slice, `builtin_commands::list()` (markdown
templates), `custom_commands::list()` (user `.md` files), and
`LuaShared.commands` (`smelt.api.cmd.register`). Only the first
three show up in the completer; Lua-registered commands don't.
Every *next* command source (file-backed, plugin-declared) would
need parallel wiring. This is the exact leaky-abstraction pattern
the north-star rules out.

Staged rollout — **F1.5a** (shipped) fixes the user-visible bugs
without rewriting dispatch; **F1.5b** (follow-up) collapses the
remaining three sources into one registry.

**F1.5a — Lua commands in the completer (done 2026-04-22).** Lua
commands now appear as a fourth read-side source, matching the
existing `custom_commands::list()` / `builtin_commands::list()`
free-function pattern:

- `cmd.register(name, handler, { desc = "…" })` accepts an optional
  third opts table for the description.
- `crate::lua::list_commands()` returns `(name, desc)` pairs from a
  process-global `OnceLock<Mutex<HashMap<…>>>` snapshot that is
  written alongside `LuaShared.commands` on every register call.
  The snapshot stores only strings (no `mlua::RegistryKey`), so it
  can live in a static without violating `!Send`.
- `crate::lua::is_lua_command(s)` mirrors `is_custom_command`.
- `Completer::commands` merges the new source after builtin/custom
  (with dedup). `Completer::is_command` OR-s it in. Both the `/`
  completer and `:` cmdline pick this up transparently.

No new field on `InputState`, no per-tick snapshot sync, no
`Arc<LuaShared>` leaking into the completer — Lua commands look
exactly like any other command source.

**F1.5b — Rust command registry (shipped 2026-04-23).** Deleted
the 17-entry `command_items()` slice in `completer/command.rs` and
the 100-line `match` in `App::handle_command`. Replaced with a
single `RUST_COMMANDS: &[RustCommand]` table in `app/commands.rs`
holding `{ name, desc: Option<&str>, handler: fn(&mut App,
Option<String>) -> CommandAction }` — each former match arm is now
a top-level `cmd_*` function. The completer reads from the table
via `rust_command_items()` (visible entries only); dispatch is
`RUST_COMMANDS.iter().find(|c| c.name == name).map(|c|
(c.handler)(app, arg))`. Hidden aliases (`/q`, `/qa`, `/wq`,
`/wqa`) dispatch but don't show in completion. Dropped
`permissions` + `agents` from the Rust list entirely (both own
their own Lua-plugin completer entries). Killed the dead
`MULTI_AGENT_ENABLED` static + `set_multi_agent` setter.

Markdown builtin/custom commands still load through their own
`resolve()` path (`builtin_commands::list` / `custom_commands::list`
merge into the completer in the same function, but dispatch routes
through `begin_custom_command_turn` rather than a handler fn).
Folding them into one registry would require unifying the two
dispatch shapes (handler fn vs. "evaluate markdown body + start
turn") — defer until there's a concrete consumer.

Net: −60 Rust LOC, +110 Rust LOC for the table boilerplate (could
be tightened with a macro later). The win is not LOC — it's that
the completer and dispatcher now read from **one source of truth**,
and a new Rust command is one row instead of two match arms.

**F2 · Tier 1 sweep — no new Rust APIs needed** (~650 Rust LOC
replaced with ~200 Lua):
- `/help` — static keybind text (109 LOC dialog)
- `/export` — two-option menu (72 LOC dialog)
- `/rewind` — turn selector (87 LOC, needs `smelt.api.session.turns`)
- `/ps` — process list (114 LOC, needs `smelt.api.process.list`)
- `/model`, `/theme`, `/stats`, `/cost`, `/yank-block` commands.

**F3 · Expose session/agent/permissions/process APIs.** Four small
Rust modules exposing already-existing internal state (thin
wrappers over `App`/`engine` calls):
- `smelt.api.session.{list, load, delete, export, turns}`
- `smelt.api.agent.{list, kill, peek}` + tick-event subscription
- `smelt.api.permissions.{list, sync}`
- `smelt.api.process.{list, kill, read_output}`

**F4 · Tier 2 sweep** (~1100 Rust LOC replaced with ~400 Lua):
- `/permissions` (207 LOC) — **shipped 2026-04-22** (−218 Rust, +102 Lua).
- `/resume` (441 LOC) — **shipped 2026-04-23** (−441 Rust, +170 Lua).
- `/agents` (409 LOC) — **shipped 2026-04-23** (−409 Rust, +215 Lua).

**F4-pre · `dialog.open` extensions** — two small surfaces that
unblock the remaining Tier 2 ports without waiting for D2/Option 3:

1. **Input panel `on_change` callback.** Today `dialog.open` input
   panels only surface their final text on `Submit`. Add an
   optional `on_change = function(ctx) … end` per input panel,
   fired on every buffer change with `ctx.inputs`, so Lua can
   rebuild sibling panels live (required for `/resume`'s fuzzy
   filter UX). Routed via a new `TaskEvent::InputChanged`; same
   pattern as `KeymapFired`.

2. **Dialog-level `on_tick` callback.** Add a top-level
   `on_tick = function(ctx) … end` to the `dialog.open` opts table,
   fired every engine tick. Lets Lua re-read external state (agent
   registry, process list) and refresh panels without closing and
   reopening. `/agents` needs this to live-update status + task
   slug while the dialog is open.

3. **`smelt.api.agent.snapshot(agent_id)`** — small addition that
   snapshots `app.agent_snapshots` (working / idle, task_slug,
   rolling log) per agent id into `LuaOps`, so `/agents` detail
   can render without going through the registry round-trip.

Net: ~60 Rust LOC added across `lua_dialog.rs` + `lua/mod.rs` +
`EngineSnapshot`; unlocks two 400+ LOC Rust dialog deletions. The
extensions survive D2/Option 3 unchanged because they live on the
dialog opts table, which `runtime/lua/smelt/dialog.lua` will carry
forward verbatim.

**Stays in Rust forever** (security / perf / protocol):
- Tools: `bash`, `read`, `write`, `edit_file`, `glob`, `grep`,
  `web_fetch`, `web_search`, `notebook`, agent-spawn/stop/message,
  `load_skill`.
- Rendering: compositor, buffers, windows, widgets (`OptionList`,
  `TextInput`), dialog framework, panels, syntax highlight, stream
  parser, transcript projection.
- `Confirm` dialog (permission UI + diff preview — security gate).
- Session lifecycle: `/clear`, `/fork`, `/compact`.
- Engine protocol + agent loop.

Expected total LOC after F1+F2+F4: net **−2000..−2500 Rust, +400..+600
Lua**. Result: `crates/tui/src/app/dialogs/` shrinks to ~3 files
(`confirm.rs`, the Lua dispatcher, and framework helpers).

---

**Total estimated net change across all phases**: **−2500..−3500
lines** after Phase A's +200–300.

**Atomic commit boundary = no intermediary scaffolding exists**. At no
point between commits does the tree contain parallel systems with "one
being migrated". Every commit closes a chapter.

## Open UX bugs (fall out of the above)

- **B5 — transcript status under float.** ✓ Shipped 2026-04-23. The
  status bar is already a top-level compositor layer, but sat at
  `zindex = 3` against floats at 50–60, so any bottom-docked or tall
  centered float obliterated the status row. Bumped the status layer to
  `zindex = 500` in `App::new`. `FitContent` / `dock_bottom_full_width`
  already reserve `above_rows = 1`; the zindex bump only matters for
  centered/manual floats that happen to reach the last row, and now
  the status row wins that collision.
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

### Pre-styled components: Picker, Cmdline, Notification

Between "raw Window" and "Dialog-with-panels" sits a third tier: **named,
opinionated components** that each do one UI concept consistently. The
caller hands in data; the component owns paint, style, placement.

This is Neovim's `pum_grid` model, confirmed by source audit: Neovim's
command-line completion and insert-mode completion *share the same
popup-menu component* (`pum_display` → one compositor grid). Vertical
placement auto-flips above/below based on available space. It never
steals focus — keys flow to the cmdline or editor, the pum just paints.
`wildoptions+=pum` exists specifically to route cmdline completion
through the same popup. One primitive, multiple callers.

Our equivalents:

| Component | Role | Focusable | Placement | Callers |
|---|---|---|---|---|
| `Dialog` | Modal panel stack | yes | DockBottom / Centered | resume, ps, agents, confirm, question, permissions, help, export, rewind |
| `Picker` | Non-focusable dropdown, externally-driven selection | **no** | AnchorCursor (auto-flip) | prompt `/` completer, cmdline `:` completer, Lua `smelt.api.picker.open` |
| `Cmdline` | Non-focusable? **focusable** single-line text float, prompt prefix | yes (owns a cursor) | DockBottom above status | `:` command mode |
| `Notification` | Ephemeral toast, fades on key | **no** | Anchored above prompt | notifications, queued-message indicator |

**Why a named `Picker` over "Dialog with one List panel":**
- Semantic contract differs: Dialog owns its cursor via keymaps; Picker's
  cursor is driven externally (the prompt / cmdline / Lua caller holds
  the selected index). Same paint, different interaction model.
- Callers shouldn't have to assemble a Dialog + pick PanelHeight + set
  `focusable:false` + figure out keymaps every time — one opinionated
  API call.
- The future `smelt.api.picker.open(items, anchor, on_select)` is
  trivially "construct a Picker." No translation layer.

**Why a named `Cmdline` over "a focusable single-panel input Dialog":**
- Behavioral differences: inverse-video cursor cell, `:` prefix, history
  integration, `wildoptions`-style completer routing. Not a panel, a
  purpose.

**Shared foundation.** All three sit on the same compositor primitives
(Window + FloatConfig + a `Component` implementation). The wrapping is
thin — ~40-80 LOC per component — but it buys consistency: one paint
path for "selectable list row," one paint path for "toast," one paint
path for "`:` input." Future callers reuse, don't reinvent.

**Placement is a property of the component, not the caller.** Neovim's
pum decides above/below internally; our `Picker::open(ui, anchor, items)`
does the same via a new `Placement::NearAnchor { auto_flip: true }`
variant. Callers say *where* (prompt cursor, cmdline cursor, Lua-
specified row/col), not *how* (above vs below, chrome vs no chrome).

**Lua API surface mirrors this directly:**
```lua
smelt.api.picker.open({
  items = { ... },
  anchor = { row = r, col = c },
  on_select = function(idx) ... end,
})
smelt.api.cmdline.open({ prompt = ":", on_submit = function(s) ... end })
smelt.api.notify("message", { level = "info" })
```
Each is a one-line wrapper over the Rust component.

### Completer: matching engine (core) vs per-completer features (plugin)

The "completer" currently in `crates/tui/src/completer/` bundles two separable concerns. They have different homes in the end state:

| Concern | Home | Why |
|---|---|---|
| Fuzzy-match algorithm (`score.rs`) | **Rust core** — `smelt.api.fuzzy.match(items, query)` | Generic ranking primitive. Every plugin building a picker needs it. Same category as `Picker` itself: shared, reusable, performance-sensitive. |
| Per-completer features (`/model`, `/theme`, `/color`, `/stats`, `/cost`, `@file`, command list) | **Lua plugins** | Each is *a feature*: what items to offer, when to trigger, what to do on accept. Not reusable across plugins. |
| Trigger wiring (type `/` → open commands completer) | **Lua plugin** | Declarative: `smelt.api.prompt.on_trigger("/", handler)`. |
| Acceptance behavior (Enter → run command / switch theme / insert path) | **Lua plugin** | Action the feature cares about. |

**Shape of a future Lua completer plugin:**
```lua
smelt.api.prompt.on_trigger("/", function(query, ctx)
  local items = smelt.api.fuzzy.match(my_commands, query)
  return {
    items = items,
    on_accept = function(item) smelt.api.cmd.run(item.label) end,
  }
end)
```
The plugin owns one local session `{ picker_win, items, selected }`. The Rust core owns `Picker` + `fuzzy.match` + `prompt.on_trigger` event hook.

**Test for "Rust core" vs "Lua feature":** would a *different* plugin reuse this exact code? Yes → Rust primitive. No → Lua feature.

### CompleterSession: co-locate model and view handle

Corollary: the completer's model (items/selected) and its view handle (`picker_win: WinId`) share one lifecycle. They belong in one owner — a `CompleterSession { model: Completer, picker_win: Option<WinId> }` — so the session-state shape already matches what a future Lua plugin would hold locally. Today lives on `InputState`; once the completer moves to Lua, the whole session struct moves with it with no restructuring.

### InputState is transitional

`InputState` today is a grab-bag: `win` (redundant with `ui::Window`), `completer`, `menu`, `stash`, `attachments`, paste flags, command-arg seeds. The text-editing fields are redundant (Window already owns cursor/vim/kill-ring/undo); the rest are session-level concerns waiting for better homes. Each concern migrates out independently:

| Field | Migrates to |
|---|---|
| `win: ui::Window` | `App.prompt_win: WinId` into `ui.wins` (existing C3 deferred item) |
| `completer` | `CompleterSession` first (step); eventually Lua plugin local state |
| `menu` | Becomes a `Picker` or Lua plugin |
| `store` (attachments) | `App.attachments` or attachments plugin |
| `stash` | `App.stash` |
| `history_saved_buf`, `from_paste` | Local to their respective handlers |
| `command_arg_sources` | Lives with Completer / the plugin that seeds items |

`InputState` empties out over time and gets deleted at the end. No big-bang refactor; one concern at a time.

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
| Completer (prompt `/` + cmdline `:`) | `Picker` component (float, `focusable=false`, externally-driven `selected`, auto-flip placement) |
| Notifications | `Notification` component (float, `focusable=false`, ephemeral) |
| Status bar | `StatusBar` component (segments set at event time) |
| Cmdline | `Cmdline` component (float, `focusable=true`, single-line text + prefix, reuses `Picker` for completion) |
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

// Picker (non-focusable dropdown — prompt completer, cmdline completer, Lua)
ui.picker_open(float_cfg, items, selected) -> Option<WinId>
ui.picker_set_items(win, items, selected)

// Cmdline (focusable `:` prompt, owns its own completer Picker)
ui.cmdline_open(float_cfg, prompt_prefix) -> Option<WinId>

// Notification (non-focusable ephemeral toast)
ui.notify_open(float_cfg, text, level) -> Option<WinId>

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

- **2026-04-23** — Phase D4 shipped: typed `WidgetEvent` enum replaces
  the stringly-typed `KeyResult::Action(String)` bus. One enum
  (`Submit | SubmitText(String) | Cancel | Dismiss | Select(usize) |
  SelectDefault | TextChanged`) carries every widget-to-dialog signal
  that used to flow through `format!("select:{idx}")` /
  `strip_prefix("submit:")` round-trips. Emitters: `OptionList`,
  `TextInput`, `ListSelect`, `Dialog`, `FloatDialog`. Consumers:
  `classify_widget_action` — now a single `match` — plus the inline
  rewrites in `Dialog::handle_key` (input-widget Submit → list
  `Select(idx)`) and `FloatDialog::handle_key` (TextInput Submit →
  `SubmitText(text)` with the final buffer). No behavior change;
  purely cuts the string-parsing cost and makes widget events a
  closed algebra enforced by the compiler.
- **2026-04-23** — Phase E2a: `Compositor::hit_test(row, col)` +
  `Ui::float_at(row, col)`. The interim "focused float captures the
  wheel" hack from the /resume port is gone — wheel events now
  route to whichever float the cursor actually lands on, regardless
  of focus, so background floats scroll under the cursor.
- **2026-04-23** — Phase E2b shipped. Dialog-panel scrollbars
  accept click-and-drag and track-jump clicks. The existing
  transcript/prompt drag state generalised from `Option<AppFocus>`
  to `Option<ScrollbarDragTarget>` with a `DialogPanel { win, panel }`
  variant; `App::begin_dialog_scrollbar_drag_if_hit` runs ahead of
  any focus-aware click handling so a click on a float's thumb or
  track is captured cleanly, and `apply_scrollbar_drag` dispatches
  on the target variant so every drag tick reaches the right
  buffer. New Dialog surface — `panel_at(row, col)`,
  `panel_viewport(idx)`, `apply_panel_scrollbar_drag(idx, thumb_top)`
  — keeps the tui side in charge of gesture state while the ui
  crate owns the panel-geometry math.
- **2026-04-23** — Phase E1 shipped: `Placement::FitContent { max }`
  with `FitMax::{HalfScreen, FullScreen}`. All Lua dialogs switched
  from the fixed `dock_bottom_full_width(Pct(60))` to
  `fit_content(HalfScreen)`. `Dialog::natural_height()` sums panel
  `line_count`s + chrome (top rule + hints + separators); under
  FitContent a panel with `Fill` height counts by its content so the
  "as tall as content, up to cap" contract holds. `Ui::dialog_open`
  now pre-runs `sync_from_bufs` before placement so the first frame
  lands at the correct size instead of the cap fallback. `resolve_
  float_rects` queries `Ui::natural_dialog_height(win_id)` on every
  render so live-updating dialogs (`/agents` on tick, `/resume` as
  the user types) shrink and grow smoothly. List panels that overflow
  the cap use the existing bottom-clip path + internal scroll — no
  new code there. Net: ~80 LOC in `ui::layout` + `ui::lib` + 1 line
  in `lua_dialog.rs`; fixes B4.
- **2026-04-23** — `/agents` ported to Lua plugin (−409 Rust, +215 Lua).
  Two views stitched by a single `smelt.task` loop: list view uses
  `on_tick` to poll `smelt.api.agent.list()` and re-render when the
  registry changes, Backspace kills the selected agent, Enter nests
  into `open_detail` (its own `dialog.open`) which also ticks to
  live-update prompt + tool-call log via `smelt.api.agent.snapshots`.
  Dismissing detail falls back into the outer loop → list reopens.
  Three supporting extensions landed: (a) `content` panel now accepts
  `buf = <id>` (previously only `text = "..."`), plus `focusable` +
  `pad_left` opts, so scrollable plugin-owned buffers can live inside
  a Lua dialog; (b) `BufAddDim` generalised to `BufAddHighlight` with
  `{fg?, bold?, italic?, dim?}` — `fg` is a theme role string, e.g.
  `fg = "agent"` for the detail title's accent; (c) `"agent"` theme
  role exposed. `AppOp::AgentsBackToList` / `AgentsOpenDetail` /
  `AgentsListDismissed` deleted alongside the Rust dialog. Completer
  still gates `/agents` on multi-agent mode because the name stays in
  `command_items()` — Lua takes precedence on dispatch.
- **2026-04-23** — `/resume` ported to Lua plugin (−441 Rust, +170 Lua).
  Exercises the full F4-pre surface: input `on_change` for live fuzzy
  filter, `list` panel kind (buffer-backed selectable rows), `BufAddDim`
  op so metadata columns (size, duration) can dim. Tab now cycles
  dialog focus (workspace-toggle moved to `alt-w`); when list is
  focused, typing falls through to the sibling `TextInput` widget so
  vim-nav (`j`/`k`/`g`/etc.) and filter-typing coexist. Up/Down fall
  through the opposite direction when input is focused. Mouse wheel
  on a focused float synthesises Up/Down keys through `ui.handle_key`
  — enough to scroll list panels without waiting for E2's full
  compositor hit-test. Hit a silent buf-id collision along the way:
  Lua's `smelt.api.buf.create` and `Ui::buf_create` both allocated
  from `1`, so `BufCreate { id: 1 }` stomped the prompt input buffer
  on first `/resume` open. Fix: partition the buf-id space with
  `LUA_BUF_ID_BASE = 1 << 32` on a dedicated `LuaShared.next_buf_id`
  atomic, and change `Ui::buf_create_with_id` to return
  `Result<BufId, BufId>` so the reducer surfaces `notify_error` on
  collision instead of silently overwriting. Scrollbar drag + click-
  to-pick-row still pending (E2).
- **2026-04-22** — D3 rewritten: **Option 3 chosen** for Lua blocking
  intents. Rather than keep per-intent Rust glue (`lua_dialog.rs`,
  `lua_picker.rs`, future `lua_cmdline.rs`) or collapse them into a
  generic `lua_float.rs` with a per-component `ResolvableFloat` trait
  (Option 2, ~10 LOC Rust per new intent), the post-D2 target is
  **zero per-intent Rust cost**: the coroutine `yield`+`resume`
  dance moves to Lua runtime files (`runtime/lua/smelt/picker.lua`,
  `dialog.lua`). Plugin-author ergonomic is unchanged
  (`local r = picker.open(...)` still reads as blocking). Deletes
  `TaskWait::Dialog`/`Picker`, `TaskDriveOutput::OpenDialog`/
  `OpenPicker`, `Yield::OpenDialog`/`OpenPicker`, both glue files
  (~800 LOC). LuaTask runtime reduces to `Ready` + `Sleep` only.
  Matches Neovim's `vim.ui.select` model exactly (Lua wrapper over
  primitives, not a Rust intent decoder). Option C's current
  `lua_picker.rs` is disposable — kept until D2 lands.
- **2026-04-22** — Phase F1.5 added to plan: unified command registry.
  Four parallel command sources today (`command_items()` hardcoded
  slice, `builtin_commands::list()` markdown templates,
  `custom_commands::list()` user `.md` files, `LuaShared.commands`);
  completer reads only the first three, so Lua-registered commands
  via `smelt.api.cmd.register` don't show up — and every new source
  would need parallel wiring. Collapse to one registry
  (`LuaShared.commands`): Rust built-ins register at startup with
  `Callback::Rust`, markdown/custom commands register at load time,
  completer reads the registry only. Deletes `command_items()` and
  duplicate source walks; unblocks F2 (Lua plugin sweep — `/model`
  etc. will show up naturally with zero extra wiring). Extends
  `cmd.register` to accept an optional `description`.
- **2026-04-22** — Phase F1.5a shipped. `cmd.register` now takes
  `(name, handler, { desc = "…" })`. Added
  `crate::lua::{list_commands, is_lua_command}` as free functions
  backed by a `OnceLock<Mutex<HashMap<String, Option<String>>>>`
  snapshot written on every register — string-only so the static can
  hold it without tripping `mlua::RegistryKey`'s `!Send`.
  `Completer::commands` merges the new source after builtin/custom
  (dedup by label); `Completer::is_command` OR-s it in. Both the `/`
  completer and the `:` cmdline pick it up transparently: `/pick-test`
  now shows with its description in the picker, and submitting
  `/fuzzy-test` dispatches to the Lua handler instead of being sent
  to the agent as chat. No new field on `InputState`, no per-tick
  sync — mirrors the existing `custom_commands::list()` /
  `builtin_commands::list()` pattern.
- **2026-04-23** — Phase F1.5b shipped. Deleted the 17-entry
  hardcoded `command_items()` slice in `completer/command.rs` and
  the 100-line `match` in `App::handle_command`. Replaced with a
  `RUST_COMMANDS: &[RustCommand { name, desc, handler }]` table in
  `app/commands.rs` — each former match arm is a top-level
  `cmd_*` fn; completer reads visible entries via
  `rust_command_items()`; dispatch is table lookup. Hidden aliases
  (`/q`, `/qa`, `/wq`, `/wqa`) dispatch but don't surface in
  completion via `desc: None`. Dropped the stale `permissions` +
  `agents` hardcoded entries (both are Lua plugins now) and the
  dead `MULTI_AGENT_ENABLED` static + `set_multi_agent` setter.
  Rust and completer now read from one source of truth; a new Rust
  command is one table row instead of two parallel edits. Markdown
  builtin/custom still load through `resolve()` + `begin_custom_command_turn`
  — folding them into the same handler-fn registry would require
  unifying two dispatch shapes (fn call vs. "evaluate markdown body
  + start agent turn"); deferred until there's a concrete consumer.
- **2026-04-22** — Phase F3 shipped. Exposed
  `smelt.api.session.{list, load, delete}`,
  `smelt.api.agent.{list, kill, peek}`,
  `smelt.api.permissions.{list, sync}`, and
  `smelt.api.process.read_output` — thin wrappers over existing
  internal state. `AppOp::DeleteSession` + `AppOp::KillAgent` added.
  `EngineSnapshot.permission_session_entries` plumbed through
  `snapshot_engine_context` so permissions.list() serves snapshot
  data without App access. Unblocked F4 Tier 2.
- **2026-04-22** — Phase F4 started. `/permissions` ported to Lua
  plugin (−218 Rust, +102 Lua). Uses `dd` chord via a Lua-side
  pending flag; Backspace fallback; syncs on every close. The
  remaining `/resume` and `/agents` hit gaps in the `dialog.open`
  Lua surface (no input `on_change` for live filter, no dialog
  `on_tick` for registry refresh, no agent snapshot API) — added
  as **F4-pre** to the plan rather than queuing them behind the
  bigger D2/Option 3 refactor.
- **2026-04-22** — `ui::Notification` component landed. Non-focusable
  ephemeral toast, sibling to `Picker`. `App.notification` changed from
  an inline `Notification` struct to `Option<WinId>`; notify/
  notify_error/dismiss rewired through `Ui::notification_open`/
  `win_close`. Inline chrome-row rendering path deleted (including
  the `render::screen::Notification` struct and 2 test fixtures).
  Added `Ui::float_config_mut` + `refresh_float_rect` as helpers for
  repositioning floats across terminal-resize frames without
  close+reopen. 875 tests pass. Two of three named components
  (`Picker`, `Notification`) now shipped.
- **2026-04-22** — Completer navigation direction fix: Up/Ctrl-K/P
  and Down/Ctrl-J/N were inverted (legacy from when the main branch
  rendered the completer bottom-up below the prompt; the new
  top-down Picker flips the mapping). Arrow + chord keys now match
  visual direction.
- **2026-04-22** — Phase C3.picker landed (task #56): `ui::Picker`
  component created (5 tests), `Ui::picker_open`/`picker_mut` added,
  completer rewired onto Picker, ~90 LOC of hand-rolled paint deleted.
  Follow-up noted: `App.completer_win` is leaky — WinId and
  `InputState.completer` share a lifecycle but live in separate
  owners. Next step: couple them into `CompleterSession` (matches
  future Lua plugin local-state shape). Plan clarifications added:
  fuzzy-match stays Rust core, per-completer features are plugins;
  InputState is transitional and dissolves as each concern migrates.
- **2026-04-22** — Plan pivot: **named, pre-styled components**
  (`Picker`, `Cmdline`, `Notification`) formalized as a third tier
  between raw `Window` and `Dialog`. Validated by Neovim source audit
  (`popupmenu.c` — their `pum_grid` is a compositor layer, never steals
  focus, auto-flips placement, shared between insert-completion and
  cmdline-completion via `wildoptions+=pum`). Our `Picker` is the same
  pattern: one opinionated component, three callers (prompt `/`
  completer, cmdline `:` completer, Lua `smelt.api.picker.open`).
  Phase C3.picker added to rework task #55's landed completer onto
  `Picker`; Phase C2 revised to ship `Cmdline` as a component
  reusing `Picker` for completion. New Lua API surface:
  `smelt.api.{picker,cmdline,notify}`.
- **2026-04-22** — Phase C3.completer landed (task #55) as an
  intermediate step: completer visible again via compositor float,
  but hand-rolled paint — to be reworked onto `Picker` in C3.picker.
  `win_open_float` now respects `focusable: false` (skips
  `compositor.focus` for popups). Fixes focus-theft bug that would
  have affected every future non-focusable float.
- **2026-04-22** — Phase D0 + callback-first keymaps landed. New
  `LuaShared.task_inbox: Mutex<Vec<TaskEvent>>` owns dialog-resolution
  + keymap-fired events; reducer no longer sees `AppOp::ResolveLuaDialog`.
  Dialog `keymaps` entries now take `on_press = function(ctx) ... end`
  (ctx carries `selected_index`, `inputs`, `close()`), replacing the
  brittle `action = "kill"` string-dispatch. Also renamed
  `LuaDialogState` → `DialogState` (leaky name). Unlocks Tier-2
  dialogs (permissions/resume/agents) that need `d/D/Tab`-style
  custom keys. Commits `20f8d63`, `ecc8645`, `eef68e6`.
- **2026-04-22** — Phase F2 dialog sweep landed (5 plugins): `/help`
  (109 LOC) + `/export` (72 LOC) + `/rewind` (87 LOC) + `/ps`
  (114 LOC) + `/yank-block` (9 LOC Rust) all ported to Lua. Kept
  primitives minimal: `smelt.api.session.{turns,rewind_to}`,
  `smelt.api.process.{list,kill}`, `smelt.api.keymap.help_sections`,
  `smelt.api.transcript.yank_block`. Remaining F2 items (`/model`,
  `/theme`, `/color`, `/stats`, `/cost`) are completer-based, not
  dialog-based — porting would require a new completer primitive or
  a UX simplification (inline completer → modal dialog). Deferred
  pending UX decision. Net so far: ~−400 Rust, +220 Lua across
  F1+F2 dialog sweep.
- **2026-04-22** — plan pivot: added North-star commitment #3
  ("Rust core, Lua features"). Phase D reframed as "Neovim-model FFI
  completion" — audit showed all primitives to build features in Lua
  already exist; remaining work is 3 focused commits (execution_mode,
  PanelSpec userdata, delete schema-parser). New Phase F added for
  Lua-plugin migration sweep (F1 keystone: `ask_user_question.lua`,
  F2 Tier 1, F3 new APIs, F4 Tier 2). Expected total delta:
  ~−2000..−2500 Rust, +400..+600 Lua.
- **2026-04-22** — `render::Screen` struct deleted; guts migrated onto
  `App` (new `app/transcript.rs`, ~60 methods moved verbatim). Commits
  `6cd800a` (−885) and `ef36101` (−97 dead-type sweep:
  `Frame`/`TerminalBackend`/`StdioBackend`/`FramePrompt`). Phase C3
  now has two deferred items (InputState→WinId, notification-floats);
  not blocking.
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
