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

### Testing interactive TUI changes via tmux

Smelt is a full-screen TUI; `cargo test` only covers unit-testable
logic. For anything visual (dialog rendering, layout, selection
highlight, prompt shifts), drive the real binary in a tmux pane and
capture the screen.

1. **Target pane = split inside `smelt:4`** (the worktree window).
   **Never** run the binary in the pane the assistant is typing
   into, and never launch a new tmux window — always split the
   worktree window so both the chat pane and the app pane stay
   visible to the user.
2. Create a side pane once and keep its `%id`:
   ```bash
   tmux split-window -h -t smelt:4 -c <worktree-path> -P -F '#{pane_id}'
   ```
3. Pre-build the binary in the assistant pane
   (`cargo build --quiet`), then launch the already-compiled binary
   in the side pane so `cargo run`'s compile output doesn't pollute
   the captured screen:
   ```bash
   tmux send-keys -t %ID './target/debug/smelt' Enter
   ```
4. Drive the app with `tmux send-keys` and inspect with
   `tmux capture-pane -t %ID -p | tail -N`. Don't interrupt the
   process with `C-c` in the chat pane unless you mean to close the
   app; when the app exits, the pane dies and you have to recreate
   it.
5. For panel content debugging, avoid `eprintln!` (crossterm raw
   mode swallows stderr). Write to a dedicated logfile:
   ```rust
   if let Ok(mut f) = std::fs::OpenOptions::new()
       .append(true).create(true).open("/tmp/smelt-draw.log")
   {
       let _ = writeln!(f, "...");
   }
   ```
   Then `grep` the file between runs. Remove the writes before
   committing.

### UI conventions

- **Dialog titles are lowercase.** `resume (workspace):`,
  `permissions`, `help`. Uppercase is reserved for proper nouns
  (session titles, model names, file paths).
- **Meta columns are dim, content is normal.** Example in resume
  list rows: size + time columns use `SpanStyle::dim()`; the actual
  title keeps the default fg. Selection then retints the *whole*
  row fg to accent so the selected row pops.
- **Selection = fg-accent on the cursor row.** No bg fill, no
  cursor glyph, no layout shift. Single mechanism across every list
  panel (Step 9.4 choice; see below).
- **Gap above the hints row.** Every dialog reserves one blank row
  between panel content and the hints so the layout breathes.

### Post-9.5b review (2026-04-22) — bugs, architecture, cleanup

After landing Steps 9.5b items 9–12 (all dialogs on the panel framework,
legacy `Dialog`/`DialogResult`/`ConfirmDialog`/`QuestionDialog` deleted),
a review surfaced these follow-ups. They're grouped so the ordering can
be picked per session.

**Keystone: finalize the rendering architecture before more
polish.** Multiple bugs (B1 stale mode pill, B3 prompt fade, B5
transcript status, B6 mouse routing) all trace back to the same
root cause — the rendering is split between two parallel systems
(legacy `render::Screen` emitting ANSI bytes + duplicated caches
like `last_mode`, vs. the new compositor with grid-diff). Every
bug fix while both paths coexist risks another duplicated-state
drift.

**Decision:** stop patching symptoms. Complete the compositor
migration (task #11 "Pin prompt window to terminal bottom" + the
C1 `Screen` deletion sweep) so there is a single grid, single
source of truth per field, and the dirty flag can be deleted
entirely. Only then return to B2/B4/B6/B7. The `last_mode`
cache is the canonical example of why: fixing it in isolation
is trivial, but the class of bug only disappears when the cache
is gone.

**Bugs to fix on the compositor path:**

