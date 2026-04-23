-- Lua-side implementation of `smelt.api.dialog.open(opts)`.
--
-- Rust still owns float/panel creation (building `PanelSpec` from an
-- `opts` table needs Ui access and render plumbing, and exposing the
-- panel types as userdata is more surface than the shortcut of passing
-- the whole table is worth). Everything else — result building, custom
-- keymaps, `on_select` / `on_change` / `on_tick`, submit/dismiss
-- routing — lives here.
--
-- Protocol (everything rides on `TaskWait::External`; no bespoke
-- dialog-yield variant):
--   1. Alloc an external task id for the open ack, call
--      `smelt.api.dialog._request_open(open_id, opts)` (queues a
--      `UiOp::OpenLuaDialog`), yield External — reducer opens the
--      float and resolves with `{win_id = <u64>}`.
--   2. Alloc a second id for the final result. Register
--      `smelt.api.win.on_event(win, "submit"|"dismiss", …)` handlers
--      that build the result, close the float, and resume via
--      `smelt.api.task.resume(result_id, result)`.
--   3. Register any user-provided keymaps (`opts.keymaps`), the
--      `on_tick` handler, and per-input `on_change` handlers.
--   4. Yield `{__yield = "external", id = result_id}` and return the
--      resumed value.

local M = {}

-- Build the legacy-shape ctx table (`{selected_index, inputs, close,
-- win}`) from a raw callback ctx (`{win, panels, …}`). `opts` is the
-- original `dialog.open` opts so we can walk panel metadata; `win_id`
-- is captured in the closing function.
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
    smelt.api.win.close(win_id)
    smelt.api.task.resume(task_id, {
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

function smelt.api.dialog.open(opts)
  if not coroutine.isyieldable() then
    error("smelt.api.dialog.open: call from inside smelt.task(fn) or tool.execute", 2)
  end
  if type(opts) ~= "table" then
    error("smelt.api.dialog.open: expected table of options", 2)
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

  -- Step 1: queue a dialog-open op and park the task. The reducer
  -- opens the float + panels and resolves us with `{win_id = <u64>}`.
  local open_id = smelt.api.task.alloc()
  smelt.api.dialog._request_open(open_id, opts)
  local opened = coroutine.yield({__yield = "external", id = open_id})
  if type(opened) ~= "table" or type(opened.win_id) ~= "number" then
    return { action = "dismiss", inputs = {} }
  end
  local win_id = opened.win_id

  -- Step 2: mint a task id for the final resume.
  local task_id = smelt.api.task.alloc()

  -- Step 3: Submit handler. Fires when the focused options/list panel
  -- sees Enter (via `WidgetEvent::Submit` auto-translation) or an
  -- input panel submits its text. Build the result and resume.
  smelt.api.win.on_event(win_id, "submit", function(raw_ctx)
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
        smelt.api.ui.notify_error("dialog on_select: " .. tostring(err))
      end
    end
    smelt.api.win.close(win_id)
    smelt.api.task.resume(task_id, {
      action = action,
      option_index = idx1,
      inputs = inputs,
    })
  end)

  -- Step 4: Dismiss handler. Fires on Esc or a configured dismiss key.
  smelt.api.win.on_event(win_id, "dismiss", function(raw_ctx)
    smelt.api.win.close(win_id)
    smelt.api.task.resume(task_id, {
      action = "dismiss",
      inputs = collect_inputs(raw_ctx, input_panels),
    })
  end)

  -- Step 5: user-provided keymaps. Each fires synchronously with a
  -- legacy-shape ctx; the callback decides whether to `ctx.close()`.
  if type(opts.keymaps) == "table" then
    for _, km in ipairs(opts.keymaps) do
      if type(km) == "table" and km.key and type(km.on_press) == "function" then
        local on_press = km.on_press
        smelt.api.win.set_keymap(win_id, km.key, function(raw_ctx)
          local ctx = build_ctx(raw_ctx, opts, win_id, task_id, option_panel_idx, input_panels)
          local ok, err = pcall(on_press, ctx)
          if not ok then
            smelt.api.ui.notify_error("dialog keymap: " .. tostring(err))
          end
        end)
      end
    end
  end

  -- Step 6: per-input `on_change` hooks. A single TextChanged event
  -- fires for the whole dialog; fan out to each registered input
  -- (there's usually one, but the dispatch handles many).
  if next(input_on_change) ~= nil then
    smelt.api.win.on_event(win_id, "text_changed", function(raw_ctx)
      local ctx = build_ctx(raw_ctx, opts, win_id, task_id, option_panel_idx, input_panels)
      for _, fn in pairs(input_on_change) do
        local ok, err = pcall(fn, ctx)
        if not ok then
          smelt.api.ui.notify_error("dialog on_change: " .. tostring(err))
        end
      end
    end)
  end

  -- Step 7: `on_tick` hook — fires each engine tick for live-refresh
  -- dialogs (agents list, process registry, session cache).
  if type(opts.on_tick) == "function" then
    local on_tick = opts.on_tick
    smelt.api.win.on_event(win_id, "tick", function(raw_ctx)
      local ctx = build_ctx(raw_ctx, opts, win_id, task_id, option_panel_idx, input_panels)
      local ok, err = pcall(on_tick, ctx)
      if not ok then
        smelt.api.ui.notify_error("dialog on_tick: " .. tostring(err))
      end
    end)
  end

  -- Step 8: park the task until a handler calls `task.resume`.
  return coroutine.yield({__yield = "external", id = task_id})
end

return M
