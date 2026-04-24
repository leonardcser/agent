-- `smelt.prompt.open_picker(opts)`
--
-- Thin wrapper over the Rust-backed `ArgPicker` completer. The
-- completer owns the prompt while open: typed characters filter
-- instead of spawning new modes, Tab inserts the highlighted label,
-- Enter accepts and resumes us with `{index, item, action = "enter"}`
-- (caller runs the command), Esc resumes with `nil`.
--
-- `opts` accepts:
--   items     = { { label, description?, ansi_color?, search_terms? }, ... }
--   on_select = function(item) -- optional, fires on nav for preview
--
-- Returns `{index, item, action}` on accept (action is `"enter"` or
-- `"tab"`), `nil` on dismiss.

function smelt.prompt.open_picker(opts)
  if not coroutine.isyieldable() then
    error("smelt.prompt.open_picker: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  if type(opts) ~= "table" then
    error("smelt.prompt.open_picker: expected table of options", 2)
  end
  if type(opts.items) ~= "table" or #opts.items == 0 then
    error("smelt.prompt.open_picker: opts.items must be a non-empty table", 2)
  end

  local items = opts.items

  -- Wrap the user's `on_select` so Rust can fire it by index and we
  -- hand the full Lua item back. When no `on_select` is provided,
  -- don't pass one — Rust skips the event entirely.
  local rust_on_select
  if type(opts.on_select) == "function" then
    rust_on_select = function(arg)
      local idx = arg and arg.index
      local ok, err = pcall(opts.on_select, idx and items[idx] or nil)
      if not ok then
        smelt.notify_error("prompt picker on_select: " .. tostring(err))
      end
    end
  end

  local task_id = smelt.task.alloc()
  smelt.prompt._request_arg_picker(task_id, {
    items     = items,
    on_select = rust_on_select,
  })
  local result = smelt.task.wait(task_id)
  if type(result) ~= "table" then return nil end
  result.item = items[result.index]
  return result
end
