# Neovim-style FFI — dialog/picker/keymap refactor

## Directives

- **Neovim-inspired first.** When in doubt, do what Neovim does.
  `nvim_open_win` returns a handle synchronously; callbacks fire with
  state mutable; plugins compose, core stays small. If a deviation
  improves the outcome, state why; otherwise, don't deviate.
- **Plans bend, code ships.** The plan can be wrong. When friction shows
  up, step back, reconsider approaches, pick the best one, keep going.
  The artefact that matters is the code at the end — not how we got
  there.
- **No cost calculus on effort.** Refactor size, migration difficulty,
  and time-to-land are not trade-offs. Only outcome quality counts:
  correctness, modularity, simplicity, maintainability.
- **Nothing is out of scope.** Any task can be split. "Too big" is not a
  blocker — break it down, ship checkpoints, keep going.
- **Commit meaningful chunks.** Each architectural step = one commit.
  Run `cargo fmt && cargo clippy --workspace --all-targets -- -D
  warnings && cargo nextest run --workspace` before committing. Green
  tree every commit.
- **Keep the plan current.** When a decision changes mid-flight — a
  better option appears, a step reorders, a deletion list grows — update
  this doc in the same commit that acts on the decision.
- **Don't stop to ask.** Execute the plan; adjust in the doc when a
  better path appears; commit and move on.

## End-state

Every dialog, picker, and transient UI in smelt is a Lua plugin composed
over `ui::Dialog` / `ui::Picker` / `ui::Cmdline` primitives. Lua plugin
code reads top-to-bottom and looks synchronous:

```lua
local buf = smelt.buf.create()
smelt.buf.set_lines(buf, { "hello" })
local win = smelt.ui.dialog.open({ panels = { ... } })
smelt.win.set_keymap(win, "enter", function() smelt.win.close(win) end)
local result = smelt.win.await(win, "submit")       -- only this yields
```

Rust owns the compositor, widgets, security-critical tools, and nothing
else. `crates/tui/src/app/dialogs/` is empty. The Lua FFI is a 1:1
mirror of the internal Rust API.

## The one real problem

mlua closures can only hold `Arc<LuaShared>`, never `&mut App`. So every
Lua call that needs to mutate compositor state either queues a "bridge"
UiOp or yields to wait for the reducer. Ten bridge variants exist:

```
OpenLuaDialog, OpenLuaPicker, OpenArgPicker,
WinBindLuaKeymap, WinBindLuaEvent, WinClearKeymap, WinClearEvent,
PickerSetSelected, PickerSetItems, PromptSetText
```

None of them carry semantic intent. They're borrow-checker workarounds.
They force every `dialog.open` / `picker.open` to yield for a `win_id`,
which in turn forces a `TaskWait::External` for resource acquisition
(distinct from waits for user input). `CompleterKind::ArgPicker` + its
bespoke event queue (`pending_arg_events`, `drain_arg_picker_events`,
`invoke_callback_value`) is ~400 LOC of Rust that exists purely to
avoid per-keystroke round-trips on prompt-docked pickers.

And a Ctrl-R class of bug: `smelt.keymap.set(mode, chord, fn)` stores
arbitrary strings; `run_keymap` looks up canonical forms ("n"/"i"/"v"
plus nvim-angle-bracket chords like `<C-r>`). Mismatches silently
fail. `history_search.lua` registers `"normal"/"insert"/"visual"` +
`"c-r"` — never hits.

## Architecture — TLS pointer ("C extension" model)

```rust
thread_local! {
    static APP: Cell<Option<NonNull<App>>> = Cell::new(None);
}

pub fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> R {
    APP.with(|cell| {
        let ptr = cell.get().expect("with_app called outside Lua entry");
        // SAFETY: pointer is set only at entry points that hold &mut App
        // exclusively. `with_app` is the only accessor; Lua is single-
        // threaded; no aliased borrow is live when a Lua fn runs.
        unsafe { f(ptr.as_ptr().as_mut().unwrap()) }
    })
}
```

