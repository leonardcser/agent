# Architecture

Decisions and rationale for smelt's target shape. Pairs with two siblings:

- **`tui-ui-architecture-target.puml`** — class diagram. _How things relate
  structurally._ Types, fields, packages, arrows. No prose.
- **`REFACTOR.md`** — implementation plan. _How we get there._ Step-by-step
  migration, ordering, deletions per step.
- **This file** — _why things are shaped this way._ Decisions, the rules they
  produce, and the alternatives we rejected.

When the three drift, the diagram is canonical for structure, this doc is
canonical for intent, and REFACTOR.md is canonical for sequencing.

## North star — three commitments

Every change lands closer to all three, never farther from any.

1. **One rendering path.** The compositor (`ui::Ui`) owns every pixel. No
   parallel ANSI emitter, no cached "last painted" state outside the grid diff.
   Anything visible is a Window registered with the compositor.

2. **Lua surface = Rust API.** Lua plugins call the same Rust functions the
   core does. `smelt.*` is a thin wrapper around `Host` methods, not a
   translation layer. No stringly-typed action tokens. No effect log. Reads go
   live; writes go direct.

3. **Rust core, Lua features.** Rust owns pixel pushing (compositor, buffers,
   windows, layout, paint), generic capabilities (process/fs/http/parse/grep),
   and the engine boundary. Lua owns the _what_: which dialogs, tools, slash
   commands, and statusline segments exist; how panels compose; how the UI
   reacts. Same model as Neovim — small Rust core, everything user-facing is a
   plugin. **A feature living in Rust that could live in Lua is a bug.**

## Crate map

Five layers, dependency arrows go one way, no cycles:

```
protocol  ←  engine  ←──┐
                        │
                        tui  ──→  ui
                        ↑
                        runtime/lua/smelt/
```

| Crate                    | Role                                                                    |
| ------------------------ | ----------------------------------------------------------------------- |
| `protocol`               | Wire types, pure data, serde-serializable. Stable contract, no behavior |
| `engine`                 | LLM core. Provider abstraction, agent loop, MCP, cancel tokens, schema-aware streaming. Tools = schema + dispatcher trait only; impls live in Lua. **No permission policy, no multi-agent concept** — sub-agents are just child processes spawned by Lua tools through `tui::subprocess`. Engine emits `RequestPermission` events when the dispatcher signals `needs_confirm`, but holds no rules, modes, or approvals. **Zero UI imports** |
| `ui`                     | Generic terminal UI primitives. Buffer, Window, LayoutTree, Overlay, Theme, Grid/diff. Could ship standalone at the data/rendering layer — no smelt knowledge |
| `tui`                    | Smelt's binary. `Core` + `TuiApp` / `HeadlessApp` frontends, subsystems, Lua bridge, EngineBridge, ToolRuntime, Rust capabilities (parse/process/subprocess/fs/http/html/notebook/grep/path/fuzzy/permissions) |
| `runtime/lua/smelt/`     | The whole UX. Widgets, dialogs, commands, statusline, transcript/diff presentation, themes, tools |

**Litmus test for placement.** Stable wire contract → `protocol`.
Frontend-agnostic LLM/tool code → `engine`. Reusable TUI primitive any app
could ship → `ui`. Smelt's Rust glue (App, capabilities, Lua bridge) → `tui`.
How smelt looks/behaves → Lua.

## Surface model — two structures, two primitives

Everything visible on screen is a **Window** over a **Buffer**. Composition
happens in two structures only:

- **`Ui::splits: LayoutTree`** — base tiling. Mutable. Things in it claim
  space by definition.
- **`Ui::overlays: Vec<Overlay>`** — z-ordered overlays. Things in it never
  claim space.

Two primitives, one role each:

- **Buffer** — shared content: lines, extmarks, namespaces, undo, modifiable
  flag, and coordinate/source-display translation.
- **Window** — one viewport over one Buffer: cursor, scroll, selection,
  keymap recipe, focusability, gutters, and rendering. Window is the concrete
  interactive/renderable unit; no separate `Surface`, `Component`,
  `PanelWidget`, or `BufferView` layer exists in the target model.

Composition/chrome lives outside Window:

- **LayoutTree** — `Vbox { gap, separator, border, title, items } | Hbox
  { gap, separator, border, title, items } | Leaf(WinId)`. Composition shape.
  Container nodes carry four parent-node properties:
  - `gap: u16` — blank rows (Vbox) or columns (Hbox) between adjacent
    siblings. Default 0 = tight tiling.
  - `separator: SeparatorStyle` — line drawn on the middle row of the gap.
    Requires `gap >= 1`. `gap=0, separator=Solid` auto-inflates to `gap=1`.
  - `border: Option<Border>` — chrome around the container's whole rect.
    `Border = { style: BorderStyle, sides: Borders }` where `Borders` is a
    bitflag picking edges (`TOP | RIGHT | BOTTOM | LEFT`).
  - `title: Option<String>` — rendered on the `TOP` edge of the border.
    Only meaningful when `sides` includes `TOP`.

  Any container can have chrome. Convention is to use it on overlays only;
  the type system allows it anywhere. Want a 1-row blank line between
  transcript and prompt? `Vbox { gap: 1, separator: None, … }`. Want a
  divider? `Vbox { gap: 1, separator: Solid, … }`. Want a bordered
  composite? `Vbox { border: Some({ style: Rounded, sides: ALL }), title:
  Some("Find"), … }`.

- **Overlay** — placement + modality wrapper around a LayoutTree:
  `{ layout, anchor, z, modal }`. Carries no chrome itself — chrome lives on
  the LayoutTree's container nodes.

A dialog, picker, text input, notification, statusline, cmdline, or list is
not a separate Rust widget category in the target. It is one or more Windows,
usually composed by Lua/tui helpers into a LayoutTree/Overlay with keymap
recipes and buffer/extmark content.

### The discriminator

> **"When this appears, does anything else shrink to make room?"**
>
> - **Yes** → splice into `Ui::splits`. Claims space.
> - **No** → push onto `Ui::overlays`.

No `claims_space` flag — the choice of structure _is_ the answer. This
collapses the old 6-variant `Placement` enum and removes the conceptual
ambiguity around "is this a docked float or a floating dock?"

