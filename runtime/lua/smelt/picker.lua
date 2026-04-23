-- Lua-side implementation of `smelt.api.picker.open(opts)`.
--
-- Rust still owns the focusable `ui::Picker` float (opening it needs
-- `&mut Ui` and builds a `PickerItem` list from the opts table). Once
-- the float is open, everything else — navigation keymaps, selection
-- tracking, Enter/Escape resolution — lives here. Lua keeps a local
-- `selected` counter and pushes it to Rust through
-- `smelt.api.picker.set_selected` each time the user moves.
--
-- Protocol (everything rides on `TaskWait::External`):
--   1. Alloc an external task id for the open ack, call
--      `smelt.api.picker._request_open(open_id, opts)` (queues a
--      `UiOp::OpenLuaPicker`), yield External — reducer opens the
--      focusable float and resolves with `{win_id = <u64>}`.
--   2. Alloc a second id for the final result. Register nav keymaps
--      (Up, Down, Ctrl-J/K/N/P) that update the local `selected`
--      counter and mirror it to Rust via `set_selected`.
--   3. Register Enter → resume with `{index, item}`; Esc → resume with
--      nil.
--   4. Yield `{__yield = "external", id = result_id}` and return the
--      resumed value.

local M = {}

function smelt.api.picker.open(opts)
  if not coroutine.isyieldable() then
    error("smelt.api.picker.open: call from inside smelt.task(fn) or tool.execute", 2)
  end
  if type(opts) ~= "table" then
    error("smelt.api.picker.open: expected table of options", 2)
  end
  if type(opts.items) ~= "table" then
    error("smelt.api.picker.open: opts.items must be a table", 2)
  end
  local items = opts.items
  local n = #items
  if n == 0 then
    error("smelt.api.picker.open: opts.items must be non-empty", 2)
  end

  -- Step 1: queue a picker-open op and park the task. The reducer
  -- opens the focusable float and resolves us with `{win_id = <u64>}`.
  local open_id = smelt.api.task.alloc()
  smelt.api.picker._request_open(open_id, opts)
  local opened = coroutine.yield({__yield = "external", id = open_id})
  if type(opened) ~= "table" or type(opened.win_id) ~= "number" then
    return nil
  end
  local win_id = opened.win_id

  -- Step 2: task id for the final resume.
  local task_id = smelt.api.task.alloc()

  -- Local selection state; kept in sync with the Rust `ui::Picker`
  -- through `set_selected` (0-based on the Rust side, 1-based here to
  -- match Lua conventions).
  local selected = 1

  local function move(delta)
    selected = ((selected - 1 + delta) % n) + 1
    smelt.api.picker.set_selected(win_id, selected - 1)
  end

  -- Step 3: navigation keymaps — don't resolve, just move + sync.
  smelt.api.win.set_keymap(win_id, "up",   function() move(-1) end)
  smelt.api.win.set_keymap(win_id, "down", function() move(1)  end)
  smelt.api.win.set_keymap(win_id, "c-k",  function() move(-1) end)
  smelt.api.win.set_keymap(win_id, "c-j",  function() move(1)  end)
  smelt.api.win.set_keymap(win_id, "c-p",  function() move(-1) end)
  smelt.api.win.set_keymap(win_id, "c-n",  function() move(1)  end)

  -- Step 4: Enter submits with `{index, item}`; Esc dismisses.
  smelt.api.win.set_keymap(win_id, "enter", function()
    smelt.api.win.close(win_id)
    smelt.api.task.resume(task_id, {
      index = selected,
      item = items[selected],
    })
  end)
  smelt.api.win.set_keymap(win_id, "esc", function()
    smelt.api.win.close(win_id)
    smelt.api.task.resume(task_id, nil)
  end)

  -- Step 5: park the task until a keymap resolves it.
  return coroutine.yield({__yield = "external", id = task_id})
end

return M
