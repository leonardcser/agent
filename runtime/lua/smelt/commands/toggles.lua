-- `/thinking` folds or unfolds thinking blocks for the current session.

smelt.cmd.register("thinking", function(arg)
  local action = "toggle"
  if arg and arg ~= "" then
    action = arg:lower()
  end
  if action == "on" or action == "open" or action == "show" then
    action = "open"
  elseif action == "off" or action == "close" or action == "hide" then
    action = "close"
  elseif action ~= "toggle" and action ~= "peek" then
    smelt.notify.error("usage: /thinking [open|close|peek|toggle]")
    return
  end
  local changed = smelt.transcript.fold_kind("thinking", action)
  local label = action == "toggle" and "toggled" or action
  smelt.notify.info("thinking blocks: " .. (changed and label or "unchanged"))
end, { desc = "set thinking block view state", args = { "open", "close", "peek", "toggle" } })

-- `/fast` - toggle accelerated inference for the current session.
smelt.cmd.register("fast", function(arg)
  local status = smelt.session.status()
  if not status.fast.supported then
    smelt.notify.error("fast mode is not supported by the current model")
    return
  end

  local enabled
  if not arg or arg == "" or arg:lower() == "toggle" then
    enabled = not status.fast.active
  elseif arg:lower() == "on" then
    enabled = true
  elseif arg:lower() == "off" then
    enabled = false
  else
    smelt.notify.error("usage: /fast [on|off|toggle]")
    return
  end

  smelt.session.set_fast_mode(enabled)
  smelt.notify.info("fast mode: " .. (enabled and "on" or "off"))
end, { desc = "toggle accelerated inference", args = { "on", "off", "toggle" } })

-- `/reasoning` - select a native effort supported by the active model.
local function reasoning_items()
  local options = smelt.reasoning.options()
  local current = smelt.reasoning.current()
  local items = {}
  for _, effort in ipairs(options.efforts) do
    local notes = {}
    if effort == current then notes[#notes + 1] = "current" end
    if effort == options.default then notes[#notes + 1] = "default" end
    items[#items + 1] = { label = effort, description = table.concat(notes, ", ") }
  end
  return items
end

local function set_reasoning(effort)
  smelt.reasoning.set(effort)
  smelt.notify.info("reasoning effort: " .. smelt.reasoning.current())
end

smelt.cmd.register_picker("reasoning", {
  desc = "select reasoning effort",
  args_fn = function() return smelt.reasoning.options().efforts end,
  items = reasoning_items,
  selected = function()
    for i, effort in ipairs(smelt.reasoning.options().efforts) do
      if effort == smelt.reasoning.current() then return i end
    end
  end,
  apply = set_reasoning,
  prepare = function()
    if not smelt.model.current() then
      smelt.notify.info("no model selected")
    elseif #smelt.reasoning.options().efforts == 0 then
      smelt.notify.info("reasoning levels unknown; use /reasoning <effort> to set explicitly")
    end
  end,
  on_enter = function(item) set_reasoning(item.label) end,
})