### Concrete examples

| Surface              | Lives in   | Anchor / placement                            |
| -------------------- | ---------- | --------------------------------------------- |
| Transcript           | splits     | `Vbox` leaf, fills available height            |
| Prompt               | splits     | `Vbox` leaf, fixed height                      |
| Statusline           | splits     | `Vbox` leaf, 1 row                             |
| Stash indicator      | splits     | inserted between transcript and prompt         |
| Cmdline (open)       | splits     | inserted above statusline                      |
| Confirm dialog       | splits     | sub-Vbox inserted before dismiss               |
| Notification toast   | overlays   | `Anchor::Win { target: prompt, attach: Top }`  |
| Completer popup      | overlays   | `Anchor::Cursor(Below)`                        |
| Fuzzy finder         | overlays   | `Anchor::Screen(Center)` + chromed Vbox + modal |
| Hover tooltip        | overlays   | `Anchor::Win { target, attach: Right }`        |
| Draggable popup      | overlays   | `Anchor::Screen { row, col }` (mutate to drag) |

### Why "Overlay" not "Float"

The word "float" suggested a kind of Window. It isn't — it's a
placement+modality wrapper that points at Windows via a LayoutTree.
"Overlay" matches the discriminator language ("overlays splits, never
claims space") and removes the implication that there's a Window subtype.

## Buffer model

Vim-style: lines + extmarks in named namespaces, plus a `modifiable` flag.

- **All decoration is extmarks** in named namespaces. Highlights, virtual
  text, conceals, signs, source-byte mapping for markdown/diff, click
  metadata, line backgrounds, folds, diagnostics — one store,
  clear-by-namespace. Mirrors `nvim_buf_set_extmark`.
- **Theme references are highlight ids, not raw colors.** Buffers store
  highlight group ids/extmark highlight ids. Theme changes should not require
  rewriting buffer contents or spans.
- **`modifiable: bool`** is the data-layer read-only guard. Source of truth
  for "can this content be edited?" Same Buffer viewed by N Windows shares
  one `modifiable` flag. Defaults: `false` for transcript / diff preview /
  notification / picker results / markdown render. `true` for prompt /
  cmdline / fuzzy-finder query / any input field.
- **Yank substitution is extmark-level.** Each `Extmark` carries an
  optional `yank: Option<YankSubst>` where

  ```rust
  enum YankSubst {
      Empty,           // elide bytes covered by this extmark
      Static(String),  // replace bytes with this string
  }
  ```

  `buffer.yank_text_for_range(range)` is a pure helper: it walks extmarks
  intersecting the range and applies each one's substitution
  (`Empty` skips, `Static` substitutes; absent = literal source bytes).
  Hidden thinking blocks attach `YankSubst::Empty` extmarks; prompt
  attachment sigils (e.g. `@file`) attach `YankSubst::Static(expanded_path)`.
  No buffer-level strategy hook, no App-side translator.

  The default — yank source bytes verbatim — is the right behaviour for
  rendered markdown: copying a line of `**bold**` yields `**bold**`, not
  the rendered glyphs. Substitution is opt-in per extmark.
- **Per-buffer edit history lives on Buffer.** Undo/redo, marks, attachment
  metadata, and stable anchors are buffer data, represented through undo state
  and extmarks. Window does not own a parallel edit buffer.
- **`Buffer::attach(spec)`** is the only way to wire parsers. `spec` carries
  parser kind (`markdown`/`diff`/`syntax`), decoration namespaces, and an
  optional `on_block` callback fired at high-level boundaries — _not_ per
  delta. No `on_lines` / `on_bytes` / `decoration_provider`. Hot-path
  callbacks aren't shipped at all.

`Buffer ≠ Grid`. Buffer is a content store (persists across frames). Grid is
the terminal frame (rebuilt every frame, differential flush). The compositor
renders Buffers through Windows into the Grid.

### Parsed metadata lives on the Buffer

Expensive upstream computation — LCS results for diff blocks, syntect token
streams for code blocks — attaches to the Buffer as **extmarks in dedicated
namespaces** (`ns: "diff"`, `ns: "syntax"`). It's computed once when the
content arrives, persists with the Buffer (so session resume doesn't redo
it), and invalidates naturally on edit.

There is no separate persisted IR cache file, no separate persisted layout
cache. Width-dependent layout (line wrapping) runs at paint time; with a
diff renderer that's cheap. Width-independent computation (the LCS, the
token stream) lives on the Buffer.

## Rendering model — event-driven, diff-based, no dirty flag

The render loop is a single `tokio::select!` merging terminal events,
engine events, the Lua callback drain, and an animation tick. **Every
handled event triggers a render.** The render builds a fresh `current`
Grid by projecting all layers, diffs `current` vs `previous`, and flushes
only the changed cells.

Why this works:

- **Diff is the gate.** If nothing visible changed, the diff is empty and
  the flush writes zero bytes. The cost of "render every event" is just the
  projection pass + memcmp.
- **Idle CPU is zero.** When no events arrive, the loop parks. There is no
  fixed-FPS tick.
- **No `request_redraw` / no dirty flag.** Subsystems mutate state. The
  next time the loop wakes for any reason, the render reflects the change.
  No invalidation tracking, no per-subsystem signaling.
- **Resize / Ctrl-L / layer add-remove clear the diff baseline.** The
  compositor zeros its previous-grid before the next render; the diff
  becomes a full repaint by virtue of writing every cell. No special
  "force repaint" flag — the data is the signal.

### Things that change over time

Anything that should update without an external event — clock, spinner,
elapsed-time counter — drives a `Cell` (next section) from a timer. The
Cell setter wakes the loop; the next render resolves bindings against the
new value; the diff flushes the changed glyphs. No special path.

## Reactive cells — one primitive for state, events, and subscriptions

`Cells` is the single observer mechanism in the runtime. There is **no
separate autocmd registry** — autocmd subscription is `Cell::subscribe`,
autocmd fire is `Cell::set`, and named events without a value are
`Cell<()>`.

A `Cell<T>` is a typed, named slot that:

1. Holds a value.
2. On `set`, wakes the loop (one `select!` branch on the cells channel).
3. Notifies all subscribers, in registration order, queued and drained
   after the current `&mut` borrows release.

```lua
-- read/bind a stateful cell (statusline)
smelt.statusline.set({
  { bind = "now", fmt = "%H:%M:%S" },
  " · ",
  { bind = "agent_mode" },
  " · ",
  { bind = "model" },
})

-- subscribe to changes
smelt.cell("agent_mode"):subscribe(function(new, old)
  -- runs after every set
end)

-- glob subscription (autocmd-style pattern)
smelt.cell:glob_subscribe("*_changed", function(name, payload)
  -- ...
end)

-- plugin-defined cell + timer
local pending = smelt.cell.new("my_plugin:pending", "0")
smelt.timer.every(2000, function()
  pending:set(tostring(count_pending()))
end)

-- pure event (no state, just a notification)
local evt = smelt.cell.new("my_plugin:thing_happened", nil)
evt:set(payload)            -- fires subscribers
```

`smelt.au.on(name, fn)` and `smelt.au.fire(name, payload)` exist as a
thin alias over `smelt.cell(name):subscribe(fn)` and
`smelt.cell(name):set(payload)`. They are sugar; the underlying
mechanism is one registry.

Built-in cells the runtime ships (stateful slots and pure events both):

| Cell                  | Type / Payload                       | Driven by                                  |
| --------------------- | ------------------------------------ | ------------------------------------------ |
| `now`                 | `DateTime`                           | `Timers` (1 Hz)                            |
| `spinner_frame`       | `u8`                                 | `Timers` (16 ms when something is animating) |
| `agent_mode`          | `AgentMode`                          | `SetAgentMode` applied                     |
| `vim_mode`            | `VimMode`                            | App's `vim_mode` flips                     |
| `model`               | `String`                             | `SetModel` applied                         |
| `reasoning`           | `ReasoningEffort`                    | `SetReasoningEffort` applied               |
| `confirms_pending`    | `bool`                               | `Confirms` registers / resolves            |
| `tokens_used`         | `TokenUsage`                         | `EngineEvent::TokenUsage`                  |
| `errors`              | `u32`                                | `TurnError` count                          |
| `cwd`                 | `String`                             | Process CWD                                |
| `session_title`       | `String`                             | `TitleGenerated`                           |
| `branch`              | `String`                             | Session branch swap                        |
| `history`             | `Cell<HistoryDelta>` (event)         | Message appended / mutated / cleared       |
| `turn_complete`       | `Cell<TurnMeta>` (event)             | Engine emits TurnComplete                  |
| `turn_error`          | `Cell<TurnError>` (event)            | Engine emits TurnError                     |
| `confirm_requested`   | `Cell<HandleId>` (event)             | Engine emits RequestPermission             |
| `confirm_resolved`    | `Cell<{handle_id, decision}>` (event)| User answered                              |
| `session_started`     | `Cell<SessionId>` (event)            | New session                                |
| `session_ended`       | `Cell<SessionId>` (event)            | Session cleared / forked                   |

Plugins create cells under their own namespace (`my_plugin:pending`,
`my_plugin:thing_happened`) to avoid collisions with built-ins.

### Why one primitive instead of two

Cells already wake the loop, queue subscriber drains, and integrate with
the spec hot path. An autocmd registry is the same machinery —
`Map<Name, Vec<Subscriber>>` plus a fire path that queues callbacks. Two
primitives doing the same job at different ergonomic surfaces forces
plugin authors to learn both. One primitive with sugar (`smelt.au.*`
and `smelt.cell.*` over the same registry) keeps the surface familiar
without doubling the implementation.

The trade-off — stringly-typed cell names — is the same as nvim's
autocmds and is checked at registration via a known-name list warning.

### Spec escape hatch

The default segment is `{ bind = cell, fmt = … }` — pure data. For ad-hoc
computation, the spec accepts:

```lua
{ call = function() return git_branch() end, deps = { "cwd", "now" } }
```

`call` runs only when one of `deps`' cells changes — never per-frame. This
keeps mlua's no-JIT cost off the hot path while preserving the "drop in a
custom function" capability.

### Why push (cells), not pull (lualine-style re-eval)

Pull recomputes every segment on every redraw — fine for LuaJIT (Neovim),
expensive for mlua. Push pays only for segments whose inputs changed: zero
work when idle, one segment per frame typically. The `deps` escape hatch
recovers pull's flexibility without its baseline cost.

## Input pipeline — three independent layers

```
App::vim_mode  (global VimMode)         what's the user trying to do?
       │
       ▼  routes key to focused Window's recipe
Window::keymap  (recipe id)             what keys are bound here?
       │
       ▼  recipe attempts mutation
Buffer::modifiable  (data guard)        final yes/no on edits
```

Each layer is independent and composes:

1. **Vim mode is global on App.** Pressing `i` enters Insert anywhere.
   Mode-specific keymaps live in the global keymap registry, indexed by
   `(mode, key)`. Two enums in the system:
   - `VimMode` — `Normal / Insert / Visual / VisualLine`. On App.
   - `AgentMode` — `Normal / Plan / Apply / Yolo`. Permission-gating policy.
     Lives in protocol. Lua: `smelt.mode` (renamed from
     `smelt.agent.mode` to avoid collision with `smelt.subprocess`,
     which is what sub-agents use now).
2. **Window keymap recipe** decides what keys do. Editor recipes bind
   `i/a/o/dd`; viewer recipes bind only `j/k/v/y`. Recipes are pure Lua,
   defined in `runtime/lua/smelt/widgets/`.
3. **`Buffer::modifiable`** is the final guard. Even a buggy editor recipe
   can't mutate a non-modifiable Buffer. Defense in depth.

There is **no `Vim` or `Completer` state-machine type in `ui`**. Per-buffer edit
history (registers, dot-repeat, undo) lives on Buffer. Per-Window cursor +
selection + Visual anchor stay on Window. Clipboard/kill-ring state lives on
the host. Completer decomposes into a ghost-text extmark in a "completer"
namespace + an Overlay for the picker dropdown + a keymap on the prompt
Window.

## Focus, hit-testing, and capture

Focus and pointer hit-testing are related but not the same thing.