B1. ✅ **Shift+Tab toggles mode globally.** Fixed by landing A1
    (global chord layer) and rewiring the status bar to read
    `self.mode` directly instead of the stale `screen.last_mode`
    cache. Two commits: `1cf2960` (chord layer: Shift+Tab /
    Ctrl+T / Ctrl+L) and the mode-pill single-source-of-truth fix.

    **Lesson worth codifying:** the mode was *actually* toggling in
    `self.mode` and in engine state — only the status-bar pill was
    stale because it read `screen.last_mode`, a cached copy updated
    only by the legacy `draw_prompt` path. Duplicated state behind a
    dirty flag → silent drift. Status-bar rendering now reads the
    live fields; the `last_mode` cache should be deleted once the
    legacy `draw_prompt` status code (screen.rs:550) is removed as
    part of the compositor migration (B3 / task #11).

B2. ✅ **Permission-gated tools prompt correctly.** Was a symptom of
    B1: the status-bar mode pill was stale (`screen.last_mode`
    cache), so the user was actually in Yolo mode without knowing
    it — `rm` was allowed by policy, not a Confirm-dialog bug. Fixed
    transitively when the mode pill was rewired to read `self.mode`
    directly. Reinforces the keystone decision above: duplicated
    state doesn't just hide visually, it hides *decisions*.

B4. **Dialog height is caller-fixed, not "fit up to half-screen".**
    The bug isn't that dialogs "grow to fit" — it's that every
    dialog picks a placement constraint that's *independent* of
    actual content:
    - `rewind.rs:48` uses `Constraint::Fixed(footer_h + 4)` with
      `footer_h = total.min(10)` → clipped at 14 rows regardless of
      terminal size; doesn't grow for long lists.
    - `resume.rs:58` uses `Constraint::Pct(60)` → always 60% of
      terminal height, so 3 entries end up in an oversized float
      whose inner list still reports scrollable (likely a Fill-panel
      line-count vs. viewport mismatch, because the Fill panel is
      sized from the 60% envelope rather than from `line_count`).
    - Other dialogs use their own ad-hoc constants.

    Desired convention: **content drives height, capped at
    `terminal_height / 2`**. Only Permissions opts into full height.
    Two changes:
    1. `Placement` gains a `FitContent { max: HeightLimit }` variant
       (or `Constraint::Fit`). The compositor computes the float's
       intrinsic height by asking `Dialog` for `content_rows()`
       (sum of resolved panel heights + chrome), then clamps against
       `HalfScreen` / `FullScreen`.
    2. Fix the Fill-vs-Fit behaviour in `resolve_panel_rects` so a
       List panel with `PanelHeight::Fit` scrolls internally past
       the cap instead of being over-allocated rows. Resume's list
       panel should be `Fit`, not `Fill`, once (1) lands.

    Migration: Rewind/Resume → `FitContent { max: HalfScreen }`;
    Permissions → `FitContent { max: FullScreen }`; other builtin
    dialogs audited case-by-case.

B5. **Transcript status/indicator disappears while a dialog is
    open.** User-reported: the transcript's overlay indicator
    (scroll-percent or scrollbar or status row, ambiguous from the
    bug report) vanishes when a float is layered on top. Likely
    cause: the compositor only repaints the transcript region below
    the float's rect when the float is dirty, but the transcript
    layer's own status painting assumes a full-width redraw. Verify
    with the repro, then either (a) paint transcript status into the
    status bar layer (which is a dedicated row) or (b) force a full
    transcript repaint on any float layer change.

B6. **Mouse wheel over a dialog scrolls the transcript beneath it.**
    Wheel events on rows inside the float's rect should be routed to
    the float, not the layer beneath. Today
    `Compositor::handle_mouse` is still pending (task #7 in the
    tracker). The scroll-under-mouse path in
    `app/mod.rs::scroll_under_mouse` hit-tests against the
    transcript/prompt but ignores float rects. Fix: add
    `Compositor::hit_test(col, row) -> Option<WinId>` and route
    wheel events to the topmost layer at that cell; fall through to
    the compositor's default scroll-focused only when no float
    covers the row.

B7. **Scrollbar is read-only.** The scrollbar rendered inside a
    panel shows the thumb but doesn't respond to clicks or drags.
    Expected: (a) click-and-drag the thumb to scroll, (b) click on
    the track to page or center the thumb on the click point.
    Infrastructure: `ui::ScrollbarState` already tracks thumb
    geometry; needs a `handle_mouse(col, row, kind)` that translates
    track clicks into a target `scroll_top`. Ties into B6 —
    mouse-routing-to-float needs to land first so the scrollbar
    even sees the click.

B3. **Prompt + status bar fade out over time.** The real cause is
    that the migration to the compositor-based diff renderer is
    incomplete: the transcript and floats are drawn via the
    compositor (grid-diff, no dirty flag), but the prompt and status
    bar are still painted through the legacy `render::Screen` path,
    which emits ANSI bytes *outside* the compositor's grid. Any
    time the compositor repaints (float open/close, transcript
    scroll, spinner tick on another layer) those bytes leak into a
    region the compositor thinks it owns, and subsequent frames
    overwrite the prompt without restoring it. The old fix was
    `Screen::dirty = true` on every tick; with legacy dialogs gone
    there's no longer a tick path that does that.

    Proper fix (ties into pending task #11, *Pin prompt window to
    terminal bottom*): move the prompt + status bar onto their own
    compositor layers so everything goes through one grid. Quick
    stopgap while that lands: mark `Screen` dirty whenever the
    compositor draws a frame (e.g. call `screen.mark_dirty()` after
    `compositor.draw()`), so the prompt is always repainted in
    lockstep.

**Architectural follow-ups:**

A1. ✅ **Global chord layer in `dispatch_terminal_event`** (commit
    `1cf2960`). Pre-route map fires regardless of focus:
    `Shift+Tab` → toggle mode, `Ctrl+T` → cycle reasoning, `Ctrl+L`
    → full redraw. Runs *before* float/cmdline/prompt dispatch.
    Still-open follow-up: retire `handle_confirm_backtab` by having
    Confirm's `tick` observe `mode` changes and resolve when the new
    mode auto-allows (today the chord layer still routes through
    `handle_confirm_backtab` when a Confirm float is focused).

A2. **Replace `lua_dialog.rs` with direct UI primitives in Lua.**
    The current shape (`Lua table {title, panels[{kind,...}]}` →
    Rust parser → `PanelSpec[]`) hard-codes a schema the plugin
    author can't extend. Replace with:
    - `smelt.api.ui.buf_create`, `buf_set_lines`, `buf_mut`
    - `smelt.api.ui.win_open_float` (returns win id) / `win_close`
    - `smelt.api.ui.dialog_open(panels, cfg)` where panels carry
      opaque widget-handle userdata
    - Widget constructors: `smelt.api.ui.option_list(items)`,
      `text_input(opts)` — return userdata with `text()` / `cursor()`
      / etc.
    Then ship a thin Lua-side `smelt.dialog.open({...})` helper in
    `runtime/lua/smelt/dialog.lua` that composes the primitives for
    the 80% case. Delete `crates/tui/src/app/dialogs/lua_dialog.rs`.
    The abstraction migrates from Rust (hard-coded enum of panel
    kinds) to Lua (plugin-authored and forkable).

A3. **Move `confirm_context` into the Confirm dialog state.**
    Currently `App::confirm_context: Option<ConfirmContext>` is
    read/written only by the Confirm flow. Move it into
    `app::dialogs::confirm::Confirm` as a field. App only needs to
    know there's a pending permission request at the `request_id`
    level.

A4. **Narrow the `App → dialog` surface to `approve_tool`,
    `deny_tool`, `add_session_rule`, `add_workspace_rule`.**
    Today `App::resolve_confirm` carries the full `ConfirmChoice`
    enum and branches on every variant. Push the branching into
    the dialog; App exposes minimal verbs. Same for
    `resolve_question`. Drops ~150 lines of glue and makes it
    obvious what a dialog can do to the app.

A5. **Consider a smaller `DialogCtx` instead of `&mut App`.**
    `DialogState::on_action(&mut self, app: &mut App, …)` gives
    dialogs the entire app. A typed context (`ui`, `lua`,
    `screen`, `approve_tool`, `notify_error`) makes the surface
    legible and the coupling intentional. Lower priority — the
    current coupling isn't actually causing bugs, just making
    the code harder to read.

A6. **Queue permission requests in the compositor, not the main
    loop.** `pending_dialogs: VecDeque<DeferredDialog>` still lives
    in the main loop even though `Ui` has proper layer stacking.
    Could move into `Ui` as a "permission requests pending user
    attention" channel that surfaces when no blocking float is
    focused. Ties into B1 (global chord layer picks up mode
    toggles regardless of queue state).

**Cleanup sweep (four buckets):**

Every file that touches the rendering / dialog path should be audited
against these four buckets. A small `CLEANUP.md` doubles as the tracker
so nothing slips.

C1. **Legacy code (marked as legacy, no callers).** `Frame`,
    `RenderOut`, `StyleState`, `paint_line`, `queue_status_line`,
    `queue_dialog_gap` — still referenced but only from the prompt
    path that Step 9.6/9.7 will also replace. Count references; mark
    `#[allow(dead_code)]` with a reason comment; delete once 9.6
    lands.

C2. **Abandoned migrations (started, never finished).** Anything that
    was "migrate X to Y" but X and Y coexist. Candidates:
    - Prompt rendering: `PromptRow` / `StyledSegment` /
      `prompt_data.rs` coexist with the buffer-backed `WindowView`
      input path (Step 6i/6j partial).
    - `render/transcript.rs` vs `render/transcript_buf.rs` — we
      projected into a buffer but the nav-text functions in
      `transcript.rs` are still alive "until 6i lands."
    - `Screen::clear_dialog_area`, `Screen::set_dialog_open`,
      `Screen::set_constrain_dialog` — no longer meaningful since
      the dialog-mode frame is gone; likely stale flags kept for
      legacy render.

C3. **Intermediary transition scaffolding (still live).** Code
    that was added *because* a migration was in flight and now has
    no reason to exist:
    - `render_dialog` / `draw_viewport_dialog_frame` — deleted in
      Step 12, but audit for any remaining dialog-mode toggles on
      `Screen` (`set_dialog_open`, `set_constrain_dialog`,
      `prev_dialog_row`).
    - `layout::LayoutState.dialog` — the old legacy-dialog layout
      slot; if unreferenced after Step 12 it can go.
    - `ActionResult::Keep` carried a `#[allow(dead_code)]` comment
      — is it actually used now?
    - `PromptState.prev_dialog_row` (kept, but verify it's still
      read).

C4. **`#[allow(dead_code)]` audit.** Every `#[allow(dead_code)]` is
    either (a) a genuine abandoned migration (move to C2), (b) a
    legitimate seam we plan to use soon (keep with a TODO and a
    step reference), or (c) obsolete and deletable.

**Suggested order for the next session:** B3 (visible regression) →
B1 + A1 (small, unlocks mode toggle) → B2 (needs reproduction) →
C1/C2/C3/C4 sweep → A3/A4 (mechanical cleanup) → A2 (big, do last).
A5/A6 are backlog.

#### Concrete audit findings (2026-04-22)

After grepping the current tree, here's the state of each cleanup
bucket. Line/symbol counts reflect what will be touched.

**B1 diagnosis (Shift+Tab routing).** The input keymap at
`crates/tui/src/keymap.rs:272` binds `BackTab` → `Action::ToggleMode`,
but only the prompt focus reaches that lookup. `AppFocus::Content`
goes through `handle_event_app_history` at `app/events.rs:1268` and
never consults the input keymap. Confirmed: Shift+Tab is dead when
the transcript is focused. Fix shape is **A1** (global chord layer
at top of `dispatch_common` running before float/cmdline/prompt
dispatch).

**B2 diagnosis (`rm` allowed).** Two plausible paths:
(i) the Confirm float opened but wasn't visibly rendered (see B3
regression) and the user hit Enter on the pre-selected "yes" option;
(ii) `permissions::decide(Mode::Normal, "bash", {"command":"rm..."})`
returned `Decision::Allow`. The engine side already gates `Mode::Yolo`
→ `check_bash("rm -rf /") = Allow`, so if mode flipped to Yolo
silently (shift+tab lost, but cycled into Yolo via another path),
that explains it. Reproduce first: confirm mode indicator + whether
the float appeared.

**B3 diagnosis (fade-out).** Root causes layered:
1. `render_normal` early-returns when `!screen.needs_draw(false)`.
   `needs_draw` is `has_unflushed || dirty`. If nothing sets `dirty`
   (no streaming, no typing, no engine events) and the spinner
   frame hasn't advanced, the compositor never redraws.
2. `screen.update_spinner` only sets `dirty` when the spinner
   *frame index* changes (every 150ms while working). When
   `working.elapsed()` is `None` (idle), nothing dirties.
3. Terminal emissions from anything outside the compositor
   (spawned subprocess, engine-side logging, Lua plugin output)
   can push the cursor and scroll the alt buffer — the compositor
   doesn't redraw to overwrite because `dirty == false`.

Fix: either (a) always repaint on the timer tick at a low frequency
(≥1 Hz), or (b) surface any byte written outside the compositor by
making the only write path go through `Ui::render`. (a) is a
one-liner; (b) is the correct long-term answer and ties into the
"no more RenderOut" Step 9.7.

**C1 — Legacy code with no callers:**
- `Screen::pause_spinner` / `resume_spinner` — called from nothing
  after `open_dialog` / `finalize_dialog_close` were deleted. Safe
  to remove.
- `Screen::clear_dialog_area` — only caller was the deleted
  `finalize_dialog_close`. Safe to remove.
- `Screen::set_constrain_dialog` — no callers. Safe to remove.
- `Screen::set_dialog_open` — only caller is `render_normal` at
  `app/events.rs:1078` calling `set_dialog_open(false)`; the `true`
  setter went away. Delete the call + the state + the method.
- `Screen::queue_dialog_gap` at `render/screen.rs:434` — zero
  callers after Step 12. Delete.
- `Screen::dialog_row()` / `prev_dialog_row` field — used to be
  read back by `handle_float_action`'s legacy prompt-clear logic.
  Verify no readers remain; delete.

**C2 — Abandoned migrations:**
- `render/transcript.rs::nav_col_to_display_col` and
  `full_transcript_nav_text` — the Step 6h comment says "Nav-text
  functions still exist in transcript.rs but have no callers from
  events.rs — will be deleted once prompt migration (6i) is
  complete." 6i is partial. Audit today's callers.
