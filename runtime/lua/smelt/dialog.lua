-- Lua-side implementation of `smelt.ui.dialog.open(opts)` plus the
-- typed panel-handle factory `smelt.ui.dialog.open_handle(opts)` used
-- by built-ins (confirm) that drive the dialog lifecycle directly.
--
-- Rust exposes the low-level `smelt.ui.dialog._open(opts) -> win_id`
-- which synchronously creates the float + panels. Everything else —
-- handle construction, result building, custom keymaps, `on_select`
-- / `on_change` / `on_tick`, submit/dismiss routing — lives here.

local M = {}

-- Single panel-handle factory: identity fields + `:focus()`. Buffer
-- panels expose `.buf` so callers can mutate / re-render without
-- re-walking the spec. Scrolling is the buffer's own job — interactive
-- content panels handle wheel + vim motions natively when focused.
local function make_panel(win_id, idx, spec)
  local self = { kind = spec.kind, idx = idx, name = spec.name, buf = spec.buf }
  function self:focus() smelt.ui.dialog._panel_focus(win_id, idx) end
  return self
end

-- Build the `{ win, panels, focus, close }` handle from a freshly
-- opened win_id and the original opts. `panels` is keyed by both
-- 1-based index *and* the optional `name` field on each spec, so
-- callers can do `d.panels[1]` or `d.panels.preview`.
local function make_handle(win_id, opts)
  local panels = {}
  if type(opts.panels) == "table" then
    for i, spec in ipairs(opts.panels) do
      if type(spec) == "table" then
        local h = make_panel(win_id, i, spec)
        panels[i] = h
        if spec.name then panels[spec.name] = h end
      end
    end
  end
  local self = { win = win_id, panels = panels }
  function self:focus(name_or_idx)
    local p = panels[name_or_idx]
    if p then p:focus() end
  end
  function self:close() smelt.win.close(win_id) end
  return self
end

-- `smelt.ui.dialog.open_handle(opts)` — synchronous; returns the
-- typed handle. For coroutine-style use (yield until submit/dismiss),
-- prefer `smelt.ui.dialog.open(opts)` further down.
function smelt.ui.dialog.open_handle(opts)
  if type(opts) ~= "table" then
    error("smelt.ui.dialog.open_handle: expected table of options", 2)
  end
  local win_id = smelt.ui.dialog._open(opts)
  if type(win_id) ~= "number" then return nil end
  return make_handle(win_id, opts)
end

-- Build the keymap-callback ctx table (`{selected_index, inputs,
-- close, win}`) from a raw callback ctx (`{win, panels, …}`). `opts`
-- is the original `dialog.open` opts so we can walk panel metadata;
-- `win_id` is captured in the closing function.
local function build_ctx(raw_ctx, opts, win_id, task_id, option_panel_idx, input_panels)
  local ctx = { win = raw_ctx.win }
  if option_panel_idx then
    local p = raw_ctx.panels and raw_ctx.panels[option_panel_idx]
    if p and p.selected then
      ctx.selected_index = p.selected
    end
  end
  ctx.inputs = {}
  for name, idx in pairs(input_panels) do
    local p = raw_ctx.panels and raw_ctx.panels[idx]
    if p then
      ctx.inputs[name] = p.text or ""
    end
  end
  ctx.close = function()
    smelt.win.close(win_id)
    smelt.task.resume(task_id, {
      action = "dismiss",
      inputs = {},
    })
  end
  return ctx
end

-- Collect input text for every input panel in `opts.panels`. Used by
-- Submit / Dismiss to assemble the resume table.
local function collect_inputs(raw_ctx, input_panels)
  local out = {}
  for name, idx in pairs(input_panels) do
    local p = raw_ctx.panels and raw_ctx.panels[idx]
    out[name] = p and p.text or ""
  end
  return out
end