- **FocusTarget** is semantic and keyboard-addressable: `Window(WinId)` only.
  A focused Window owns cursor, selection, mode label, keymap dispatch, and
  statusline context.
- **HitTarget** is geometric and mouse-addressable:
  `Window(WinId) | Scrollbar { owner: WinId } | Chrome { owner: OverlayId }`.
- **CaptureTarget** reuses `HitTarget` for in-flight gestures. A scrollbar can
  capture a drag, but it never becomes focused.

Scrollbar behaviour:

- Clicking or dragging a scrollbar routes mouse events to
  `HitTarget::Scrollbar { owner }`.
- Focus stays on, or moves to, the owning Window when appropriate.
- Keyboard events, Esc chain, statusline mode, cursor shape, and selection all
  continue to read from `FocusTarget::Window(owner)`, never from the
  scrollbar.

This split avoids overloading one `TargetId` enum with two meanings. Focus is
semantic; hit/capture is geometric.

Rules:

- **`Ui::focus: Option<WinId>`** is the source of truth for keyboard focus.
  Public API is explicit: `focus() -> Option<WinId>` reads it,
  `set_focus(win) -> bool` changes it. `set_focus` pushes the prior
  focus onto `focus_history`. **`overlay_close` pops `focus_history`**
  back to the most recent still-existing focusable Window — handles
  the dialog-close case automatically.
- **Focused Window access is a convenience over `focus`.**
  `focused_window()` / `focused_window_mut()` return the focused Window, if the
  id still exists. There is no target `focused_buffer_window()` API because every
  focusable surface is already a Window over a Buffer.
- **Overlay focus is derived, not stored.** `focused_overlay()` returns the
  overlay containing the focused Window. `active_modal()` returns the topmost
  modal overlay, independent of focus.
- **Mouse routing uses `hit_test(row, col) -> Option<HitTarget>`.** Hit-testing
  applies modal filtering and can return scrollbars/chrome that are not
  focusable.
- **Click promotes focus** only when the hit target resolves to a focusable
  Window and no modal Overlay above absorbs the hit. Clicking a scrollbar may
  focus its owner, never the scrollbar.
- **Tab cycles are modal-aware.** Inside a modal Overlay → cycle the overlay's
  focusable Windows. Otherwise → walk all focusable Windows in z-order.
- **Esc chain.** Focused Window's `handle_key` first; if `Ignored`,
  `WinEvent::Dismiss` fires on the enclosing Overlay (Lua handles via
  `smelt.win.on_event`). No `on_dismiss` field.
- **Cursor shape is global on `Ui`** — single field, nvim-style. Not
  per-Window.

## Frontends — Core, TuiApp, HeadlessApp

The runtime is split along the only axis that matters: does this code
need a Ui?

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

struct TuiApp { core: Core, well_known: WellKnown, ui: ui::Ui }
struct HeadlessApp { core: Core, sink: HeadlessSink }
```

`Core` runs the event loop and holds everything that doesn't need the
compositor: tools, engine bridge, cells, timers, autocmd-style
subscriptions, Lua runtime, session state, confirms, clipboard.
`TuiApp` adds `well_known` (the `WinId`s of transcript / prompt /
statusline / cmdline) and a `ui::Ui`. `HeadlessApp` adds a
`HeadlessSink` that emits JSON / text instead of pixels.

This split lets `smelt -p "..."` (one-shot CLI) and multi-agent worker
processes use the same Core/EngineBridge/Tools as the TUI without
loading `ui::Ui`.

**One binary, two entry points.** A single `smelt` binary; `main`
inspects argv:

```
smelt                       → TuiApp (interactive terminal)
smelt -p "..."              → HeadlessApp + JSON/text sink
smelt --agent <id> [...]    → HeadlessApp + socket sink (sub-agent worker)
```

There is no `smelt-worker` second binary, no `EngineConfig.interactive`
flag inside the engine, and no `if interactive { … } else { … }`
branches scattered through tui code. Each entry point constructs the
right `App` type up-front and the borrow checker keeps them apart.
`HeadlessApp` differs from `TuiApp` only by which trait it impls
(`Host` vs `Host + UiHost`) and what kind of sink it carries — the
event loop, Cells, Timers, Lua runtime, EngineBridge, and tools are
identical.

### Side effects — `Host` and `UiHost`, no Effect enum

Side effects are direct method calls on a host trait. Two traits, one
extends the other:

```rust
trait Host {
    fn clipboard(&mut self) -> &mut dyn ClipboardWrite;
    fn cells(&mut self)     -> &mut Cells;
    fn timers(&mut self)    -> &mut Timers;
    fn lua(&mut self)       -> &mut LuaRuntime;
    fn tools(&mut self)     -> &mut ToolRuntime;
    fn engine(&mut self)    -> &mut EngineBridge;
    fn session(&mut self)   -> &mut Session;
    fn confirms(&mut self)  -> &mut Confirms;
    // … nothing that mentions Ui / Window / Buffer / Overlay
}