- `render/prompt_data.rs` (1074 lines) — the declared step-9.6/9.9
  deletion target. `PromptRow`, `StyledSegment`, bar_row helpers
  all live here. `render_normal` still calls into it. Half-migrated
  to `WindowView` per Step 6i.
- `render/completions.rs` (415 lines) — completer popup still drawn
  via this module instead of the compositor (Step 9.6 target).
- `render/cmdline.rs` (226 lines) — cmdline overlay still legacy
  (Step 9.6 target).
- `render/dialogs/confirm.rs` internals: `render_notebook_preview`
  takes `&mut S: LayoutSink` and is called from `ConfirmPreview::
  render_into_buffer` via `SpanCollector`. Fine. But the module
  has a `_count_rows_unused` `#[allow(dead_code)]` stub left from
  the old `total_rows` API — delete.
- `render/to_buffer.rs::render_into_buffer` still `#[allow(dead_code)]`
  at the top even though it has real callers now. Drop the attr.

**C3 — Intermediary transition scaffolding:**
- `PromptState.prev_dialog_row` at `render/prompt.rs:16` — written
  by `render_normal` at `screen.rs:2125` and read back by the
  (now-deleted) dialog-frame path. Trace who still reads it; likely
  dead.
- `layout::LayoutState.dialog` — was the legacy dialog layout slot.
  After Step 12, `render_normal` always passes `dialog_height:
  None`. Delete the field.
- `Screen.constrain_dialog` / `Screen::dialog_open` fields if they
  persist — dead state.
- `ActionResult::Keep` at `app/dialogs/mod.rs:26` carries a
  `#[allow(dead_code)]` comment "used by Confirm's Always-menu
  expansion (next step)" — Confirm migration landed, no
  Always-menu expansion. Drop `Keep` or make it live.
- `app/dialogs/confirm.rs` has a `"// Match the legacy App::
  handle_dialog_result Confirm arm."` and `"Mirrors legacy."`
  comments — references deleted code. Clean up.
- `render/dialogs/mod.rs` is now a ~30-line shim holding only
  `AgentSnapshot` + `parse_questions` re-exports. Move these up
  to `render/mod.rs` or a dedicated `render/snapshots.rs` +
  `render/question.rs` and delete the `dialogs/` directory.

**C4 — `#[allow(dead_code)]` audit (7 sites):**
- `app/dialogs/mod.rs:26` `ActionResult::Keep` → C3 resolution.
- `render/to_buffer.rs:25` `render_into_buffer` → drop the attr.
- `render/dialogs/confirm.rs:305` `_count_rows_unused` → delete.
- `lua/task.rs:43, 54, 73, 80, 148, 275` (6 sites) — all tagged
  with "wired in step (iv)". Step (iv) landed. Either remove the
  attrs or delete unused fields/methods.

This gives the next session a ~30-minute mechanical sweep (C1/C3/C4)
that drops several hundred lines of dead code before the bigger
work (A1/A2/A3/A4) begins.

---

### Known bugs to address during Step 9.4 / 9.5

Stashed here as the migration uncovers them; each gets fixed inside
the step that rewrites the surrounding code, not as a separate
patch.

- **Clear command leaves the screen in a half-drawn state.** After
  `/clear` the prompt window's chrome (top/bottom bars,
  notification row) disappears or renders stale glyphs. Happens
  because the clear path tears down screen state but doesn't force
  a full compositor redraw against the new buffer contents. Fix
  during the prompt + compositor cleanup in 9.6 / 9.9.
- ~~Confirm / Question / Agents dialogs still use the legacy~~ (resolved
  in Step 9.5b items 9–12; all dialogs now on the panel framework.)
- **(historical) Confirm / Question / Agents dialogs used the legacy
  `trait Dialog`.** They render via the separate `Frame` / `RenderOut`
  path and therefore ignore the fg-accent selection convention,
  the hints-gap rule, and the callback registry. Fixed when they
  migrate in Step 9.5.
- **Resume search typed chars collide with vim j/k.** Typed chars
  feed the search query so `j` / `k` never reach navigation. Arrow
  keys work. Revisit when the search moves to a real Input panel
  (Step 9.6 unlocks that).
- **No hardware cursor in dialogs.** `Dialog::cursor` returns
  `None` for now; once Input panels are wired we surface a proper
  hardware cursor for the text-edit panel.

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

### Window vs Component: the two-concept model

Two orthogonal primitives, following Neovim's architecture:

- **`ui::Component`** — anything that draws into a grid. Gets a rect
  and paints into a `GridSlice`. Also handles keys (returns
  `KeyResult`). That's the entire contract. Components are what the
  compositor stacks as layers.
- **`ui::Window`** — a buffer viewer. Owns `BufId`, cursor, scroll
  offset, vim state, selection, kill ring. A `Window` is state, not
  a renderer — a Component wraps a Window to paint its buffer.

**Not every Component needs a Window.** Neovim doesn't make its
statusline a window either — the statusline is a row drawn inside
the owning window's grid, governed by `w_status_height` (0 or 1).
Popups, messages, and cmdline use separate grids but not windows.

**Rules of thumb:**

| Question | Answer → Window or Component? |
|---|---|
| User puts cursor in it? | Window |
| User selects text + copies? | Window |
| Is it pure decoration (bars, labels)? | Component |
| Fixed-height, app-driven rendering? | Component |

**Focusable flag.** A `Window` can opt out of the focus cycle via a
`focusable: bool` on its float config (default `true`). Modeled
directly on Neovim's `WinConfig.focusable`: `<C-w>w` cycling skips
non-focusable floats, and the cursor can't roam into them. Splits
are always focusable; only floats carry the flag.

**Chrome split.** Not everything above the prompt needs to be a
window. Apply the rules:

| Surface | Kind | Focusable? |
|---|---|---|
| Transcript | Window (split) | yes |
| Prompt input | Window (split) | yes |
| Normal dialogs (resume, agents, rewind, etc.) | Window (float) | yes |
| Confirm / Question dialogs | Window (float) | yes |
| Completer (fuzzy finder) | Window (float) | **no** — matches nvim-cmp |
| Notification | Window (float) | **no** — selectable by mouse/cmd but not tab-target |
| Queued messages | Window (float) | **no** |
| Top / bottom prompt bars | Component | n/a — no buffer |
| Status bar | Component | n/a |
| Stash indicator | Component | n/a |

Chrome items become real windows only when their content needs
selection or copy. Pure decoration stays as a Component.

### Layout stack vs focus graph

Two more orthogonal concepts:

- **Layout** — where a surface is on screen. All windows *and*
  components participate in the layout. Adding a chrome window
  shrinks the transcript by its row count.
- **Focus graph** — which windows `<C-w>` cycles through. Only
  focusable windows are in the graph.

A window can be in the layout without being in the focus graph
(notification, completer). A component is always in the layout and
never in the focus graph (status bar, decorative bars). This
decoupling is what resolves "should chrome be a window?" — layout
participation and focus participation are separate choices.

### Dialogs are stacks of panels, panels are windows

A dialog is a **compositor float window** containing a vertical stack of
**panels**. Every panel is a real `ui::Window` backed by a `ui::Buffer`.
There is no separate "dialog content" type — panels are windows, and
windows have cursor, scroll, vim, selection, kill ring, mouse routing
for free.

```
────────────────────────────────────────   ← top rule (solid ─, accent color)
 edit_file: src/foo.rs                      ← title panel (Content, Fixed height)
  making a small diff                       ← summary panel (Content, Fit)
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌   ← dashed ╌ separator between panels
   12  │ fn foo() {                         ← preview panel (Content, Fill)
   13- │     old_line();                      full vim + selection + scrollbar
   13+ │     new_line();                      on the right edge of the panel
   14  │ }
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
 Allow edits?                               ← action prompt (Content, Fixed)
  1. Yes                                    ← options panel (List, Fit)
  2. Yes + always                             mouse click + wheel scroll work
  3. No                                       LineDecoration paints selection
  type message here…                        ← msg input panel (Input, Fit/Fixed)
                                              — shown only when user starts typing
 ENTER confirm · m add msg · j/k scroll · ESC cancel   ← hints (StatusBar, Fixed 1)
```

**Chrome** is drawn by the `Dialog` component, not by the panels:
- Top rule: one accent-colored row of `─` across the dialog's rect.
- Dashed `╌` separators between panels (per-panel config).
- Hints row: a `StatusBar` component at the bottom, not a bespoke
  string pair. Left/right segments, dim style.
- Background: solid black fill across the dialog's rect.
- No side or bottom edges. No border box. The terminal's bottom rows
  visually *become* the dialog.

**Placement** lives on `FloatConfig`:
```rust
pub enum Placement {
    DockBottom { above_rows: u16, full_width: bool },
    Centered { width: Constraint, height: Constraint },
    AnchorCursor { width, height },
    Manual { anchor, row, col, width, height },
}
```
Built-in dialogs default to `DockBottom { above_rows: 1, full_width: true }`
(one row gap above the status bar). Completer uses `AnchorCursor`. Lua
floats default to `Centered`.

