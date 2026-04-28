# Vertical-slice trace

One user gesture, walked end-to-end through the **target** architecture.
Validates the design before P0 destruction. If something wedges, fix the
diagram / `ARCHITECTURE.md` / `REFACTOR.md` in the same change as the
fix.

**Scope:**

> User types `"list the files in the current directory"` into the prompt
> and hits Enter. The engine streams a short markdown response that
> includes a fenced code block, then issues a `bash` tool call which
> requires a permission confirm before execution. After the user
> approves, the tool runs and the result feeds back to the LLM, which
> emits a closing turn message.

That single gesture exercises: keymap dispatch, `WinEvent::Submit`, the
prompt → engine boundary, streaming through `EngineBridge`,
`Buffer::attach` block callbacks, syntect highlighting via
`tui::parse::syntax`, the tool dispatcher trait, the Lua hook returning
`needs_confirm`, the `Confirms` gate, `RequestPermission` ↔
`PermissionDecision` round-trip, `tui::process` spawning, cell-driven
statusline updates.

If any stage below has a "?" or feels invented, that's a design hole.
Document the resolution and update the canonical doc.

---

## Stage 0 — startup, before any input

`smelt` boots:

1. `main.rs` parses argv, picks `TuiApp` (no `-p`, no `--agent`).
2. Builds `Core` (config, session, confirms, clipboard, timers, cells,
   lua, tools, engine_bridge), then `TuiApp { core, well_known, ui }`.
3. `LuaRuntime::load_runtime()` — embedded `runtime/lua/smelt/*.lua`
   autoloads run first: `transcript.lua`, `diff.lua`, `status.lua`,
   `modes.lua`, `widgets/*.lua`, `dialogs/*.lua`,
   `colorschemes/default.lua`, `tools/*.lua`, `plugins/*.lua`. Each
   registers its surface (cells, callbacks, tools, commands).
4. `~/.config/smelt/init.lua` runs after autoloads — user config and
   plugin overrides. Example:

   ```lua
   -- ~/.config/smelt/init.lua

   smelt.provider.register("anthropic", {
     api_key_env = "ANTHROPIC_API_KEY",
     default_model = "claude-opus-4-7",
   })

   smelt.permissions.set_rules {
     normal = {
       bash  = { allow = { "git status", "ls", "ls -*" }, ask = { "rm *" } },
       tools = { allow = { "read_file", "glob", "grep" } },
     },
     apply = { tools = { allow = { "edit_file", "write_file" } } },
   }

   smelt.mcp.register("filesystem", {
     command = "mcp-filesystem",
     args    = { "/Users/leo/dev" },
   })

   smelt.theme.use("default")
   smelt.model.set("anthropic/claude-opus-4-7")
   smelt.reasoning.set("med")
   smelt.auto_compact(true)

   smelt.keymap.set("normal", "<C-p>", function()
     smelt.cmd.run("picker")
   end)

   smelt.au.on("TurnComplete", function(meta)
     if meta.token_usage.input > 100000 then
       smelt.notify("token usage high: " .. meta.token_usage.input)
     end
   end)
   ```

5. `Core::start()` materializes the engine config from what `init.lua`
   populated, opens an `EngineHandle` (single channel pair), and the
   event loop enters its `select!`.

6. Well-known windows open via Lua autoloads: a transcript Window on
   buffer `transcript:1`, a prompt Window on buffer `prompt:1`, a
   statusline Window driven by `status.lua`'s spec. Layout:
   ```
   Vbox {
     Leaf(transcript)  -- Fill(1)
     Leaf(prompt)      -- Length(3)
     Leaf(statusline)  -- Length(1)
   }
   ```
7. `Ui::set_focus(prompt)`. `vim_mode` cell starts as `Insert`.

State of the loop: parked, waiting on `terminal_rx`, `engine.event_rx`,
`lua_callback_rx`, `cells_rx`, `timers_rx`.

---

## Stage 1 — user hits Enter

1. **crossterm** delivers `Event::Key(Enter)` to `terminal_rx`.
2. The `select!` branches on terminal_rx; `Ui::dispatch_event` runs.
3. Hit-test isn't needed (it's a key event). Routing uses
   `Ui::focus → Window(prompt)`.
