-- Ctrl+R reverse history search.
--
-- Opens a filterable picker over past prompts ranked by the history
-- scorer (word-boundary matches + recency). Typing filters live;
-- Enter commits the selected entry to the prompt buffer; Esc restores
-- whatever the user had before opening.

local function entry_label(entry)
  for line in (entry or ""):gmatch("[^\r\n]+") do
    local trimmed = line:match("^%s*(.-)%s*$")
    if trimmed ~= "" then return trimmed end
  end
  return ""
end

local function build_items()
  local entries = smelt.history.entries()
  if #entries == 0 then return {} end
  -- Newest-first (reverse order). Stash the full entry on the item so
  -- on_enter / on_dismiss can look it up without another indirection.
  local items = {}
  for i = #entries, 1, -1 do
    items[#items + 1] = {
      label        = entry_label(entries[i]),
      search_terms = entries[i],
      _entry       = entries[i],
    }
  end
  return items
end

local saved_text
local function open()
  saved_text = smelt.prompt.text()
  if #smelt.history.entries() == 0 then return end
  smelt.spawn(function()
    local r = smelt.prompt.open_picker({ items = build_items() })
    if r and r.action == "enter" then
      smelt.prompt.set_text(r.item._entry or "")
    else
      smelt.prompt.set_text(saved_text or "")
    end
  end)
end

for _, mode in ipairs({ "normal", "insert", "visual" }) do
  smelt.keymap.set(mode, "c-r", open)
end

smelt.cmd.register("history", open, { desc = "search prompt history" })