trait UiHost: Host {
    fn ui(&mut self) -> &mut Ui;
    fn focus(&mut self, win: WinId);
    fn fire_win_event(&mut self, win: WinId, ev: WinEvent);
    fn buf_create(&mut self, …) -> BufId;
    fn buf_mut(&mut self, id: BufId) -> Option<&mut Buffer>;
    fn win_open(&mut self, …) -> WinId;
    fn win_close(&mut self, id: WinId);
    fn win_mut(&mut self, id: WinId) -> Option<&mut Window>;
    fn overlay_open(&mut self, ov: Overlay) -> OverlayId;
    fn overlay_close(&mut self, id: OverlayId);
}
```

`Core` impls `Host`. `TuiApp` impls both `Host` and `UiHost`.
`HeadlessApp` impls only `Host`. `Window::handle` takes
`&mut dyn UiHost`. Lua bindings divide by trait:

- **Host-only bindings** (work in headless and tui):
  `smelt.cell`, `smelt.timer`, `smelt.au`, `smelt.clipboard`,
  `smelt.cmd`, `smelt.engine`, `smelt.permissions`,
  `smelt.confirm`, `smelt.mode`, `smelt.session`, `smelt.tools`,
  `smelt.os`, `smelt.fs`, `smelt.http`, `smelt.html`,
  `smelt.notebook`, `smelt.path`, `smelt.parse`, `smelt.grep`,
  `smelt.fuzzy`, `smelt.theme`, `smelt.subprocess`,
  `smelt.frontend` (`.is_interactive()`, `.kind()`).
- **UiHost bindings** (require a Ui — headless errors at call site):
  `smelt.ui`, `smelt.win`, `smelt.buf`, `smelt.statusline`.

No reducer, no serialization-through-data, no leaky return-value side
effects like `Yank(String)`. Helix/nvim-shaped: handlers mutate via the
appropriate trait.

Two cross-runtime cases keep their typed forms because they cross channels:

- **Engine boundary** — typed via `UiCommand` in `protocol`.
- **Lua coroutine resumption** — `host.lua().resume_task(id, payload)`.
  Direct method call, not an enum variant.

Observability is `tracing::trace!` inside subsystem methods. No effect log.

## Lua surface contract

**Spec for hot paths, callback for events, coroutine for flows.** Strict by
design — far more so than nvim.

- **Hot paths (per-frame / per-token):** _spec only._ Statusline segments,
  gutter functions, decoration parsers, keymap RHS, theme/highlight, command
  metadata. Spec segments bind to `Cell`s; segments re-resolve only when a
  bound cell changes. The `{ call = fn, deps = {...} }` escape hatch runs
  Lua only on dep changes — never per-frame. Lua contributes data
  (extmarks, cell values); Rust paints.
- **Event handlers:** _callback._ Engine events, buffer attach block events,
  focus, click — fired by Rust at the right moment.
- **User flows:** _coroutine._ Picker, dialog, plugin tools — yield-driven,
  multi-step. One suspend mechanism (`mlua::Thread`); plugin code looks
  synchronous.

What we don't ship from nvim's surface: `decoration_provider`, `foldexpr`,
`on_lines`, `on_bytes`, `on_changedtick`. Hot-path callbacks aren't escape
hatches — they aren't shipped at all.

### Lua → Rust writes (TLS pointer)

Lua FFI functions reborrow `&mut App` from a TLS pointer
(`crate::lua::with_app(|app| ...)`) and call Host methods through it. Same
surface Rust handlers see — no Effect indirection. One pattern, used
everywhere.

`install_app_ptr(self)` is set at every `&mut App` site that drives Lua: the
main loop tick, startup `load_plugins`, the deferred callback drain. Lua
callbacks registered via `ui::Callbacks` would collide with the `&mut Ui`
borrow held during dispatch, so they're queued and drained after the borrow
releases.

### Bindings layout

One file per Lua namespace, all under `crates/tui/src/lua/api/<name>.rs`:
`ui.rs`, `win.rs`, `buf.rs`, `statusline.rs` (UiHost-only),
`parse.rs`, `theme.rs`, `timer.rs`, `cell.rs`, `clipboard.rs`, `cmd.rs`,
`engine.rs`, `permissions.rs`, `confirm.rs`, `mode.rs` (AgentMode
Plan/Apply/Yolo), `session.rs`, `tools.rs`, `os.rs`, `fs.rs`, `http.rs`,
`html.rs`, `notebook.rs`, `path.rs`, `grep.rs`, `fuzzy.rs`,
`subprocess.rs`, `frontend.rs`, `au.rs` (Host-tier).

No "tool FFI" / "UI FFI" tier; every namespace is just a binding file.
Each binding declares whether it needs `Host` or `UiHost`; calling a
UiHost binding from a `HeadlessApp` raises a runtime error in Lua.

## App-level events — folded into Cells

There is no separate event registry. Window-scoped events still use
`WinEvent` (Submit, Dismiss, TextChanged, …) on the `Window` /
`Overlay` surface; everything else — mode flips, model swaps, history
growth, turn boundaries, confirm lifecycle — flows through `Cells`
(see "Reactive cells" above). State-changes are stateful cells with
typed values; pure events (e.g. `turn_complete`) are `Cell<TurnMeta>`
that subsystems set and listeners subscribe to.

`smelt.au.on / smelt.au.fire` exist as a thin alias over
`smelt.cell(name):subscribe` / `smelt.cell(name):set`, kept for nvim
familiarity.

### Built-in cell-events

The same payloads listed in "Reactive cells" above. PascalCase
autocmd-style aliases for the major ones:

| Autocmd alias          | Underlying cell                      |
| ---------------------- | ------------------------------------ |
| `AgentModeChanged`     | `agent_mode`                         |
| `VimModeChanged`       | `vim_mode`                           |
| `ModelChanged`         | `model`                              |
| `ReasoningChanged`     | `reasoning`                          |
| `BranchChanged`        | `branch`                             |
| `HistoryChanged`       | `history`                            |
| `TokenUsageUpdated`    | `tokens_used`                        |
| `TurnComplete`         | `turn_complete`                      |
| `TurnError`            | `turn_error`                         |
| `SessionStarted`       | `session_started`                    |
| `SessionEnded`         | `session_ended`                      |
| `ConfirmRequested`     | `confirm_requested`                  |
| `ConfirmResolved`      | `confirm_resolved`                   |

Plugins create their own cells under a namespaced name
(`my_plugin:thing_happened`) instead of registering autocmds.

## Engine boundary — channels only

`EngineHandle` is the entire surface engine exposes to a frontend:

- `cmd_tx: Sender<UiCommand>` — frontend → engine
- `event_rx: Receiver<EngineEvent>` — engine → frontend

No trait impls, no callbacks, no UI types crossing the boundary. **No
engine-side state leaks** — today's `processes`, `permissions`, and
`runtime_approvals` fields on `EngineHandle` go away in P5 when their
owners move into `tui::*`. The same channel-only shape powers headless
and sub-agent frontends without cherry-picking engine internals.

`tui::EngineBridge` drains `event_rx` and calls Host methods directly
(focus, fire_event, perms.register). Lua/UI actions call
`app.engine.send(UiCommand)`.

**Single event loop.** One `select!` merges `terminal_rx`, `engine.event_rx`,
`lua_callback_rx`, `cells_rx`, and the animation tick. Each event dispatches
through Host directly — no Effect serialization.

### Streaming pipeline

Streaming chunks must reach the screen with the lowest possible latency,
and must not pay FFI cost per chunk. The path is:

```
EngineEvent::TextDelta { delta }
   │
   ▼