4. `prompt_window.handle(event, ctx, host)`:
   - `vim_mode = Insert`. Looks up the prompt Window's keymap recipe
     (registered by `widgets/input.lua`) for `(Insert, Enter)`.
   - Recipe entry: `{ "<CR>", function(ctx) submit(ctx) end }`.
5. The recipe calls into Lua. Lua reborrows `&mut UiHost` via the TLS
   pointer (`crate::lua::with_ui_host`).
6. Lua `submit(ctx)`:
   ```lua
   local function submit(ctx)
     local buf  = smelt.win.buf(ctx.win)
     local text = smelt.buf.yank_text(buf, smelt.buf.full_range(buf))
     -- yank_text walks extmarks; YankSubst::Static replaces @file sigils
     -- with expanded paths. Empty path → no-op (just literal text).
     if text:match("^%s*$") then return end
     smelt.buf.set_lines(buf, 0, -1, {})              -- clear prompt
     smelt.engine.start_turn { message = text }
   end
   ```
7. `smelt.engine.start_turn` is a Host-tier binding that calls
   `host.engine().send(UiCommand::StartTurn { message, … })`. The
   command goes onto `engine.cmd_rx`.
8. `Window::handle` returns `Status::Consumed`. `Ui::render` runs (the
   prompt is now empty); the diff flushes the cleared lines.

Loop returns to `select!`.

---

## Stage 2 — engine takes the turn

1. Engine's task picks up `UiCommand::StartTurn` from `cmd_rx`.
2. `agent.rs` builds the request, hands to `Provider::stream`.
3. Provider opens the HTTPS stream to Anthropic. As bytes return:
   - The provider parses chunks into `EngineEvent::TextDelta { delta }`,
     `ToolStarted`, etc.
   - These get pushed onto `event_tx` (engine-internal sender, mirrored
     into `event_rx` consumed by the frontend).
4. Engine emits `EngineEvent::Thinking` first (if reasoning enabled) →
   `cells.set("spinner_frame", ...)` indirectly via the bridge so the
   statusline starts spinning.

The engine knows nothing about who's listening. It just streams.

---

## Stage 3 — TextDelta arrives, transcript updates

1. `select!` fires on `engine.event_rx`. `EngineBridge::handle_event`:
   ```rust
   match ev {
     EngineEvent::TextDelta { delta } => {
       let buf = host.buf_mut(host.well_known().transcript);
       buf.append_at_cursor(delta);            // pure Rust mutation
     }
     // …
   }
   ```
   No Lua runs per chunk. `Buffer::append_at_cursor` advances the
   in-flight write position and bumps `content_tick`.
2. `Buffer` has `attach { parser = "markdown", on_block = … }`
   registered when the transcript window was created (see Stage 0). The
   markdown parser is part of `tui::parse::markdown` and tracks
   stream-state (open block, fence kind, fence depth) across appends.
3. **Loop iterates** (the engine event was handled — render runs).
   `Ui::render` walks splits/overlays, projects each Window over its
   Buffer into the new Grid, diffs vs previous, flushes the changed
   cells. The user sees the partial token appear.
4. Repeat for every TextDelta. **No Lua per chunk; one render per
   event; idle CPU stays at zero between events.**

When the markdown parser sees a complete `````rust ... ``` ```` block:

5. Parser fires `on_block` with payload:
   ```rust
   OnBlock {
     kind:   "code",
     lang:   Some("rust"),
     range:  (line_start, line_end),
     source: "fn main() { println!(\"hi\"); }".into(),
   }
   ```
   The callback was registered as a Lua function in `transcript.lua`:
   ```lua
   smelt.buf.attach(buf, {
     parser = "markdown",
     on_block = function(buf, block)
       if block.kind == "code" and block.lang then
         local tokens = smelt.parse.syntax(block.source, block.lang)
         smelt.buf.clear_namespace(buf, "syntax",
                                   block.range.start, block.range.stop)
         for _, tok in ipairs(tokens) do
           smelt.buf.set_extmark(buf, "syntax", tok.line, tok.col, {
             hl_id = smelt.theme.get(tok.scope),
             length = tok.length,
           })
         end
       elseif block.kind == "tool_call" then
         render_tool_block(buf, block)
       end
     end,
   })
   ```
