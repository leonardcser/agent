-- `/theme` — change the accent color.
--
-- With an arg (`/theme lavender`), applies the named preset directly.
-- Without an arg, opens a prompt-owning picker: typing filters presets,
-- navigation live-previews the accent, Tab inserts the name into the
-- buffer, Enter runs `/theme <name>`, Esc restores the original accent.

local function apply_by_name(name)
  for _, p in ipairs(smelt.theme.presets()) do
    if p.name == name then
      smelt.theme.set("accent", { ansi = p.ansi })
      return true
    end
  end
  return false
end

local function open_picker()
  local presets = smelt.theme.presets()
  local original = smelt.theme.get("accent")
  local original_ansi = original and original.ansi

  local items = {}
  for _, p in ipairs(presets) do
    items[#items + 1] = {
      label       = p.name,
      description = p.detail,
      ansi_color  = p.ansi,
    }
  end

  smelt.spawn(function()
    local result = smelt.prompt.open_picker({
      items     = items,
      on_select = function(item)
        if item and item.ansi_color then
          smelt.theme.set("accent", { ansi = item.ansi_color })
        end
      end,
    })
    if result and result.action == "enter" then
      smelt.cmd.run("/theme " .. result.item.label)
    elseif not result and original_ansi then
      smelt.theme.set("accent", { ansi = original_ansi })
    end
  end)
end

local preset_names = (function()
  local names = {}
  for _, p in ipairs(smelt.theme.presets()) do names[#names + 1] = p.name end
  return names
end)()

smelt.cmd.register("theme", function(arg)
  if arg and arg ~= "" then
    if not apply_by_name(arg) then
      smelt.notify_error("unknown theme: " .. arg)
    end
    return
  end
  open_picker()
end, { desc = "change accent color", args = preset_names })
