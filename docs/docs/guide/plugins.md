# Plugin Authoring

smelt's runtime exposes a Lua API on the global `smelt` table. Register slash
commands, react to lifecycle events, paint extmarks, drive the engine, add
custom tools, and so on. Plugins are plain Lua files loaded from
`~/.config/smelt/plugins/*.lua`, `.smelt/plugins/*.lua`, or explicitly with
`require(...)` from `init.lua`.

The full surface lives in the [Lua API reference](../reference/api/index.md);
this page walks through the workflow and patterns for writing plugins against
it.

!!! warning "Plugins are trusted code"

    Lua configuration and plugins run in-process with the same operating-system
    privileges as smelt. They are not sandboxed. Review third-party plugins and
    project `.smelt` configuration before trusting or loading them.

## Anatomy of a plugin

A plugin is a Lua module with no manifest. Top-level statements run when the
file is loaded, so that is where you declare a stable plugin scope and register
behavior.

```lua
-- ~/.config/smelt/plugins/hello.lua
local M = smelt.plugin("hello")

M.state.greetings = M.state.greetings or 0
smelt.cmd.register("hello", function(arg)
  M.state.greetings = M.state.greetings + 1
  local name = (arg and arg ~= "") and arg or "world"
  smelt.notify.info("hello, " .. name)
end, { desc = "greet someone" })

return M
```

`smelt.plugin(name)` must run in a module body or `init.lua`. It gives the module
a stable scope, exposes ephemeral hot-reload state as `M.state`, and assigns
stable declaration-order names to unnamed buffers, windows, overlays, and paint
registrations. It does not prefix command, signal, event, keymap, or lifecycle
names, so choose globally distinctive names for those registrations.

Files under `~/.config/smelt/plugins/` load automatically. Reusable modules under
`~/.config/smelt/lua/` can be loaded explicitly from `init.lua`:

```lua
require("smelt.plugins.which_key") -- bundled opt-in plugin
require("hello")                   -- ~/.config/smelt/lua/hello.lua
```