EngineBridge (Rust)
   │
   ▼
Buffer::append(span)              ' pure mutation, no Lua
   │
   ▼
loop wakes → render → diff        ' chunk visible
```

**Lua never runs per chunk.** The Buffer's attach-spec `on_block` callback
fires only at semantic boundaries: end of a markdown block, start/stop of
a tool call, end of a turn. That's the right granularity for parsing,
extmark population, and any plugin reactions.

**Mid-block turn end.** When a turn ends with an open block (e.g. the
LLM stopped mid-fence), the parser flushes the open block on
`TurnComplete` with `incomplete: true` in the payload. Lua's `on_block`
handler treats it the same as a complete block (highlights what's
there); the `incomplete` flag is informational so plugins can render a
truncation marker if they want.

The `EngineBridge` translates engine events into Buffer mutations + `WinEvent`
fires. It does not own segment state, does not orchestrate transitions, does
not maintain content caches. The Buffer is the source of truth.

## Dialogs — one question per dialog

The dialog framework is a thin wrapper: an Overlay containing N Window
panels, with focus routing and a result coroutine. **It owns no
multi-question state.** The framework primitive is "open a dialog with one
prompt; coroutine yields until submit/dismiss."

Multi-question flows are a Lua pattern, not a framework feature:

```lua
local answers = {}
for _, q in ipairs(questions) do
  local r = smelt.ui.dialog.open(spec_for(q))
  if r.action == "dismiss" then return end
  answers[q.id] = r.value
