-- Declarative picker sugar for `smelt.cmd.register`.
--
-- Base form is still `smelt.cmd.register(name, handler, opts)`. When
-- `opts` carries any of `on_enter`, `on_select`, `on_dismiss`, or
-- `stay_open`, a default handler is generated that:
--
--   1. If the command is invoked with an argument (`/name foo`),
--      the caller-provided `handler` (if any) fires for direct
--      dispatch; otherwise `on_enter({ label = arg })` fires.
--   2. With no argument, a prompt-docked picker opens. Navigation
--      calls `on_select(item)` for live preview. Enter calls
--      `on_enter(item, index)` and either closes or re-opens
--      (when `stay_open = true`). Esc / Tab closes and calls
--      `on_dismiss()` (Esc path only — Tab treated as dismiss but
--      after the caller's own `on_enter` via the picker's action).
--
-- Items shape: a list of `{ label, description?, ansi_color?,
-- search_terms? }`, or a function returning such a list (re-evaluated
-- each iteration when `stay_open` is true, so toggle-style menus can
-- show the refreshed state on the reopen).
--
-- Plugins with custom logic beyond this (save/restore state on
-- dismiss, non-trivial item construction, etc.) can still pass their
-- own `handler` as before.

local rust_register = smelt.cmd.register

local function resolve_items(items)
  if type(items) == "function" then return items() end
  return items
end

local function run_declarative_picker(spec)
  smelt.spawn(function()
    while true do
      local items = resolve_items(spec.items)
      if not items or #items == 0 then
        if spec.on_dismiss then pcall(spec.on_dismiss) end
        return
      end
      local r = smelt.prompt.open_picker({
        items     = items,
        on_select = spec.on_select,
      })
      if not r then
        if spec.on_dismiss then pcall(spec.on_dismiss) end
        return
      end
      if r.action == "enter" then
        if spec.on_enter then
          local ok, err = pcall(spec.on_enter, r.item, r.index)
          if not ok then
            smelt.notify_error("cmd.register on_enter: " .. tostring(err))
            return
          end
        end
        if not spec.stay_open then return end
      else
        return
      end
    end
  end)
end

function smelt.cmd.register(name, handler, opts)
  opts = opts or {}

  local has_picker_hook = opts.on_enter
    or opts.on_select
    or opts.on_dismiss
    or opts.stay_open

  if has_picker_hook then
    -- If no `items` provided, derive from static `args` as plain labels.
    local items = opts.items
    if not items and type(opts.args) == "table" then
      items = {}
      for _, s in ipairs(opts.args) do items[#items + 1] = { label = s } end
    end

    -- The `handler` runs in two modes: with an argument for direct
    -- dispatch (`/name foo`), and with nil for the picker-open path —
    -- callers use the nil path to snapshot pre-open state (e.g.
    -- theme.lua records the original accent so `on_dismiss` can
    -- restore it).
    local inner = handler
    local wrapped = function(arg)
      if arg and arg ~= "" then
        if type(inner) == "function" then
          inner(arg)
        elseif opts.on_enter then
          opts.on_enter({ label = arg }, nil)
        end
        return
      end
      if type(inner) == "function" then inner(nil) end
      run_declarative_picker({
        items      = items,
        on_select  = opts.on_select,
        on_enter   = opts.on_enter,
        on_dismiss = opts.on_dismiss,
        stay_open  = opts.stay_open,
      })
    end

    rust_register(name, wrapped, { desc = opts.desc, args = opts.args })
    return
  end

  rust_register(name, handler, opts)
end