Installed on entry to every `&mut App` site that drives Lua (tick loop,
task driver, callback invocation), cleared on exit. Lua bindings become
direct synchronous wrappers: `smelt.ui.dialog.open(opts)` calls
`with_app(|app| app.ui.dialog_open(...))` and returns the `WinId`.

The one subtlety: Lua callbacks registered on `ui::Callbacks` fire from
inside `ui.handle_key` / `ui.dispatch_event`, which hold `&mut Ui` — so
a Lua callback body that calls `with_app` would collide. Fix: **defer
Lua callback invocation out of the ui borrow scope**. The `LuaInvoke`
closure pushes `(handle, win, payload, panels)` onto a queue on
`LuaRuntime`; after `handle_key` returns, App drains the queue and
invokes each Lua fn with the TLS pointer set. Not a round-trip by a
tick — just a borrow release.

## What deletes

Bridge UiOps and their apply arms:
`OpenLuaDialog`, `OpenLuaPicker`, `OpenArgPicker`, `WinBindLuaKeymap`,
`WinBindLuaEvent`, `WinClearKeymap`, `WinClearEvent`,
`PickerSetSelected`, `PickerSetItems`, `PromptSetText`.

ArgPicker machinery:
`CompleterKind::ArgPicker`, `completer/arg_picker.rs`,
`ArgPickerHandles`, `ArgPickerKey`, `ArgPickerEvent`,
`pending_arg_events`, `drain_arg_picker_events`,
`PromptState::set_arg_picker`, `LuaRuntime::invoke_callback_value`,
`smelt.prompt._request_arg_picker`, `open_arg_picker` /
`build_arg_picker` in `lua/ui_ops.rs`.

Confirm in Rust:
`app/dialogs/confirm.rs`, `app/dialogs/confirm_preview.rs` (→ moves to
a Rust helper exposed as `smelt.confirm.build_preview_buf`),
`DomainOp::ConfirmBackTab`, `DomainOp::ResolveConfirm`.

Task infrastructure:
the `_request_open` / yield-for-win_id shape in `dialog.lua` /
`picker.lua` / `prompt_picker.lua` deletes. `TaskWait::External`
remains but only carries user-input waits.

## What lands

Rust:
- `with_app` helper (~30 LOC, isolated `unsafe`)
- Deferred Lua-callback invocation queue on `LuaRuntime`
- `smelt.confirm.build_preview_buf(tool, args) -> buf_id` primitive

Lua runtime files:
- `dialog.lua` / `picker.lua` / `prompt_picker.lua` rewritten against
  the sync API — each shrinks to roughly the result-yield dance only
- `confirm.lua` — the migrated Confirm dialog
- `cmd.lua` — H-sugar helper; `smelt.cmd.register("name", fn,
  { args, on_select, on_enter, stay_open })` auto-opens the prompt-
  docked picker when the command runs with no arg

Canonicalization (Ctrl-R and friends):
- `smelt.keymap.set(mode, chord, fn)` normalizes `mode` (accepts
  `"normal"/"n"`, `"insert"/"i"`, `"visual"/"v"`, `""`; case-
  insensitive) and `chord` (parse via `parse_keybind`, emit canonical
  nvim form via `chord_string`). Unknown mode or chord → Lua error at
  registration, not silent miss at dispatch.

## Migration order

1. **Canonicalize keymap + result shapes.** Fix Ctrl-R and the whole
   class of silent-miss registrations. Unify
   `prompt_picker.lua` / `picker.lua` / `dialog.lua` result tables to a
   consistent `{ action, index, item, inputs }` shape. Track
   `runtime/lua/smelt/prompt_picker.lua` in git. Green commit.
2. **TLS pointer + deferred callback invocation.** Add `with_app`,
   install at every Lua-entry site, move the `LuaInvoke` closure to a
   pending queue + drain-after-handle_key pattern. No Lua API surface
   change yet — infrastructure only. Green commit.
