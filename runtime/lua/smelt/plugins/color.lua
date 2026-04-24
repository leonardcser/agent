-- `/color` — change the task-slug label color.
--
-- Same shape as `/theme` but targets the "slug" role.

local function apply_by_name(name)
  for _, p in ipairs(smelt.theme.presets()) do
    if p.name == name then
      smelt.theme.set("slug", { ansi = p.ansi })
      return true
    end
  end
  return false
end

local presets = smelt.theme.presets()
local preset_names = {}
local items = {}
for i, p in ipairs(presets) do
  preset_names[i] = p.name
  items[i] = { label = p.name, description = p.detail, ansi_color = p.ansi }
end

local original_ansi
smelt.cmd.register("color", function(arg)
  if arg and arg ~= "" then
    if not apply_by_name(arg) then smelt.notify_error("unknown color: " .. arg) end
    return
  end
  original_ansi = (smelt.theme.get("slug") or {}).ansi
end, {
  desc       = "set task slug color",
  args       = preset_names,
  items      = items,
  on_select  = function(item) if item.ansi_color then smelt.theme.set("slug", { ansi = item.ansi_color }) end end,
  on_enter   = function(item) smelt.cmd.run("/color " .. item.label) end,
  on_dismiss = function() if original_ansi then smelt.theme.set("slug", { ansi = original_ansi }) end end,
})