function smelt.ui.dialog.open(opts)
  if not coroutine.isyieldable() then
    error("smelt.ui.dialog.open: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  if type(opts) ~= "table" then
    error("smelt.ui.dialog.open: expected table of options", 2)
  end

  -- Walk panels once to find the (first) options panel and map input
  -- names → panel indices. Done before opening so closure captures are
  -- ready when callbacks register.
  local option_panel_idx = nil
  local input_panels = {}   -- name → 1-based panel index
  local options_meta = {}   -- 1-based option index → {action, on_select}
  local input_on_change = {} -- panel index → on_change fn
  if type(opts.panels) == "table" then
    for i, p in ipairs(opts.panels) do
      if type(p) == "table" then
        if p.kind == "options" and not option_panel_idx then
          option_panel_idx = i
          if type(p.items) == "table" then
            for j, item in ipairs(p.items) do
              options_meta[j] = {
                action = item.action or "select",
                on_select = item.on_select,
              }
            end
          end
        elseif p.kind == "list" and not option_panel_idx then
          option_panel_idx = i
        elseif p.kind == "input" then
          input_panels[p.name or ("input_" .. i)] = i
          if type(p.on_change) == "function" then
            input_on_change[i] = p.on_change
          end
        end
      end
    end
  end

  -- Open the float synchronously. Rust returns the `win_id` directly.
  local win_id = smelt.ui.dialog._open(opts)
  if type(win_id) ~= "number" then
    return { action = "dismiss", inputs = {} }
  end

  local task_id = smelt.task.alloc()

  -- Submit handler. Fires when the focused options/list panel sees
  -- Enter (via `WidgetEvent::Submit` auto-translation) or an input
  -- panel submits its text. Build the result and resume.
  smelt.win.on_event(win_id, "submit", function(raw_ctx)
    local idx1 = nil
    if option_panel_idx then
      local p = raw_ctx.panels and raw_ctx.panels[option_panel_idx]
      if p and p.selected then
        idx1 = p.selected
      end
    end
    local action = "select"
    local on_select_fn = nil
    if idx1 and options_meta[idx1] then
      action = options_meta[idx1].action
      on_select_fn = options_meta[idx1].on_select
    end
    local inputs = collect_inputs(raw_ctx, input_panels)
    if type(on_select_fn) == "function" then
      local ok, err = pcall(on_select_fn)
      if not ok then
        smelt.notify_error("dialog on_select: " .. tostring(err))
      end
    end
    smelt.win.close(win_id)
    smelt.task.resume(task_id, {
      action = action,
      option_index = idx1,
      inputs = inputs,
    })
  end)

  -- Dismiss handler. Fires on Esc or a configured dismiss key.
  smelt.win.on_event(win_id, "dismiss", function(raw_ctx)
    smelt.win.close(win_id)
    smelt.task.resume(task_id, {
      action = "dismiss",
      inputs = collect_inputs(raw_ctx, input_panels),
    })
  end)

  -- User-provided keymaps. Each fires synchronously with a
  -- legacy-shape ctx; the callback decides whether to `ctx.close()`.
  if type(opts.keymaps) == "table" then
    for _, km in ipairs(opts.keymaps) do
      if type(km) == "table" and km.key and type(km.on_press) == "function" then
        local on_press = km.on_press
        smelt.win.set_keymap(win_id, km.key, function(raw_ctx)
          local ctx = build_ctx(raw_ctx, opts, win_id, task_id, option_panel_idx, input_panels)
          local ok, err = pcall(on_press, ctx)
          if not ok then
            smelt.notify_error("dialog keymap: " .. tostring(err))
          end
        end)
      end
    end
  end

  -- Per-input `on_change` hooks. A single TextChanged event fires for
  -- the whole dialog; fan out to each registered input (there's
  -- usually one, but the dispatch handles many).
  if next(input_on_change) ~= nil then
    smelt.win.on_event(win_id, "text_changed", function(raw_ctx)
      local ctx = build_ctx(raw_ctx, opts, win_id, task_id, option_panel_idx, input_panels)
      for _, fn in pairs(input_on_change) do
        local ok, err = pcall(fn, ctx)
        if not ok then
          smelt.notify_error("dialog on_change: " .. tostring(err))
        end
      end
    end)
  end

  -- `on_tick` hook — fires each engine tick for live-refresh dialogs
  -- (agents list, process registry, session cache).
  if type(opts.on_tick) == "function" then
    local on_tick = opts.on_tick
    smelt.win.on_event(win_id, "tick", function(raw_ctx)
      local ctx = build_ctx(raw_ctx, opts, win_id, task_id, option_panel_idx, input_panels)
      local ok, err = pcall(on_tick, ctx)
      if not ok then
        smelt.notify_error("dialog on_tick: " .. tostring(err))
      end
    end)
  end

  -- Park the task until a handler calls `task.resume`.
  return smelt.task.wait(task_id)
end

return M