3. **Migrate Lua bindings to sync.** `smelt.ui.dialog.open`,
   `smelt.ui.picker.open`, `smelt.win.set_keymap`,
   `smelt.win.on_event`, `smelt.win.clear_keymap`,
   `smelt.win.clear_event`, `smelt.win.close`, `smelt.buf.*`,
   `smelt.prompt.set_text`, `smelt.ui.picker.set_selected`,
   `smelt.ui.picker.set_items`. Rewrite `dialog.lua` / `picker.lua` /
   `prompt_picker.lua` against sync API. Delete the ten bridge UiOps
   and matching apply arms. Green commit.
4. **Delete ArgPicker.** Remove `CompleterKind::ArgPicker` and the
   entire event-queue machinery. `prompt_picker.lua` now composes
   `ui::Picker + DockedAbove + text_changed + set_items` over the sync
   API. Five plugins (`model`, `theme`, `color`, `settings`,
   `history_search`) stay unchanged at the call site. Green commit.
5. **H-sugar.** Add `cmd.lua` helper; extend `smelt.cmd.register` to
   accept `{ args, on_select, on_enter, stay_open }`; five plugins
   shrink to declarations. Green commit.
6. **Migrate Confirm to Lua.** Add `smelt.confirm.build_preview_buf`;
   write `confirm.lua`; delete `app/dialogs/confirm.rs` + two
   DomainOps. Green commit.

## Progress

- **Step 1 — Canonicalize keymap.** Done (`bb3adce`). `smelt.keymap.set`
  normalizes `mode` + `chord` at registration, raises Lua error on
  unknown input, `prompt_picker.lua` now tracked.
- **Step 2 — TLS pointer + deferred callbacks.** Done (`bcf9fc4`).
  `crates/tui/src/lua/app_ref.rs` (NonNull<App> in TLS,
  `install_app_ptr`/`with_app`/`try_with_app`), `LuaRuntime` now queues
  `PendingInvocation`s via `queue_invocation` and drains them via
  `drain_lua_invocations` with two-phase prepare/call; TLS installed at
  top of the main loop. No API surface change yet.
- **Step 3 — Migrate Lua bindings to sync.** Done. `smelt.buf.*`,
  `smelt.win.*`, `smelt.prompt.set_text`, `smelt.ui.picker.*`, and
  `smelt.ui.dialog._open` now call `crate::lua::with_app(…)` directly;
  the nine bridge UiOps (`WinBindLuaKeymap`, `WinBindLuaEvent`,
  `WinClearKeymap`, `WinClearEvent`, `PickerSetSelected`,
  `PickerSetItems`, `OpenLuaDialog`, `OpenLuaPicker`, `PromptSetText`)
  plus the now-unused `BufCreate`, `BufSetLines`, `BufSetSource`,
  `BufAddHighlight` arms are gone. `dialog.lua` and `picker.lua` shrink
  to the final-result yield only. `OpenArgPicker` stays until Step 4.
- **Step 4 — Delete ArgPicker machinery.** Done.
  `CompleterKind::ArgPicker`, `completer/arg_picker.rs`,
  `ArgPickerHandles`/`Key`, `ArgPickerEvent`, `pending_arg_events`,
  `drain_arg_picker_events`, `invoke_callback_value`,
  `_request_arg_picker`, and the `UiOp::OpenArgPicker` op are all
  removed. `prompt_picker.lua` now composes `smelt.ui.picker._open`
  (prompt_docked) + `smelt.win.set_keymap(PROMPT_WIN, …)` for Up/Down/
  Enter/Tab/Esc + `on_event("text_changed")` for live re-filter via
  `smelt.fuzzy.score` + `smelt.ui.picker.set_items`. The five plugin
  callers (`model`, `theme`, `color`, `settings`, `history_search`)
  stay unchanged.
- **Step 5 — H-sugar.** Done. `runtime/lua/smelt/cmd.lua` layers
  declarative picker opts (`items`, `on_select`, `on_enter`,
  `on_dismiss`, `stay_open`) over `smelt.cmd.register`. The four
  arg-picker plugins (`theme`, `color`, `model`, `settings`) shrink
  to declarations; `history_search` still uses the lower-level
  `smelt.prompt.open_picker` directly because its tab-vs-enter
  restore logic doesn't match the generic shape.
