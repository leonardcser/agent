# Refactor — sequencing the move to target architecture

**Sequencing plan only.** Meta-rules (greenfield / red-tree-OK /
friction handling / doc-sync / `P<n>.md` template / cold-start) live
in `README.md`. Target intent lives in `ARCHITECTURE.md` and the
puml. **The plan below is a sketch — better paths beat following it
to the letter; see `README.md`.**

## State of the gap (one paragraph)

`engine` and `protocol` are basically aligned with target — they need core
tools moved out (to Lua), `Mode` gating relocated (to Lua tool hooks), and
the entire `engine/permissions/` module pulled out to `tui::permissions`
(engine becomes policy-free; it emits `RequestPermission` and consumes
`PermissionDecision` and that's it).
`ui` needs a primitive rebuild: `Buffer` gains namespaces/extmarks/attach,
`BufferView`/`Component`/`PanelWidget`/`Surface` die, `Placement(6)` becomes
`splits` + `overlays`, `LayoutTree` becomes a real Vbox/Hbox/Leaf with
chrome, `Float` becomes `Overlay`, a `Theme` registry replaces constants.
`tui` needs `App` split into a headless-safe `Core` plus `TuiApp` /
`HeadlessApp` frontends, with subsystems carved out and a `Host` /
`UiHost` trait pair (`Host` for everything Ui-agnostic, `UiHost` for the
compositor-touching surface). It needs `Cells` as the unified reactive +
event-bus primitive (autocmds become subscriptions over the same
registry), real `Timers`, an `EngineBridge` struct, capability modules
(`parse`/`process`/`subprocess`/`fs`/`http`/`html`/`notebook`/`grep`/
`path`/`fuzzy`/`permissions`), and a Lua binding layout that's one
file per namespace. The Lua runtime needs the missing namespaces
(`cell`, `timer`, `au` sugar, `clipboard`, `parse`, `os`, `fs`,
`http`, `html`, `path`, `permissions`, `fuzzy`, `grep`, `subprocess`,
`frontend`, `mode` — renamed from `agent.mode`), the missing dirs
(`widgets/`,
`dialogs/`, `tools/`, `colorschemes/`), and the 15 Rust-side core tool
impls moved in from engine (with intricate logic — bash AST parsing,
mtime-checked edits, workspace rule matching — exposed via the
capability modules as FFI for the Lua tools to call).

Everything else is downstream of those.

## Phase ordering at a glance

```
P0  clear the deck                 ── delete what won't survive
        │
        ▼
P1  ui primitives                  ── Buffer, LayoutTree, Overlay, Window,
        │                              Theme, Ui facade, Host trait
        ▼
P2  tui app restructure            ── Core / TuiApp / HeadlessApp,
        │                              Host + UiHost, subsystems, Cells
        │                              (subsumes autocmds), Timers,
        │                              EngineBridge, WellKnown, ToolRuntime
        ▼
P3  rust capabilities + lua api    ── tui::parse/process/fs/http/...
        │                              /permissions, one file per smelt.*
        │                              namespace
        ▼
P4  lua takes the ux               ── transcript.lua, diff.lua, status.lua,
        │                              widgets/, dialogs/, colorschemes/
        ▼
P5  tools to lua                   ── 15 Rust tool impls to Lua, mode
        │                              gating becomes a Lua hook concern;
        │                              intricate logic via FFI capabilities
        ▼
P6  streaming + lifecycle polish   ── per-block callbacks, cooperative
        │                              cancel, confirms gate, cell-event
        │                              fan-out
        ▼
P7  finalize                       ── docs, examples, dead-code sweep
```

P1 is the load-bearing one. After it, P2/P3/P4 can interleave somewhat;
before it, nothing else has its target shape.

---

## P0 — Clear the deck

**Goal:** delete the things the target architecture removes, before we
build their replacements. This is intentionally destructive. Tree is red
at the end — that's fine.

- Delete `crates/ui/src/buffer_view.rs` and every `BufferView::new(...)`
  call site. Window will read `Buffer` directly after P1.
- Delete `crates/ui/src/component.rs` (`Component` trait, `WidgetEvent`).
  Window is the only interactive unit in the target.
- Delete `PanelWidget` trait and the panel-widget multiplexing inside
  `crates/ui/src/dialog.rs`. Dialogs become "Overlay + LayoutTree + N
  Windows" in P1.
- Delete the 6-variant `Placement` enum and the `add_layer` /
  `register_split` / `set_layer_rect` / `focus_layer` stringly-typed
  layer plumbing. Replaced by `splits: LayoutTree` + `overlays:
  Vec<Overlay>` in P1.
- Delete `crates/tui/src/theme.rs` constants module (tracked task
  `20260426-083607`). The theme registry replaces it.
- Delete the per-widget `selection_style` fields on `TextInput`,
  `NotificationStyle`, `DialogConfig`, `DrawContext`, `Compositor`,
  and the `Ui::set_selection_bg` / `Ui::selection_style()` shim. All
  selection styling reads `theme.get("Visual")` after P1.
- Delete `Ui::handle_mouse_with_lua` / `Ui::handle_mouse_for` /
  `classify_widget_action`. Mouse dispatch moves to App via `Host`
  in P1.
- Delete `MouseAction::Yank(String)` and `WidgetEvent::Yank(String)`.
  Yank goes through `host.clipboard().write()` directly.
- Delete `crates/ui/src/buffer_list.rs`. Any consumer that wants a
  list-of-buffers view in the new model builds it as N Windows in an
  Overlay during P4 — no separate widget type.

End of P0: `ui` doesn't compile. `tui` doesn't compile. The bones of
the target are visible because the noise is gone.

---

## P1 — UI primitives (the load-bearing phase)

**Goal:** rebuild `crates/ui` around the target's two-primitive model
(Buffer + Window) with two structures (splits + overlays) and a real
theme registry. Everything downstream of `ui` rides on this.

Sub-phases below can interleave but each must end at a coherent boundary.

### P1.a — `Buffer` rewrite

- Lines + namespaces + extmarks. Mirrors `nvim_buf_set_extmark`.
- `modifiable: bool` is the data-layer guard.
- `Buffer::attach(spec)` registers a parser (`markdown`/`diff`/`syntax`)
  with namespaces and an `on_block` callback fired at semantic
  boundaries (not per-chunk).
- `Extmark` gains `yank: Option<YankSubst>` where
  `enum YankSubst { Empty, Static(String) }`. `Empty` elides bytes the
  extmark covers; `Static(s)` substitutes them.
- `Buffer::yank_text_for_range(range)` is a pure helper that walks
  extmarks intersecting the range and applies their `YankSubst` (absent
  = literal source bytes). No buffer-level translator hook, no App-side
  branching. Hidden-thinking blocks attach `YankSubst::Empty`; prompt
  attachment sigils attach `YankSubst::Static(expanded_path)`.
- Soft-wrap state lives on Buffer keyed by `(content_tick, width)`.
  Multiple Windows on the same Buffer share the wrap result.
- Parsed metadata (LCS for diffs, syntect tokens for code) lives as
  extmarks in dedicated namespaces. No separate IR cache file.

### P1.b — `LayoutTree`

- `Vbox { gap, separator, border, title, items } | Hbox { ... } |
  Leaf(WinId)`.
- `Item = (Constraint, LayoutTree)`. Constraints: `Length / Percentage
  / Ratio / Min / Max / Fill / Fit`.
- Container nodes carry chrome: `gap: u16`, `separator:
  SeparatorStyle`, `border: Option<Border>`, `title: Option<String>`.
- Type system allows chrome on any container; convention restricts it
  to overlays.

### P1.c — `Overlay` replacing `Float`

- `Overlay { layout: LayoutTree, anchor: Anchor, z: u16, modal: bool }`.
- `Anchor::{ Screen(Center) | Screen { row, col } | Cursor(Edge) |
  Win { target, attach } }`.
- Drag = mutate the anchor.
- `Float`/`FloatId` go away. Overlays have an `OverlayId` for chrome
  hit-testing.

### P1.d — `Window` as the only interactive unit

- Cursor, scroll, selection, keymap recipe id, focusable flag,
  gutters.
- `render(buf, grid)`. `handle(event, ctx, host) -> Status`. No
  multiple traits, no `Component`/`PanelWidget`.
- **Vim state machine decomposes**, `crates/ui/src/vim/` and
  `window_cursor.rs` are deleted:
  - `VimMode` (`Normal/Insert/Visual/VisualLine`) lives on **App**
    as a single global field. Lua: `smelt.vim.mode`.
  - Registers, dot-repeat, undo → **Buffer** (per-buffer edit
    history).
  - Cursor / scroll / selection / Visual anchor → **Window**.
  - Kill ring → `Clipboard` subsystem (reached via `Host`).
- **Completer state machine decomposes**:
  - Ghost text → extmark in a `"completer"` namespace on the prompt
    Buffer.
  - Picker dropdown → an `Overlay` over a list Window.
  - Behaviour → keymap recipe on the prompt Window.
  - Existing `crates/tui/src/completer/` and
    `crates/tui/src/attachment/` collapse along these axes.

### P1.e — `Theme` registry

- `Theme { groups, links }` with `get`/`set`/`link`.
- Buffer extmarks reference highlight ids, never raw colors.
- Selection bg = `theme.get("Visual")`. One source of truth.

### P1.f — `UiHost` trait + `Status` + event types

`Host` itself is defined in `tui` (see P2.b) — it's the Ui-agnostic
trait. `ui` only needs the Ui-shaped subset:

- `UiHost: Host` (extension): `ui()`, `focus(WinId)`,
  `fire_win_event(WinId, WinEvent)`, `buf_create`/`buf_mut`,
  `win_open`/`win_close`/`win_mut`, `overlay_open`/`overlay_close`.
  `Window::handle` takes `&mut dyn UiHost`.
- No `request_redraw` — rendering is event-driven.
- `Event::{ Key | Mouse | Resize | Focus | Blur }`.
- `Status::{ Consumed | Capture | Ignored }`.
- `WinEvent::{ Open | Close | FocusGained | Lost | Submit | Dismiss
  | Select(idx) | TextChanged }`.
- `FocusTarget::Window(WinId)`. `HitTarget::{ Window(WinId) |
  Scrollbar { owner: WinId } | Chrome { owner: OverlayId } }`.

The `Host` super-trait lives in `tui` (alongside `Core`); `ui` declares
`UiHost: Host` and uses it for handler signatures. This keeps `ui`
agnostic of the `Host` body while letting handlers reach `lua()` /
`engine()` through the supertrait when they need to.

### P1.g — `Ui` facade

- `bufs: Map<BufId, Buffer>`, `wins: Map<WinId, Window>`,
  `splits: LayoutTree`, `overlays: Vec<Overlay>`,
  `focus: Option<WinId>`, `focus_history`, `capture: Option<HitTarget>`,
  `cursor_shape: CursorShape` (single global), `theme`.
- API: `buf_create`/`buf_mut`, `win_open`/`win_close`/`win_mut`,
  `overlay_open`/`overlay_close`, `dispatch_event`, `render`,
  `focus`/`set_focus`/`focused_window`/`focused_overlay`/
  `active_modal`, `hit_test`, `focus_next`/`focus_prev`
  (modal-aware).
- Render: event-driven, diff-based, no dirty flag. Resize/Ctrl-L
  zeros the previous-grid baseline; the diff becomes a full repaint
  by virtue of writing every cell.

End of P1: `ui` compiles in isolation. Has unit tests against fake
grids. `tui` is still red — it consumes the new shapes in P2.

---

## P2 — TUI App restructure

**Goal:** split `App` into a headless-safe `Core` plus `TuiApp` /
`HeadlessApp` frontends, carve subsystems out of the god-struct,
install the `Host` + `UiHost` traits, build `EngineBridge` and
`ToolRuntime` as real types, and introduce `Cells` (which also
subsumes the autocmd registry) + `Timers`. The 106-field god-struct
goes away.

Order within the phase: subsystem structs first (ownership boundaries),
then `Core` aggregates them, then `TuiApp` and `HeadlessApp` wrap
`Core`, then `Host` / `UiHost` impls, then the reactive layer (Cells +
Timers), then bridges (EngineBridge, ToolRuntime).

Headless is a first-class consumer of this split — there is no
"headless mode flag" inside the TUI; `HeadlessApp` is its own struct
that composes the same `Core`. The TUI binary builds `TuiApp`; the
headless binary entry point builds `HeadlessApp`. Sub-agent workers
(spawned by the agent tool) build `HeadlessApp` too.

### P2.a — `Core` + frontends + carve subsystems

Land each subsystem as its own struct, then aggregate:

- `AppConfig` — model/api/provider/settings/keymap/theme path.
- `Session { history, costs, turn_metas }`. (Sub-agent state lives
  in Lua cells fed by `tui::subprocess` `on_event` callbacks — see
  P5.b — not on `Session`.)
- `Confirms { requests: Map<HandleId, Sender<Decision>>, next_handle,
  is_clear() }`.
- `Clipboard` — platform sink + kill ring.
- `Timers { set, set_interval, cancel }` — real timer registry.
  Built-in `now` cell ticks here. Plugin timers live here.
- `Cells { Map<CellName, Box<dyn Any>>, new, get, set, subscribe,
  glob_subscribe }` — also the event bus (autocmds = subscriptions
  over named cells).
- `LuaRuntime` — plugin scripts, callback queue, coroutine tasks, TLS
  Host pointer. **No separate autocmd registry** — `smelt.au.*` calls
  through to `Cells`.
- `ToolRuntime { registry: Map<name, LuaTool> }` — own type, impls
  `engine::ToolDispatcher`.
- `EngineBridge { drains event_rx → host calls }` — own type, owns
  the `EngineHandle`.
- `WellKnown { transcript, prompt, statusline, cmdline }` — holds the
  well-known `WinId`s. **Lives on `TuiApp`, not `Core`** (headless has
  no Windows).

Aggregate:

```rust
struct Core {
    config:        AppConfig,
    session:       Session,
    confirms:      Confirms,
    clipboard:     Clipboard,
    timers:        Timers,
    cells:         Cells,
    lua:           LuaRuntime,
    tools:         ToolRuntime,
    engine_bridge: EngineBridge,
}
struct TuiApp      { core: Core, well_known: WellKnown, ui: ui::Ui }
struct HeadlessApp { core: Core, sink: HeadlessSink }
```

The TUI `main` builds `TuiApp` and runs its event loop. A
`smelt --headless` (or sub-agent worker) builds `HeadlessApp` and
runs the same loop, sans terminal events and sans `Ui` rendering.

### P2.b — `Host` + `UiHost` impls

Two traits live in `tui`:

- `Host` (Ui-agnostic): `clipboard / cells / timers / lua / tools /
  engine / session / confirms`. `Core` impls this.
- `UiHost: Host` (compositor-aware): `ui / focus / fire_win_event /
  buf_create / buf_mut / win_open / win_close / win_mut /
  overlay_open / overlay_close`. `TuiApp` impls this on top of its
  inner `Core`.

`HeadlessApp` impls only `Host` — calling a `UiHost`-only Lua binding
from headless raises a runtime error. Handlers in `ui` take
`&mut dyn UiHost`. The TLS pointer (`crate::lua::with_host` /
`with_ui_host`) exposes the right trait depending on the binding's
declaration. Subsystem-scoped borrows compose without fighting the
borrow checker.

### P2.c — `Cells` reactive layer + event bus

Cells is a single registry that doubles as the autocmd-style event
bus. Built-ins:

- Stateful: `now`, `spinner_frame`, `agent_mode`, `vim_mode`, `model`,
  `reasoning`, `confirms_pending`, `tokens_used`, `errors`, `cwd`,
  `session_title`, `branch`.
- Event-shaped (typed payload, no persistent state): `history`,
  `turn_complete`, `turn_error`, `confirm_requested`,
  `confirm_resolved`, `session_started`, `session_ended`.

`Cell::set` wakes the loop (one `select!` branch on the cells
channel) and notifies subscribers (spec bindings re-resolve next
render; subscribed Lua callbacks queue and drain after `&mut`
borrows release). No fixed-FPS tick.

Engine bridges and subsystem setters publish through Cells:
`SetAgentMode` → `cells.set("agent_mode", new)`;
`EngineEvent::TurnComplete` → `cells.set("turn_complete", meta)`;
etc. There is no separate `fire_au` path — it would do exactly the
same thing.

`smelt.au.on / smelt.au.fire` are thin Lua wrappers over
`smelt.cell(name):subscribe / :set` for nvim-style ergonomics.

### P2.d — `EngineBridge`

Drains `engine.event_rx` in the `select!`. Translates events into
direct host calls:

- `TextDelta` → `Buffer::append(span)` (Rust-only, no Lua per chunk).
- `ToolStarted`/`Finished` / block-end → `Buffer::attach`'s
  `on_block` callback fires at semantic boundaries.
- `RequestPermission` → `Confirms::register`, gates next pull until
  `is_clear()`.
- `TurnComplete`, `TokenUsage`, etc. → `cells.set(name, payload)`.
  Subscribers (statusline spec bindings, plugin `smelt.au.on`
  callbacks) fan out from the same registry.

### P2.e — Single `select!` loop

One loop merges `terminal_rx`, `engine.event_rx`, `lua_callback_rx`,
`cells_rx`, `timers_rx`. Each event handled = render runs; if the
diff is empty, nothing flushes.

End of P2: tree is green again. App is a thin coordinator over
named subsystems. No god-struct, no inline engine drain, no
parallel renderer state.

---

## P3 — Rust capabilities + Lua API split

**Goal:** put the generic Rust capabilities behind named modules
under `tui::`, and split the Lua bindings to one file per namespace.
This phase is mechanical but unblocks P4 and P5.

### P3.a — Capability modules

Land each as `crates/tui/src/<name>.rs` (or a small folder if the
unit warrants it):

- `tui::parse` — markdown / diff / syntax (delegates to syntect, LCS).
- `tui::process` — short-lived shell commands. `spawn(cmd, args, opts)
  -> Handle` for streaming control; `run(cmd, args, opts)` awaits
  exit and returns `{ stdout, stderr, exit_code }` (used by `bash`,
  glob, grep, one-shot helpers). Group / kill / cancel on the handle.
- `tui::subprocess` — long-lived child with bidirectional event
  channel (`spawn`, `send`, `on_event`, `wait`, `kill`). Used by
  sub-agents, MCP servers, long-running background commands. Wire
  format is opaque (stdio / socket); JSON framing is a convention
  the consumer enforces.
- `tui::fs` — read / write / edit / glob / lock.
- `tui::http` — fetch / cache / redirects.
- `tui::html` — html → markdown.
- `tui::notebook` — Jupyter JSON ops.
- `tui::grep` — ripgrep wrapper.
- `tui::path` — normalize / canonical / relative.
- `tui::fuzzy` — fuzzy matching / scoring (folds `tui/fuzzy.rs` +
  `tui/completer/score.rs`).
- `tui::permissions` — **all permission policy.** Absorbs every file
  in `engine/permissions/` (bash AST, rules, workspace check,
  RuntimeApprovals, 1617-line test suite) plus
  `tui/workspace_permissions.rs` (workspace JSON store). No
  `Permissions` aggregate type — Lua hooks compose the pieces.

Each module is independent. No umbrella folder.

**Engine "utility tool" files fold into the capabilities** — they're
helpers for tools, not tools themselves, so they belong with the
capability they serve:

- `engine/tools/background.rs` (228 LOC) → `tui::process` (registry
  + spawn/group/streaming/kill).
- `engine/tools/file_state.rs` (340 LOC) → `tui::fs::file_state`
  (mtime tracking for edit_file race detection).
- `engine/tools/web_cache.rs` (51 LOC) → `tui::http::cache`.
- `engine/tools/web_shared.rs` (436 LOC) → `tui::http` (fetch +
  redirects, the bulk of `tui::http`).
- `engine/tools/result_dedup.rs` (169 LOC) → `tui::tools::dedup`
  (helper).
- `engine/socket.rs` (345 LOC) + `engine/registry.rs` (262 LOC) →
  `tui::subprocess::{socket, registry}` (sub-agent IPC layer).

After this, `engine/tools/` retains only `ToolSchema` +
`ToolDispatcher` + `ToolResult` + ctx — engine's tool surface is
mechanically thin.

### P3.b — Lua API per namespace

Move `crates/tui/src/lua/api/{dispatch, state, widgets}.rs` to one
file per namespace under `crates/tui/src/lua/api/<name>.rs`:

UiHost-only (require a Ui — error in headless): `ui.rs`, `win.rs`,
`buf.rs`, `statusline.rs`.

Host-tier (work in tui and headless): `parse.rs`, `theme.rs`,
`timer.rs`, `cell.rs`, `clipboard.rs`, `cmd.rs`, `engine.rs`,
`permissions.rs`, `confirm.rs`, `mode.rs` (AgentMode Plan/Apply/Yolo),
`session.rs`, `tools.rs`, `os.rs`, `fs.rs`, `http.rs`, `html.rs`,
`notebook.rs`, `path.rs`, `grep.rs`, `fuzzy.rs`, `subprocess.rs`,
`frontend.rs`, `au.rs`.

### P3.c — Add the missing namespaces

Newly bound Lua surface:

- `smelt.cell` — `new(name, initial)`, `cell(name):get()`,
  `cell(name):set(v)`, `cell(name):subscribe(fn)`,
  `cell:glob_subscribe(pattern, fn)`. The single registry: stateful
  cells and pure events both.
- `smelt.timer` — `set(ms, fn)`, `every(ms, fn)`, `cancel(id)`.
- `smelt.au` — `on(name, fn)`, `fire(name, payload)`. **Sugar over
  `smelt.cell` — same registry underneath.** Kept for nvim
  familiarity.
- `smelt.clipboard` — read/write.
- `smelt.permissions` — full FFI surface for Lua tool hooks:
  `parse_bash`, `compile_pattern`, `match_ruleset`,
  `rules_for(mode, kind)` (read accessor for the ruleset configured
  via `set_rules`), `outside_workspace_paths`, `is_approved`,
  `approve`, `load_workspace`, `save_workspace`, `set_rules`.
- `smelt.subprocess` — `spawn`, `send`, `on_event`, `wait`, `kill`.
  Long-lived child IPC; sub-agents and any other long-running child
  compose this.
- `smelt.frontend` — `is_interactive()`, `kind()`. Tools branch on
  this when they need the human-vs-headless distinction.
- `smelt.mode` — `get / set / cycle` over AgentMode (Plan/Apply/Yolo).
  Renamed from `smelt.agent.mode` to avoid collision with
  `smelt.subprocess` (sub-agents).
- `smelt.parse`, `smelt.fs`, `smelt.http`, `smelt.html`,
  `smelt.notebook`, `smelt.path`, `smelt.os`, `smelt.fuzzy`,
  `smelt.grep` — wrap their capability module.

End of P3: every Lua-callable Rust thing has a binding file with the
same name. Lua plugins can compose Rust capabilities directly.

---

## P4 — Lua takes the UX

**Goal:** move every "what does smelt look/behave like" decision out
of `tui` into `runtime/lua/smelt/`. Rust shrinks to capability
provision and pixel pushing. Order within the phase is by leverage —
the highest-touched UI surfaces first.

### P4.a — `runtime/lua/smelt/` layout

Create the missing dirs and seed files:

- `widgets/` — `input.lua`, `options.lua`, `list.lua`, `picker.lua`,
  `cmdline.lua`, `statusline.lua`, `notification.lua`. Each is a
  keymap recipe + helpers for opening the corresponding Window.
- `dialogs/` — `confirm.lua`, `permissions.lua`, `agents.lua`,
  `rewind.lua`, `resume.lua`. Move from `plugins/`.
- `colorschemes/` — at least one default theme via `smelt.theme.set`
  / `smelt.theme.link` calls.
- Top-level: `transcript.lua`, `diff.lua`, `status.lua`, `modes.lua`,
  `commands.lua`. Move from `plugins/` where applicable.

### P4.b — Transcript and diff parsers in Lua

- `transcript.lua` calls `buf.attach { parser = "markdown",
  on_block = fn }`. Lua walks the result and writes extmarks via
  `smelt.buf.set_extmark`.
- `diff.lua` calls `buf.attach { parser = "diff", on_block = fn }`.
  Same shape.
- Rust `tui::parse` does the fast pure parse; Lua handles
  presentation policy.
- Drop the Rust-side `crates/tui/src/content/highlight/*` and
  `transcript_buf.rs` / `transcript_present/` machinery — extmarks
  on the Buffer carry it now.

### P4.c — Reactive statusline

`status.lua` registers a segment spec via `smelt.statusline.set({
... })`. Segments bind to cells (`{ bind = "now", fmt = "%H:%M:%S" }`,
`{ bind = "agent_mode" }`, ...). Escape hatch: `{ call = fn, deps =
{ ... } }` runs Lua only when a dep cell changes.

### P4.d — Dialogs fully orchestrated in Lua

Each Rust dialog file (`crates/tui/src/app/dialogs/*.rs`) collapses
to a request-emit + a resolution primitive on the appropriate
namespace (`smelt.confirm.*`). Lua composes the panels via
`smelt.ui.dialog.open` (Overlay + LayoutTree + N Windows).
Multi-question flows are a Lua loop opening N dialogs in sequence —
no tab-strip widget in the framework.

### P4.e — Slash commands fully Lua

`runtime/lua/smelt/commands.lua` registers every builtin via
`smelt.cmd.register`. Engine/`tui` no longer has a `RUST_COMMANDS`
table. The Rust `App::handle_command(line)` becomes
`lua.run_command(line) -> CommandOutcome` and acts on it.

### P4.f — Modes registry in Lua

`modes.lua` exposes `smelt.modes` — register a mode (name, display,
keymap-overlay). Existing `mode_cycle` and `reasoning_cycle` flows
become Lua functions firing the right autocmds via `smelt.au.fire`.

End of P4: `crates/tui/src/app/dialogs/` is empty (or near-empty).
`crates/tui/src/builtin_commands` is gone. Statusline updates
without polling. Transcript and diff have no Rust presentation
code — only parsing + extmark population.

---

## P5 — Tools to Lua

**Goal:** the 15 Rust tool implementations in `engine/tools/` move into
`runtime/lua/smelt/tools/` (plus reorganization of the few tools already
in Lua: `ask_user_question`, `exit_plan_mode`, `read_process_output` /
`stop_process` / `run_in_background` flag from `background_commands`).
Engine becomes schema + dispatcher only. Mode gating becomes a
Lua-tool `hooks` concern. The 5 utility files in `engine/tools/`
(`background`, `file_state`, `web_cache`, `web_shared`, `result_dedup`)
already moved to `tui::*` capabilities in P3.a.

### P5.a — Tool dispatcher trait shape

In `engine`:

- `ToolSchema { name, description, parameters: JSONSchema }`. Drop
  the `modes` field. Drop `Tool::needs_confirm`/`preflight`/
  `approval_patterns` from the trait — those move to Lua hooks.
- `trait ToolDispatcher { async fn dispatch(call_id, name, args, ctx)
  -> ToolResult; async fn evaluate_hooks(name, args, mode, turn_ctx)
  -> Hooks }`.
- **Wiring:** engine takes a `Box<dyn ToolDispatcher>` at
  `engine::start(config, dispatcher)`. The trait methods are `async`;
  engine's task `.await`s them on the same tokio thread. No
  cross-channel ping-pong.
- Engine never executes tools itself. It calls the dispatcher.

`tui::ToolRuntime` impls `ToolDispatcher`, walking the registry
populated from Lua at startup.

### P5.b — Migrate core tools to Lua

Land in `runtime/lua/smelt/tools/`:

`bash.lua`, `read_file.lua`, `write_file.lua`, `edit_file.lua`,
`glob.lua`, `grep.lua`, `web_fetch.lua`, `web_search.lua`,
`notebook_edit.lua`, plus `load_skill.lua` and the agent-management
tools (`spawn_agent.lua`, `stop_agent.lua`, `message_agent.lua`,
`peek_agent.lua`, `list_agents.lua`).

**Agent tools are subprocess tools.** They compose
`tui::subprocess` and a thin Lua-side registry — no engine
knowledge. The transcript renders these calls as ordinary tool
calls (no special widget). Concretely:

- `spawn_agent.lua` calls
  `smelt.subprocess.spawn("smelt", { "--agent", id, … })`,
  registers `on_event` to fire `agent:<id>:event` cell, stores the
  handle in a Lua table keyed by `id`, returns the handle id as
  the tool result.
- `message_agent.lua` looks up the handle, sends a JSON message
  through `subprocess.send`, yields the coroutine until the next
  reply arrives via the cell, returns the reply text.
- `stop_agent.lua` calls `subprocess.kill` on the handle.
- `peek_agent.lua` reads the latest cell value.
- `list_agents.lua` enumerates the Lua-side registry table.

A built-in `runtime/lua/smelt/plugins/multi_agent.lua` carries the
shared state (the `id → handle` table, the `agent:<id>:status`
cells) so the tools don't duplicate it. Plugin authors who want
fancier agent UI (live token streaming, dedicated transcript
panel) build it on top of `agent:<id>:event` subscriptions and
custom Buffer attaches.

Each tool:

- Declares schema via `smelt.tools.register({ name, schema, run,
  hooks })`.
- `run` composes `tui::process` / `tui::fs` / `tui::http` / etc. via
  the Lua bindings landed in P3. Coroutine yields on async Rust calls.
- `hooks(args, mode)` returns `"allow" | "needs_confirm" | "deny"`
  based on `AgentMode`. This is where the Plan/Apply/Yolo policy
  lives.

**Intricate logic stays in Rust, called via FFI.** Tools never
re-implement complicated parsing / safety logic in Lua. Specifically:

- `bash.lua` calls `smelt.permissions.parse_bash(cmd)` to get a
  structured AST, then walks the result in Lua to decide allow / deny
  against workspace rules (also via `smelt.permissions.match_rule`).
- `edit_file.lua` calls `smelt.fs.apply_edit_with_mtime_check(path,
  old, new, expected_mtime)` for the read-compare-write-fsync atomic
  step. The Lua side handles error formatting and confirmation; the
  Rust side handles the race-free filesystem dance.
- `notebook_edit.lua` calls `smelt.notebook.parse / apply_edit` —
  Jupyter JSON validation stays in Rust.
- `web_fetch.lua` calls `smelt.http.fetch` (cache + redirects) and
  `smelt.html.to_markdown` for the Reader transform.
- `grep.lua` calls `smelt.grep.run` (ripgrep wrapper).

The principle: the tool body, schema, hooks, and output formatting are
Lua; anything fragile or performance-sensitive is a one-line FFI call
into a `tui::*` capability.

### P5.c — Engine cleanup

- Delete `crates/engine/src/tools/{bash, read_file, write_file,
  edit_file, glob, grep, web_fetch, web_search, notebook}.rs` and
  the supporting infrastructure they each carry.
- **Pull `crates/engine/src/permissions/` out of engine entirely.**
  All five files (`mod.rs`, `bash.rs`, `rules.rs`, `workspace.rs`,
  `approvals.rs` + `tests.rs`) move into `tui::permissions` (landed
  in P3.a). Engine has zero `Permissions` references after this
  step. The new permission flow:
  1. Engine calls `dispatcher.evaluate_hooks(name, args)`.
  2. tui's `ToolRuntime` invokes the Lua tool's `hooks(args, mode)`.
  3. The hook composes `tui::permissions.*` calls (parse bash,
     check workspace, look up runtime approvals, consult workspace
     store) and returns `"allow" | "needs_confirm" | "deny"`.
  4. On `"needs_confirm"`, engine emits `RequestPermission`.
     `Confirms` registers; the user answers; `PermissionDecision`
     flows back. The Lua hook's earlier "approve always" branch
     also writes to `tui::permissions.approvals` and (if
     workspace-scoped) the persistent store.
- `agent.rs` loses its `permissions: Permissions` field and the
  `permissions.decide(...)` call in the tool-launch path. The agent
  asks the dispatcher for hooks, nothing more.
- Per-turn permission overrides (`protocol::PermissionOverrides`)
  become payload the Lua hook reads from the turn context the
  dispatcher passes through (`hooks(args, mode, turn_ctx)`).
  Engine forwards `PermissionOverrides` on `StartTurn` like any
  other turn parameter; it doesn't apply them itself. Override
  composition is a Lua-side concern.
- Engine still owns: provider abstraction, single-agent loop, MCP,
  cancel tokens, schema-aware streaming. Engine emits
  `RequestPermission` and consumes `PermissionDecision` on its
  protocol surface — that's the full engine permission surface.
- **Drop `EngineConfig.interactive: bool`.** Engine doesn't
  differentiate frontends; all today-`interactive` branches in
  `agent.rs` / tool registration / `ToolCtx` go away. Tools that
  need to know "is there a user to ask?" call
  `smelt.frontend.is_interactive()` (Host-tier Lua binding that
  reads which frontend type wraps the `Core`). Tools that only
  make sense interactively (e.g. `ask_user_question`) deny in
  their `hooks` when `is_interactive()` is false.
- **`EngineHandle` becomes channels-only.** Drop the `processes`,
  `permissions`, and `runtime_approvals` public fields. Frontends
  reach those through `Core` / `tui::*` instead. The handle's
  surface is exactly `cmd_tx: Sender<UiCommand>` +
  `event_rx: Receiver<EngineEvent>`.
- **Drop the multi-agent concept from engine entirely.** Specific
  removals:
  - `EngineConfig.multi_agent: Option<MultiAgentConfig>` and the
    `MultiAgentConfig` struct.
  - `EngineEvent::{AgentMessage, AgentExited, Spawned}`.
  - `UiCommand::AgentMessage`.
  - `engine::tools::AgentMessageNotification` broadcast channel +
    every send/recv site in `agent.rs`.
  - `engine/registry.rs` (262 LOC) — moved to
    `tui::subprocess::registry` in P3.a.
  - `engine/socket.rs` (345 LOC) — moved to
    `tui::subprocess::socket` in P3.a.
  - `protocol::Role::Agent` and `protocol::AgentBlockData`.
  - The 5 dedicated agent tool files
    (`spawn_agent.rs` / `stop_agent.rs` / `message_agent.rs` /
    `peek_agent.rs` / `list_agents.rs`) follow P5.b to Lua. The
    agent-management Lua tools compose `tui::subprocess` only —
    engine never sees them. (`load_skill.rs` is unrelated to
    multi-agent and follows P5.b on its own track.)
  - `agent.rs`'s multi-agent loop branch (~400 LOC of the 2129)
    deletes; `agent.rs` becomes single-agent only.

  After this, engine has _no opinion on sub-agents whatsoever_.
  Spawning one is `tui::subprocess.spawn("smelt", …)` from a Lua
  tool; the parent's LLM sees "tool call, tool result" like any
  other tool.
- `crates/tui/src/workspace_permissions.rs` is folded into
  `tui::permissions::store` in P3.a. P5.b's `bash.lua` and
  `edit_file.lua` call `tui::permissions.*` for parsing, rule
  matching, and approval lookup.

### P5.d — Drop `config.yaml`, all config in `init.lua`

Greenfield — no migration story. One config format only.

- Delete `serde_yml` dependency and the YAML config loader (today's
  `Permissions::load` reads `~/.config/smelt/config.yaml`; that goes
  along with the rest of the engine permissions move in P5.c).
- Delete the TOML keymap loader (if present) — keymaps are
  `smelt.keymap.set(mode, key, fn)` calls in `init.lua` or plugins.
- Bind every today-YAML setting through Lua:
  - `smelt.provider.register(name, { api_key, default_model, base_url, … })`
  - `smelt.permissions.set_rules { normal, plan, apply, yolo }`
  - `smelt.mcp.register(name, { command, args, env })`
  - `smelt.model.set(name)`, `smelt.reasoning.set(level)`
  - `smelt.auxiliary.set(task, { model, api })`
  - `smelt.redact_secrets(true)`, `smelt.auto_compact(true)`,
    `smelt.context_window(N)`
- `EngineConfig` becomes constructible only via Lua-driven population
  at startup. `init.lua` runs first, registers everything, then
  `Core::start()` materializes the engine config.
- No settings registry, no "settings key" schema. A setting is a
  binding argument. Validation is at the FFI boundary.
- Update `runtime/lua/smelt/colorschemes/default.lua` and seed
  `runtime/lua/smelt/init.lua` to be the example user config the
  user copies and edits.

End of P5.d: no YAML files anywhere except `Cargo.toml`. Single
config language end to end.

### P5.e — Protocol rename pass

Align names with the diagram in one sweeping rename. Greenfield — no
back-compat. Specific renames:

- `protocol::Mode` → `protocol::AgentMode`.
- `EngineEvent::ExecutePluginTool` → `ToolDispatch`.
- `EngineEvent::EvaluatePluginToolHooks` → `ToolHooksRequest`.
- `UiCommand::SetMode` → `SetAgentMode`.
- `UiCommand::PluginToolResult` / `PluginToolHooksResult` → `ToolResult`
  / `ToolHooksResponse`.
- Drop the `Plugin` prefix from `PluginToolDef` / `PluginToolHookFlags`
  / `PluginToolHooks` since "plugin tool" and "core tool" no longer
  exist as a distinction (engine sees one registry).

End of P5: `engine` has no opinion on Plan/Apply/Yolo. The same
registry holds plugin and "core" tools — engine doesn't distinguish.

---

## P6 — Streaming + lifecycle polish

**Goal:** lock down the engine→buffer streaming path and the
lifecycle gates (confirms gate, cooperative cancel, dialog
stacking).

- **Streaming pipeline.** `EngineEvent::TextDelta { delta }` →
  `EngineBridge` → `Buffer::append` (Rust-only). Lua never runs per
  chunk. Buffer's `on_block` callback fires at markdown-block end /
  tool start/stop / turn end. Verify via `parse_marker` traces.
- **Confirms gate.** `EngineBridge` checks `Confirms::is_clear()`
  before pulling the next request from `engine.event_rx`. Resumes
  when the dialog closes. One gate.
- **Cooperative cancel.** Each Lua coroutine task carries a
  `CancellationToken`. Cancel = the token flips, in-flight async
  Rust calls return `Err(Canceled)`, the coroutine resumes with
  that error so normal Lua `pcall` flow handles cleanup.
- **Dialog stacking** = `Ui::overlays` z-stack. Open order = z
  order. Modal-on-top blocks focus to lower overlays. No special
  framework code.
- **Esc chain** = focused Window's `handle_key` first; if `Ignored`,
  `WinEvent::Dismiss` fires on the enclosing Overlay; Lua handles.
- **Cursor shape global.** Single `Ui::cursor_shape`, not
  per-Window. Updated on focus change.
- **Cell-event fan-out.** Verify every state-changing event in the
  engine pipeline reaches the right cell setter (`agent_mode`,
  `model`, `reasoning`, `tokens_used`, `turn_complete`,
  `confirm_requested`, `history`, …). Each cell name has at least one
  built-in subscriber test asserting the callback runs. The
  `smelt.au.*` alias is exercised the same way.

---

## P7 — Finalize

- Sweep `#[allow(dead_code)]`, `// removed`, `// kept for now`
  comments. Anything that survived should not have those markers.
- Sweep `LuaShared` mirrors. App state is read live via `with_app`;
  the only thing in `LuaShared` is genuine Lua-runtime state
  (handle registries, atomic counters, coroutine runtime, deferred
  invocation queue).
- Update `README.md` and `docs/` for any user-visible changes
  (statusline reactive, dialog open API, tool registration shape).
- Drop `tui-ui-architecture.puml` (the old diagram). Rename
  `tui-ui-architecture-target.puml` → `tui-ui-architecture.puml`.
- Run the workspace through one full `cargo fmt && cargo clippy
  --workspace --all-targets -- -D warnings && cargo nextest run
  --workspace`. This is the *one* hard gate at the end of the
  refactor.
- Walk a parity matrix by hand in a running TUI: triple-click yank
  on transcript / prompt / dialog buffer, drag-extend, esc chain,
  selection bg, vim modes, cmdline, picker, confirm dialog with
  diff preview, notification toast, statusline live update, theme
  switch. Visual behaviour is not test-covered — the human walk is
  the gate.

---

## What we are deliberately not solving here

- **Theme content.** A pretty default theme is its own task once the
  registry exists (P1.e). It is not in this refactor's path.
- **Plugin marketplace / install / discovery.** Plugins are
  Lua under `runtime/lua/smelt/plugins/` plus the user's
  `~/.config/smelt/init.lua`. Anything fancier comes after.
- **A second backend.** `ui` is shaped to be backend-agnostic at
  the data layer, but the only backend is crossterm. We do not
  introduce a second one in this refactor.
- **Per-buffer / per-window options registry.** Nice-to-have but
  out of scope; revisit only if a real consumer shows up.

## One sequencing note

Order phases by *what unblocks what*, not by *what's easiest*. P1 is
the load-bearing one; do it first even though it's the largest. After
P1, P2/P3/P4 can interleave somewhat. Anything else (friction,
diagram drift, atomic-move discipline) is in `README.md`.
