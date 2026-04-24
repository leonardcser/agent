-- `/settings` — toggle boolean settings.
--
-- Each Enter toggles the selected setting and reopens the picker with
-- the refreshed `on`/`off` labels. Esc closes. Typing filters.

local SETTINGS_META = {
  { key = "vim",                   label = "vim mode",             terms = "vim editor" },
  { key = "auto_compact",          label = "auto compact",         terms = "" },
  { key = "show_tps",              label = "show tok/s",           terms = "tokens tok tps speed throughput" },
  { key = "show_tokens",           label = "show tokens",          terms = "" },
  { key = "show_cost",             label = "show cost",            terms = "" },
  { key = "show_prediction",       label = "input prediction",     terms = "predict prediction autocomplete ghost" },
  { key = "show_slug",             label = "task slug",            terms = "task slug label title" },
  { key = "show_thinking",         label = "show thinking",        terms = "thinking reasoning thought thoughts" },
  { key = "restrict_to_workspace", label = "restrict to workspace", terms = "workspace cwd project directory" },
  { key = "redact_secrets",        label = "redact secrets",       terms = "redact secrets mask hide credentials tokens keys" },
}

local function build_items()
  local snap = smelt.settings.snapshot()
  local items = {}
  for _, m in ipairs(SETTINGS_META) do
    items[#items + 1] = {
      label        = m.label,
      description  = snap[m.key] and "on" or "off",
      search_terms = m.key .. " " .. (m.terms or ""),
    }
  end
  return items
end

smelt.cmd.register("settings", function()
  smelt.spawn(function()
    while true do
      local result = smelt.prompt.open_picker({ items = build_items() })
      if not result or result.action ~= "enter" then return end
      local meta = SETTINGS_META[result.index]
      if meta then smelt.settings.toggle(meta.key) end
    end
  end)
end, { desc = "open settings menu" })
