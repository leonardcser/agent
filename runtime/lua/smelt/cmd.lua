-- Register a slash-command that opens a prompt-docked picker when called
-- without arguments, or calls `apply(arg)` directly when given one.
--
-- `opts`:
--   desc       string                      - completion description.
--   args       table                       - completion args; defaults to `items[*].label`.
--   args_fn    function() → table          - live completion args; errors fall back to `args`.
--   items      table | function() → table  - picker entries. A function re-evaluates after each
--                                             `on_enter` when `stay_open = true`, so the picker
--                                             reflects mutated state (toggles, edits, etc.).
--   selected   integer | function() → integer - initial selected row (1-based).
--   apply      function(arg)               - direct dispatch.
--   prepare    function()                  - runs once before opening.
--   on_select  function(item)              - fires on every navigation.
--   on_enter   function(item, idx)         - Enter accept.
--   on_dismiss function()                  - Esc dismiss.
--   stay_open  bool                        - keep picker open after Enter (persistent mode).

local function report_picker_error(event, err)
  local msg = tostring(err)
  smelt.log.error("cmd.register_picker_callback_failed", { event = event, error = msg })
  smelt.notify.error("cmd.register_picker " .. event .. ": " .. msg)
end

local function safe_dismiss(fn)
  if not fn then return end
  local ok, err = pcall(fn)
  if not ok then report_picker_error("on_dismiss", err) end
end

local function run_picker(opts)
  smelt.spawn(function()
    local selected = opts.selected
    if type(selected) == "function" then selected = selected() end
    if opts.stay_open then
      -- Persistent mode: the picker itself owns the lifecycle and re-evaluates
      -- `opts.items` (function form) after each on_enter, so the cursor stays
      -- on the row the user just acted on instead of resetting.
      smelt.picker.open({
        placement  = "prompt_docked",
        items     = opts.items,
        selected  = selected,
        on_select = opts.on_select,
        on_enter  = function(item, idx)
          if opts.on_enter then
            local ok, err = pcall(opts.on_enter, item, idx)
            if not ok then
              report_picker_error("on_enter", err)
            end
          end
        end,
      })
      safe_dismiss(opts.on_dismiss)
      return
    end

    -- Single-shot mode: open once, dispatch once, close.
    local items = type(opts.items) == "function" and opts.items() or opts.items
    if not items or #items == 0 then
      safe_dismiss(opts.on_dismiss)
      return
    end
    local r = smelt.picker.open({
      placement = "prompt_docked",
      items     = items,
      selected  = selected,
      on_select = opts.on_select,
    })
    if not r then
      safe_dismiss(opts.on_dismiss)
      return
    end
    if r.action == "enter" and opts.on_enter then
      local ok, err = pcall(opts.on_enter, r.item, r.index)
      if not ok then
        report_picker_error("on_enter", err)
      end
    end
  end)
end

-- Register a slash command `name` that opens a prompt-docked picker when
-- called without arguments, or invokes `opts.apply(arg)` directly when given
-- one. `opts` accepts `desc`, `args`, `args_fn`, `items` (an array or producer function),
-- `selected` (1-based index or producer), `apply`, `prepare`, `on_select`,
-- `on_enter`, `on_dismiss`, `stay_open`,
-- `busy`, and `startup_ok`. With `stay_open = true`, the item producer is
-- re-evaluated after each Enter action. Returns nothing; the command lives
-- until `/reload`.
---@type fun(name: string, opts: table?): nil
function smelt.cmd.register_picker(name, opts)
  opts = opts or {}

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
    run_picker(opts)
  end, {
    desc       = opts.desc,
    args       = args,
    args_fn    = opts.args_fn,
    busy       = opts.busy,
    startup_ok = opts.startup_ok,
  })
end