- **Step 5b — Picker polish + cmd.picker split.** Done. Three
  follow-ups: (1) `prompt_picker.lua` direction inverted for
  `prompt_docked` (logical idx 0 sits at the bottom row, so Up/c-k/
  c-p move toward higher indices); the keys list and teardown loop
  cover all six chord variants. `to_picker_items` and the inner
  `all_items` now thread the explicit `prefix` field through, and
  filtering delegates to `smelt.fuzzy.rank` (matches the field-
  independent scoring of the deleted Rust ArgPicker). (2)
  `crates/tui/src/lua/api.rs` callback registration extracts a pair
  of helpers in `crates/tui/src/lua/mod.rs` —
  `register_callback_handle` (registry-value + atomic-id + insert)
  and `drop_displaced_lua_handle` (drop the Lua handle id stashed in
  a displaced `Callback::Lua`). The four `smelt.win.*` bindings
  shrink ~30 LOC. (3) `smelt.cmd.register` reverts to a pure
  passthrough; declarative picker behaviour moves to a separate
  `smelt.cmd.picker(name, opts)` with explicit `apply` (direct
  dispatch) / `prepare` (pre-open snapshot) hooks instead of the
  dual-mode `handler(arg|nil)`. `theme`, `color`, `model`, and
  `settings` migrate to the new entry point. Plugins also gain a
  visual `prefix = "● "` pill on theme/color items and lavender /
  lilac descriptions deduplicate to "cool purple" / "warm purple".
- **Step 6 — Rip out the `AppOp` queue.** Done. Lua bindings
  reached `&mut App` through a typed enum queue (`UiOp` /
  `DomainOp` → `App::apply_ops` reducer); now they call methods
  directly via `with_app`, and the queue payload reduced to plain
  `Box<dyn FnOnce(&mut App) + Send>` closures used by the one
  remaining Rust caller (Confirm dialog). Net delete of ~410 LOC
  across `ops.rs` (~150) + `ops_apply.rs` (~230) + the
  `push_op!` / `snap_read!` macros plus the redundant reducer
  variants in `lua/api.rs`. Landed in seven sub-steps:

  - **6.1 Visibility bumps.** `apply_model`, `set_mode`,
    `set_settings`, `toggle_mode`, `set_reasoning_effort`,
    `sync_permissions`, `resolve_confirm`, `finish_turn`,
    `cancel_agent`, `send_permission_decision` and the
    `pending_quit` / `agent` fields go from `pub(super)` to
    `pub(crate)`. No behaviour change.
  - **6.2 UiOp → with_app.** `Notify`, `NotifyError`,
    `SetGhostText`, `ClearGhostText` (5 binding sites + 1
    `record_error` route through `smelt.notify_error`). Tests
    that observed via the queue now use a Lua override
    (`install_test_notify` writes to `_G.test_log` /
    `_G.test_err`).
  - **6.3 Simple DomainOp → with_app.** `SetModel`, `SetMode`,
    `SetReasoningEffort`, `Submit`, `Cancel`, `LoadSession`,
    `DeleteSession`, `KillAgent`, `KillProcess`,
    `YankBlockAtCursor`, `RemovePromptSection`,
    `SetPromptSection` (12 binding sites). Snapshot-queueing
    tests (`engine_*_queues_op`) deleted — they exercised the
    old contract.
  - **6.4 Complex DomainOp → wrapper methods.** Reducer arms
    that did multi-step work (`RunCommand`, `Compact`,
    `ToggleSetting`, `SyncPermissions`, `RewindToBlock`,
    `EngineAsk`, `ResolveToolResult`) lifted into
    `pub(crate) fn` methods on `App` in a new
    `crates/tui/src/app/lua_handlers.rs`. Lua bindings call
    them through `with_app`. Each method is named for the
    semantic (`apply_lua_command`, `toggle_named_setting`,
    `compact_or_notify`, `rewind_to_block`,
    `load_session_by_id`, `yank_current_block`,
    `sync_permissions_from_lua`, `engine_ask`,
    `resolve_tool_result`).
  - **6.5 Closure queue + Confirm migration.** `AppOp` enum,
    `UiOp`, `DomainOp` deleted. Replaced with `pub type
    Deferred = Box<dyn FnOnce(&mut App) + Send + 'static>`.
    `OpsHandle::push` now takes a closure. `confirm.rs`'s
    three callbacks (BackTab, Submit, Dismiss) push closures
    that call two new wrapper methods
    (`handle_confirm_back_tab`, `handle_confirm_resolve`).
    `fire_callback` and `pump_task_events` now return `()`
    (Lua bindings reach `&mut App` directly, no ops to
    propagate). `record_error` routes through
    `smelt.notify_error` so test overrides catch internal
    errors too.
  - **6.6 Snapshot-read migration.** Deferred. Lua bindings
    still read via `LuaOps`'s pre-tick snapshot fields
    (`transcript_text`, `engine`, `settings`,
    `available_models`, `input_history`); migrating each to a
    live `with_app` read churns ~20 readers + ~7 snapshot
    tests for marginal architectural gain. The snapshot is
    arguably correct caching — Lua bindings see consistent
    state during a tick. Park this until there's a concrete
    pain point that demands live reads.
  - **6.7 Inline `apply_ops`.** `ops_apply.rs` was a 12-line
    file with a 3-line method; folded into
    `lua_bridge.rs::apply_lua_ops` and deleted.