### Panels

```rust
pub struct DialogPanel {
    pub win: WinId,                          // a real ui::Window
    pub kind: PanelKind,
    pub height: PanelHeight,
    pub separator_above: Option<SeparatorStyle>,
}

pub enum PanelKind {
    /// Readonly or static text. Supports vim motions, visual selection,
    /// click-drag, mouse scroll. Used for titles, summaries, previews.
    Content,
    /// Selectable rows (cursor line = selected).
    /// LineDecoration paints the selection highlight. Mouse click moves
    /// cursor; Enter selects. Multi-select via per-line metadata.
    List { multi: bool },
    /// Editable buffer. Single-line for searches, multi-line for
    /// message/confirm textareas. Same window/buffer as the prompt.
    Input { multiline: bool },
}

pub enum PanelHeight {
    Fixed(u16),   // title, hints, prompt bars
    Fit,          // shrink to content (short lists)
    Fill,         // take remaining space (preview)
}
```

**Selection rendering**: the List panel reads its window's cursor line
and applies `LineDecoration::fill_bg` to that line via the same
mechanism the transcript uses for line highlights. No "selected row"
concept separate from "cursor line."

**Scrolling**: each panel draws its own scrollbar on its right edge
from `WindowViewport` / `ScrollbarState` — the same state the
transcript and prompt use. Inline `[x/y]` scroll readouts in the
legacy dialog chrome are deleted; the scrollbar is the readout.

**Focus**: `Dialog` owns the focused panel index. Tab cycles forward,
Shift-Tab backward. Mouse click on a panel focuses it. The focused
panel receives keys via `Compositor::handle_key` routing.

| Dialog | Panels (top → bottom) |
|---|---|
| help | title | keybinding-list |
| rewind | title | turn-list |
| export | title | options-list |
| resume | title | search-input | session-list |
| permissions | title | entries-list (section headers via LineDecoration) |
| ps | title | process-list |
| agents list | title | agent-list |
| agents detail | title | prompt-content | tool-calls-content |
| confirm (bash) | title+body-content | options-list | msg-input (when used) |
| confirm (edit) | title | summary | preview-content | action | options-list | msg-input |
| question | title | question-text | options-list | msg-input |
| completer | suggestion-list |
| cmdline | prompt-input |
| notification | text-content |

### Shared rendering for diffs and code

Diff `-`/`+` coloring, bash syntax highlighting, search-match highlights
all project into `ui::Buffer` as `Span` / `SpanStyle` and per-line
`LineDecoration`. The same buffer rendering that the transcript uses
draws confirm previews, code diffs, notebook previews, and search-
result highlights. Lua plugins get the same highlight API.

**The pipeline already exists.** The legacy preview renderers —
`print_inline_diff`, `render_notebook_preview`, `print_syntax_file`,
`BashHighlighter::print_line`, `render_markdown_inner` — all write
to a generic `LayoutSink` trait. `SpanCollector: LayoutSink` in
`render/layout_out.rs` accumulates their output into a
`DisplayBlock` (lines + styled spans + fill-bg decorations).
`transcript_buf.rs::project_display_line` + `apply_to_buffer`
project that `DisplayBlock` into a `ui::Buffer` — theme-resolved
`SpanStyle` highlights + `LineDecoration`.

Confirm's preview migration is therefore **not** a renderer
rewrite. It's:

1. Promote the transcript's `project_display_line` + `apply_to_buffer`
   helpers to a reusable module (`render/to_buffer.rs` or
   public on `BufferProjection`).
2. For each preview variant, at dialog-open time: create a
   `SpanCollector`, call the existing renderer against it, finish
   into a `DisplayBlock`, project into a fresh `ui::Buffer`.
3. Plant the buffer in `Ui::bufs` and open the Confirm dialog
   with a `PanelSpec::content(preview_buf, PanelHeight::Fill)`
   panel.

The buffer is static after creation (previews never update
post-open), so no re-projection loop is needed. Scrolling, cursor,
selection, and the scrollbar come from `ui::Window` +
`ui::BufferView` with zero dialog-specific code.

### Reuse inventory

This model works because the dialog framework is almost entirely
reuse. Components that carry real weight inside panels:

| Reused | Used for | Already exists |
|---|---|---|
| `ui::Window` | every panel's interaction state | yes |
| `ui::Buffer` | every panel's content | yes |
| `WindowView` (tui) | panel draw + scrollbar + cursor + hit-test | yes (steps 6g–6j) |
| `ui::Viewport::hit` | panel mouse routing | yes |
| `ui::Vim` | normal/visual/yank on any panel | yes |
| `ui::WindowCursor` | mouse-drag selection in panels | yes |
| `LineDecoration` | selected-row bg, diff bg, section headers | yes |
| `SpanStyle` | bash/diff/search highlight | yes |
| `StatusBar` | dialog hints row | yes |
| `EditBuffer` + `KillRing` + `UndoHistory` | Input panels | yes |
| `LayoutTree` | panel-height resolution inside the float rect | yes |

Retired as part of the panel rewrite:
- `FloatDialog` (renamed to `Dialog`, rewritten around panels)
- `ListSelect` — a List panel is just a buffer with cursor and
  LineDecoration; no separate struct.
- `TextInput` — an Input panel is a small editable window; the
  prompt's path already supports everything TextInput did plus vim.
- `FloatDialogConfig::{hint_left, hint_right, hint_style, footer_height}`
  — replaced by Dialog's StatusBar hints row and per-panel height.
- `paint_completer_float`, `render/completions.rs` draw path,
  `render/cmdline.rs` — completer and cmdline become Dialogs.
- Inline `[x/y]` scroll position rendering in confirm chrome —
  replaced by the buffer scrollbar.

### Widgets inside panels

`PanelKind::{Content, List, Input}` covers simple dialogs but
can't express composite UX (tabs, multi-select with chord keys,
syntax-highlighted preview, reason-message textarea). Instead of
multiplying kinds per UX idea, a panel holds **either a buffer or
a widget**:

```rust
enum PanelContent {
    Buffer(BufId),                // current — 6 migrated dialogs use this
    Widget(Box<dyn PanelWidget>), // custom rendering + keys
}

pub trait PanelWidget {
    fn prepare(&mut self, rect: Rect, ctx: &DrawContext) {}
    fn draw(&self, rect: Rect, grid: &mut GridSlice<'_>, ctx: &DrawContext);
    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult;
    fn cursor(&self) -> Option<(u16, u16)> { None }
    fn as_any_mut(&mut self) -> &mut dyn Any;
}
```

`ui::Dialog` still owns outer chrome (accent rule, hints, dismiss,
focus between panels). A widget-panel delegates draw/keys to its
widget; a buffer-panel behaves as today.

**Shipped widgets** (`crates/ui/src/widgets/`):
- `TextInput` — wraps a `Window` + `Buffer`, reuses kill ring / vim
  / undo. For Question's "Other:" and Confirm's reason message.
- `OptionList` — single- or multi-select; per-item checkbox glyph;
  chord-key map for pre-default keybinds (Confirm's `a` / `n` /
  `e` / `l`).
- `TabBar` — single-row label strip for multi-question headers.

Previews (Confirm's inline diff / notebook diff / syntax-highlit
file / markdown) are **not** widgets. They are buffer-backed
`Content` panels whose buffer is populated once, at open time, via
the legacy renderer → `SpanCollector` → `DisplayBlock` →
`Buffer` pipeline (see "Shared rendering for diffs and code"
below). Scrollbar, viewport, selection, and vim motions come for
free from `ui::Window` + `ui::BufferView`. No renderer code
changes.

Dialog authors can also write ad-hoc widgets inline.

### Escape hatch: bare Component floats

For truly custom floats (image viewer, mini-editor, dataviz), a
dialog isn't needed. `Ui::add_float_component(rect, zindex, Box<dyn
Component>)` registers a raw compositor layer — no chrome, no
panels. This is the Neovim-parity story: "draw whatever you want
in a window." Most dialogs still use `Dialog` + widgets because
chrome consistency is worth the small ceremony.

### Dialog simplifications

Two legacy patterns die as part of migrating to the widget model:

- **Question's tab state machine** (`active_tab`, `visited`,
  `answered`, `multi_toggles`, `other_areas`, `editing_other`)
  becomes a sequential wizard: one question per Dialog, advance on
  answer. Or — if multi-question display is valuable — a single
  Dialog with a `TabBar` widget on top.
- **Agents dialog two-mode design** (list ↔ detail with mode
  switch) becomes two separate Dialogs — already done
  (`d17ba9b`). Selecting an agent closes the list dialog and opens
  a detail dialog.

### LuaTask runtime — one suspend mechanism

Before looking at dialogs specifically: every "imperative-wants-to-wait"
Lua API in this codebase has the same root need — suspend the
plugin, drive Rust-side work, resume with a result. Today we have
three parallel half-solutions (`PendingOp::ResolveToolResult`,
`callbacks: HashMap<u64, LuaHandle>`, declarative `confirm = {...}`
specs) because there is no real suspend primitive. We replace all
three with one.

**Design: coroutine-driven tasks via `mlua::Thread`.** Any handler
that needs to wait runs inside a `mlua::Thread`. A yielding API
yields a typed request, Rust handles it, Rust resumes the thread
with the answer. Plugin code reads as synchronous.

```rust
// crates/tui/src/lua/task.rs (new)
pub enum LuaYield {
    OpenDialog(DialogSpec),     // resumed with { action, option_index, inputs }
    EngineAsk(AskRequest),      // resumed with response string
    Sleep(Duration),            // resumed with ()
    // future: Input, Confirm, FileSelect, Pick, ...
}

