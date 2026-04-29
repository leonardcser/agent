-- `/theme` — change the accent color.
--
-- With an arg (`/theme lavender`), applies directly. Without, opens a
-- prompt-docked picker: typing filters presets, navigation live-
-- previews the accent, Enter commits, Esc restores the original.

local presets = smelt.theme.presets()
local preset_names, items = {}, {}
for i, p in ipairs(presets) do
  preset_names[i] = p.name
  items[i] = { label = p.name, description = p.detail, ansi_color = p.ansi, prefix = "● " }
end

local original_ansi
smelt.cmd.picker("theme", {
  desc       = "change accent color",
  args       = preset_names,
  items      = items,
  apply      = function(arg)
    for _, p in ipairs(presets) do
      if p.name == arg then
        smelt.theme.set("accent", { ansi = p.ansi })
        return
      end
    end
    smelt.notify_error("unknown theme: " .. arg)
  end,
  prepare    = function() original_ansi = (smelt.theme.get("accent") or {}).ansi end,
  on_select  = function(item) if item.ansi_color then smelt.theme.set("accent", { ansi = item.ansi_color }) end end,
  on_enter   = function(item) smelt.cmd.run("/theme " .. item.label) end,
  on_dismiss = function() if original_ansi then smelt.theme.set("accent", { ansi = original_ansi }) end end,
})
