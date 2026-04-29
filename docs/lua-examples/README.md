# Lua plugin examples

Drop any of these files into `~/.config/smelt/init.lua` (or `dofile` them from your own `init.lua`) to try them out.

- **per_project.lua** — auto-load `$PWD/.smelt/init.lua` on top of the user config.
- **mode_keybinds.lua** — `<C-y>` copies transcript or prompt depending on focused window, demonstrating `smelt.win.focus()` for context-aware keybinds.
- **yank_block.lua** — opts into the optional `/yank-block` plugin and binds `<Space>y` to it.
- **statusline.lua** — three statusline sources (cwd, git branch, clock) added alongside the built-ins via `smelt.statusline.register(name, fn, opts?)`.
- **override.lua** — register a custom command (`/hello`) and remap a keybind (`<C-s>` to `/fork`).

## API surface

The full Lua API is documented in `crates/tui/src/lua.rs`. Everything
hangs off `smelt.*` — flat namespace, Neovim-style.

### Core

- `smelt.version` — API version string
- `smelt.notify(msg)` / `smelt.notify_error(msg)` — show a notification
- `smelt.clipboard(text)` — copy to system clipboard
- `smelt.keymap.set(mode, chord, fn)` — register a keymap (`"n"`, `"i"`, `"v"`, or `""` for any mode)
- `smelt.keymap.help()` — help-section table (used by `/help`)
- `smelt.on(event, fn)` — register an autocmd handler
- `smelt.defer(ms, fn)` — schedule a one-shot timer
- `smelt.spawn(fn)` — run `fn` as a coroutine-backed task (so it can yield on `sleep` / `dialog.open` / `picker.open`)
- `smelt.statusline.register(name, fn)` / `unregister(name)` — add or remove a status bar source (items append to built-ins)

### Commands

- `smelt.cmd.register(name, fn, opts?)` — register a user command (`opts.desc` for completer text)
- `smelt.cmd.run(line)` — execute a command
- `smelt.cmd.list()` — list registered command names

### Transcript / Prompt / Windows

- `smelt.transcript.text()` — transcript content (snapshot)
- `smelt.transcript.yank_block()` — copy the block under the cursor
- `smelt.buf.text()` — prompt buffer content (snapshot)
- `smelt.win.focus()` — `"transcript"` or `"prompt"`
- `smelt.win.mode()` — vim mode string (`"Normal"`, `"Insert"`, `"Visual"`)

### UI primitives

- `smelt.ui.picker.open(opts)` — focusable picker over `opts.items` (yields result)
- `smelt.ui.dialog.open(opts)` — modal dialog with panels + keymaps (yields result)
- `smelt.ui.ghost_text.set(text)` / `smelt.ui.ghost_text.clear()` — prompt ghost text

### Engine

- `smelt.engine.model()` / `set_model(key)` — current model
- `smelt.engine.mode()` / `set_mode(mode)` — agent mode (`"normal"`, `"plan"`, `"apply"`, `"yolo"`)
- `smelt.engine.reasoning_effort()` / `set_reasoning_effort(level)` — reasoning level
- `smelt.engine.is_busy()` — whether an agent turn is running
- `smelt.engine.cost()` — session cost in USD
- `smelt.engine.context_tokens()` — prompt tokens used (nil if unknown)
- `smelt.engine.context_window()` — context window size (nil if unknown)
- `smelt.engine.cancel()` — cancel the running turn
- `smelt.engine.compact(instructions?)` — compact conversation history
- `smelt.engine.submit(text)` — queue a user message for submission
- `smelt.engine.ask({system, messages?, question?, task?, on_response})` — one-shot auxiliary LLM call
- `smelt.engine.history()` — structured message history

### Session / Agents / Processes / Permissions

- `smelt.session.{title, cwd, id, dir, created_at_ms, turns, list, load, delete, rewind_to}`
- `smelt.agent.{list, kill, snapshots, peek}`
- `smelt.process.{list, kill, read_output}`
- `smelt.permissions.{list, sync}`

### Tools / Prompt sections

- `smelt.tools.register({name, description, parameters, execute, …})` — register a Lua tool
- `smelt.tools.unregister(name)` / `smelt.tools.resolve(request_id, call_id, result)`
- `smelt.prompt.set_section(name, content)` / `remove_section(name)` — custom prompt chrome

### Buffers / Windows / Tasks / Theme / Fuzzy

- `smelt.buf.{create, set_lines, set_source, add_highlight, add_dim, text}`
    - `buf.create({ mode = "plain"|"markdown"|"bash"|"file"|"diff", ... })` —
      installs a formatter that turns the buffer's source into styled,
      soft-wrapped lines. `mode = "file"` requires `path = "..."`; `mode =
      "diff"` requires `path = "..."` and takes optional `old = "..."`
      (treats `set_source` as the post-edit side).
    - `buf.set_source(buf, text)` — replace the source feeding the formatter.
      The buffer re-renders at the host panel / window's content width on
      the next frame; call repeatedly to stream updates (e.g. from
      `engine.ask`'s `on_response`).
    - `buf.set_lines(buf, lines)` — still works for plain buffers (no `mode`)
      when you want raw line control + manual highlights.
- `smelt.win.{focus, mode, close, set_keymap, on_event}`
- `smelt.task.{alloc, resume}` — external task ids for Lua coroutine plumbing
- `smelt.sleep(ms)` — yields the current task
- `smelt.theme.{accent, get, set, snapshot, is_light}`
- `smelt.fuzzy.score(text, query)`

### Events

Register handlers with `smelt.on(event, fn)`. Simple events pass `(event_name)`, data events pass `(event_name, data_table)`.

| Event | Data | Description |
|---|---|---|
| `block_done` | — | A rendered block finished |
| `cmd_pre` | — | Before a command runs |
| `cmd_post` | — | After a command completes |
| `session_start` | — | Session loaded |
| `shutdown` | — | App is quitting |
| `turn_start` | — | Agent turn dispatched |
| `turn_end` | `{ cancelled }` | Agent turn completed |
| `mode_change` | `{ from, to }` | Agent mode changed |
| `model_change` | `{ from, to }` | Model changed |
| `tool_start` | `{ tool, args }` | Tool execution started |
| `tool_end` | `{ tool, is_error, elapsed_ms }` | Tool execution finished |
| `input_submit` | `{ text }` | User submitted a message |