pub struct LuaTask {
    thread: mlua::Thread,
    on_complete: Option<Box<dyn FnOnce(mlua::Value)>>,
}

pub struct LuaTaskRuntime {
    active: Vec<LuaTask>,
    // keyed waits: dialog_id → task_index, ask_id → task_index, etc.
}
```

`LuaTaskRuntime::drive` runs each frame: resumes any task whose
wait was just satisfied, collects the new yield (if any), dispatches
it (compositor push / engine.ask enqueue / timer register) and
re-parks the task. When a task `return`s from its top-level function
the result is handed to `on_complete` (for tool execute, that's
"deliver result to engine").

**Lua side: one yielding primitive per concern, plus `smelt.task()`.**

```lua
-- Yielding primitives (only callable from inside a task):
local r         = smelt.api.dialog.open({...})
local response  = smelt.api.engine.ask_sync({...})
smelt.api.sleep(200)

-- Kickoff from a sync context (keymap / on_select / event hook):
smelt.task(function()
  local ok = smelt.api.dialog.open({...})
  if ok.action == "approve" then ... end
end)
```

**Which handlers become tasks:**

| Handler                                | Runs as task | Can yield |
|----------------------------------------|:------------:|:---------:|
| `tool.execute`                         | always       | yes       |
| `engine.ask(..., on_response)`         | internally   | yes       |
| `smelt.task(fn)` kickoff               | always       | yes       |
| `smelt.defer(ms, fn)` / `sleep`        | internally   | yes       |
| keymap / cmd handler                   | no           | no*       |
| `on_select`, `on_dismiss`              | no           | no*       |
| `mode_change`, `model_change`, `input_submit` | no    | no*       |
| `statusline`                           | no (hot path)| no        |
| `tool_start`, `tool_end`               | no           | no*       |

\* Sync handlers can spawn work with `smelt.task(fn)` — they don't
yield themselves, but they can fire-and-forget a task that does.

Yielding from a non-task context is a Lua error with a clear
message (`"smelt.api.dialog.open: call from inside smelt.task(fn) or tool.execute"`).

**What collapses:**

1. `PendingOp::ResolveToolResult`, the `callbacks: HashMap<u64, LuaHandle>`
   registry, and `smelt.api.tools.resolve(request_id, call_id, result)`
   → **deleted.** Tool `execute` returns naturally; if it needs to
   wait mid-flight it yields.
2. `smelt.api.engine.ask({..., on_response = fn})` — callback form
   kept as a thin wrapper (`smelt.task(function() fn(ask_sync(req)) end)`),
   but the ID-plumbing disappears internally.
3. `smelt.defer(ms, fn)` — callback form kept, `sleep(ms)` is the
   new primitive. `timers: Vec<(Instant, LuaHandle)>` reused as the
   internal wait queue for sleeping tasks.
4. Declarative `confirm = {...}` block — deleted; plugin calls
   `smelt.api.dialog.open` from its `execute` (which is already a
   task).

**Cross-cutting benefits:** every future interactive API (input
prompt, picker, confirm-with-reason, multi-step wizard, git log
browser, diff picker) becomes **one new `LuaYield` variant** — no
new callback registry, no new declarative spec. The architecture
doesn't grow per feature.

**Scope for 11a:** land the runtime with exactly two yield variants
— `OpenDialog` and `Sleep` — plus `smelt.task()` and the sync-side
`smelt.api.dialog.open`. `EngineAsk` and the follow-up consolidation
of `callbacks` / `ResolveToolResult` / declarative `confirm` come
in Phase 8 against the same runtime, no further architecture
needed. Migrate `plan_mode.lua` off `confirm = {...}` in 11a as
the end-to-end proof.

### Lua-driven dialogs

`smelt.api.dialog.open` is the first `LuaYield` variant. It mirrors
the Rust `ui::Dialog` primitives 1:1:

```lua
-- Yields inside a task; returns when the user resolves the dialog.
local result = smelt.api.dialog.open({
  title    = "plan · Implement?",
  accent   = smelt.api.theme.accent(),     -- not hardcoded ansi 79
  panels   = {
    { kind = "markdown", text = args.plan_summary },
    { kind = "options", items = {
      { label = "yes, and auto-apply", action = "approve",
        on_select = function() smelt.api.engine.set_mode("apply") end },
      { label = "yes", action = "approve" },
      { label = "no",  action = "deny" },
    }},
  },
})
-- result = { action = "approve" | "deny" | "dismiss",
--            option_index = N, inputs = { [name] = "text", ... } }
```

Mapping onto the existing framework:

| Lua `kind`  | Rust panel                                     |
|-------------|------------------------------------------------|
| `content`   | `PanelContent::Buffer` with plain text         |
| `markdown`  | `PanelContent::Buffer` filled via `render_into_buffer(..render_markdown_inner..)` |
| `diff`      | `PanelContent::Buffer` filled via `print_inline_diff` |
| `code`      | `PanelContent::Buffer` filled via `print_syntax_file` |
| `options`   | `PanelContent::Widget(OptionList)`             |
| `input`     | `PanelContent::Widget(TextInput)` — value returned in `result.inputs[name]` |

Callback-style `on_select` on an option remains available for
side effects (e.g. mode switch *before* the dialog closes); the
main flow value comes back through the task resume.

`plan_mode.lua` becomes a clean end-to-end example: tool `execute`
is auto-run as a task, it calls `smelt.api.dialog.open`, awaits the
answer, saves the plan on approve, and returns the result string to
the engine — no special-case bridge between Lua config and Rust
rendering. The `confirm = {...}` block, `PluginConfirmMeta`,
`PluginConfirmSpec`, `ConfirmPreview::PluginMarkdown`,
`preview_field`, and plugin-branch `accent_color` plumbing all go
away. The core Confirm dialog handles only built-in tool approvals
(edit_file, bash, etc.).

### Theme access from Lua

Plugins must not hardcode ansi values. The theme API exposes both
**read** and **write** access to every theme role:

```lua
-- Read the live accent color (honors light/dark terminal).
local c = smelt.api.theme.accent()      -- { ansi = 208 } or { rgb = {r,g,b} }

-- Read any named role: "accent" | "slug" | "user_bg" | "code_block_bg"
--                   | "bar" | "tool_pending" | "reason_off" | "muted"
local m = smelt.api.theme.get("muted")

-- Snapshot all roles at once (matches `theme::Theme`).
local t = smelt.api.theme.snapshot()

-- Set a role. Accepts ansi (0..255), rgb triple, or preset name.
smelt.api.theme.set("accent", { ansi = 79 })
smelt.api.theme.set("accent", { preset = "sage" })