6. The Lua callback queues — it doesn't run inline (a `&mut` borrow on
   `Ui` is live during dispatch). After `dispatch_event` returns, the
   `lua_callback_rx` drain fires the queued callbacks. They mutate the
   buffer's `"syntax"` namespace (extmarks for each token).
7. Next render: highlighted code visible.

**Design check:** does `on_block` fire fast enough? Markdown blocks end
on a closing fence; if the LLM streams the closing ``` then keeps
streaming text, the highlighted block appears immediately after the
fence closes. ✓

**Design check:** what happens if the LLM emits a code fence and never
closes it (turn ends mid-block)? The parser flushes the open block on
`turn_complete` with `kind = "code", incomplete = true`. The Lua
handler highlights the partial source the same way. ✓ (worth noting in
ARCH § Streaming.)

---

## Stage 4 — tool call event, hooks evaluated

1. The LLM's response includes a tool call: `bash { command: "rm -rf /tmp/foo" }`.
2. Engine emits `EngineEvent::ToolStarted { call_id, tool: "bash", args }`,
   then internally calls
   `dispatcher.evaluate_hooks(name="bash", args={"command":"rm -rf /tmp/foo"})`.

   **Open design point — wiring the dispatcher:**
   - Option A: engine holds `Box<dyn ToolDispatcher>` injected at
     `engine::start(config, dispatcher)`. Trait methods are async;
     engine `.await`s them.
   - Option B: engine emits `EngineEvent::EvaluateHooks { call_id, … }`
     and waits for `UiCommand::HooksResponse { call_id, result }` —
     bridge-and-channel pattern, no trait.

   Option A is cleaner if `ToolRuntime` lives in tui. Engine takes a
   trait object at startup; the trait's methods take `&mut` so the
   engine task `.await`s them on the same tokio thread. We pick A in
   P5.a; flag this here so it's not a surprise.

3. `tui::ToolRuntime::evaluate_hooks("bash", args)`:
   - Looks up `"bash"` in its registry. Found: `LuaTool { hooks_fn,
     run_fn, schema }`.
   - Spawns a Lua coroutine running `hooks_fn(args, mode, turn_ctx)`.
   - The coroutine's `hooks_fn` body lives in `tools/bash.lua`:
     ```lua
     -- runtime/lua/smelt/tools/bash.lua

     local M = {}

     M.schema = {
       name        = "bash",
       description = "Run a shell command.",
       parameters  = {
         type       = "object",
         properties = { command = { type = "string" } },
         required   = { "command" },
       },
     }

     M.hooks = function(args, mode, ctx)
       local cmd = args.command
       if not cmd or cmd == "" then
         return { decision = "deny", reason = "empty command" }
       end

       -- Already approved this exact call this session?
       if smelt.permissions.is_approved("bash", args) then
         return { decision = "allow" }
       end

       -- Walk subcommands via Rust FFI parser.
       local ast = smelt.permissions.parse_bash(cmd)
       for _, sub in ipairs(ast.commands) do
         local d = smelt.permissions.match_ruleset(
                     smelt.permissions.rules_for(mode, "bash"),
                     sub.text)
         if d == "deny" then
           return { decision = "deny", reason = "denied: " .. sub.text }
         end
         if d == "ask" then
           return {
             decision = "needs_confirm",
             reason   = "permission required for: " .. sub.text,
             approval_patterns = sub.text,  -- offered as "allow always"
           }
         end
       end

       -- Workspace boundary check (paths in args).
       local outside = smelt.permissions.outside_workspace_paths(
                         "bash", args, ctx.workspace)
       if #outside > 0 then
         return {
           decision = "needs_confirm",
           reason   = "paths outside workspace: " .. table.concat(outside, ", "),
         }
       end

       return { decision = "allow" }
     end

     M.run = function(call_id, args, ctx)
       -- Coroutine yields on subprocess.spawn / process.run.
       local result = smelt.process.run("bash", { "-c", args.command }, {
         cwd = ctx.cwd,
         timeout_ms = 60000,
       })
       return {
         stdout    = result.stdout,
         stderr    = result.stderr,
         exit_code = result.exit_code,
       }
     end

     smelt.tools.register(M)

     return M
     ```
   - The hook walks `parse_bash`, finds `"rm -rf /tmp/foo"` matches the
     `"rm *"` ask pattern from `init.lua`, returns
     `{ decision = "needs_confirm", reason = "permission required for: rm -rf /tmp/foo" }`.
4. `ToolRuntime` returns the Hooks struct to the engine.

---

## Stage 5 — confirm round-trip

1. Engine sees `decision = "needs_confirm"`. Emits
   `EngineEvent::RequestPermission { handle_id, tool: "bash", args, reason }`.
2. **Engine pauses generation.** `EngineBridge` checks
   `Confirms::is_clear()` before pulling the next event from
   `event_rx`. With a pending request, the next event sits in the
   channel until clear.
3. `EngineBridge` handles `RequestPermission`:
   - Calls `host.confirms().register(handle_id) -> oneshot::Receiver<Decision>`.
     The receiver is stashed; engine's reply will resolve it.
   - Sets `cells.set("confirm_requested", { handle_id, tool: "bash",
     args, reason })`.
4. The `confirm_requested` cell has subscribers. `dialogs/confirm.lua`
   subscribed at startup:
   ```lua
   smelt.cell("confirm_requested"):subscribe(function(req)
     local choice = smelt.ui.dialog.open {
       title  = "permissions",
       layout = vbox_with({
         text(req.tool .. ": " .. req.args.command),
         text(req.reason),
         buttons { "Approve", "Approve always", "Deny" },
       }),
       modal = true,
     }
     -- coroutine yields until the dialog returns
     if choice == "Approve" then
       smelt.confirm.resolve(req.handle_id, "allow")
     elseif choice == "Approve always" then
       smelt.permissions.approve("bash", req.args, "session")
       smelt.confirm.resolve(req.handle_id, "allow")
     else
       smelt.confirm.resolve(req.handle_id, "deny")
     end
   end)
   ```
5. The dialog opens as an `Overlay` containing a Vbox of N Windows
   (one buffer per text/button row). Focus moves into it. Statusline
   reflects modal state via the `confirms_pending` cell (already true).
6. User clicks `Approve`. The dialog's button keymap recipe fires the
   coroutine's resume value. `smelt.confirm.resolve(handle_id, "allow")`
   calls into Rust:
   ```rust
   // tui/src/lua/api/confirm.rs
   fn resolve(host: &mut dyn Host, handle_id: HandleId, decision: Decision) {
       host.confirms_mut().resolve(handle_id, decision);
       host.engine().send(UiCommand::PermissionDecision {
           request_id: handle_id, approved: matches!(decision, Decision::Allow), message: None,
       });
   }
   ```
7. Engine's task receives `UiCommand::PermissionDecision`, resumes its
   tool launch path. `Confirms::is_clear()` is true again. Engine
   pulls next pending event from event_rx.
8. Engine calls `dispatcher.dispatch(call_id, "bash", args, ctx)`.

---

## Stage 6 — dispatch runs the tool

1. `tui::ToolRuntime::dispatch` runs `bash.lua`'s `M.run` function as
   a Lua coroutine.
2. Inside `run`:
   - `smelt.process.run("bash", {"-c", "rm -rf /tmp/foo"}, {...})`
     calls into `tui::process::run`. The Lua coroutine yields.
   - `tui::process` spawns the child, captures stdout/stderr to
     buffers, awaits exit (or timeout). When done, it resumes the
     coroutine with `{ stdout, stderr, exit_code }`.
3. `run` returns the result table. `ToolRuntime` wraps it as
   `ToolResult` and returns to engine.
4. Engine receives ToolResult, emits
   `EngineEvent::ToolFinished { call_id, result }`.
5. `EngineBridge` handles ToolFinished:
   - The transcript buffer's markdown parser tracks tool blocks too.
     `on_block` fires with `{ kind: "tool", tool: "bash", call_id,
     result }`. `transcript.lua` renders it (truncated stdout, exit
     code badge, etc.).
   - `cells.set("history", { kind: "tool_finished", call_id })` for
     any plugin subscribers.
6. Engine feeds the tool result back to the LLM. The provider stream
   continues; more `TextDelta` events. Loop continues stage 3.

---

## Stage 7 — turn completes

1. Provider stream ends. Engine emits `EngineEvent::TurnComplete { meta }`
   where `meta: TurnMeta` carries token usage, agent blocks, timing.
2. `EngineBridge`:
   ```rust
   EngineEvent::TurnComplete { meta } => {
       host.session_mut().push_turn_meta(meta.clone());
       host.cells_mut().set("turn_complete", meta.clone());
       host.cells_mut().set("tokens_used", meta.token_usage);
   }
   ```
3. Subscribers fan out:
   - The `init.lua` subscriber (`smelt.au.on("TurnComplete", …)`)
     runs — fires `smelt.notify` if usage exceeds threshold.
   - `status.lua`'s statusline spec rebinds: the `tokens_used` segment
     reformats; the `now` segment ticks normally; the spinner stops.
4. `vim_mode` cell unchanged (stayed `Insert`). Cursor returns to
   prompt (focus already on prompt? — actually focus may have moved
   to the dialog and stayed on prompt after; a `focus_history.pop()`
   in dialog-close logic restores prompt focus before TurnComplete).
5. Render. Diff. Flush. User sees: highlighted code block, tool block
   showing `rm -rf /tmp/foo` exit 0, closing model text, statusline
   updated.

Loop returns to `select!`, idle.

---

## Holes the trace surfaced (resolve in this commit)

These are issues found writing the trace. Each gets a fix landed in
the same change as this doc.

1. **`ToolDispatcher` wiring.** The puml shows a trait but didn't say
   how engine reaches it. **Decision:** Option A — engine takes
   `Box<dyn ToolDispatcher>` at `engine::start(config, dispatcher)`.
   The trait's methods are `async`; the engine task awaits them on
   the same tokio thread. P5.a lists the trait shape; the wiring
   sentence needs adding.

2. **Mid-block turn end.** What happens when the LLM stops streaming
   inside an open code fence? **Decision:** the markdown parser
   flushes the open block on `turn_complete` with `incomplete: true`.
   The Lua `on_block` handler highlights it the same way. Add to
   ARCHITECTURE § Streaming.

3. **`smelt.permissions.rules_for(mode, kind)`.** Used by `bash.lua`'s
   hook. The capability namespace lists `set_rules` for writing but
   we hadn't named the read accessor. Add `rules_for(mode, kind)` to
   the FFI surface in REFACTOR P3.c and ARCHITECTURE Tools section.

4. **Focus restore after dialog close.** The dialog overlay grabs
   focus via `set_focus(dialog_button_window)`, which pushes the
   prior focus onto `focus_history`. On overlay close, focus pops.
   **Decision:** `Ui::overlay_close` automatically pops
   `focus_history`. Document in ARCHITECTURE § Focus.

5. **`smelt.process.run` vs `smelt.process.spawn`.** Trace used
   `run` for synchronous-with-output, but P3.a only listed `spawn`.
   **Decision:** `tui::process` exposes both: `spawn` (returns a
   handle for streaming) and `run` (awaits output, returns
   `{stdout, stderr, exit_code}`). Add to REFACTOR P3.a.

These holes are small — the design held up under the trace.

## What the trace did not exercise

- Multi-agent (covered separately by `tui::subprocess` design).
- Vim Visual selection / yank with extmark `YankSubst`.
- Mouse interaction (HitTarget routing, scrollbar drag).
- Compaction / title-generation auxiliary task routing.
- Cancellation (`Ctrl-C` mid-stream).

If any of those feel under-specified after P0, write a follow-up
trace. They're all P2/P5 territory.