end
```

Each prior dialog has already returned by the time the next opens, so cancel
semantics are obvious (ESC the current dialog → loop returns) and the call
stack _is_ the state machine. No tab strip widget, no per-tab focus, no
"complete-all-tabs-to-submit" gate.

If a future flow wants "all questions visible at once," it builds a custom
dialog with N input panels in a Vbox — same primitives, different layout.
Not a framework concern.

## Lifecycle gates

Three small invariants the runtime enforces.

1. **Engine pauses while a confirm is pending.** `EngineBridge` checks
   `Confirms::is_clear()` before pulling the next request from the queue.
   Resumes when the dialog closes. One gate, not scattered checks.
2. **Cancellation is cooperative.** Each Lua coroutine task carries a
   `CancellationToken`. Cancel = the token flips, in-flight async Rust calls
   return `Err(Canceled)`, and the coroutine resumes with that error so
   normal Lua flow handles cleanup. No forced kill.
3. **Dialog stacking is the compositor's existing layer stack.** Open order
   = z order. Top dialog has focus; below ones stay rendered (dimmed if the
   theme says so) but cannot be focused while a modal is on top. No special
   framework code.

## Tools — Lua-owned, Rust-composed (FFI for intricate logic)

All tools live in `runtime/lua/smelt/tools/*.lua`. Bash, read, write, edit,
glob, grep, web_fetch, web_search, notebook, agent — every one. Engine
holds only the schema + dispatcher trait, no Rust impls.

**Engine asks the dispatcher → tui's `ToolRuntime` finds the Lua impl →
runs it as a coroutine → returns the result.** The coroutine yields on
async Rust calls (process spawn, HTTP fetch, FS read), resumes when they
complete.

The principle is **everything in Lua; FFI for intricate logic**. The
tool body — schema, hooks, control flow, error handling, output
formatting — is Lua. Anything that's gnarly enough to want a Rust
implementation is exposed as a capability function the Lua tool calls,
not as a Rust tool. Concrete cases:

- **Bash command parsing.** `tui::permissions.parse_bash(cmd)` returns a
  structured AST (commands, redirects, pipes, substitutions, env
  assignments, heredocs) the Lua `bash` tool walks to decide allow/deny.
  This is the 515-line bash splitter that lives in `engine` today;
  parsing in Lua would be slow and bug-prone.
- **Pattern + ruleset matching.** `tui::permissions.compile_pattern(s)` /
  `tui::permissions.match_ruleset(rules, value)` — pure glob logic.
- **Workspace boundary check.**
  `tui::permissions.outside_workspace_paths(tool, args, workspace)` —
  extracts paths from a tool call and returns those outside the
  workspace. Used by Lua tool hooks to escalate to `needs_confirm`.
- **Runtime approvals.** `tui::permissions.is_approved(tool, args)` and
  `.approve(tool, args, scope = "session" | "workspace")` — query and
  update the in-memory `RuntimeApprovals` table that records the user's
  "always allow" answers. Workspace-scoped approvals also persist
  through the workspace store.
- **Workspace store.** `tui::permissions.load_workspace(cwd)` /
  `.save_workspace(cwd, rules)` — JSON I/O for
  `~/.local/state/smelt/workspaces/<encoded-cwd>/permissions.json`.
- **Edit-with-mtime-check.** `tui::fs.apply_edit_with_mtime_check(path,
  old, new, expected_mtime)` does the read-compare-write-fsync dance
  atomically and returns a typed error on conflict. The Lua `edit_file`
  tool calls it once; no race, no half-written file.
- **Notebook AST.** `tui::notebook.parse(json)` /
  `tui::notebook.apply_edit(nb, edit)` keeps Jupyter JSON validation in
  Rust where it belongs.
- **Glob, ripgrep, html→markdown, http fetching with cache** — already
  in the `tui::*` capabilities list.

The litmus: if implementing X in Lua would be slow, fragile, or
duplicate carefully-tested Rust logic, expose X as a `tui::<cap>`
function. Never split a tool into "half Rust impl, half Lua wrapper."

Why this shape:

- **Plugin parity.** Built-in tools and plugin-authored tools land in the
  same registry. The engine doesn't distinguish — there's no
  Plugin-vs-Core split anywhere in the protocol or events.
- **All permission policy is UX-side.** Engine has no `Permissions`
  struct, no per-mode rule plumbing, no `RuntimeApprovals`. The Lua
  tool's `hooks(args, mode)` consults `tui::permissions.*` (bash AST,
  workspace check, runtime approvals, workspace store) and returns
  `"allow" | "needs_confirm" | "deny"`. AgentMode (Plan/Apply/Yolo) is
  one input the hook reads from; engine has no opinion on it. When the
  hook returns `"needs_confirm"`, engine emits `RequestPermission`;
  the user's answer flows back as `PermissionDecision`. That's the full
  engine surface for permissions.
- **Schema is data.** `ToolSchema` is name + description + JSON Schema for
  parameters. Generated from the Lua registration call.

## Rust capabilities — parse-then-present pattern

`tui::parse`, `tui::process`, `tui::subprocess`, `tui::fs`,
`tui::http`, `tui::html`, `tui::notebook`, `tui::grep`, `tui::path`,
`tui::fuzzy`, `tui::permissions` — generic primitives, _not_
tool-specific. Any Lua plugin composes from them. A statusline source
reads git state via `tui::fs`. A custom command shells out via
`tui::process`. A theme finds files via `tui::fs::glob`. A picker
ranks candidates via `tui::fuzzy`. The `bash` tool checks command
shape via `tui::permissions.parse_bash`. The `spawn_agent` tool
manages a child smelt process via `tui::subprocess`. Tools are just
one consumer.

`tui::process` (foreground process spawning, used by short-lived
shell commands) and `tui::subprocess` (long-lived child with
bidirectional event channel) are sibling primitives — short-lived
process spawning is a degenerate case of subprocess but carries its
own ergonomic shape (output capture, exit code, timeout) so it
stands as its own module.

Each module is independent (no umbrella folder) and is bound to Lua via a
sibling file in `crates/tui/src/lua/api/<name>.rs`.

For parse-then-present work (markdown, diff, syntax), the pattern is:

1. **Rust does the fast pure parse** — yields structured output.
2. **Lua walks the result** and writes Buffer extmarks via `ui::buf::*`.

Hybrid: Rust hot path, Lua presentation policy. Same shape for any future
binding.

## What goes where

| Concern                                                   | Owner                                       |
| --------------------------------------------------------- | ------------------------------------------- |
| Pixel pushing (grid, diff, SGR)                           | `ui::`                                      |
| Buffer / Window primitives                                | `ui::`                                      |
| LayoutTree / Overlay / Border / Anchor / Constraint       | `ui::`                                      |
| Compositor event routing (focus-driven keys, hit-tested mouse) | `ui::Ui`                                  |
| Theme groups, highlight resolution                        | `ui::Theme`                                 |
| Engine handle, agent loop, providers, MCP                 | `engine::`                                  |
| Wire types (UiCommand, EngineEvent, AgentMode, …)         | `protocol::`                                |
| Permission policy (rules, modes, runtime approvals, workspace store, bash AST) | `tui::permissions` capability        |
| Generic capabilities (parse / process / fs / http / permissions / …) | `tui::` (one module each)            |
| App subsystems (Session, Confirms, Clipboard, Timers, Cells) | `tui::Core`                              |
| Frontends (TuiApp, HeadlessApp)                           | `tui::` (own files)                         |
| Lua bridge (TLS pointer + bindings)                       | `tui::lua::api/<name>.rs`                   |
| Engine ↔ Lua bridge (RequestPermission, queue_event)      | `tui::EngineBridge`                         |
| Tool dispatch (ToolDispatcher impl)                       | `tui::ToolRuntime`                          |
| Slash commands (`/model`, `/theme`, `/resume`, …)         | Lua                                         |
| Dialogs (confirm, permissions, agents, rewind, …)         | Lua                                         |
| Tools (bash, read, write, edit, glob, grep, …)            | Lua (compose Rust capabilities)             |
| Statusline content                                        | Lua sources via `smelt.statusline.register` |
| Themes                                                    | Lua (`runtime/lua/smelt/colorschemes/*.lua`) |

**Test for "Rust core" vs "Lua feature":** would a _different_ plugin reuse
this exact code? Yes → Rust primitive. No → Lua feature.

## Why not ratatui

- Immediate-mode rebuilds every frame; we want grid diffing.
- No persistent windows with cursor / scroll / focus.
- No z-order beyond render order.
- `ratatui::Buffer` = cell grid; our `ui::Buffer` = content model.

We borrow only the cell-grid concept as the intermediate rendering surface.

## Why `ui` is a separate crate

- Forces clean boundaries — `ui` cannot import `protocol` or `engine`.
- Testable in isolation: buffer/window/layout/grid logic uses fake grids and
  writers. Terminal raw-mode and crossterm event adaptation stay in `tui` unless
  a real second backend appears.
- The public API surface _is_ the API. `pub` items in `ui` are the contract.

## Code rules — eternal

These describe the *target* codebase. Refactor-process rules
(red-tree-OK inside phases, friction handling, doc sync) live in
`README.md`.

- **Neovim-inspired first.** When in doubt, do what Neovim does. Deviate
  only if it improves the outcome, and say why.
- **No `#[allow(dead_code)]`.** Use it, remove it, or leave the warning as a
  tracking marker.
- **No stringly-typed dispatch.** `Callback::Lua` / `Callback::Rust` are the
  behaviour mechanism. `KeyResult::Action(String)` survives inside `ui` only
  as the widget→container internal protocol.
- **No state mirrors in `LuaShared`.** App state stays on App and is read
  live via `with_app`. `LuaShared` holds only genuine Lua-runtime state
  (handle registries, atomic counters, the coroutine runtime, the deferred
  invocation queue).
- **No cost calculus on effort.** Refactor size, time-to-land, and
  migration difficulty are not trade-offs. Only outcome quality counts:
  correctness, modularity, simplicity, maintainability.
- **Comments answer "why", not "what".** Default to writing none. Don't
  reference the current task / fix / caller — that belongs in the commit
  message.
- **One commit = one architectural move** (post-refactor norm). A migration
  that deletes the old surface alongside the new is one commit. Adding the
  new while leaving the old in place is incomplete.
- **Green tree every commit** (post-refactor norm).
  `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo nextest run --workspace`.
  *During the refactor this is relaxed inside a phase — see `README.md`.*

### Testing TUI changes

`cargo nextest run` covers unit-testable logic. For visual behaviour
(dialog rendering, layout, selection), drive the real binary in a tmux
side pane. **You're already inside tmux** — never `tmux kill-server` /
`kill-session` / `new-session`. Split the current window:

```bash
tmux split-window -h -t <session:window> -c <worktree-path> -P -F '#{pane_id}'
tmux send-keys -t %ID './target/debug/smelt' Enter
tmux capture-pane -t %ID -p | tail -N
```

Stop with `tmux send-keys -t %ID C-c`. Close the pane with
`tmux kill-pane -t %ID`.

Debug with `eprintln!` — stderr lands in
`~/.local/state/smelt/logs/stderr.log`. Tail it from another shell. Strip
the `eprintln!`s before committing.

### UI conventions

- Dialog titles are lowercase (`resume (workspace):`, `permissions`).
  Uppercase is reserved for proper nouns.
- Meta columns are `dim`, content is normal weight. Selection retints the
  whole row fg.
- Selection = fg-accent on the cursor row. No bg fill, no cursor glyph, no
  layout shift.

## Multi-agent — a plugin pattern, not an engine concept

Engine has no notion of "agents." There is no `Role::Agent`, no
`AgentBlockData`, no `AgentMessage` event, no `EngineConfig.multi_agent`,
no agent registry inside engine. From engine's perspective every
sub-agent is _just another tool call from the parent's LLM_:
`spawn_agent`, `message_agent`, `stop_agent`, etc. are Lua tools
composing one generic capability — `tui::subprocess`.

```rust
// tui::subprocess — generic IPC primitive for any long-running child.
pub struct Handle { /* opaque */ }