-- Terminal brightness detection.
local light = smelt.api.theme.is_light()
```

Implementation sits on top of `crates/tui/src/theme.rs` (already
atomic, already snapshotable). Setters update the atomic; a theme
snapshot is taken once per paint pass so a single frame stays
consistent. Any existing `theme::set_*` helper that isn't yet
exposed gets a Lua wrapper; roles with no setter (e.g. derived
backgrounds) stay read-only.

Plugins receive theme colors as table literals (`{ ansi = u8 }`
or `{ rgb = {r, g, b} }`), the same shape Lua already uses for
`accent_color` in the old tool-confirm spec. Rust converts to
`ColorValue` at the API boundary, not in plugin code.

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

### Dialog (panel stack)

`Dialog` is the single compositor component behind every built-in
dialog, completer, cmdline, notification, and Lua float. It owns a
vertical stack of `DialogPanel`s and draws the chrome around them.

- **Each panel is a real `ui::Window` backed by a `ui::Buffer`.**
  Not a view. Not a component-with-a-buffer. A window. It has a
  cursor, scroll, vim state, selection anchor, kill ring, modifiable
  flag. Mouse, keyboard, selection, copy all work because the window
  machinery already handles them.
- **`WindowView` draws each panel.** The same component used for
  transcript and prompt. Scrollbar on the right edge, cursor overlay,
  viewport hit-testing — all free.
- **Chrome = top `─` rule + dashed `╌` separators + `StatusBar`
  hints row + solid bg fill.** No borders, no sides. Legacy design
  language, new plumbing.
- **Placement** lives on `FloatConfig`. Built-in dialogs default to
  `DockBottom { above_rows: 1, full_width: true }`.
- **Panel kinds**: `Content` (readonly text / preview), `List`
  (selectable rows via cursor line + LineDecoration), `Input`
  (editable line or multi-line).
- **Focus routing**: Tab / Shift-Tab cycles focused panel; mouse click
  focuses the hit panel; keys route through `Compositor::handle_key`
  to the focused window.

`ListSelect` and `TextInput` are retired. They were single-purpose
wrappers around what `ui::Window` already does.

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
| Transcript cursor/scroll/selection/vim | `ui::Window` | Focusable split window |
| Transcript tail-follow | `ui::Window::tail_follow` | Generic property; transcript sets true by default |
| Prompt editable text | `ui::Buffer` | Editable buffer, same type as transcript |
| Prompt cursor/scroll/selection/vim | `ui::Window` | Focusable split window |
| Buffer modifiability | `ui::Buffer::modifiable` + mirrored on `ui::Window` | Gates insert mode uniformly |
| Selection rendering | `ui::Window` / `WindowView::draw` | Reverse-video overlay painted by window, not per-surface |
| Dialog content | `ui::Buffer` per panel or widget-owned state | `PanelContent::Buffer` or `::Widget` |
| Dialog semantic state | `App.float_states[WinId]: Box<dyn DialogState>` | Take/put-back dispatch; per-dialog file |
| Dialog rendering/layout | `ui::Dialog` component + `Placement` config | Chrome and placement are framework; behavior is app |
| Dialog background | `ui::Dialog::draw` | Solid fill across dialog rect |
| Mouse z-order | `Compositor::handle_mouse` | Topmost layer at hit point consumes event |
| Completer (fuzzy finder) | Float `Window` + `Dialog`, `focusable = false`, `Placement::AnchorCursor` | Matches nvim-cmp pattern |
| Notifications | Float `Window`, `focusable = false`, ephemeral | Selectable by mouse / cmd; not in focus cycle |
| Queued messages | Float `Window`, `focusable = false` | Content selectable; never a focus target |
| Top / bottom prompt bars | `Component` (no buffer) | Pure decoration |
| Stash indicator | `Component` (no buffer) | Single-row label |
| Status bar | `StatusBar` component | Set at event time, not recomputed per frame |
| Cmdline | Float `Window` (single-panel Input, `focusable = true`) | Modal input, closes on Esc |
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

**Step 9.1 — New `Dialog` + `DialogPanel` framework (big-bang).**
Rewrite `ui::FloatDialog` as `ui::Dialog`:
- A dialog is a vertical stack of `DialogPanel`s, each one a real
  `ui::Window` backed by a `ui::Buffer`.
- `PanelKind { Content, List { multi }, Input { multiline } }`.
- `PanelHeight { Fixed(n), Fit, Fill }` — a `LayoutTree` over the
  dialog's float rect resolves panel rects.
- Chrome: top `─` rule (accent), dashed `╌` separators between
  panels, `StatusBar` hints row at the bottom, solid black bg fill.
  No border box.
- List panels render their selection by painting
  `LineDecoration::fill_bg` on the cursor line — same mechanism the
  transcript uses for line highlights.
- Every panel draws its own scrollbar via `WindowViewport` /
  `ScrollbarState`. Legacy inline `[x/y]` scroll readouts deleted.

Retired in the same commit:
- `FloatDialog` (superseded by `Dialog`).
- `ListSelect` (List panel = buffer + cursor + LineDecoration).
- `TextInput` (Input panel = small editable window — same code path
  as the prompt).
- `FloatDialogConfig::{hint_left, hint_right, hint_style,
  footer_height}` (replaced by the StatusBar hints panel and
  per-panel height).

**Step 9.2 — `Placement` on `FloatConfig`.**
```rust
pub enum Placement {
    DockBottom { above_rows: u16, full_width: bool },
    Centered { width: Constraint, height: Constraint },
    AnchorCursor { width: Constraint, height: Constraint },
    Manual { anchor: Anchor, row: i32, col: i32,
             width: Constraint, height: Constraint },
}
```
Built-in dialogs default to `DockBottom { above_rows: 1, full_width: true }`.
Completer → `AnchorCursor`. Lua floats → `Centered`.

**Step 9.3 — `Compositor::handle_mouse` with z-order hit-testing.**
Walks layers top-down; the topmost layer whose rect contains the
point consumes the event (click, drag, wheel). Inside a dialog, the
panel under the cursor receives the event via its window's
`Viewport::hit`. Fixes "wheel-over-dialog scrolls transcript" and
"click in Resume list doesn't select" and every other mouse-on-
dialog regression. `app/events.rs` stops hand-routing mouse events
to `Content`/`Prompt`.

**Step 9.4 — Unified behavior model: per-window keymaps + per-dialog trait. ✅ DONE**

All six built-in dialogs (Help, Export, Rewind, Permissions, Ps,
Resume) now live under `crates/tui/src/app/dialogs/`, each in its
own file with a struct + `impl DialogState`. `events.rs` lost the
`BuiltinFloat` enum, the `handle_builtin_float_*` dispatchers, the
permission-item / resume-filter free helpers, and every
`open_X_float` method — ~800 lines out of `events.rs`, ~900 lines
of focused per-dialog code in `dialogs/`. `intercept_float_key`
collapsed to the take/put-back dispatcher shown below.



The current shape — every built-in dialog's logic smeared across
`open_X_float`, `handle_builtin_float_select`, `intercept_float_key`,
and free helper fns at the bottom of `events.rs` — is Neovim-style
data primitives (Buffer, Window) paired with ad-hoc behavior glue in
an `App` god-object. Three parallel behavior mechanisms exist:

1. Legacy `render::dialogs::Dialog` trait (being retired).
2. `BuiltinFloat` enum + `intercept_float_key` +
   `handle_builtin_float_select` + `handle_builtin_float_dismiss`.
3. `lua::PendingOp` + `lua::Shared::callbacks` — already the right
   actor/effect shape, used only by Lua today.

**The end state: two behavior paths that share primitives.**

After extensive discussion (see the three options explored in the
plan history: A=keep enum, B=full `Rc<RefCell>`, C=per-dialog trait)
we settled on **C as the built-in path and a separate per-window
callback registry for Lua / pure-UI consumers**. The two paths share
`ui::Buffer`, `ui::Window`, `ui::Dialog`, `ui::Component` — the
*primitives* — but dispatch behavior differently based on whether
the dialog author has direct access to `App`.

**Path 1 — Built-in Rust dialogs use `DialogState` trait.**

One file per dialog under `tui/src/dialogs/`. Each file defines a
struct holding that dialog's state and `impl DialogState` for it.
Struct + logic + helper fns all sit together.

```rust
// crates/ui/src/component.rs (or a new dialog_state.rs in tui)
pub trait DialogState: 'static {
    /// Handle a key for this dialog's custom behavior. Returns
    /// `true` if consumed; `false` falls through to the Dialog
    /// component's default nav.
    fn handle_key(
        &mut self,
        app: &mut App,
        win: WinId,
        code: KeyCode,
        mods: KeyModifiers,
    ) -> bool;

    /// Fired when the Dialog emits `select:N` (Enter on a List).
    fn on_select(
        &mut self,
        app: &mut App,
        win: WinId,
        idx: usize,
        agent: &mut Option<TurnState>,
    ) {}

    /// Fired when the dialog is dismissed (Esc / Ctrl+C /
    /// configured dismiss key). Runs before `close_float`.
    fn on_dismiss(&self, app: &mut App, win: WinId) {}
}

// In App:
float_states: HashMap<WinId, Box<dyn DialogState>>,
```

The key routing in `App::dispatch_terminal_event` collapses to:

```rust
fn intercept_float_key(&mut self, code, mods) -> Option<ui::KeyResult> {
    let win = self.ui.focused_float()?;
    // Take the state out so the handler can borrow `&mut App`
    // without overlapping with the `float_states` table.
    let mut state = self.float_states.remove(&win)?;
    let consumed = state.handle_key(self, win, code, mods);
    self.float_states.insert(win, state);
    consumed.then_some(ui::KeyResult::Consumed)
}
```

Each dialog file looks roughly like `resume.rs`:
```rust
pub struct Resume {
    entries: Vec<ResumeEntry>,
    title_haystacks: Vec<String>,
    cwd: String,
    query: String,
    workspace_only: bool,
    filtered: Vec<usize>,
    pending_d: bool,
    content_cache: Option<HashMap<String, String>>,
    list_buf: BufId,
    title_buf: BufId,
}

impl Resume {
    pub fn open(app: &mut App, entries: Vec<ResumeEntry>) { /* builds bufs, panels, dialog */ }
    fn refresh_list(&self, ui: &mut Ui) { /* writes rows with dim spans */ }
    fn refresh_title(&self, ui: &mut Ui) { /* inline title + query */ }
    fn delete_selected(&mut self, app: &mut App, win: WinId) { /* ... */ }
}

