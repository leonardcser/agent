-- `smelt.prompt.open_picker(opts)`
--
-- Prompt-docked picker: a non-focusable `ui::Picker` floats above the
-- prompt; the user types into the prompt as normal and each keystroke
-- re-filters the list. Up/Down navigate, Enter accepts, Tab inserts
-- the selected label into the prompt, Esc dismisses. All routing
-- lives here — no Rust completer involved.
--
-- `opts` shape:
--   items     = { { label, description?, ansi_color?, search_terms? }, ... }
--   on_select = function(item) -- optional, fires on every navigation
--
-- Returns `{ index, item, action }` on accept (action `"enter"` or
-- `"tab"`), `nil` on dismiss. `index` is the position in the caller's
-- original `items` table.

local function filter_items(all_items, query)
  -- Defer to smelt.fuzzy.rank — it scores label / description /
  -- search_terms as separate fields and takes the best, matching the
  -- old Rust ArgPicker's ranking. Empty query short-circuits to the
  -- original ordering.
  local order = smelt.fuzzy.rank(all_items, query)
  local out = {}
  for i, idx in ipairs(order) do out[i] = all_items[idx] end
  return out
end

local function to_picker_items(list)
  local out = {}
  for i, it in ipairs(list) do
    out[i] = {
      label       = it.label,
      description = it.description,
      ansi_color  = it.ansi_color,
      prefix      = it.prefix,
    }
  end
  return out
end

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

  local original = opts.items
  local on_select = opts.on_select

  -- Stamp each entry with its 1-based index into the caller's items so
  -- filtering + sorting can resolve back to the original row.
  local all_items = {}
  for i, it in ipairs(original) do
    all_items[i] = {
      label        = it.label,
      description  = it.description,
      ansi_color   = it.ansi_color,
      prefix       = it.prefix,
      search_terms = it.search_terms,
      _idx         = i,
    }
  end

  local current = all_items
  local selected = 1

  local PROMPT = smelt.prompt.win_id()
  local initial_query = smelt.prompt.text() or ""
  if initial_query ~= "" then
    current = filter_items(all_items, initial_query)
  end

  local win_id = smelt.ui.picker._open({
    items     = to_picker_items(current),
    placement = "prompt_docked",
  })

  local task_id = smelt.task.alloc()

  local function fire_on_select()
    if on_select and current[selected] then
      local orig = original[current[selected]._idx]
      local ok, err = pcall(on_select, orig)
      if not ok then
        smelt.notify_error("prompt picker on_select: " .. tostring(err))
      end
    end
  end
  fire_on_select()

  local keys = { "up", "down", "c-k", "c-j", "c-p", "c-n", "enter", "tab", "esc" }
  local text_changed_id

  local function teardown()
    for _, k in ipairs(keys) do
      smelt.win.clear_keymap(PROMPT, k)
    end
    if text_changed_id then
      smelt.win.clear_event(PROMPT, "text_changed", text_changed_id)
    end
    smelt.win.close(win_id)
  end

  local function close_with(result)
    teardown()
    smelt.task.resume(task_id, result)
  end

  local function move(delta)
    local n = #current
    if n == 0 then return end
    selected = ((selected - 1 + delta) % n) + 1
    smelt.ui.picker.set_selected(win_id, selected - 1)
    fire_on_select()
  end

  local function accept(action)
    local picked = current[selected]
    if not picked then
      close_with(nil)
      return
    end
    local idx = picked._idx
    close_with({ action = action, index = idx, item = original[idx] })
  end

  -- The picker is rendered reversed (logical index 0 sits at the
  -- bottom visual row, closest to the prompt). Pressing Up moves
  -- toward higher logical indices (worse matches, higher on screen);
  -- Down moves toward lower indices (better matches, closer to the
  -- prompt). Mirror that for c-k/c-p (vim/emacs "up") and c-j/c-n
  -- ("down").
  smelt.win.set_keymap(PROMPT, "up",    function() move(1)  end)
  smelt.win.set_keymap(PROMPT, "down",  function() move(-1) end)
  smelt.win.set_keymap(PROMPT, "c-k",   function() move(1)  end)
  smelt.win.set_keymap(PROMPT, "c-j",   function() move(-1) end)
  smelt.win.set_keymap(PROMPT, "c-p",   function() move(1)  end)
  smelt.win.set_keymap(PROMPT, "c-n",   function() move(-1) end)
  smelt.win.set_keymap(PROMPT, "enter", function() accept("enter") end)
  smelt.win.set_keymap(PROMPT, "tab",   function()
    local picked = current[selected]
    if picked then
      smelt.prompt.set_text(picked.label)
    end
    accept("tab")
  end)
  smelt.win.set_keymap(PROMPT, "esc",   function() close_with(nil) end)

  text_changed_id = smelt.win.on_event(PROMPT, "text_changed", function(ctx)
    local query = ctx.text or ""
    current = filter_items(all_items, query)
    selected = 1
    smelt.ui.picker.set_items(win_id, to_picker_items(current))
    smelt.ui.picker.set_selected(win_id, 0)
    fire_on_select()
  end)

  return smelt.task.wait(task_id)
end
