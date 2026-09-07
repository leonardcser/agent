-- Task-yielding primitives. Autoloaded before user init.lua so all plugins
-- see `smelt.sleep` and dialog/picker helpers can reference them.

local internal = __smelt_internal

-- Yieldable-context guard. `level = 3` so the error points at the caller's
-- caller (the user code), not this helper.
local function require_yieldable(name)
  if not coroutine.isyieldable() then
    error(name .. ": call from inside smelt.spawn(fn) or tool.execute", 3)
  end
end

-- Yield with a payload and unwrap cancellation uniformly. Internal to the
-- task primitives below - single source for the `__cancelled` check.
local function yield_with_cancel(payload)
  local result = coroutine.yield(payload)
  if type(result) == "table" and result.__cancelled then
    error("cancelled", 0)
  end
  return result
end

-- Check whether an error value represents a task or engine-ask cancellation.
-- Matches both the task-runtime `RuntimeError("cancelled")` shape and the
-- engine-ask `{ kind = "cancelled", message = "..." }` shape.
---@type fun(err: any): boolean
function smelt.task.is_cancelled(err)
  if type(err) == "table" and err.kind == "cancelled" then
    return true
  end
  if type(err) == "string" then
    local first = err:match("^([^\r\n]*)")
    return first == "cancelled"
  end
  return false
end

-- Sleep for `ms` milliseconds. Must be called from inside `smelt.spawn(fn)`
-- or a `tool.execute`. Raises `cancelled` if the task is cancelled while
-- parked.
---@type fun(ms: integer): any
function smelt.sleep(ms)
  require_yieldable("smelt.sleep")
  return yield_with_cancel({ __yield = "sleep", ms = ms })
end

-- Park the running task until `smelt.task.resume(id, value)` fires. Returns the resumed value.
-- Pass `{ interactive = true }` (or `{ pauses_deadline = true }`) for user-facing
-- waits such as dialogs; tool watchdog deadlines do not count wall time spent
-- waiting for the user.
---@type fun(id: integer, opts?: table): any
function smelt.task.wait(id, opts)
  require_yieldable("smelt.task.wait")
  opts = opts or {}
  return yield_with_cancel({
    __yield = "external",
    id = id,
    pauses_deadline = opts.pauses_deadline or opts.interactive or false,
  })
end

-- Allocate an external task id, invoke `start(id)` to kick off whatever
-- will eventually call `smelt.task.resume(id, value)` (or resolve through
-- the Rust resume sink), and park until that resolution arrives. Returns
-- the resolved value. Raises `cancelled` if the task is cancelled while
-- parked. Plugin authors bridging custom Rust extensions use this to
-- avoid hand-rolling the alloc + start + wait dance.
---@type fun(start: fun(id: integer)): any
function smelt.task.external(start)
  require_yieldable("smelt.task.external")
  local id = smelt.task.alloc()
  start(id)
  return yield_with_cancel({ __yield = "external", id = id })
end

-- Call another tool from within `execute`. Pass `parent_call_id` so streamed
-- output groups under the parent invocation. Returns `{ content, is_error, metadata? }`.
---@type fun(name: string, args: table?, parent_call_id: string?): { content: string, is_error: boolean?, metadata: table? }
function smelt.tools.call(name, args, parent_call_id)
  return smelt.task.external(function(id)
    internal.tools.__send_call(id, parent_call_id or "", name, args or {})
  end)
end

--- Format a path for a tool summary. During argument streaming, paths that may
--- collapse to the current working directory or home directory return an empty
--- string; once final, all paths use `smelt.path.display`.
---@type fun(path: string, ctx?: { final: boolean }, opts?: { prefix: string?, suffix: string? }): string
function smelt.tools.path_summary(path, ctx, opts)
  path = path or ""
  opts = opts or {}
  local shown
  if ctx == nil or ctx.final then
    shown = smelt.path.display(path)
  else
    shown = smelt.path.display_streaming(path)
  end
  if shown == "" then return "" end
  return (opts.prefix or "") .. shown .. (opts.suffix or "")
end

local function cwd_prefixes()
  local cwd = smelt.session.cwd()
  if not cwd or cwd == "" then return {} end
  local last = cwd:sub(-1)
  if last == "/" or last == "\\" then return { cwd } end
  return { cwd .. "/", cwd .. "\\" }
end