impl DialogState for Resume {
    fn handle_key(&mut self, app, win, code, mods) -> bool { /* custom keys */ }
    fn on_select(&mut self, app, win, idx, agent) { /* load session */ }
}
```

`BuiltinFloat` enum dies; each variant becomes a struct in its own
file. `handle_builtin_float_select` and `handle_builtin_float_dismiss`
die (inlined into `DialogState::on_select` / `on_dismiss`).
`intercept_float_key` becomes the four-line dispatcher above.

Estimated shrinkage in `events.rs`: ~1500 lines out, ~100 in
(just the dispatcher + helpers used by more than one dialog).

**Path 2 — Lua + pure-UI consumers use the `ui::callback` registry.**

Already built (crates/ui/src/callback.rs). `Ui::win_set_keymap` and
`Ui::win_on_event` store Rust closures or `LuaHandle`s keyed by
WinId. Dispatch happens in `Compositor::handle_key` *before*
falling through to `Component::handle_key` (the fallback for
generic nav).

Lua plugins register via `smelt.ui.win_set_keymap(win, key, fn)`.
Rust callers that don't need `&mut App` (or that want Lua parity)
can register the same way. Built-in dialogs that *do* need App use
Path 1 instead — no acrobatics required to reach engine/session
state through a closure boundary.

Both paths can coexist on the same dialog. A built-in dialog can
implement `DialogState` for its chords and also register a Lua-style
callback for user-extensible hotkeys.

**Effects**

- Built-in dialogs (Path 1): mutate `&mut App` directly inside
  `handle_key` / `on_select` / `on_dismiss`. Typed, synchronous,
  obvious. This is where most built-in behavior lives.
- Lua / pure-UI callbacks (Path 2): push `PendingOp` variants via
  `CallbackCtx::ops`, drained by `App::apply_ops`. The reducer is
  shared with Lua today and keeps the clean "effects go through a
  queue" story for plugin authors.

**Key routing order**

`Compositor::handle_key` dispatches:

1. Focused window's keymap table from `ui::callback` (Path 2).
2. `Component::handle_key` fallback (generic nav: Tab / arrows /
   j / k / Enter for List-submit / Esc+Ctrl+C for dismiss).
3. If the component emits a `KeyResult::Action`, App interprets
   it. For built-in dialogs, that's where Path 1 hooks in:
   `select:N` → `DialogState::on_select`, `dismiss` →
   `DialogState::on_dismiss` + `close_float`.
4. If no handler consumed, App may also give the active
   `DialogState` a raw-key pass via `intercept_float_key` for
   custom chords (`dd`, `ctrl+w` search toggle, typed-char search
   input).

**Retroactive scope**

This step refactors the six already-migrated dialogs (Help, Export,
Rewind, Permissions, Ps, Resume) into per-dialog files under
`tui/src/dialogs/`, each owning its `DialogState` impl. Step 9.5
below then migrates the last three (Confirm, Question, Agents)
straight to the final shape.

**Lessons from the testing-driven iteration**

Real-device testing in a tmux pane (see "Testing interactive TUI
changes via tmux" above) surfaced several bugs inside the new
Dialog plumbing that unit tests missed. All fixed; record them so
future work doesn't re-introduce them:

- `Buffer::set_all_lines` used to early-return on `modifiable=false`,
  silently leaving every dialog buffer empty. `modifiable` now
  guards only interactive window edits, not framework API calls.
  Every builtin buffer creates with `modifiable: false` and is
  populated by `set_all_lines` without issue.
- `BufferView` now carries a `default_style` used as the cell
  fallback; `Dialog::new` and `sync_from_bufs` propagate the
  dialog's bg so panel glyphs stay readable on the dialog fill.
- `Dialog::prepare` synchronizes `panel.win.scroll_top` →
  `panel.view.scroll_offset` each frame. Without this, scrolling
  moved only the scrollbar thumb, not the visible rows.
- List selection is fg-accent retint on the cursor row. One shared
  mechanism across every dialog, no bg fill (bg fills hid content
  with stub `slice_cell` returning `' '` over real glyphs). The
  old "block cursor arrow" approach was mis-using absolute rects
  and got dropped.
- Every dialog reserves a 1-row gap above the hints row inside the
  layer itself (`hints_rows = 2` in `resolve_panel_rects`, hints
  painted at the bottom, blank row above) so the layout breathes.
- Ctrl+C is a built-in dismiss key, matching the legacy UX.

**Step 9.5 — Migrate the final three dialogs onto the unified model.**

**Agents — ✅ done** (2026-04-21). Lives at
`crates/tui/src/app/dialogs/agents.rs` as two `DialogState`s
(`AgentsList`, `AgentsDetail`) that swap via Enter (list → detail)
and Esc (detail → list, restoring the parent selection). Added a
`DialogState::tick()` hook + `App::tick_focused_float()` so the
agents dialog refreshes from live `SharedSnapshots` every
event-loop iteration. `DialogResult::AgentsClosed` variant removed;
dismissal runs `refresh_agent_counts` inline via `on_dismiss`.

Prep already done: the six other legacy dialog impls
(Export/Help/Permissions/Ps/Resume/Rewind/Agents) under
`render/dialogs/*.rs` are deleted, their `DialogResult` variants
removed, their dead arms pruned from `handle_dialog_result`.
`PermissionEntry` moved to `render::history`. Only Confirm and
Question remain on the legacy `render::Dialog` trait.

---

### Step 9.5b — Implementation order for Confirm / Question migration

The widget architecture, escape hatch, chrome ownership, and
focusable-flag design all live under "Design principles" above.
This step is the sequenced landing plan. Each item is one commit,
independently reversible.

**Foundations (landed):**

1. ✅ **Foundation A — blocking semantics.** `DialogState::blocks_agent()`
   gates engine-drain loop on the focused float. Commit `588d1d2`.

2. ✅ **Foundation B — `FloatConfig.focusable: bool`.** Plumbed
   through `dialog_open` and `win_open_float`. Commit `fdc10db`.

3. ✅ **Foundation C — `PanelContent` + `PanelWidget`.** Panels
   hold either a `Buffer(BufId)` or a `Widget(Box<dyn PanelWidget>)`.
   All six migrated dialogs keep working. Commit `84dc825`.

**Projection pipeline (prerequisite for Confirm previews):**

4. **Buffer projection helper.** Promote `project_display_line` +
   `apply_to_buffer` out of `render/transcript_buf.rs` into a
   reusable helper (`render/to_buffer.rs`). API shape:
   ```rust
   pub fn render_into_buffer(
       buf: &mut ui::Buffer,
       width: u16,
       theme: &Theme,
       fill: impl FnOnce(&mut SpanCollector),
   );
   ```
   `fill` is handed a SpanCollector and calls any `LayoutSink`
   renderer (`print_inline_diff`, `render_markdown_inner`, etc.)
   against it. Helper finishes, projects, writes into buf.
   Transcript keeps using its existing path; new callers use
   the helper.

**Widgets:**

5. **Widget — `TextInput`** in `crates/ui/src/widgets/text_input.rs`.
   Wraps a `Window` + `Buffer`. Delegates keys to
   `Window::apply_action`. Exposes `text()`, `clear()`,
   `append(&str)`.

6. **Widget — `OptionList`** in `crates/ui/src/widgets/option_list.rs`.
   Single- or multi-select. Checkbox glyph when multi. Chord-key
   map for pre-default keybinds (Confirm's `a` / `n` / `e` / `l`).
   Rows carry optional per-item description strings for Question.

7. **Widget — `TabBar`** in `crates/ui/src/widgets/tab_bar.rs`.
   Single-row label strip. Left/Right arrows to switch tabs;
   emits `Action("tab:N")`. For Question's multi-question header.

**Escape hatch (optional, backlog):**

8. **`Ui::add_float_component(rect, zindex, cfg, Box<dyn Component>)`.**
   Raw compositor layer for oddball custom floats. Not needed to
   migrate Confirm/Question but useful for future Lua plugins.

**Migrations:**

9. **Migrate Question** → `crates/tui/src/app/dialogs/question.rs`.
   Panels: `[TabBar (widget, if >1), Header(Content), Options
   (OptionList widget), Other(TextInput widget, when Other row
   selected)]`. `impl DialogState for Question`. `blocks_agent()
   = true`. Emits answer via `App::resolve_question`.

10. ✅ **Migrate Confirm previews** — `ConfirmPreview::render_into_buffer`
    projects each variant (`Diff`, `Notebook`, `FileContent`,
    `BashBody`) through `render/to_buffer::render_into_buffer`. No
    changes needed to `print_inline_diff`, `render_notebook_preview`,
    `print_syntax_file`, `BashHighlighter`. `PluginMarkdown` deleted.

11a. **LuaTask runtime + dialog/theme APIs + drop plugin_confirm.**
    Five small commits, landing in order:

    **(i) LuaTask runtime.** New module `crates/tui/src/lua/task.rs`
    with `LuaTask`, `LuaYield::{OpenDialog, Sleep}`, and
    `LuaTaskRuntime::drive(&mut self, ctx)`. Driver resumes tasks
    whose waits are satisfied, collects next yield, dispatches
    (dialog → compositor, sleep → timer queue), reparks. Panic /
    error handling: task errors produce a `NotifyError` op; session
    shutdown drops all tasks. Add `smelt.task(fn)` Lua function that
    spawns a task from a sync context. Tests: spawn task, yield
    `Sleep`, resume after N ms; spawn task that errors; spawn task
    that returns a value into `on_complete`.

    **(ii) Theme API.** Add `smelt.api.theme.{accent,get,set,snapshot,is_light}`
    on top of `crates/tui/src/theme.rs`. Color shape is
    `{ ansi = u8 } | { rgb = {r,g,b} } | { preset = "sage" }`.
    Pure sync API — no task needed. Tests: read each role, set
    accent by ansi / preset / rgb, verify snapshot consistency.

    **(iii) Tool execute as task.** Wrap `execute_plugin_tool` so
    the Lua handler runs inside a `LuaTask`. Return value flows to
    `on_complete` which resolves the tool call. Back-compat: if the
    handler never yields, behavior is identical to today. Tests:
    existing `plan_mode` sync path still works.

    **(iv) `smelt.api.dialog.open`.** First `LuaYield::OpenDialog`
    wired end-to-end. Yielding from outside a task raises
    `"smelt.api.dialog.open: call from inside smelt.task(fn) or tool.execute"`.
    Build `DialogSpec` on the Rust side from the Lua table, push as
    a compositor dialog, resume task with
    `{ action, option_index, inputs }` when the user resolves.
    Tests: open dialog from task, verify resume with each action;
    open with `input` panel, verify `inputs[name]` round-trip.

    **(v) Delete plugin_confirm plumbing + migrate plan_mode.**
    Delete `PluginConfirmMeta`, `PluginConfirmSpec`,
    `PluginConfirmOption`, `ConfirmPreview::PluginMarkdown`,
    `preview_field` extraction, `plugin_confirm` on `ConfirmRequest`,
    `is_plugin` / plugin-branch `accent_color` in legacy Confirm
    renderer, `confirm = {...}` parsing in `crates/tui/src/lua.rs`,
    `get_confirm_meta`, `invoke_confirm_callback`. Rewrite
    `runtime/lua/smelt/plugins/plan_mode.lua` to open the dialog
    imperatively from inside `execute`. Update
    `plan_mode_shows_confirm` integration test.

    Lands **before** item 11 so the new `app/dialogs/confirm.rs`
    never inherits plugin branches. Phase 8 later extends the
    runtime with `EngineAsk` and collapses `callbacks` /
    `ResolveToolResult`.

11. ✅ **Migrate Confirm dialog** → `crates/tui/src/app/dialogs/confirm.rs`.
    Panels: `[Title(Content Fit, not focusable),
    Summary(Content Fit, collapse_when_empty),
    Preview(Content Fill, dashed separator, collapse_when_empty),
    Options(OptionList widget Fit, default focus),
    Reason(TextInput widget Fit, collapse_when_empty)]`. PageUp/Down
    scroll the preview (via new `Dialog::scroll_panel` public helper);
    `e` focuses the Reason input. `blocks_agent() = true`. Deferred-
    open via `pending_dialogs` queue, now gated by
    `focused_float_blocks_agent` instead of legacy `active_dialog`.
    Handles built-in tool approvals only; plugin tools use
    `smelt.api.dialog.open`. Trade-off: the inline-textarea-per-option
    UX from the legacy dialog is replaced by a dedicated Reason panel
    (simpler framework fit).

**Cleanup:**

12. ✅ **Delete legacy dialog infra.** Landed with the Confirm
    migration:
    - `ConfirmDialog` struct + legacy render fn gone from
      `render/dialogs/confirm.rs` (only `ConfirmPreview` remains as a
      pub(crate) data holder).
    - `QuestionDialog` struct gone from `render/dialogs/question.rs`
      (only `Question`, `QuestionOption`, `parse_questions` remain).
    - `trait render::Dialog`, `render::DialogResult`,
      `render::dialogs::{TextArea, begin_dialog_draw,
      finish_dialog_frame, render_inline_textarea, char_to_byte}`
      all deleted.
    - `App::active_dialog` local + `App::open_dialog`,
      `App::handle_dialog_result`, `App::finalize_dialog_close`,
      `App::render_frame`, `App::render_dialog`,
      `Screen::draw_viewport_dialog_frame`, `DialogPlacement`,
      and dead `OpenDialog` variants on
      `CommandAction`/`InputOutcome`/`EventOutcome` all deleted.
    - `DeferredDialog` kept (it's a queued-permission-request enum,
      not a legacy dialog trait).
    - `RenderOut` / `StyleState` / `Frame` / `paint_line` retained
      for Step 9.7.
    - Obsolete harness helpers (`open_confirm_dialog*`,
      `draw_dialog_tick`, `confirm_cycle`) and the two
      legacy-rendering integration test files
      (`dialog_lifecycle.rs`, `dialog_overlay_interaction.rs`) removed;
      new panel-based dialog tests land with the compositor-level
      harness in Step 9.7.

Bug fixes bundled with this step:
- Dialog dismiss restoring vim mode when Question was opened from
  Insert.
- Confirm preview double-rendering when tool-call `PreToolUse`
  updates arrive mid-dialog (previews are static post-open, so
  this goes away naturally).

**Step 9.6 — Migrate overlays to Dialogs.** Each overlay becomes a
one-panel `Dialog` on the compositor, with the focusable flag set
per the rules above.

| Overlay | Panel | Placement | `focusable` |
|---|---|---|---|
| Completer (fuzzy finder) | List (of matches) | `AnchorCursor` | `false` |
| Cmdline | Widget (`TextInput`) | `DockBottom` | `true` |
| Notification | Content (message) | `DockBottom`, ephemeral | `false` |
| Queued messages | Content (one line each) | `DockBottom` (above prompt) | `false` |

Completer specifically matches the **nvim-cmp** pattern: float
window backed by a buffer of result rows, `focusable = false` so
`<C-w>` skips it and the cursor never moves in. A `DialogState`
handles fuzzy filtering on typed characters (reading the
underlying prompt window's buffer delta) and `Enter` / `Tab` to
accept.

Cmdline is the one overlay that *is* focusable: it's a modal
input. Entering cmdline mode focuses it; Esc dismisses. No need
for a separate `Screen::cmdline` drawing path.

Deletes: `paint_completer_float`, `render/completions.rs` draw
path, `render/cmdline.rs`, `draw_prompt_sections` overlay
branches, `Screen::cmdline` drawing.

**Step 9.7 — Delete legacy rendering.** After 9.1–9.6 land, the
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

**Step 9.8 — Bug fixes on the unified path.** Each collapses to a
small, localized change once the seam is gone:
- **Selection** — `WindowView::draw` reads `window.selection_range()`
  and paints a generic reverse-video overlay into its grid slice.
  Dead `paint_visual_range`/`paint_transcript_cursor` in `screen.rs`
  go away with `Screen`. The `_visual` discard at `events.rs:1171`
  disappears (the range no longer needs to be threaded by hand).
  Dialog panels inherit the same selection mechanism because they're
  windows.
- **Prompt shift on newline** — prompt window's layer rect is
  bottom-anchored; height = `clamp(content_rows, 1..=max)`. Chrome
  (notification, queued, top/bottom bars) stacks as separate layers
  above it. Adding a line grows the prompt upward, doesn't shift it.
- **Click off-by-one** — `Viewport::hit` is the single authoritative
  coord translator. Every other `pad_left` subtraction goes away.
- **Scrollbar center-on-click** — `apply_scrollbar_drag` subtracts
  `thumb_size / 2` on a click outside the current thumb; drags
  inside the thumb preserve their grab offset. Applies to dialog
  panels too.

**Step 9.9 — `tail_follow` as a `ui::Window` property.**
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

**Step 9.10 — Delete `Screen`.** With the legacy render path gone,
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

### Seam check (2026-04-22)

Re-audit after Step 9.5b items 9–12 landed. Every dialog now runs
through the compositor panel framework (Confirm, Question, Agents,
Resume, Rewind, Permissions, Ps, Help, Export + Lua-driven dialogs).
`trait Dialog`, `DialogResult`, `active_dialog`, and
`Screen::draw_viewport_dialog_frame` are gone. The remaining legacy-
path surface:
- Completer popup — still drawn via `draw_prompt_sections` /
  `paint_completer_float`.
- Cmdline — custom overlay in `draw_prompt_sections`; no compositor
  float yet.
- Notification overlays — legacy overlay row.
- `Frame`, `RenderOut`, `StyleState`, `paint_line`, `queue_status_line`,
  `queue_dialog_gap` — still used by `render_normal` / prompt
  rendering; deletion scheduled for Step 9.7 after 9.6 migrates
  completer/cmdline/notifications.

Step 9.6 + 9.7 pick up from here.

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
buf.set_lines). Step 9.5b item 11a lands `smelt.api.dialog.*` and
`smelt.api.theme.*`. This phase adds the remaining operations:
- `buf.set_highlights`, `buf.add_virtual_text`, `buf.set_mark`
- `win.set_cursor`, `win.get_cursor`, `win.set_scroll`
- Port `predict.lua` (plan_mode.lua already migrated in 11a)
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
