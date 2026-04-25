-- Built-in background_commands plugin.
--
-- Owns the `bash run_in_background=true` story end-to-end in Lua:
--   * Overrides the core `bash` tool to add the `run_in_background` flag.
--     When false (the common case) it delegates back to the core tool via
--     `smelt.tools.call`, so streaming, permissions, and timeouts come
--     for free. When true it spawns through `smelt.process.spawn_bg`
--     and returns the process id.
--   * Registers the `read_process_output` and `stop_process` tools that
--     the LLM uses to interact with backgrounded jobs.
--   * Registers the `/ps` slash-command (formerly `smelt.plugins.ps`)
--     for managing those processes from the TUI.
--
-- Acts as the canonical example of "FFI = internal API": the plugin
-- mixes plugin-tool registration, core-tool side calls, the
-- `process.*` registry, and dialog UI without any C bridging.

-- ── bash override ─────────────────────────────────────────────────────

local BG_PARAM_DESC =
"Run the command in the background and return a process ID. Use read_process_output to check output and stop_process to kill it."

smelt.tools.register({
  name = "bash",
  override = true,
  description = "Execute a non-interactive bash command and return its output. The working directory persists between calls. Commands time out after 2 minutes by default (configurable up to 10 minutes). For long-running processes set run_in_background=true. Do not use shell backgrounding (`&`) in the command string. Do not run interactive commands (editors, pagers, interactive rebases, etc.) — they will hang. If there is no non-interactive alternative, ask the user to run it themselves.",
  parameters = {
    type = "object",
    properties = {
      command = { type = "string", description = "Shell command to execute" },
      description = { type = "string", description = "Short (max 10 words) description of what this command does" },
      timeout_ms = { type = "integer", description = "Timeout in milliseconds (default: 120000, max: 600000)" },
      run_in_background = { type = "boolean", description = BG_PARAM_DESC },
    },
    required = { "command" },
  },
  execute = function(args, ctx)
    local command = args.command or ""
    if args.run_in_background then
      local err = smelt.shell.check_interactive(command)
      if err then
        return { content = err, is_error = true }
      end
      err = smelt.shell.check_background_op(command)
      if err then
        return { content = err, is_error = true }
      end
      local ok, id_or_err = pcall(smelt.process.spawn_bg, command)
      if not ok then
        return { content = tostring(id_or_err), is_error = true }
      end
      return "background process started with id: " .. id_or_err
    end

    -- Foreground delegates to the core bash tool. `run_in_background`
    -- is stripped so the core doesn't see an unknown arg.
    local fwd = {}
    for k, v in pairs(args) do
      if k ~= "run_in_background" then fwd[k] = v end
    end
    return smelt.tools.call("bash", fwd, ctx.call_id)
  end,
})

-- ── read_process_output ───────────────────────────────────────────────

local function format_read_result(output, running, exit_code)
  local status
  if running then
    status = "running"
  else
    status = string.format("exited (code %d)", exit_code or -1)
  end
  if output == nil or output == "" then
    return string.format("[%s]", status)
  end
  return string.format("%s\n[%s]", output, status)
end

smelt.tools.register({
  name = "read_process_output",
  description =
  "Read output from a background bash process (proc_1, proc_2, etc). Blocks until the process finishes by default. Set block=false for a non-blocking check of current output.",
  parameters = {
    type = "object",
    properties = {
      id = { type = "string", description = "Bash process ID (e.g. proc_1)" },
      block = { type = "boolean", description = "Wait for process to finish (default: true). Set to false for a non-blocking check." },
      timeout_ms = { type = "integer", description = "Max wait time in ms when blocking (default: 30000)" },
    },
    required = { "id" },
  },
  execute = function(args)
    local id = args.id or ""
    local block = args.block
    if block == nil then block = true end

    if not block then
      local r = smelt.process.read_output(id)
      if r == nil or next(r) == nil then
        return { content = "no process with id '" .. id .. "'", is_error = true }
      end
      return format_read_result(r.text, r.running, r.exit_code)
    end

    local timeout_ms = math.min(args.timeout_ms or 30000, 600000)
    local deadline_ms = timeout_ms
    local elapsed = 0
    local accumulated = ""

    while true do
      local r = smelt.process.read_output(id)
      if r == nil or next(r) == nil then
        return { content = "no process with id '" .. id .. "'", is_error = true }
      end
      if r.text and r.text ~= "" then
        if accumulated ~= "" then accumulated = accumulated .. "\n" end
        accumulated = accumulated .. r.text
      end
      if not r.running then
        return format_read_result(accumulated, false, r.exit_code)
      end
      if elapsed >= deadline_ms then
        return format_read_result(accumulated, true, nil)
      end
      smelt.sleep(100)
      elapsed = elapsed + 100
    end
  end,
})

-- ── stop_process ──────────────────────────────────────────────────────

smelt.tools.register({
  name = "stop_process",
  description = "Stop a running background bash process and return its accumulated output.",
  parameters = {
    type = "object",
    properties = {
      id = { type = "string", description = "Bash process ID (e.g. proc_1)" },
    },
    required = { "id" },
  },
  execute = function(args)
    local id = args.id or ""
    -- Drain whatever's been buffered before killing, then kill.
    local r = smelt.process.read_output(id)
    if r == nil or next(r) == nil then
      return { content = "no process with id '" .. id .. "'", is_error = true }
    end
    smelt.process.kill(id)
    local output = r.text or ""
    if output == "" then
      return "process stopped (no output)"
    end
    return "process stopped\n" .. output
  end,
})

-- ── /ps slash-command ─────────────────────────────────────────────────

local function format_duration(secs)
  if secs < 60 then
    return string.format("%ds", secs)
  elseif secs < 3600 then
    return string.format("%dm %ds", secs // 60, secs % 60)
  else
    local h = secs // 3600
    local rest = secs % 3600
    return string.format("%dh %dm %ds", h, rest // 60, rest % 60)
  end
end

local function format_proc(p)
  return string.format("%s — %s %s", p.command, format_duration(p.elapsed_secs or 0), p.id)
end

smelt.cmd.register("ps", function()
  local procs = smelt.process.list()
  if #procs == 0 then
    smelt.notify_error("no background processes")
    return
  end

  smelt.spawn(function()
    while true do
      procs = smelt.process.list()
      if #procs == 0 then
        return
      end

      local items = {}
      for _, p in ipairs(procs) do
        table.insert(items, { label = format_proc(p) })
      end

      local snapshot = procs
      local should_reopen = false

      smelt.ui.dialog.open({
        title = "processes",
        panels = {
          { kind = "options", items = items },
        },
        keymaps = {
          { key = "bs", hint = "\u{232b}: kill selected", on_press = function(ctx)
            if ctx.selected_index then
              local target = snapshot[ctx.selected_index]
              if target then
                smelt.process.kill(target.id)
                should_reopen = true
              end
            end
            ctx.close()
          end },
        },
      })

      if not should_reopen then
        return
      end
    end
  end)
end, { desc = "manage background processes" })
