-- Lua-side implementation of `smelt.ui.picker.open(opts)`.
--
-- Rust exposes the low-level `smelt.ui.picker._open(opts) -> win_id`
-- which synchronously creates the focusable float. Everything else —
-- navigation keymaps, selection tracking, Enter/Escape resolution —
-- lives here. Lua keeps a local `selected` counter and mirrors it to
-- Rust through `smelt.ui.picker.set_selected` on every move.

local M = {}

function smelt.ui.picker.open(opts)
  if not coroutine.isyieldable() then
    error("smelt.ui.picker.open: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  if type(opts) ~= "table" then
    error("smelt.ui.picker.open: expected table of options", 2)
  end
  if type(opts.items) ~= "table" then
    error("smelt.ui.picker.open: opts.items must be a table", 2)
  end
  local items = opts.items
  local n = #items
  if n == 0 then
    error("smelt.ui.picker.open: opts.items must be non-empty", 2)
  end

  local win_id = smelt.ui.picker._open(opts)
  if type(win_id) ~= "number" then
    return nil
  end

  local task_id = smelt.task.alloc()

  -- Local selection state; kept in sync with the Rust `ui::Picker`
  -- through `set_selected` (0-based on the Rust side, 1-based here to
  -- match Lua conventions).
  local selected = 1

  local function move(delta)
    selected = ((selected - 1 + delta) % n) + 1
    smelt.ui.picker.set_selected(win_id, selected - 1)
  end

  -- Navigation keymaps — don't resolve, just move + sync.
  smelt.win.set_keymap(win_id, "up",   function() move(-1) end)
  smelt.win.set_keymap(win_id, "down", function() move(1)  end)
  smelt.win.set_keymap(win_id, "c-k",  function() move(-1) end)
  smelt.win.set_keymap(win_id, "c-j",  function() move(1)  end)
  smelt.win.set_keymap(win_id, "c-p",  function() move(-1) end)
  smelt.win.set_keymap(win_id, "c-n",  function() move(1)  end)

  -- Enter submits with `{index, item}`; Esc dismisses.
  smelt.win.set_keymap(win_id, "enter", function()
    smelt.win.close(win_id)
    smelt.task.resume(task_id, {
      index = selected,
      item = items[selected],
    })
  end)
  smelt.win.set_keymap(win_id, "esc", function()
    smelt.win.close(win_id)
    smelt.task.resume(task_id, nil)
  end)

  return smelt.task.wait(task_id)
end

return M
