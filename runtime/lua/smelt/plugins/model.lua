-- `/model` — switch active model.
--
-- With an arg, applies directly. Without, opens a filterable picker
-- of available models (matches on name + provider + key).

local function open_picker()
  local models = smelt.engine.models()
  if not models or #models == 0 then
    smelt.notify_error("no models available")
    return
  end

  local items = {}
  for _, m in ipairs(models) do
    items[#items + 1] = {
      label        = m.name,
      description  = m.provider,
      search_terms = (m.key or "") .. " " .. (m.provider or ""),
    }
  end

  smelt.spawn(function()
    local result = smelt.prompt.open_picker({ items = items })
    if result and result.action == "enter" then
      -- Use the model's key for resolution — more stable than label.
      local model = models[result.index]
      if model and model.key then
        smelt.cmd.run("/model " .. model.key)
      end
    end
  end)
end

local model_keys = (function()
  local keys = {}
  for _, m in ipairs(smelt.engine.models() or {}) do
    keys[#keys + 1] = m.key
  end
  return keys
end)()

smelt.cmd.register("model", function(arg)
  if arg and arg ~= "" then
    smelt.engine.set_model(arg)
    return
  end
  open_picker()
end, { desc = "switch model", args = model_keys })