local function compact_cwd_path(path, prefixes)
  path = path or ""
  for _, prefix in ipairs(prefixes) do
    if path:sub(1, #prefix) == prefix then return path:sub(#prefix + 1) end
  end
  return path
end

-- Compact repeated absolute cwd prefixes in model-facing tool output. This is
-- display-only policy for structured path outputs, not a filesystem primitive.
function internal.tools.compact_cwd_path(path)
  return compact_cwd_path(path, cwd_prefixes())
end

-- Compact each path in an array while preserving order.
function internal.tools.compact_cwd_paths(paths)
  local prefixes = cwd_prefixes()
  if #prefixes == 0 then return paths or {} end
  local out = {}
  for i, path in ipairs(paths or {}) do
    out[i] = compact_cwd_path(path, prefixes)
  end
  return out
end

-- Remove the active working-directory prefix from the start of each line in
-- model-facing text while preserving line endings. Returns `""` for `nil`.
function internal.tools.compact_cwd_prefix_lines(content)
  if not content or content == "" then return content or "" end
  local prefixes = cwd_prefixes()
  if #prefixes == 0 then return content end

  local has_prefix = false
  for _, prefix in ipairs(prefixes) do
    if content:find(prefix, 1, true) then
      has_prefix = true
      break
    end
  end
  if not has_prefix then return content end

  local out = {}
  for line, newline in content:gmatch("([^\n]*)(\n?)") do
    if line == "" and newline == "" then break end
    out[#out + 1] = compact_cwd_path(line, prefixes) .. newline
  end
  return table.concat(out)
end

-- Attach an outer watchdog to a tool definition. This is intentionally
-- separate from the tool's own timeout handling: builtins use a small grace
-- period so their domain-specific timeout result wins before the watchdog fires.
function internal.tools.with_watchdog(def, opts)
  opts = opts or {}
  local default_ms = opts.default_ms or 30000
  local max_ms = opts.max_ms or 600000
  local grace_ms = opts.grace_ms or 0
  def.watchdog_timeout_ms = default_ms + grace_ms
  def.watchdog_max_timeout_ms = max_ms + grace_ms
  if opts.arg then def.watchdog_timeout_arg = opts.arg end
  if opts.arg_scale_ms then def.watchdog_timeout_arg_scale_ms = opts.arg_scale_ms end
  if grace_ms > 0 then def.watchdog_grace_ms = grace_ms end
  return def
end

-- Shared provider-result helpers. Async search providers used by completers and
-- prompt pickers return `{ items, searching?, scanning?, message?, status? }`.
-- Loading messages are caller-controlled so UIs can delay them or keep stale rows.
smelt.provider = smelt.provider or {}
internal.provider = internal.provider or {}

--- Provider rows and loading state returned by `smelt.provider.normalize`.
---@class smelt.provider.NormalizedResult
---@field rows table[] Rows to render after optional synthetic message insertion.
---@field result? table Original provider result when the input used the provider shape.
---@field loading boolean True while the provider is still scanning or searching.

--- Return true when a provider or normalized result is still loading.
---@type fun(result: table?): boolean
function smelt.provider.is_loading(result)
  return type(result) == "table"
    and (result.scanning == true or result.searching == true or result.loading == true)
    or false
end

--- Normalize a provider result or plain row list into renderable rows plus loading state.
---@type fun(result: table[]|table?, opts?: { show_message?: boolean, loading_message?: string }): smelt.provider.NormalizedResult
function smelt.provider.normalize(result, opts)
  opts = opts or {}
  local is_provider = type(result) == "table" and result.items ~= nil
  local rows = is_provider and (result.items or {}) or (result or {})
  local loading = smelt.provider.is_loading(result)
  local message = is_provider and (result.message or (loading and opts.loading_message))
  if is_provider and #rows == 0 and message and (opts.show_message or not loading) then
    rows = { { label = message, description = result.status, _synthetic = true } }
  end
  return { rows = rows, result = is_provider and result or nil, loading = loading }
end

--- Return the stable identity key used to preserve selection across provider refreshes.
---@type fun(item: table?): string?
function smelt.provider.item_key(item)
  if not item then return nil end
  return item.id or item.path or item.insert_text or item.label
end

--- Return true when the row list contains at least one non-synthetic row.
---@type fun(rows: table[]?): boolean
function smelt.provider.has_real_rows(rows)
  for _, row in ipairs(rows or {}) do
    if not row._synthetic then return true end
  end
  return false
end

--- Return true when the row list is exactly one synthetic status/message row.
---@type fun(rows: table[]?): boolean
function smelt.provider.synthetic_only(rows)
  local list = rows or {}
  return #list == 1 and list[1]._synthetic == true
end

-- Keep stale rows visible while a provider is loading an empty or synthetic
-- refresh, instead of flashing to a status row.
function internal.provider.should_keep_stale_rows(result, rows, current_rows)
  local list = rows or {}
  return smelt.provider.is_loading(result)
    and (#list == 0 or smelt.provider.synthetic_only(list))
    and smelt.provider.has_real_rows(current_rows)
end

-- Return the 1-based row position for a stable item key.
function internal.provider.position_of_key(rows, key)
  if not key then return nil end
  for i, item in ipairs(rows or {}) do
    if smelt.provider.item_key(item) == key then return i end
  end
  return nil
end

-- Select the fallback row unless stable-key preservation succeeds.
function internal.provider.select_row(rows, old_key, preserve, fallback)
  fallback = fallback or 1
  if preserve then
    return internal.provider.position_of_key(rows, old_key) or fallback
  end
  return fallback
end

-- Combine variadic `Reg`s into one. `:remove()` on the result fires every
-- inner `:remove()` in order, idempotent across repeat calls. Inputs may
-- include `nil` (skipped) so call sites don't need to filter. Returns a
-- `Reg`. Typical use: a plugin that owns several reactive subscriptions
-- returns one composed Reg to its caller.
--
-- ```lua
-- return smelt.reg.compose(
--   smelt.keymap.set("n", "<leader>x", handler),
--   smelt.fs.watch(path, on_change),
--   smelt.timer.every(1000, tick)
-- )
-- ```
---@type fun(...: smelt.Reg?): smelt.Reg
function smelt.reg.compose(...)
  -- Use `select("#", ...)` rather than `#regs`; `ipairs` stops at the
  -- first `nil`, which would silently skip Regs after a `nil` slot.
  local n = select("#", ...)
  local regs = { ... }
  return smelt.reg.new(function()
    for i = 1, n do
      local r = regs[i]
      if r and r.remove then r:remove() end
    end
  end)
end

-- Run `fn` with an `ms`-millisecond deadline. Returns `(result, nil)` if
-- `fn` finishes in time, or `(nil, "timeout")` if the deadline fires
-- first - in which case `fn`'s coroutine is cancelled (any in-flight
-- `smelt.sleep` / `task.wait` raises `cancelled` and the coroutine
-- unwinds). Must run inside a yielding context.
---@type fun(ms: integer, fn: fun(): any): any, string?
function smelt.task.timeout(ms, fn)
  require_yieldable("smelt.task.timeout")
  local payload = smelt.task.external(function(id)
    local timer_reg, task_reg
    local settled = false
    task_reg = smelt.spawn(function()
      local ok, value = pcall(fn)
      if settled then return end
      settled = true
      if timer_reg then timer_reg:remove() end
      smelt.task.resume(id, ok and { result = value } or { error = tostring(value) })
    end)
    timer_reg = smelt.timer.set(ms, function()
      if settled then return end
      settled = true
      task_reg:remove()
      smelt.task.resume(id, { timeout = true })
    end)
  end)
  if payload.timeout then return nil, "timeout" end
  if payload.error then return nil, payload.error end
  return payload.result, nil
end

-- Run `fns` concurrently; first to return wins. Returns the winner's
-- `(index, result)` as multi-value, mirroring `task.timeout`'s
-- `(value, err)` shape. All other branches are cancelled. Errors from
-- any branch propagate (losers cancelled first). Must run inside a
-- yielding context.
---@type fun(...: fun(): any): integer, any
function smelt.task.race(...)
  require_yieldable("smelt.task.race")
  local fns = { ... }
  if #fns == 0 then error("smelt.task.race: requires at least one function", 2) end
  local payload = smelt.task.external(function(id)
    local regs = {}
    local settled = false
    for i, fn in ipairs(fns) do
      regs[i] = smelt.spawn(function()
        local ok, value = pcall(fn)
        if settled then return end
        settled = true
        for j, r in ipairs(regs) do
          if j ~= i and r then r:remove() end
        end
        smelt.task.resume(id, ok and { index = i, result = value } or { error = tostring(value) })
      end)
    end
  end)
  if payload.error then error(payload.error, 2) end
  return payload.index, payload.result
end

-- Run `fns` concurrently; wait for all to finish. Returns an array of
-- results in the same order as the input. Errors from any branch
-- propagate; the remaining branches still complete and their results
-- are discarded. Must run inside a yielding context.
---@type fun(...: fun(): any): any[]
function smelt.task.all(...)
  require_yieldable("smelt.task.all")
  local fns = { ... }
  if #fns == 0 then return {} end
  local results = {}
  local first_err
  local payload = smelt.task.external(function(id)
    local remaining = #fns
    for i, fn in ipairs(fns) do
      smelt.spawn(function()
        local ok, value = pcall(fn)
        if ok then
          results[i] = value
        else
          first_err = first_err or tostring(value)
        end
        remaining = remaining - 1
        if remaining == 0 then
          smelt.task.resume(id, { error = first_err })
        end
      end)
    end
  end)
  if payload.error then error(payload.error, 2) end
  return results
end

-- Lifecycle guards capture stable app epochs and let async callbacks cheaply
-- ignore stale completions after a session/history/input boundary changes.
if smelt.lifecycle and smelt.signal then
  local epoch_cells = {
    session = "session_epoch",
    history = "history_epoch",
    input = "input_epoch",
  }
  local lifecycle_latest = {}

  local function normalize_guard_scopes(scopes)
    local out = {}
    local function add(scope)
      if type(scope) ~= "string" or scope == "" then return end
      table.insert(out, epoch_cells[scope] or scope)
    end

    if scopes == nil then
      return out
    end
    if type(scopes) == "string" then
      add(scopes)
      return out
    end
    if type(scopes) ~= "table" then
      error("smelt.lifecycle.guard: scopes must be a string or table", 3)
    end

    for k, v in pairs(scopes) do
      if type(k) == "number" then
        add(v)
      elseif v then
        add(k)
      end
    end
    return out
  end

  --- Cancellation and supersession guard returned by `smelt.lifecycle.guard`.
  ---@class smelt.lifecycle.Guard
  ---@field alive fun(self:smelt.lifecycle.Guard):boolean Return true while every captured epoch still matches and the guard was not cancelled or superseded.
  ---@field cancel fun(self:smelt.lifecycle.Guard) Mark the guard stale immediately.
  ---@field latest fun(self:smelt.lifecycle.Guard,key:string):smelt.lifecycle.Guard Mark this guard as the latest request for `key`; older guards with the same key become stale.
  ---@field wrap fun(self:smelt.lifecycle.Guard,fn:function):function Return a wrapper that calls `fn` only while the guard is alive.
  ---Create a guard whose `:alive()` flips false when any scoped epoch changes.
  ---Scopes are `"session"`, `"history"`, `"input"`, or a concrete signal name.
  ---Use `:latest(key)` when only the newest request in a family may complete.
  ---@type fun(scopes: string|string[]|table?): smelt.lifecycle.Guard
  function smelt.lifecycle.guard(scopes)
    local snapshot = {}
    for _, signal_name in ipairs(normalize_guard_scopes(scopes)) do
      snapshot[signal_name] = smelt.signal.get(signal_name)
    end

    local active = true
    local latest_key = nil
    local latest_token = nil
    local guard = {}

    function guard:alive()
      if not active then return false end
      if latest_key and lifecycle_latest[latest_key] ~= latest_token then return false end
      for signal_name, value in pairs(snapshot) do
        if smelt.signal.get(signal_name) ~= value then return false end
      end
      return true
    end

    function guard:cancel()
      active = false
    end

    function guard:latest(key)
      latest_key = tostring(key)
      latest_token = {}
      lifecycle_latest[latest_key] = latest_token
      return self
    end

    function guard:wrap(fn)
      return function(...)
        if self:alive() then
          return fn(...)
        end
      end
    end

    return guard
  end
end

-- `smelt.engine.ask({ guard = ... })` suppresses stale callbacks centrally.
if smelt.engine and smelt.lifecycle then
  internal.__raw_engine_ask = internal.__raw_engine_ask or smelt.engine.ask
  internal.__raw_engine_ask_inherited = internal.__raw_engine_ask_inherited or smelt.engine.ask_inherited
  local raw_engine_ask = internal.__raw_engine_ask
  local raw_engine_ask_inherited = internal.__raw_engine_ask_inherited

  local function guarded_ask(raw, spec)
    if type(spec) == "table" and spec.guard then
      local guard = spec.guard
      local wrapped = {}
      for k, v in pairs(spec) do
        if k ~= "guard" then wrapped[k] = v end
      end
      local on_response = spec.on_response
      if type(on_response) == "function" then
        wrapped.on_response = function(...)
          if guard:alive() then
            return on_response(...)
          end
        end
      end
      local on_delta = spec.on_delta
      if type(on_delta) == "function" then
        wrapped.on_delta = function(...)
          if guard:alive() then
            return on_delta(...)
          end
        end
      end
      spec = wrapped
    end
    return raw(spec)
  end

  ---@tier ui_host
  smelt.engine.ask = function(spec)
    return guarded_ask(raw_engine_ask, spec)
  end

  ---@tier ui_host
  smelt.engine.ask_inherited = function(spec)
    return guarded_ask(raw_engine_ask_inherited, spec)
  end
end

-- Idempotent across `/reload`: cache the raw register privately so each
-- bootstrap run re-wraps the same raw - never the previous wrapper.
internal.__raw_tools_register = internal.__raw_tools_register or smelt.tools.register
local raw_tools_register = internal.__raw_tools_register
smelt.tools.register = function(def)
  if type(def) == "table" then
    if def.summary == nil then
      def.summary = smelt.tools.default_summary
    end
  end
  return raw_tools_register(def)
end

local function external_or_err(start)
  local result = smelt.task.external(start)
  if result.err ~= nil then return nil, result.err end
  return result, nil
end

-- Read `path` off the main thread. Must be called from inside
-- `smelt.spawn(fn)` or a `tool.execute` (anything that runs on the Lua
-- task runtime). Returns `(content, nil)` on success or `(nil, err)` on
-- failure - same convention as `smelt.fs.read`. Third return value is the
-- file mtime in milliseconds when available.
---@type fun(path: string): string?, string?, integer?
function smelt.fs.read_async(path)
  local result = smelt.task.external(function(id) internal.fs.__start_read(id, path) end)
  if result.content ~= nil then return result.content, nil, result.mtime_ms end
  return nil, result.err, nil
end

-- Return `{ is_file, len, mtime_ms }` for `path` off the main thread, or `(nil, err)`.
---@type fun(path: string): table?, string?
function smelt.fs.file_info_async(path)
  return external_or_err(function(id) internal.fs.__start_file_info(id, path) end)
end

-- Write `contents` to `path` off the main thread. Same yielding rules as
-- `smelt.fs.read_async`. Returns `(true, nil, mtime_ms)` on success or
-- `(false, err, nil)` on failure - mirrors `smelt.fs.write`.
---@type fun(path: string, contents: string): boolean, string?, integer?
function smelt.fs.write_async(path, contents)
  local result = smelt.task.external(function(id) internal.fs.__start_write(id, path, contents) end)
  if result.ok then return true, nil, result.mtime_ms end
  return false, result.err, nil
end

-- Create `path` and parents off the main thread. Same return shape as
-- `smelt.fs.mkdir_all`.
---@type fun(path: string): boolean, string?
function smelt.fs.mkdir_all_async(path)
  local result = smelt.task.external(function(id) internal.fs.__start_mkdir_all(id, path) end)
  if result.ok then return true, nil end
  return false, result.err
end

-- Find paths matching `pattern` under `path` off the main thread. `opts`
-- accepts `max`, `max_scanned`, and `timeout_ms`. Returns a table with
-- `{ paths, scanned, truncated, timed_out }` or `(nil, err)`.
---@type fun(pattern: string, path: string?, opts: table?): table?, string?
function smelt.fs.glob_async(pattern, path, opts)
  return external_or_err(function(id) internal.fs.__start_glob(id, pattern, path or "", opts or {}) end)
end

-- Read and base64-encode a file off the main thread. Without `mime`, the MIME
-- type is inferred as for `smelt.image.read_as_data_url`.
---@type fun(path: string, mime?: string): string?, string?
function smelt.image.read_as_data_url_async(path, mime)
  local result = smelt.task.external(function(id)
    internal.image.__start_read_as_data_url(id, path, mime)
  end)
  if result.url ~= nil then return result.url, nil end
  return nil, result.err
end

if smelt.notebook then
  -- Render a notebook off the main thread. Same return shape as
  -- `smelt.notebook.read`, plus raw notebook JSON and mtime on success.
  ---@type fun(path: string, offset: integer, limit: integer): string?, string?, string?, integer?
  function smelt.notebook.read_async(path, offset, limit)
    local result = smelt.task.external(function(id)
      internal.notebook.__start_read(id, path, offset, limit)
    end)
    if result.content ~= nil then return result.content, nil, result.raw, result.mtime_ms end
    return nil, result.err, nil, nil
  end

  -- Apply a notebook edit off the main thread. Same return shape as
  -- `smelt.notebook.apply_edit`.
  ---@type fun(args: table): table?, string?
  function smelt.notebook.apply_edit_async(args)
    local result = smelt.task.external(function(id)
      internal.notebook.__start_apply_edit(id, args or {})
    end)
    if result.err ~= nil then return nil, result.err end
    return { message = result.message, metadata = result.metadata }, nil
  end
end

-- Spawn `command` as a contained background shell job. Yields while smelt
-- selects and verifies the containment backend, then returns an opaque job ID.
-- Must be called from inside `smelt.spawn(fn)` or a `tool.execute`.
---@type fun(command: string): string
function smelt.process.spawn_bg(command)
  local result = smelt.task.external(function(id)
    internal.process.__start_spawn_bg(id, command)
  end)
  if result.err ~= nil then error(result.err, 2) end
  return result.id
end

-- Run `cmd` with `args` off the main thread. Yields the calling
-- coroutine until the child exits; must be called from inside
-- `smelt.spawn(fn)` or a `tool.execute`. `opts` accepts `cwd`, `env`,
-- `timeout_secs`, `stdin`, and `max_output_bytes` (default 100 KB, maximum
-- 32 MB). Returns `({ stdout, stderr, exit_code, timed_out }, nil)` on success or
-- `(nil, err)` on spawn failure. If the calling coroutine is cancelled
-- (e.g. by `smelt.task.timeout` or by `:remove()` on the parent spawn),
-- the child process is killed (SIGTERM to its process group) and
-- `smelt.task.external` raises `cancelled` - same shape as every other
-- yielding API.
---@type fun(cmd: string, args: string[]?, opts: table?): { stdout: string, stderr: string, exit_code: integer, timed_out: boolean }?, string?
function smelt.process.run(cmd, args, opts)
  return external_or_err(function(id) internal.process.__start_run(id, cmd, args, opts) end)
end

--- Load separate unstaged (index to worktree) and staged (HEAD to index) changes
--- without passing patch bodies through Lua. Yields inside `smelt.spawn`; cancellation terminates Git. `cwd`
--- defaults to the runtime workspace and `context` to 3. The snapshot is stable
--- until explicitly refreshed; a clean repository returns an empty diff.
---@type fun(opts: { cwd: string?, context: integer? }?): { root: string, branch: string, diff: smelt.diff.Diff }?, string?
function smelt.git.diff(opts)
  require_yieldable("smelt.git.diff")
  local id = smelt.task.alloc()
  local job = internal.git.__start_diff(id, opts)
  smelt.task.wait(id)
  return job:take()
end

--- Stage, unstage, or toggle one file from a Git snapshot. File indices are
--- one-based. Toggle stages an unstaged entry and unstages a staged entry.
--- Updates the real index, never worktree contents, then acquires and indexes
--- both sections off the UI thread. Yields inside `smelt.spawn`. A successful
--- index operation returns index_updated=true even if refresh fails: diff is then
--- nil and refresh_error explains why. Retain the old snapshot as stale and
--- refresh before another mutation. nil, error means the mutation did not report
--- success; cancellation/timeouts may require refreshing to verify repository state.
---@type fun(diff: smelt.diff.Diff, file: integer, action: "stage"|"unstage"|"toggle"): { root: string, index_updated: boolean, branch: string?, diff: smelt.diff.Diff?, refresh_error: string? }?, string?
function smelt.git.index(diff, file, action)
  require_yieldable("smelt.git.index")
  local id = smelt.task.alloc()
  local job = internal.git.__start_index(id, diff, file, action)
  smelt.task.wait(id)
  return job:take()
end

-- Stop the supervised shell job `id` and return its bounded output. Yields until
-- containment termination and removes the completed job. Returns `({ text },
-- nil)` on success or `(nil, err)` when the job does not exist or cannot stop.
---@type fun(id: string): { text: string }?, string?
function smelt.process.stop(id)
  return external_or_err(function(task_id) internal.process.__start_stop(task_id, id) end)
end

-- Perform an HTTP GET against `url`. Yields the calling coroutine until the
-- response lands; the runtime stays responsive throughout. `opts` accepts
-- `headers`, `timeout_secs`, `max_redirects`, `max_response_bytes`, and
-- `max_retries`. The total timeout includes retries. `truncated` reports that
-- the body limit was reached. Non-UTF-8 response bytes are base64 encoded in
-- `body` and reported by `body_encoding = "base64"`. Cancellation of the parent
-- task drops the in-flight request and raises `cancelled` from this call.
---@type fun(url: string, opts: table?): { status: integer, final_url: string, body: string, body_encoding: string?, headers: table, truncated: boolean }?, string?
function smelt.http.get(url, opts)
  return external_or_err(function(id) internal.http.__start_get(id, url, opts) end)
end

-- Perform an HTTP POST against `url` with `body` bytes. Yields the calling
-- coroutine until the response lands. Same return shape as `smelt.http.get`.
---@type fun(url: string, body: string?, opts: table?): { status: integer, final_url: string, body: string, body_encoding: string?, headers: table, truncated: boolean }?, string?
function smelt.http.post(url, body, opts)
  return external_or_err(function(id) internal.http.__start_post(id, url, body, opts) end)
end

--- Run an authenticated request against a provider-owned endpoint using
--- smelt-managed credentials. Credentials stay in Rust; Lua receives only
--- the HTTP status and body. `opts.path` must be an absolute path without a
--- URL scheme; `opts.method` defaults to `GET`.
---@type fun(provider: string, opts: { path: string, method: string?, body: string? }): { status: integer, body: string }?, string?
function smelt.auth.request(provider, opts)
  return external_or_err(function(id) internal.auth.__start_request(id, provider, opts or {}) end)
end

--- Fetch parsed managed-provider usage. Credentials stay in Rust; Lua receives
--- provider-neutral `{ summary, limits }` rows with `{ label, used, limit,
--- resetHint? }`. Only providers with managed quota endpoints support this.
---@type fun(provider: string): { summary: table?, limits: table[] }?, string?
function smelt.auth.managed_usage(provider)
  return external_or_err(function(id) internal.auth.__start_managed_usage(id, provider) end)
end

-- Run ripgrep with `pattern` over `path` off the main thread. Yields the
-- calling coroutine until the child exits; must be called from inside
-- `smelt.spawn(fn)` or a `tool.execute`. `opts` accepts the same fields
-- as the underlying `smelt.grep` namespace (`mode`,
-- `case_insensitive`, `multiline`, `line_numbers`, `before_context`,
-- `after_context`, `context`, `glob`, `type`, `include_ignored`,
-- `timeout_secs`). Returns
-- `({ stdout, stderr, exit_code, timed_out }, nil)` on success or
-- `(nil, err)` on spawn failure. Cancellation kills the child (SIGKILL)
-- and `smelt.task.external` raises `cancelled`. Exit code 1 (no match)
-- is not an error - inspect `exit_code` on the result.
---@type fun(pattern: string, path: string, opts: table?): { stdout: string, stderr: string, exit_code: integer, timed_out: boolean }?, string?
function smelt.grep.run(pattern, path, opts)
  return external_or_err(function(id) internal.grep.__start_run(id, pattern, path, opts) end)
end

smelt.tick = smelt.tick or {}

-- Reload-safe periodic work. Subscribes to the `now` cell (a one-second
-- wall-clock tick the host publishes while the app is alive) and fires
-- `fn` at most once every `secs` seconds. `fn` runs inside a fresh
-- `smelt.spawn`, so it may yield (HTTP, processes, sleeps) without
-- blocking the cell pump.
--
-- Unlike `smelt.timer.every`, this idiom is safe to call from a
-- plugin's module body: cell subscriptions are wiped on `/reload` and
-- re-armed when the body re-runs, so no Reg juggling is required for
-- hot-reload survival.
--
-- Returns a `Reg` whose `:remove()` unsubscribes.
---@type fun(secs: integer, fn: fun()): smelt.Reg
function smelt.tick.every(secs, fn)
  if type(secs) ~= "number" or secs <= 0 then
    error("smelt.tick.every: secs must be a positive number", 2)
  end
  if type(fn) ~= "function" then
    error("smelt.tick.every: fn must be a function", 2)
  end
  local last = 0
  return smelt.signal.subscribe("now", function(now)
    if (now or 0) - last >= secs then
      last = now or 0
      smelt.spawn(function() fn() end)
    end
  end)
end

-- Filesystem watcher. Calls `handler(event)` for each event, where
-- `event = { kind, detail?, paths }`. `kind` is one of `"create" | "modify" | "remove" | "rename" | "access" | "other" | "any"`;
-- `detail` carries notify's sub-kind when one is reported (e.g. `kind = "create"` → `detail = "file" | "folder"`).
-- `opts.recursive` defaults to true; set false to watch only the immediate
-- entries of a directory. Returns a `Reg` whose `:remove()` stops the
-- watcher and cancels the polling coroutine.
---@type fun(path: string, handler: fun(event: { kind: string, detail: string?, paths: string[] }), opts: table?): smelt.Reg
function smelt.fs.watch(path, handler, opts)
  if type(path) ~= "string" then
    error("smelt.fs.watch: path must be a string", 2)
  end
  if type(handler) ~= "function" then
    error("smelt.fs.watch: handler must be a function", 2)
  end
  local watcher_id, err = internal.fs.__watch_register(path, opts or {})
  if not watcher_id then
    error("smelt.fs.watch: " .. tostring(err), 2)
  end
  local task = smelt.spawn(function()
    while true do
      local task_id = smelt.task.alloc()
      internal.fs.__watch_arm(watcher_id, task_id)
      local events = smelt.task.wait(task_id)
      if events == nil then return end
      for _, ev in ipairs(events) do
        local ok, perr = pcall(handler, ev)
        if not ok then
          smelt.log.error("fs.watch_handler_failed", {
            path = path,
            event = ev,
            error = tostring(perr),
          })
        end
      end
    end
  end)
  return smelt.reg.compose(task, smelt.reg.new(function()
    internal.fs.__watch_stop(watcher_id)
  end))
end

-- Shared spinner - pure Lua, loaded once so plugins see the same table.
if smelt.time and smelt.time.now_ms then
  smelt.spinner = smelt.spinner or require("smelt.spinner")
end

-- `smelt.theme` is UiHost. Only attach the convenience loader when it exists.
if smelt.theme then
  local bundled = require("smelt.colorschemes._two_face")
  local registry = {
    {
      name = "default",
      module = "default",
      syntax = "Monokai Extended",
      detail = "Smelt default",
    },
  }
  local syntax_names = {}
  for _, name in ipairs(smelt.theme.syntax_theme_names()) do syntax_names[name] = true end
  local bundled_syntax = {}
  for _, scheme in ipairs(bundled.schemes or {}) do
    if not syntax_names[scheme.syntax] then
      error("bundled colorscheme `" .. scheme.name .. "` references unknown syntax theme `" .. tostring(scheme.syntax) .. "`")
    end
    bundled_syntax[scheme.syntax] = true
    registry[#registry + 1] = scheme
  end
  for name in pairs(syntax_names) do
    if not bundled_syntax[name] then
      error("missing bundled colorscheme for syntax theme `" .. name .. "`")
    end
  end

  local function copy_scheme(scheme)
    if not scheme then return nil end
    return {
      name = scheme.name,
      module = scheme.module,
      syntax = scheme.syntax,
      light = scheme.light,
      detail = scheme.detail,
    }
  end

  local function resolve_scheme(name)
    for _, scheme in ipairs(registry) do
      if scheme.name == name or scheme.module == name then return scheme end
    end
    return nil
  end

  --- Return built-in colorschemes as `{ name, module, syntax, light, detail? }` rows.
  ---@type fun(): table[]
  function smelt.theme.list()
    local out = {}
    for i, scheme in ipairs(registry) do out[i] = copy_scheme(scheme) end
    return out
  end

  --- Return metadata for a built-in colorscheme by display name or module slug.
  ---@type fun(name: string): table?
  function smelt.theme.info(name)
    return copy_scheme(resolve_scheme(name))
  end

  -- Load colorscheme `name` from `runtime/lua/smelt/colorschemes/<name>.lua`
  -- and apply it. `name` may be either a module slug (`catppuccin-mocha`) or
  -- the display syntax theme name (`Catppuccin Mocha`).
  ---@tier ui_host
  ---@type fun(name: string): nil
  function smelt.theme.use(name)
    local scheme = resolve_scheme(name)
    local module = scheme and scheme.module or name
    local spec = require("smelt.colorschemes." .. module)
    if type(spec) ~= "table" then
      error("smelt.theme.use: colorscheme `" .. name .. "` must return a ThemeSpec table", 2)
    end
    smelt.theme.apply(spec)
  end

  -- Built-in session-color presets. Used by `/color`; user
  -- colorschemes can extend this list. Each entry is
  -- `{ name = string, detail = string, ansi = integer }`.
  smelt.theme.presets = smelt.theme.presets or {
    { name = "ember",    detail = "default",         ansi = 208 },
    { name = "coral",    detail = "salmon pink",     ansi = 210 },
    { name = "rose",     detail = "soft pink",       ansi = 211 },
    { name = "gold",     detail = "warm yellow",     ansi = 220 },
    { name = "ice",      detail = "cool white-blue", ansi = 159 },
    { name = "sky",      detail = "light blue",      ansi = 117 },
    { name = "blue",     detail = "classic blue",    ansi = 69  },
    { name = "lavender", detail = "cool purple",     ansi = 147 },
    { name = "lilac",    detail = "warm purple",     ansi = 183 },
    { name = "mint",     detail = "soft green",      ansi = 115 },
    { name = "sage",     detail = "muted green",     ansi = 108 },
    { name = "silver",   detail = "grey",            ansi = 244 },
  }
end

-- Per-name state. Two flavours:
--   smelt.state.get(name?)        → ephemeral table; survives /reload only.
--   smelt.state.persistent(name)  → JSON-backed wrapper; survives restart.
--
-- Ephemeral storage lives in the VM-private capability tree so bootstrap
-- re-runs preserve it without exposing it to user chunks. Touch tracking is
-- reset on every reload; Rust invokes the private sweep hook after autoload to
-- prune slots no plugin touched this cycle.
internal.__state = internal.__state or {}
internal.__persistent_state = internal.__persistent_state or {}
local state = internal.__state
local state_touched = {}
local persistent_state = internal.__persistent_state
local persistent_state_touched = {}

-- Persistent wrapper: backed by JSON under
-- `$XDG_STATE_HOME/smelt/plugins/<name>.json`. Top-level writes are
-- debounced and auto-saved; nested mutations require an explicit
-- `s.save()` call. Reads pass through to the loaded table.
local function persistent_entry(name)
  persistent_state_touched[name] = true
  local entry = persistent_state[name]
  if not entry then
    entry = { data = internal.state.__load(name), pending = nil, dirty = false }
    persistent_state[name] = entry
  end
  return entry
end

function __smelt_internal.__flush_persistent_state()
  local errors = {}
  for name, entry in pairs(persistent_state) do
    if entry.pending then entry.pending:remove(); entry.pending = nil end
    if entry.dirty then
      local ok, err = pcall(internal.state.__save, name, entry.data or {})
      if ok then
        entry.dirty = false
      else
        errors[#errors + 1] = tostring(err)
      end
    end
  end
  if #errors > 0 then error(table.concat(errors, "\n"), 2) end
end

-- Return the JSON-backed state wrapper for `name`. Top-level assignments are
-- saved after `opts.debounce_ms` (100 ms by default). Call `state.save()` after
-- mutating nested values; `state.all` exposes the underlying table. Pending
-- top-level writes are also flushed during runtime teardown and reload.
---@type fun(name: string, opts: { debounce_ms: integer? }?): table
smelt.state.persistent = function(name, opts)
  opts = opts or {}
  local debounce_ms = opts.debounce_ms or 100
  local entry = persistent_entry(name)
  local data = entry.data
  local function flush()
    if entry.pending then entry.pending:remove(); entry.pending = nil end
    internal.state.__save(name, data)
    entry.dirty = false
  end
  local function schedule()
    entry.dirty = true
    if entry.pending then return end
    entry.pending = smelt.timer.set(debounce_ms, function()
      entry.pending = nil
      internal.state.__save(name, data)
      entry.dirty = false
    end)
  end
  return setmetatable({}, {
    __index = function(_, k)
      if k == "save" then return flush end
      if k == "all" then return data end
      return data[k]
    end,
    __newindex = function(_, k, v)
      data[k] = v
      schedule()
    end,
    __pairs = function() return pairs(data) end,
  })
end

-- Plugin scope stack. Each Rust loader (autoload, init.lua, plugin
-- files) pushes a placeholder frame around the module body via the private
-- scope wrapper. The frame stays unnamed (`false`) by default -
-- the module body opts in to hot-reload survival by calling
-- `smelt.plugin("name")`, which promotes its frame to that name.
-- While the frame is named, `smelt.state.get()` and the unnamed-resource
-- constructors (`smelt.paint.register`, `smelt.overlay.new`,
-- `smelt.win.new`, `smelt.buf.new`) auto-name on the plugin's behalf
-- so survival is implicit for the rest of the body.
local scope_stack = {}
-- Per-scope per-type counter - minted in declaration order during a
-- module body run, reset every time `smelt.plugin(name)` promotes a
-- fresh frame so auto-names stay stable across `/reload` when the
-- module body runs the same constructors in the same order.
local scope_counters = {}

-- Push an unnamed frame, run `fn(...)`, pop it. The frame starts as
-- `false` (no auto-naming, no scoped state); the module body can
-- promote it to a named plugin scope via `smelt.plugin(name)`. Errors
-- propagate after restoring the stack so a failing module body doesn't
-- leak its frame.
function __smelt_internal.__with_scope(fn, ...)
  scope_stack[#scope_stack + 1] = false
  local ok, ret = pcall(fn, ...)
  scope_stack[#scope_stack] = nil
  if not ok then error(ret, 0) end
  return ret
end

-- Resolve the current scope name, if any. Returns nil when no plugin
-- scope is active (no `smelt.plugin(...)` call, or running inside a
-- callback fired from the event loop). Used by `smelt.state.get()` and
-- unnamed-resource auto-naming.
function __smelt_internal.__current_scope()
  local top = scope_stack[#scope_stack]
  if top == false then return nil end
  return top
end

-- Mint the next auto-name for `kind` ("paint" | "buf" | "win" | "overlay")
-- in the current scope. Returns nil if no scope is active. The naming
-- is `"<scope>:<kind>:<idx>"`, deterministic per (scope, kind, declaration
-- order); idx counts from 0.
function __smelt_internal.__auto_name(kind)
  local scope = internal.__current_scope()
  if not scope then return nil end
  local counters = scope_counters[scope]
  if not counters then
    counters = { paint = 0, buf = 0, win = 0, overlay = 0 }
    scope_counters[scope] = counters
  end
  local idx = counters[kind] or 0
  counters[kind] = idx + 1
  return string.format("%s:%s:%d", scope, kind, idx)
end

--- Handle returned by `smelt.plugin` for a named plugin scope.
---@class smelt.Plugin
---@field name string Stable scope name.
---@field state table Ephemeral JSON-shaped state that survives `/reload` but not restart.

-- Promote the current loader frame to plugin scope `name` and return a
-- small handle exposing the plugin's per-cycle state slot:
--
--   local M = smelt.plugin("banner")
--   M.state.fires = 0          -- M.state is smelt.state.get("banner")
--   M.name == "banner"
--
-- After this call, bare `smelt.state.get()` also resolves to the named
-- slot and unnamed resource constructors auto-name keyed by `name`.
-- Idempotent within a single module body run: counters reset on every
-- promotion so declaration order is what matters.
--
-- The handle deliberately doesn't wrap `smelt.signal` / `smelt.cmd` /
-- `smelt.keymap` / `smelt.lifecycle.*` - those calls would not be
-- scope-aware (signal/cmd names are global), so a method facade would
-- imply encapsulation it can't deliver. Call them directly through
-- `smelt.*` and namespace your signal/cmd names explicitly.
--
-- Must be called from a module body (or init.lua). Outside a loader
-- frame (e.g. from an event callback) it raises immediately.
---@type fun(name: string): smelt.Plugin
function smelt.plugin(name)
  if type(name) ~= "string" or name == "" then
    error("smelt.plugin: name must be a non-empty string", 2)
  end
  local i = #scope_stack
  if i == 0 then
    error("smelt.plugin: must be called from a module body (or init.lua)", 2)
  end
  scope_stack[i] = name
  scope_counters[name] = { paint = 0, buf = 0, win = 0, overlay = 0 }
  return setmetatable({ name = name }, {
    __index = function(_, key)
      if key == "state" then return smelt.state.get(name) end
    end,
  })
end

-- Return an ephemeral state table. With an explicit name, returns the
-- table for that name. With no arg, returns the current plugin's scoped
-- table keyed by the current scope name. Raises if called with no arg
-- outside a module body (no scope active).
---@type fun(name?: string): table
function smelt.state.get(name)
  if name == nil then
    name = internal.__current_scope()
    if not name then
      error("smelt.state.get(): no plugin scope active - pass an explicit name from outside module body", 2)
    end
  end
  state_touched[name] = true
  local s = state[name]
  if not s then
    s = {}
    state[name] = s
  end
  return s
end

function __smelt_internal.__sweep_state()
  for k in pairs(state) do
    if not state_touched[k] then
      state[k] = nil
    end
  end
  for k, entry in pairs(persistent_state) do
    if not persistent_state_touched[k] and not entry.dirty then
      persistent_state[k] = nil
    end
  end
end

if smelt and smelt.notify then
  --- Source-bound notify handle returned by `smelt.notify.scoped`.
  --- Use `handle.info(msg)`, `handle.error(msg)`, or `handle.warn(msg)` to
  --- tag every toast with the bound source.
  ---@class smelt.notify.Scoped
  ---@field info fun(msg: string) Raise an informational toast tagged with the bound source.
  ---@field error fun(msg: string) Raise an error toast tagged with the bound source.
  ---@field warn fun(msg: string) Raise a warning toast tagged with the bound source.

  -- Bind `source` once and return a small bag that forwards to
  -- `smelt.notify.info` / `smelt.notify.error` / `smelt.notify.warn` with the
  -- source pinned. A plugin opts in with one line at the top of the file:
  --   local notify = smelt.notify.scoped("upgrade")
  --   notify.info("downloading …")
  --   notify.error("/upgrade: spawn failed")
  -- so the per-call-site `, "upgrade"` repetition goes away. Same toast +
  -- `/messages` semantics as the underlying calls. Skipped on headless
  -- where `smelt.notify` isn't bound.
  ---@type fun(source: string): smelt.notify.Scoped
  function smelt.notify.scoped(source)
    if type(source) ~= "string" or source == "" then
      error("smelt.notify.scoped: source must be a non-empty string", 2)
    end
    return {
      info  = function(msg) smelt.notify.info(msg, source) end,
      error = function(msg) smelt.notify.error(msg, source) end,
      warn  = function(msg) smelt.notify.warn(msg, source) end,
    }
  end
end

if smelt and smelt.model then
  local function prefs() return smelt.state.persistent("model_preferred") end
  -- Read or update a named model preference. With only `name`, returns the
  -- stored model key or `nil`. Passing a second argument stores that model key;
  -- passing `nil` explicitly clears it. Preferences survive Lua reloads through
  -- `smelt.state.persistent("model_preferred")`.
  ---@type fun(name: string, ...: string?): string?
  smelt.model.preferred = function(name, ...)
    if select("#", ...) == 0 then
      return prefs()[name]
    end
    local value = ...
    prefs()[name] = value
    return value
  end
end
