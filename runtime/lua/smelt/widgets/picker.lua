-- Implements `smelt.picker.open(opts)`. Navigation, selection, and
-- Enter/Esc resolution live here; Rust provides the float window.
--
-- Selection is the buffer cursor, so wheel-scroll naturally pans the list
-- while the highlight tracks the cursor's screen row - same model as the
-- resume/rewind dialogs.

__smelt_internal.picker = __smelt_internal.picker or {}

local function new_picker_lifecycle(picker, opts)
  local task_id = smelt.task.alloc()
  local registrations = {}
  local finished = false
  local lifecycle = {}

  function lifecycle:add(registration)
    if registration then registrations[#registrations + 1] = registration end
    return registration
  end

  function lifecycle:remove(registration)
    if not registration then return end
    for i = #registrations, 1, -1 do
      if registrations[i] == registration then
        table.remove(registrations, i)
        break
      end
    end
    registration:remove()
  end

  function lifecycle:call(name, callback, ...)
    if type(callback) ~= "function" then return true end
    local ok, err = pcall(callback, ...)
    if not ok then smelt.notify.error("picker " .. name .. ": " .. tostring(err)) end
    return ok
  end

  local function teardown()
    for i = #registrations, 1, -1 do registrations[i]:remove() end
    picker:close()
  end

  local function finish(result, after_close)
    if finished then return false end
    finished = true
    teardown()
    if after_close then after_close() end
    smelt.task.resume(task_id, result)
    return true
  end

  function lifecycle:close(result)
    return finish(result)
  end

  function lifecycle:dismiss()
    return finish(nil, function()
      lifecycle:call("on_dismiss", opts.on_dismiss)
    end)
  end

  function lifecycle:wait()
    return smelt.task.wait(task_id, { interactive = true })
  end

  return lifecycle
end

__smelt_internal.picker.new_lifecycle = new_picker_lifecycle

--- Accepted value returned by `smelt.picker.open`; dismissal returns `nil`.
---@class smelt.picker.OpenResult
---@field index integer 1-based accepted item index.
---@field item any Original item from `opts.items`.
---@field action "enter"|"tab" Accept action. Floating pickers return `"enter"`.

-- Open a picker and yield until the user accepts or dismisses. Static pickers
-- use any low-level placement. Prompt-docked pickers additionally support
-- fuzzy ranking, async providers, lazy items, and persistent `on_enter`.
---@type fun(opts: smelt.picker.OpenOpts): smelt.picker.OpenResult?
function smelt.picker.open(opts)
  if not coroutine.isyieldable() then
    error("smelt.picker.open: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  if type(opts) ~= "table" then
    error("smelt.picker.open: expected table of options", 2)
  end

  local uses_prompt = opts.placement == "prompt_docked"
    or opts.provider ~= nil
    or opts.rank ~= nil
    or opts.on_enter ~= nil
    or type(opts.items) == "function"
  if uses_prompt then
    if opts.placement ~= nil and opts.placement ~= "prompt_docked" then
      error("smelt.picker.open: providers, ranking, and persistent pickers require prompt_docked placement", 2)
    end
    return __smelt_internal.picker.open_prompt(opts)
  end

  if type(opts.items) ~= "table" or #opts.items == 0 then
    error("smelt.picker.open: opts.items must be a non-empty table", 2)
  end
  local items = opts.items
  local picker = smelt.picker.new(opts)
  if not picker then return nil end
  picker:selected(math.max(1, math.min(#items, math.floor(tonumber(opts.selected) or 1))) - 1)
  local win = picker:win()
  local lifecycle = new_picker_lifecycle(picker, opts)

  local function selected_item()
    local idx = picker:selected()
    if idx == nil then return nil, nil end
    return idx, items[idx + 1]
  end

  local function fire_on_select()
    local _, item = selected_item()
    if item ~= nil then lifecycle:call("on_select", opts.on_select, item) end
  end

  local function nav(delta)
    return function()
      picker:move(delta)
      fire_on_select()
    end
  end

  lifecycle:add(win:key("up",   nav(-1)))
  lifecycle:add(win:key("down", nav(1)))
  lifecycle:add(win:key("c-k",  nav(-1)))
  lifecycle:add(win:key("c-j",  nav(1)))
  lifecycle:add(win:key("c-p",  nav(-1)))
  lifecycle:add(win:key("c-n",  nav(1)))

  lifecycle:add(win:key("enter", function()
    local idx, item = selected_item()
    lifecycle:close(idx and {
      action = "enter",
      index = idx + 1,
      item = item,
    } or nil)
  end))

  lifecycle:add(win:key("esc", function() lifecycle:dismiss() end))
  lifecycle:add(win:key("c-c", function() lifecycle:dismiss() end))

  fire_on_select()
  return lifecycle:wait()
end