pub fn spawn(cmd: &str, args: &[&str], env: &HashMap<...>) -> Handle;

impl Handle {
    pub fn send(&self, msg: &[u8]);                   // → child stdin / socket
    pub fn on_event(&self, cb: LuaFn);                // event from child
    pub fn wait(&self) -> ExitStatus;                 // block on exit
    pub fn kill(&self);
}
```

The child can be anything — a sub-smelt process, a long-running bash
command, an MCP server. The parent receives structured events through
`on_event` and decides what to do with them. This generalizes past
agents into a single primitive.

A sub-agent under this model:

- `spawn_agent.lua` calls `tui::subprocess.spawn("smelt", {"--agent", id})`,
  registers an `on_event` handler that fires a Lua-side cell (e.g.
  `agent:<id>:status`), returns the handle id as the tool result.
- `message_agent.lua` finds the handle, sends a JSON message, blocks
  the coroutine until the child responds, returns the response as the
  tool result.
- `peek_agent.lua` reads the latest event payload from the cell.

The transcript renders these tool calls _the same way it renders any
other tool call_ — no special widget. A plugin that wants fancy agent
UI (live token streaming, dedicated panel, etc.) builds it on top of
the cell + a custom Buffer attach. That's a plugin author's choice,
not a built-in.

**Bidirectional async: solved by the event channel.** A sub-agent
finishing a task while the parent is mid-turn fires events into the
parent via `on_event`. Those events update Lua-side cells; subscribers
react. The parent's LLM sees the updates next turn (or is woken
mid-turn if a plugin chooses to inject something). No
`AgentMessageNotification` broadcast in engine — it's tui-side
plumbing built on a primitive that's useful for any subprocess.

**What dies:** `protocol::Role::Agent`, `protocol::AgentBlockData`,
`EngineEvent::{AgentMessage, AgentExited, Spawned}`,
`UiCommand::AgentMessage`, `engine::tools::AgentMessageNotification`,
`EngineConfig.multi_agent`, `MultiAgentConfig`, the engine-side
registry/socket modules (or they relocate behind `tui::subprocess`),
`Session.agents` / `agent_snapshots`, `transcript_present/agent.rs`.

**What's added:** `tui::subprocess` capability + Lua bindings under
`smelt.subprocess` (`spawn`, `send`, `on_event`, `wait`, `kill`).

## Configuration — one format, one entry point

User configuration lives in **one place, one language**:
`~/.config/smelt/init.lua`. No `config.yaml`, no separate keymap TOML,
no parallel format. The Lua-everywhere principle extends end to end:
permissions, providers, MCP servers, theme, keymap, model defaults,
auxiliary tasks, redaction — every option a user might set is a Lua
call.

```lua
-- ~/.config/smelt/init.lua

smelt.provider.register("anthropic", {
  api_key = os.getenv("ANTHROPIC_API_KEY"),
  default_model = "claude-opus-4-7",
})

smelt.permissions.set_rules {
  normal = { allow = { "bash:git status", "bash:ls" }, ask = { "bash:rm" } },
  apply  = { allow = { "edit_file:*" } },
}

smelt.mcp.register("filesystem", { command = "mcp-filesystem", args = { "/" } })

smelt.theme.use("default")
smelt.keymap.set("normal", "<C-p>", function() smelt.cmd.run("picker") end)
```

Embedded autoloads under `runtime/lua/smelt/` are the source of truth
for the default UX (statusline, dialogs, built-in tools, default
theme). `init.lua` runs after autoloads and can override anything.
Plugins go in `~/.config/smelt/plugins/*.lua`, autoloaded after
`init.lua`.

The Rust side ships no YAML/TOML parser for config and no schema for
"settings keys" — there is no settings registry. A setting *is* a Lua
binding's argument.

## Assumptions

- Single-threaded TUI loop. The Lua runtime is `!Send` (mlua holds Lua
  thread state); cross-thread state crosses through `Arc<Mutex<…>>`
  boundaries (engine sender, agent registries, process registry,
  agent_snapshots).
- The TLS app pointer is sound because Rust never touches its `&mut App`
  borrow while Lua is executing — the FFI call is synchronous on a single
  thread, so the reborrow inside `with_app` is sole.
- Embedded Lua modules under `runtime/lua/smelt/` are the source of truth
  for autoloaded plugins. User `~/.config/smelt/init.lua` runs after
  autoloads and can override anything. There is no second config format
  (no YAML, no TOML keymap).

## Out-of-scope tasks

- **Theme registry.** Standalone task (not a refactor step). Replaces the
  old `crate::theme` constants module. Tracked in the `task` CLI as
  `20260426-083607`.
