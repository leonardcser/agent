-- `/model` — switch active model.
--
-- With an arg, applies directly. Without, opens a filterable picker
-- of available models (matches on name + provider + key).

local function models_list()
  return smelt.engine.models() or {}
end

local function build_items()
  local out = {}
  for _, m in ipairs(models_list()) do
    out[#out + 1] = {
      label        = m.name,
      description  = m.provider,
      search_terms = (m.key or "") .. " " .. (m.provider or ""),
      _key         = m.key,
    }
  end
  return out
end

local model_keys = {}
for _, m in ipairs(models_list()) do
  model_keys[#model_keys + 1] = m.key
end

smelt.cmd.picker("model", {
  desc     = "switch model",
  args     = model_keys,
  items    = build_items,
  apply    = function(arg) smelt.engine.set_model(arg) end,
  prepare  = function()
    if #models_list() == 0 then smelt.notify_error("no models available") end
  end,
  on_enter = function(item) if item._key then smelt.cmd.run("/model " .. item._key) end end,
})
