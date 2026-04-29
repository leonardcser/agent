-- `smelt.cmd.picker(name, opts)` — register a slash-command whose
-- bareword form opens a prompt-docked picker.
--
-- Direct dispatch (`/name foo`) calls `apply(arg)`. With no argument,
-- `prepare()` runs (e.g. snapshot pre-open state for `on_dismiss` to
-- restore), then a prompt-docked picker opens. Navigation calls
-- `on_select(item)` for live preview, Enter calls `on_enter(item, idx)`
-- and either closes or re-opens (when `stay_open = true`). Esc / Tab
-- closes; Esc routes through `on_dismiss()`. Tab inserts the label
-- into the prompt and closes without re-firing apply.
--
-- `opts`:
--   desc       string  — completion description.
--   args       table   — completion args; defaults to `items[*].label`.
--   items      table | function() table  — picker entries
--                  ({ label, description?, ansi_color?, prefix?,
--                     search_terms?, ... }). A function is re-evaluated
--                  on every reopen so toggle-style menus refresh.
--   apply      function(arg)               — direct dispatch.
--   prepare    function()                  — runs once before opening.
--   on_select  function(item)              — fires on every navigation.
--   on_enter   function(item, idx)         — Enter accept.
--   on_dismiss function()                  — Esc dismiss.
--   stay_open  bool                        — reopen after Enter.

local function resolve_items(items)
  if type(items) == "function" then return items() end
  return items
end

local function run_picker_loop(opts)
  smelt.spawn(function()
    while true do
      local items = resolve_items(opts.items)
      if not items or #items == 0 then
        if opts.on_dismiss then pcall(opts.on_dismiss) end
        return
      end
      local r = smelt.prompt.open_picker({
        items     = items,
        on_select = opts.on_select,
      })
      if not r then
        if opts.on_dismiss then pcall(opts.on_dismiss) end
        return
      end
      if r.action == "enter" and opts.on_enter then
        local ok, err = pcall(opts.on_enter, r.item, r.index)
        if not ok then
          smelt.notify_error("cmd.picker on_enter: " .. tostring(err))
          return
        end
      end
      if r.action ~= "enter" or not opts.stay_open then return end
    end
  end)
end

function smelt.cmd.picker(name, opts)
  opts = opts or {}

  -- Derive completion args from static items if not supplied.
  local args = opts.args
  if not args and type(opts.items) == "table" then
    args = {}
    for i, it in ipairs(opts.items) do args[i] = it.label end
  end

  smelt.cmd.register(name, function(arg)
    if arg and arg ~= "" then
      if opts.apply then opts.apply(arg) end
      return
    end
    if opts.prepare then opts.prepare() end
    run_picker_loop(opts)
  end, { desc = opts.desc, args = args })
end