- **Step 7 — Migrate Confirm to Lua.** ✅ Landed.
  - **7.1 Confirm-request registry + Rust primitives.** Added
    `App::confirm_requests: HashMap<u64, ConfirmEntry>` keyed by
    handle id; rewrote `app/dialogs/confirm.rs` as buffer/option
    builders only (`build_title_buf`, `build_summary_buf`,
    `build_preview_buf`, `build_options`); created
    `lua/confirm_ops.rs` exposing 8 `smelt.confirm._*` primitives
    (`_build_*_buf`, `_option_labels`, `_scroll_preview`,
    `_focus_reason`, `_back_tab`, `_resolve`, `_info`).
  - **7.2 `runtime/lua/smelt/confirm.lua`.** Default
    `smelt.confirm.open(handle_id)` composes panels via
    `smelt.ui.dialog._open` (dock_bottom, 60% height) and wires up
    keymaps (page_up/down, e, s-tab) plus submit/dismiss handlers.
    Plugins can override the function for a custom UI.
  - **7.3 Agent.rs fires the Lua entry point.** The
    `RequestPermission` arm in `agent.rs:1379` allocates a handle,
    inserts a `ConfirmEntry`, and calls
    `self.lua.fire_confirm_open(handle_id)`. No Rust dialog setup
    survives.
  - **7.4 Delete Rust dialog orchestration.** Removed the old
    panel-builder / keymap-wiring code from `confirm.rs`, the
    matching `confirm_preview.rs` helpers, the `BackTab` /
    `Resolve` enum threading, and the `Decision`/`HashMap` imports
    that fell off.
  - **7.5 Delete `OpsHandle` / closure queue.** With Confirm
    migrated, no Rust caller queues deferred closures anymore.
    Deleted `crates/tui/src/app/ops.rs`, `pub mod ops;` in
    `app/mod.rs`, the `pub use Deferred/OpsHandle` re-export,
    `LuaOps.deferred`, `LuaOps::drain`, `LuaRuntime::drain_ops`,
    `LuaRuntime::ops_handle`, and the drain loop inside
    `apply_lua_ops`. The `apply_lua_ops` body is now just
    `drain_lua_invocations` + `pump_task_events`.
  - 358 tests passing, clippy clean. The neovim-FFI architecture
    is the only path now: Lua callbacks reach `App` directly via
    `with_app`; nothing routes through a typed effect log or a
    closure queue.