For a real walkthrough, the bundled plugins under
[`runtime/lua/smelt/plugins/`](https://github.com/leonardcser/smelt/tree/main/runtime/lua/smelt/plugins)
are the canonical examples. Every pattern below comes straight from them.

## Virtual read-only views

The bundled `/diff` plugin composes side-by-side panes, a collapsible file tree
and a native indexed row source entirely from Lua. Use this pattern for large
read-only views instead of filling `buf:lines` with every line:

```lua
smelt.spawn(function()
  local snapshot, err = smelt.git.diff()
  if not snapshot then smelt.notify.error(err); return end
  local buf = smelt.buf.new()
  local win = smelt.win.new(buf, { surface = "readonly_text", vim_enabled = true })
  win:document(snapshot.diff:document())
  smelt.overlay.new({
    anchor = "center", width = "90%", height = "80%", modal = true,
    layout = smelt.ui.layout.leaf(win),
  })
  win:focus()
end)
```

`smelt.document.text(text)` builds a generic indexed plain-text source;
`smelt.diff.parse(patch)` indexes an existing Git unified patch. These constructors
are synchronous. `smelt.git.diff()` performs both Git acquisition and indexing
off the UI thread and supports cancellation via the parent spawn handle. Neither
constructor waits for whole-patch highlighting: visible rows request background
syntax and inline-diff chunks, and completed work wakes the renderer. Inline
ranges publish before distant syntax seeks. Text and navigation are available
while highlighting catches up; caches and parser checkpoints are bounded.

A source can back multiple windows, each with its own scroll/cursor state,
private scratch storage, and highlighting subscription. The host allocates and
protects that scratch storage; `win:buf()` still returns the original backing
buffer, which is retained unchanged and may be shared. Windows materialize only
the visible row range. Vim movement and selection can cross that range; copy
resolves against the full document in bounded batches without requesting syntax.
`win:document(nil)` restores the original buffer and view state. Sharing a document
shares its folds; `diff:view()` creates independent folds over the same patch and
syntax cache. Visible work takes priority over background syntax prefetch for the
first 128-256 rows of the selected file's previous and next neighbors.

Diff metadata and navigation stay separate from presentation: `:file_count()`
and `:file(index)` provide lazy metadata without copying all entries through Lua.
`:file_at(row)` maps a display position to a file, `:file_row(index)` resolves a
file jump after folding, and `:toggle_fold(row)` expands or collapses context
without reparsing. Both the preview and `diff:tree()` use `unstaged` then `staged`
sections, directory-first within each. Partial files have distinct entries,
patches and counts in both sections. The tree renders only viewport rows,
including section totals, colored status letters and muted nonzero line counts.
Each `diff:tree()` has independent directory folds; attached windows supply their
content width automatically, including for copying. Use `tree:node(row)` for
selection metadata and `tree:file_row(index, true)` to reveal collapsed ancestors.

`smelt.git.index(diff, index, action)` accepts `"stage"`, `"unstage"`, or `"toggle"`
and yields while updating only that file's index entries. It never edits the
worktree. Mutation success returns `index_updated=true` along with a fresh
`{root, branch, diff}` snapshot acquired and indexed off-thread. If the mutation
succeeds but loading the new snapshot fails, the result instead contains
`refresh_error` and no `diff`. Keep the old snapshot visible as stale, and refresh
before another mutation. Toggle stages an unstaged entry or unstages a staged
entry. `fresh:restore(previous, cursor, top)` preserves expanded context and maps
cursor/scroll anchors by file identity and source line numbers. Use
`diff:find_file(section, key)` to restore a raw path's selection,
`diff:section_range(section)` to advance within a section, and
`tree:section_row(section)` for an empty section. All display rows are zero-based;
file indices are one-based. `win:pan(delta)` scrolls horizontally in terminal cells
and `win:scroll().left` reports the current pan.

See [`smelt.plugins.diff`](https://github.com/leonardcser/smelt/blob/main/runtime/lua/smelt/plugins/diff.lua)
for cancellation, sidebar synchronization, refresh and keyboard handling.

## Resizable layouts

`smelt.ui.layout.hsplit(first, second, opts)` puts two subtrees side by side;
`vsplit` stacks them top to bottom. Both provide a draggable divider and compose
with ordinary `hbox`, `vbox`, and `leaf` nodes. Resizing is owned by the layout,
not by the buffers: focus, selection, and document position remain intact.

```lua
local layout = smelt.ui.layout
local files = smelt.win.new(smelt.buf.new(), { surface = "list_inert" })
local changes = smelt.win.new(smelt.buf.new(), { surface = "readonly_text" })
local body = layout.hsplit(layout.leaf(files), layout.leaf(changes), {
  size = 34, min_first = 26, min_second = 32,
})
smelt.dialog.new({ layout = body, height = 20, focus = files })
```

The same `body` can be supplied to `smelt.overlay.new({ layout = body, ... })`
or returned from a root layout composer. Retain the node outside the composer
callback so rebuilding the layout keeps the user's preferred split. `size` is
the initial first-pane extent in cells, a percentage, or `ratio:N/M`; it defaults
to `"50%"`. Minima include child borders/padding. User resizing defaults to
`resize = "proportional"`, which keeps the chosen ratio across terminal changes.
Use `resize = "cells"` to retain a fixed first-pane width or height instead.
Temporary small-terminal clamping does not discard either preference.
Fit-sized parents account for declared cell sizes and minima; unwrapped windows
(`wrap = false`) also contribute their actual content width.

For composers that replace children, retain the split separately:

```lua
local split = layout.split("horizontal", {
  size = 34, min_first = 26, min_second = 32, resize = "cells",
})
smelt.ui.layout.set(function()
  return split:layout(layout.leaf(files), layout.leaf(changes), { title = "changes" })
end)
-- Save split:size(); restore with split:set_size(saved).
-- split:resize(delta), split:equalize(), and split:reset() target this handle.
```

Identity and configuration are immutable; only preferred sizing changes. Mount
one handle once at a time. Constructor options control sizing and divider styles;
outer border/title/padding belong to `split:layout(...)`.

Use `layout.frame(body, { border = "single", padding = 1, title = "details" })`
to add chrome around any subtree. It preserves the child's natural size and
existing chrome, while letting the child fill the available inset space when
resized. Composed dialogs use a frame for their outer chrome.

`win:resize("width", 4)` grows the nearest horizontal split pane by four cells;
`win:resize("height", -1)` shrinks the nearest vertical pane by one row. Without
a matching split, these resize the containing overlay or docked dialog, using
the same bounds as its mouse handles. `win:equalize()` balances enclosing splits.
These methods return false when no corresponding resizable owner exists.

Built-in keyboard equivalents are `Ctrl-W >` / `Ctrl-W <` for width,
`Ctrl-W +` / `Ctrl-W -` for height, and `Ctrl-W =` to equalize. Explicit window,
container, and global bindings for `Ctrl-W` take precedence.

`smelt.dialog.new` and `smelt.dialog.open` accept either a composed `layout` or
the convenient `panels`/`bottom_panels` recipe, not both. They discover window
leaves automatically for focus and lifecycle callbacks. For other compositions,
`layout.windows(node)` returns unique window handles in declaration order.

## Editor setup

On every launch, smelt mirrors its embedded Lua runtime to `builtins/lua/smelt/`
in the [data directory](../reference/configuration.md#storage-paths). The default
is `~/.local/share/smelt/builtins/lua/smelt/` on Linux and macOS and
`%LOCALAPPDATA%\smelt\data\builtins\lua\smelt\` on Windows. It includes the
bundled implementation and generated LuaCATS stubs under `_meta/`. Treat the
mirror as read-only because an upgrade refreshes it.

Point Lua Language Server at the parent `builtins/lua` directory. For example,
place a `.luarc.json` in the project where you edit smelt config, replacing the
path with the expanded absolute path on your machine:

```json
{
  "runtime.version": "Lua 5.4",
  "workspace.library": [
    "/home/you/.local/share/smelt/builtins/lua"
  ],
  "diagnostics.globals": ["smelt"]
}
```

This provides completions and diagnostics for Rust-backed APIs, bundled Lua
functions, option tables, and string-literal aliases. In a source checkout, use
`runtime/lua` as the library path instead. The generated
[Lua API reference](../reference/api/index.md) renders the same annotations.

## Agent semantic code tools (LSP)

The opt-in `smelt.plugins.lsp` plugin gives the agent semantic navigation,
diagnostics, and rename tools backed by standard stdio language servers. The
server executables must already be installed and available on `PATH`.

```lua
local lsp = require("smelt.plugins.lsp")

lsp.setup({
  servers = {
    rust = {
      cmd = { "rust-analyzer" },
      extensions = { "rs" },
      language_id = "rust",
      root_markers = { "Cargo.toml", ".git" },
    },
  },
})
```

Each key under `servers` is a local label:

| Field | Description |
| ----- | ----------- |
| `cmd` | Executable followed by arguments (required) |
| `extensions` | Filename extensions handled by the server, without dots |
| `language_id` | LSP language id sent for opened documents |
| `root_markers` | Files or directories searched upward to choose a project root |
| `init_timeout_ms` | Initialize/workspace-ready timeout, default `120000` |
| `request_timeout_ms` | Per-request timeout, default `30000` |
| `startup_wait_ms` | Short wait for a starting server or fresh diagnostics, default `5000` |
| `initialization_options` | JSON-shaped value sent in the initialize request |
| `settings` | JSON-shaped workspace configuration sent after initialization |

The plugin adds `language_server_status`, `outline`, `find_symbol`,
`inspect_symbol`, `inspect_symbol_at`, `find_definition`, `find_references`,
`diagnostics`, `preview_rename`, and `rename_symbol`. Servers start lazily for the
matching file and project root. Navigation, diagnostics, and rename previews are
read effects and allowed by default. Applying `rename_symbol` is allowed in
Normal, denied in Plan, asks in Apply, and is allowed in Yolo.

## Hot reload

Saved Lua files reload automatically by default; press `F5` or run `/reload` to
reload manually. The current transcript and agent state stay put. smelt evaluates
early config, autoloaded modules, user config, and trusted project config in a
fresh candidate, then replaces its generation-owned declarations and resources
only if every file succeeds. On failure, the running commands, tools, keymaps,
hooks, providers, settings, permissions, and UI resources stay active, and
diagnostics land in `/messages`.

The transaction covers state managed by smelt's generation machinery, not every
possible effect of trusted Lua code. Arbitrary filesystem, process, network, or
other external effects are not rolled back. APIs that directly affect the live
application reject calls during candidate evaluation.

Manual reload also refreshes the on-disk inputs that feed the agent's system
prompt and tool surface, not just Lua:

- `AGENTS.md` (global `~/.config/smelt/AGENTS.md` plus the nearest project copy)
  is re-read.
- Every `SKILL.md` under `~/.config/smelt/skills/`, `~/.claude/skills/`,
  `~/.agents/skills/`, `.smelt/skills/`, `.claude/skills/`, and
  `.agents/skills/` is rescanned, so new skills, renamed skills, and edits to
  descriptions or bodies all show up on the next turn.
- MCP servers declared with `smelt.mcp.register` are reconciled: new
  registrations spawn, removed registrations stop, and servers whose config
  changed are restarted. Pending tool calls finish on the old connection.
- `--system-prompt <file>`, when the flag pointed at a file path, is re-read
  from disk.

`smelt.settings.auto_reload` defaults to `true`, so Lua config edits usually
skip the manual F5: smelt watches Lua files under `~/.config/smelt/` and
`.smelt/`, debounces a 250 ms window, then runs `/reload` for you. Edits that
land while an agent turn is running or a modal dialog is open are deferred to
the next quiet window. Prompt inputs such as `AGENTS.md`, `SKILL.md`,
`--system-prompt` files, and markdown custom-command registration still require
manual `/reload`. Existing markdown custom commands read their file body on each
invocation, so content edits do not need reload unless you add, remove, or
rename the command file.

### Surviving reload smoothly

Module bodies run with the frontend host in scope on cold start and every
successful `/reload`. Start each module with `smelt.plugin(name)` so state and
resources reconnect automatically:

1. `M.state` is a JSON-shaped table that survives `/reload`, but not process
   restart. Store `is_open`, cursor position, and other rebuildable UI state here.
2. Unnamed `smelt.buf.new`, `smelt.win.new`, `smelt.overlay.new`, and
   `smelt.paint.register` calls receive stable scope and declaration-order names.
   The matching Rust resources survive reload while their Lua callbacks, layout,
   and mutable options are replaced by the new generation.
3. Re-run `open()` from the module body when `M.state.is_open` is true. The calls
   reconnect to the existing named resources instead of duplicating them.

```lua
local M = smelt.plugin("my_plugin")

local function open()
  local buf = smelt.buf.new()
  local win = smelt.win.new(buf, { focusable = true })
  smelt.paint.register(paint_fn)
  smelt.overlay.new({ layout = smelt.ui.layout.leaf(win) })
  M.state.is_open = true
end

if M.state.is_open then open() end
return M
```

Use explicit `opts.name` values when resource identity must stay stable across
source reordering or when several constructors are created dynamically. Calling
the constructors without a plugin scope leaves them anonymous, so they are
reaped on reload.

Paint leaves can also receive pointer events directly (`press`, `release`,
`drag`), which is useful for canvas-like overlays that should not be forced
through a buffer-backed window. The leaf that owned the `press` keeps receiving
`drag` and `release` even if the pointer drifts outside its rect:

```lua
local paint = smelt.paint.register(draw_fn, { name = "myplugin.paint" })
paint:on("press",   function(ev) smelt.notify.info("down @ "..ev.row..","..ev.col) end)
paint:on("drag",    function(ev) ... end)
paint:on("release", function(ev) ... end)
```

Use `smelt.lifecycle.on_ready(fn)` only when you need code that fires _after_
every bring-up's plugin pass completes (cell subscriptions, deferred wiring).
The hook fires with `ctx = { kind = "launch" | "reload" }` so launch-only
handlers can early-return on `ctx.kind ~= "launch"`.

## Bundled plugins

<!-- BUNDLED_PLUGINS_BEGIN -->
<!-- This region is auto-generated by `cargo xtask gen-lua-docs`. Do not edit by hand. -->

Bundled with smelt. Drop a file under `~/.config/smelt/plugins/` to add your own.

### Autoloaded

Loaded on every launch unless opted out via `smelt.builtins.disable({ plugins = { "<name>" } })` in `early.lua`.

| Plugin | Summary |
| --- | --- |
| `smelt.plugins.banner` | Empty-state logo decoration + shutdown logo/resume-hint banner. |
| `smelt.plugins.compact` | Compacts older history while preserving a live recent suffix. |
| `smelt.plugins.debug_panel` | F3 debug panel. |
| `smelt.plugins.diff` | /diff: continuous, virtualized Git diff viewer. |
| `smelt.plugins.esc_chord` | Esc-Esc: cancel in-flight foreground/background work (`smelt.work.busy` tokens, e.g. /compact), or rewind to the previous turn when idle. |
| `smelt.plugins.goal` | Goal lifecycle plugin. |
| `smelt.plugins.perf_panel` | F12 perf panel. |
| `smelt.plugins.plan_mode` | Plan-mode plugin: registers the `plan` mode and `present_plan` tool. |
| `smelt.plugins.predict` | Input prediction plugin. |
| `smelt.plugins.process_control` | Ctrl-G: stop following a foreground bash job while it keeps running. |
| `smelt.plugins.scroll_pills` | Clickable scroll-pill overlays navigate the transcript. |
| `smelt.plugins.terminal_title` | Keeps the terminal window/tab title in sync with smelt. |
| `smelt.plugins.title` | Session title plugin. |
| `smelt.plugins.turn_notifications` | Optional terminal desktop notification when an agent turn ends. |
| `smelt.plugins.upgrade` | Autoupgrade plugin. |
| `smelt.plugins.version` | /version - surface the running smelt build identity as a notification. |

### Opt-in

Shipped but not autoloaded. Add `require("smelt.plugins.<name>")` to `~/.config/smelt/init.lua` to enable.

| Plugin | Summary |
| --- | --- |
| `smelt.plugins.inspect` | Optional plugin: `/inspect` opens a local web UI for browsing sessions, their history, and provider request/response audit data. |
| `smelt.plugins.lsp` | Optional LSP tool facade for agent code navigation. |
| `smelt.plugins.which_key` | Which-key style popup for pending global Lua keymaps. |

<!-- BUNDLED_PLUGINS_END -->

## Host vs UiHost

Bindings are tagged with one of two tiers. The
[Lua API index](../reference/api/index.md) groups namespaces by tier and each
per-namespace page calls it out in the header.

- **Host**: works everywhere, including headless mode (`smelt --headless`).
  Examples: `smelt.fs`, `smelt.http`, `smelt.process`, `smelt.signal`,
  `smelt.events`, `smelt.tools`.
- **UiHost**: requires a live terminal UI. Calling a UiHost function from
  headless mode raises. Examples: `smelt.win`, `smelt.buf`, `smelt.theme`,
  `smelt.notify`, `smelt.dialog`, `smelt.keymap`.

The split matters because the same plugin can run in a TUI session and in a CI
script (`smelt --headless`). Keeping UI logic behind a tier check lets you write
one plugin that works everywhere: core logic in Host, presentation layer in
UiHost.

## Slash commands

`smelt.cmd.register` adds a `/name` command. The handler receives the argument
string (everything after `/name `, possibly empty); `opts` covers description,
busy-state policy, and visibility.

```lua
smelt.cmd.register("ps", function(_arg)
  -- ...
end, {
  desc   = "manage background processes",
  busy   = "run", -- run (default), reject, queue_request, or queue_command
  hidden = false, -- skip /help and the picker
})
```

Use `smelt.cmd.run("name args")` to invoke another command (with or without the
leading slash). Markdown files in `~/.config/smelt/commands/` register
automatically; see [Custom Commands](customization.md#custom-commands) for that
path.

## Lifecycle events

`smelt.events.on(name, handler)` subscribes to runtime events: agent turns,
session load, tool start/end, and so on. Events carry only future occurrences,
so handlers receive the payload without a previous value.
`smelt.signal.subscribe(name, handler)` subscribes to durable runtime state such
as mode changes. Both return a `Reg` whose `:remove()` drops the subscription.
The full lists are the `smelt.events.Name` and `smelt.signal.Name` aliases in
[`_types.lua`](https://github.com/leonardcser/smelt/blob/main/runtime/lua/smelt/_meta/_types.lua);
common ones:

| API | Name | Payload | When |
| --- | ---- | ------- | ---- |
| event | `session_started` | none | A session has been loaded |
| event | `turn_start` | none | The agent dispatched a turn |
| event | `turn_end` | `{ cancelled }` | Turn complete or interrupted |
| event | `tool_start` | `{ tool, args }` | A tool call began |
| event | `tool_end` | `{ tool, is_error, elapsed_ms }` | A tool call finished |
| signal | `agent_mode` | `"normal"`, `"plan"`, `"apply"`, `"yolo"` | Agent mode changed |
| event | `input_submit` | submitted text | User submitted a message |
| event | `shutdown` | none | App is about to quit |

```lua
smelt.events.on("turn_end", function(payload)
  if payload.cancelled then return end
  -- ... e.g. kick off a prediction call
end)

smelt.signal.subscribe("agent_mode", function(mode)
  if mode == "plan" then activate() else deactivate() end
end)
```

You can declare your own durable signals with
`smelt.signal.new("my_plugin:state", initial)` and broadcast updates with
`smelt.signal.set("my_plugin:state", value)`. For occurrence-shaped plugin
hooks, use `smelt.events.on("my_plugin:event", handler)` and
`smelt.events.emit("my_plugin:event", payload)`.

## Provider middleware

Use provider middleware when a plugin needs to observe or rewrite assembled
assistant responses:

```lua
smelt.provider.middleware({
  on_response = function(message)
    -- inspect or return a replacement assistant message
  end,
})
```

Hooks fire in registration order; each hook sees the previous hook's
replacement. To observe streaming tokens without mutating the response,
subscribe to runtime events such as `stream_delta`. See the
[`smelt.provider` reference](../reference/api/provider.md) for exact payload
shapes.

## Keymaps

`smelt.keymap.set(mode, chord, handler)`. The mode is `"n"`, `"i"`, `"v"`, or
`""` for any mode; handlers receive a context table.

```lua
smelt.keymap.set("n", "<C-y>", function()
  if smelt.focus() == "transcript" then
    smelt.transcript.loaded_text_expensive(function(text)
      smelt.clipboard.write(text)
    end)
  else
    smelt.clipboard.write(smelt.prompt.text())
  end
end)

smelt.keymap.set("", "<Esc><Esc>", function(ctx)
  if ctx.vim_mode_at_chord_start == "insert" then
    -- ...
  end
  -- returning `false` lets the chord fall through to the next binding
end)
```

Use the callback form of `loaded_text_expensive` and
`loaded_blocks_expensive` when a sparse session may need payload hydration. The
call returns immediately, retains the callback for one invocation on the UI
thread, and releases the hydrated payload after the callback finishes. Without
a callback, these functions only inspect currently materialized content and can
return an empty string or table while hydration is pending.

Per-window bindings (transcript-only, picker-only, etc.) go through
`win:key(chord, handler)`, which returns a `Reg` whose `:remove()` undoes the
binding.

## Window events and marks

Plugins that paint into existing buffers subscribe to per-window events and draw
with marks scoped to a namespace they own. The pattern is:

```lua
local prompt = smelt.prompt.win()
local ns     = smelt.ns("my_plugin")

prompt:on("text_changed", function()
  local buf = prompt:buf()
  if not buf then return end
  buf:clear_ns(ns):mark(ns, 1, 0, {
    end_col  = 999,
    hl_group = "DiagnosticHint",
    priority = 200,
  })
end)
```

Both `"text_changed"` and the `mark` opts table are type-checked: an unknown
event name or a typo'd field surfaces as a diagnostic in your editor before the
plugin ever runs.

## Floating windows and overlays

For overlays that own their own buffer and rect, such as picker panels, perf
HUDs, and side docks, open a buffer, attach it to a window, then mount that
window in an overlay:

```lua
local buf = smelt.buf.new()
local win = smelt.win.new(buf, { focusable = false })

smelt.overlay.new({
  title     = { { text = " perf ", bold = true } },
  anchor    = "screen_at",
  corner    = "ne",
  width     = 44,           -- cells
  height    = 14,           -- cells
  modal     = false,
  draggable = true,
  layout    = smelt.ui.layout.leaf(win),
})
```

Overlay sizing is two orthogonal concepts:

- **Anchor**: where the overlay lives. Valid values:
  - `"dock_bottom"` (default), docked above the statusline.
  - `"dock_top"` / `"dock_left"` / `"dock_right"`, docked to the named edge. All
    dock anchors reserve the bottom statusline row.
  - `"center"`, centered on the screen.
  - `"screen_at"`, absolute position; pair with `corner` + `row` + `col`.
  - `"win"`, attached to another window; pair with `target` (win id), `attach`
    (corner), and `row_offset` / `col_offset`.
- **Size**: `width` / `height` set a fixed extent; `max_width` / `max_height`
  shrink-to-fit with a cap. Setting both fixed and max on the same axis is an
  error. Each value accepts:
  - an integer (cells),
  - a `"N%"` string (percent of the anchor's available extent on that axis),
  - `"fill"` (the entire available extent).

Anchor defaults: `dock_bottom` / `dock_top` are full-width × 60% tall;
`dock_left` / `dock_right` are 30% wide × full-height; `center` is 70% × 60%;
`screen_at` / `win` default to 60×20 cells.

For modal dialogs (a markdown panel + an option list + a free-text input, etc.)
`smelt.dialog.open` is the higher-level surface. It returns the result of the
user's choice. The bundled dialogs in
[`runtime/lua/smelt/dialogs/`](https://github.com/leonardcser/smelt/tree/main/runtime/lua/smelt/dialogs)
(`confirm`, `permissions`, `resume`, `rewind`) are the reference
implementations.

Dialog height has two modes:

- `height = "N%"` (or cells, or `"fill"`): fixed size. Default `"60%"`. Use this
  when the body should always fill the dock regardless of content size.
- `max_height = "N%"` (or cells, or `"fill"`): dialog shrinks to fit its
  content, capped at this value. Panels with no explicit `height` default to
  `"fit"` so a single-panel dialog actually shrinks; longer content triggers the
  panel's scrollbar at the cap. Setting both `height` and `max_height` is an
  error.

## Tasks: tool calls, dialogs, sleeps

Anything that yields (`smelt.sleep`, `smelt.dialog.open`, `smelt.picker.open`,
`smelt.tools.call`, `smelt.task.wait`) must run inside a task-yielding context.
Yielding keeps the TUI responsive: while your coroutine waits for a dialog
answer or a slow HTTP response, the main thread continues rendering and handling
input. There are two contexts:

1. **Inside `tool.execute`**: every plugin tool already runs on a coroutine.
2. **Wrapped in `smelt.spawn(fn)`**: fire-and-forget coroutine for everything
   else (a slash-command handler that opens a dialog, a timer callback that
   parks on a tool call, etc.).

```lua
smelt.cmd.register("ps", function()
  smelt.spawn(function()
    local result = smelt.dialog.open({ ... })
    if result.action == "approve" then ... end
  end)
end)
```

Calling `smelt.sleep` from outside a yielding context raises immediately; that's
how you tell which side of the line you're on.

`smelt.spawn(fn)` returns a `Reg` whose `:remove()` cancels the coroutine. Any
in-flight `smelt.sleep` / `smelt.task.wait` raises `cancelled` and the task
unwinds. When a plugin owns several reactive subscriptions, combine them with
`smelt.reg.compose(...)` and return one handle:

```lua
return smelt.reg.compose(
  smelt.keymap.set("n", "<leader>x", handler),
  smelt.fs.watch(path, on_change),
  smelt.timer.every(1000, tick)
)
```

`smelt.reg.new(fn)` wraps an arbitrary teardown function as a `Reg` for cases
that need custom cleanup logic.

If you catch a yielding call, use `smelt.task.is_cancelled(err)` to distinguish
cancellation without parsing an error message:

```lua
local ok, value = pcall(function()
  return smelt.process.run("slow-command", {})
end)
if not ok and smelt.task.is_cancelled(value) then
  return
end
if not ok then error(value, 0) end
```

Failure shapes are consistent by role. Invalid arguments and unavailable
runtime capabilities raise Lua errors. Fallible I/O returns `(value, nil)` or
`(nil, message)`; APIs add a status/code field when callers need a typed branch.
A completed process with a non-zero exit is still a process result. Tool calls
return `{ content, is_error, metadata? }` because tool failures are model-visible
results rather than Lua transport failures.

## Concurrency combinators

`smelt.task.timeout`, `smelt.task.race`, and `smelt.task.all` compose multiple
coroutines through `smelt.spawn` + `smelt.task.external`. All require a yielding
context.

```lua
-- Bound a yielding op with a deadline.
local out, err = smelt.task.timeout(2000, function()
  return smelt.process.run("slow-command", {})
end)
if err == "timeout" then ... end

-- First to finish wins; losers are cancelled.
local index, result = smelt.task.race(
  function() return smelt.fs.read_async("/etc/hostname") end,
  function() smelt.sleep(500); return "fallback" end
)
print(index, result)

-- Wait for everything; results stay in input order.
local results = smelt.task.all(
  function() return smelt.fs.read_async("a.txt") end,
  function() return smelt.fs.read_async("b.txt") end
)
```

## Plugin state

`smelt.state.get(name)` returns an ephemeral table scoped to `name`. Survives
`/reload` but not a restart. Use it for live UI state, such as whether a panel
is open, the current scroll position, or a cache that can be rebuilt. Plugins
removed since the last load have their slots swept automatically.

```lua
local s = smelt.state.get("my_plugin")
s.counter = (s.counter or 0) + 1
```

`smelt.state.persistent(name)` returns a JSON-backed wrapper that writes to
`plugins/<name>.json` in the
[state directory](../reference/configuration.md#storage-paths). Use it for user
preferences or data that must survive restarts. Top-level assignments are
debounced and auto-saved; nested mutations need an explicit `.save()` call.

```lua
local s = smelt.state.persistent("recent_files")
s.last_opened = "/path/to/file"   -- debounced auto-save
s.history = s.history or {}
table.insert(s.history, "another")
s.save()                           -- nested mutation: save explicitly
```

## Filesystem watching

`smelt.fs.watch(path, handler, opts?)` calls `handler(event)` for each
filesystem change under `path`. `event = { kind, detail?, paths }`:

- `kind`:
  `"create" | "modify" | "remove" | "rename" | "access" | "other" | "any"`.
- `detail`: finer-grained sub-kind when notify reports one. Examples:
  `kind = "create"` → `detail = "file" | "folder"`; `kind = "rename"` →
  `detail = "from" | "to" | "both"`; `kind = "modify"` →
  `detail = "data" | "metadata"`.
- `paths`: list of affected paths.

Set `opts.recursive = false` to watch only direct children. Returns a `Reg`:

```lua
local reg = smelt.fs.watch(smelt.session.info().cwd, function(ev)
  for _, p in ipairs(ev.paths) do
    smelt.log.info(ev.kind .. " " .. p)
  end
end)
-- later: reg:remove()
```

## Off-thread filesystem, process, and grep I/O

`smelt.process.run` and `smelt.grep.run` yield the calling coroutine through the
task runtime instead of blocking the main loop; they must run inside
`smelt.spawn(fn)` or a `tool.execute` body. Off-thread I/O keeps the TUI
responsive while a large file is read or a long command runs. The agent can
still stream tokens, and you can still scroll and type. The same is true for the
explicit `smelt.fs.read_async` / `smelt.fs.write_async` variants when reading or
writing large files. `smelt.fs.read` / `smelt.fs.write` stay synchronous and are
fine for small config-time reads:

```lua
smelt.spawn(function()
  local content, err = smelt.fs.read_async("/path/to/big.json")
  if not content then return io.stderr:write(err) end
  local ok = smelt.fs.write_async("/tmp/out", transform(content))

  local out = smelt.process.run("ripgrep", { "TODO", "." })
  if out then print(out.stdout) end

  local matches = smelt.grep.run("TODO", ".", { line_numbers = true })
  if matches then print(matches.stdout) end
end)
```

**Cancellation semantics.** When the calling coroutine is cancelled
(`smelt.task.timeout` deadline, `smelt.task.race` loser, or `:remove()` on the
spawn Reg), every yielding API raises `cancelled` and unwinds. The underlying
work differs by kind:

- `smelt.process.run`: the child's process group receives SIGTERM; the future
  resolves once the kill completes.
- `smelt.grep.run`: the `rg` child receives SIGKILL and the future resolves once
  `wait()` returns.
- `smelt.fs.read_async` / `smelt.fs.write_async`: the std::fs call can't be
  interrupted mid-syscall, so the worker thread runs to completion and the
  result is discarded. Bounded waste (file-size dependent); no external side
  effects leak.
- `smelt.sleep` / `smelt.task.wait`: instantaneous.

## Pickers

`smelt.picker.open(opts)` is the high-level entry point for choose-one prompts.
Items can be plain strings or `{ label, description, ansi_color, search_terms }`
records. Set `placement = "prompt_docked"` to filter them through the prompt with
`smelt.fuzzy.rank`. The call returns `{ index, item, action }` on accept or `nil`
on dismiss.

```lua
smelt.spawn(function()
  local choice = smelt.picker.open({
    items = { "first", "second", "third" },
    placement = "prompt_docked",
  })
  if choice then smelt.log.info("picked " .. choice.item) end
end)
```

## Transcript grouping and renderers

Transcript history stays flat, but the display can group adjacent matching
blocks. The built-in `explore` group combines broad discovery through
`read_file`, `grep`, `glob`, `outline`, and `find_symbol`. Focused semantic
inspection through `inspect_symbol`, `inspect_symbol_at`, `find_definition`,
`find_references`, and `diagnostics` uses the built-in `lsp` group. Adjacent
`web_search` and `web_fetch` calls use the built-in `web` group. Language server
status and rename operations remain standalone, while background-process
completion notes use their own group. User fold state is session-local.

Quick display preferences live in `smelt.settings.transcript`:

```lua
smelt.settings.transcript = {
  show_timestamps = true, -- user messages, tool calls, and tool groups
  view = {
    blocks = { thinking = "peek" },
    tools = { read_file = "collapsed", grep = "collapsed", glob = "collapsed" },
    groups = { explore = "collapsed", lsp = "collapsed", web = "collapsed" },
  },
  limits = { tool_rows = 20, thinking_peek_rows = 4 },
}
```

Register custom display-only groups with `smelt.transcript.groups.register`.
Selectors are declarative so Rust can plan adjacent runs without calling Lua for
every block. Group registration contains planning metadata only. Present the
resulting semantic group node through ordinary root-renderer middleware.

```lua
local layout = smelt.layout

smelt.transcript.groups.register({
  name = "cargo-test-batch",
  cache_key = "my.cargo-test-batch-plan:v1",
  min = 2,
  default_view = "collapsed",
  selector = {
    kind = "tool",
    name = "bash",
    terminal = true,
    fields = { ["args.description"] = "Run cargo tests" },
  },
})

smelt.transcript.extend_renderer("my.cargo-test-batch", function(next, node, ctx)
  if node.kind ~= "group" or node.name ~= "cargo-test-batch" then
    return next(node, ctx)
  end

  local summary = layout.text("ran " .. tostring(node.child_count) .. " test commands")
  if ctx.view_state ~= "expanded" then return summary end
  return layout.vbox({ summary, layout.group_children() })
end, { cache_key = "my.cargo-test-batch-renderer:v1" })
```

Use `bucket = "args.package"` or `bucket = { "name", "args.package" }` when one
rule matches several categories but should split adjacent runs by field value.
Omitting either cache key opts the corresponding planner or renderer state out of
persisted display-layout caching. Bump a key whenever its output can change across
restarts.

## Custom tools

`smelt.tools.register({ name, execute, ... })` exposes a tool to the model. Only
`name` and `execute` are required; the rest of `smelt.tools.ToolDef` is optional
metadata that controls summaries, approvals, and per-mode behaviour. Tool
transcript rendering is handled by the root transcript renderer; customize it
with `smelt.transcript.extend_renderer` when a plugin needs custom display.

```lua
smelt.tools.register({
  name        = "present_plan",
  description = "Present a written plan for the user to save as a draft, approve, or approve in apply mode.",
  modes       = { "plan" },           -- only registered in plan mode
  parameters  = {
    type = "object",
    properties = {
      title     = { type = "string", description = "..." },
      slug      = { type = "string", description = "..." },
      plan      = { type = "string", description = "..." },
      plan_path = { type = "string", description = "..." },
    },
  },
  permission_defaults = { plan = "allow" },
  summary  = function(args) return args.title or args.plan_path or "plan" end,
  execute  = function(args)
    local action = smelt.dialog.open({ ... }) -- yields, allowed inside execute
    if action == "approve" then smelt.mode.set("normal") end
    if action == "apply" then smelt.mode.set("apply") end
    return "ok"
  end,
})
```

Return either a plain string (success) or `{ content, is_error }`. From inside
`execute` you can also:

- **Chain another tool** with `smelt.tools.call("name", args, parent_call_id)`;
  it yields until the child resolves and returns its `{ content, is_error }`.
- **Park on user input** with `smelt.task.wait(id)`, resumed later by
  `smelt.task.resume(id, value)` from a key handler, event subscriber, etc.

Set `override = true` to replace a built-in tool of the same name. Use
`smelt.tools.unregister(name)` to take it back out.

Register tool presentation separately from execution. Focused callbacks let the
default renderer retain its status marker, inline duration, invocation time, and
body hierarchy while replacing semantic pieces:

```lua
smelt.transcript.register_tool("present_plan", {
  cache_key = "my.present-plan-presentation:v1",
  title = function(tool, ctx)
    return tool.args.title or tool.args.plan_path or "plan"
  end,
  body = function(tool, ctx)
    return smelt.layout.markdown(tool.args.plan or tool.output.content or "")
  end,
  compact = function(tool, ctx)
    return tool.args.plan_path
  end,
})
```

Use `render` instead when the plugin must own the complete visual result,
including the status marker and both timing values. Returning a static layout
also omits all refresh work:

```lua
smelt.transcript.register_tool("present_plan", {
  cache_key = "my.present-plan-complete:v1",
  render = function(tool, ctx)
    return smelt.layout.vbox({
      smelt.layout.text("plan: " .. (tool.args.title or "untitled")),
      smelt.layout.markdown(tool.args.plan or ""),
    })
  end,
})
```

Use `smelt.transcript.extend_renderer` for middleware that wraps multiple tools,
groups, or ordinary transcript blocks. Recursive group children pass through the
same middleware chain via `ctx.render`.

For tool-authoring conventions (parameter shape, `summary`, `approval_patterns`,
`preflight`, `paths_for_workspace`), the implementations under
[`runtime/lua/smelt/tools/`](https://github.com/leonardcser/smelt/tree/main/runtime/lua/smelt/tools)
are the source of truth.

## Statusline sources

Plugins can append segments to the statusline. The handler is called once per
refresh with the current snapshot and returns one segment or a list of segments:

```lua
local statusline = require("smelt.statusline")

statusline.add("clock", function()
  return { {
    text = os.date(" %H:%M "),
    style = { fg = { ansi = 245 } },
    priority = 2,
    align_right = true,
  } }
end)
```

`statusline.remove(name)` removes it. Built-in segments (slug, vim mode, agent
mode, running processes, permission state, and cursor position) keep rendering
alongside whatever you add.

## String-literal aliases

String parameters typed as `smelt.<namespace>.<Name>` accept a closed set of
labels. The IDE shows them in autocomplete and rejects typos. Closed aliases
require canonical names only: `smelt.vim.set_mode("normal")` works,
`smelt.vim.set_mode("n")` does not (short forms `"n"`, `"i"`, `"v"`, `"V"`, and
PascalCase variants like `"Insert"` are not accepted). Open aliases (e.g.
[`smelt.signal.Name`](../reference/api/types.md#smeltsignalname)) keep accepting
any string and just expose well-known names as completion hints.
