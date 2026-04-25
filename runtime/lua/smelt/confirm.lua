-- `smelt.confirm.open(handle_id)` — built-in tool-approval dialog.
--
-- Fired from the agent loop (`agent.rs:1379`-ish) after registering a
-- request in `App::confirm_requests`. Plugins can override this
-- function in their init.lua to swap the default UI.
--
-- Panels (top → bottom, indices below match the keymap callbacks):
--   1 title    — bash-syntax-highlit ` tool: desc Allow?`
--   2 summary  — optional muted summary, hidden when empty
--   3 preview  — diff / notebook / file / bash body, hidden when empty
--   4 options  — yes / no + dynamic "always allow …" entries
--   5 reason   — optional message attached to the decision
--
-- Keys:
--   <PageUp>/<PageDown>  scroll the preview half-page
--   e                    focus the reason input
--   <S-Tab>              toggle app mode; auto-allow + close when the
--                        new mode covers this request
--   Enter                resolve with the focused option (+ reason text)
--   Esc / Ctrl-C         resolve as "no"

local PANEL_OPTIONS = 4
local PANEL_REASON  = 5

function smelt.confirm.open(handle_id)
  local title_buf   = smelt.confirm._build_title_buf(handle_id)
  local summary_buf = smelt.confirm._build_summary_buf(handle_id)
  local preview_buf = smelt.confirm._build_preview_buf(handle_id)
  local labels      = smelt.confirm._option_labels(handle_id)
  if not labels then return end  -- registry entry vanished

  local items = {}
  for i, label in ipairs(labels) do
    items[i] = { label = label }
  end

  local panels = {
    { kind = "content", buf = title_buf,   height = "fit",  focusable = false },
    { kind = "content", buf = summary_buf, height = "fit",  focusable = false, collapse_when_empty = true },
    {
      kind                = "content",
      buf                 = preview_buf,
      height              = "fill",
      focusable           = false,
      collapse_when_empty = true,
      separator           = "dashed",
    },
    { kind = "options", items = items, focus = true },
    { kind = "input", placeholder = "reason (optional)…", collapse_when_empty = true },
  }

  local win_id = smelt.ui.dialog._open({
    panels           = panels,
    blocks_agent     = true,
    placement        = "dock_bottom",
    placement_height = 60,
  })
  if type(win_id) ~= "number" then return end

  local resolved = false
  local function close_with(idx, message)
    if resolved then return end
    resolved = true
    smelt.confirm._resolve(handle_id, idx, message)
    smelt.win.close(win_id)
  end

  smelt.win.set_keymap(win_id, "page_up",   function() smelt.confirm._scroll_preview(win_id, -1) end)
  smelt.win.set_keymap(win_id, "page_down", function() smelt.confirm._scroll_preview(win_id,  1) end)
  smelt.win.set_keymap(win_id, "e",         function() smelt.confirm._focus_reason(win_id) end)
  smelt.win.set_keymap(win_id, "s-tab",     function()
    if smelt.confirm._back_tab(handle_id) then
      resolved = true
      smelt.win.close(win_id)
    end
  end)

  smelt.win.on_event(win_id, "submit", function(ctx)
    local panels_snap = ctx.panels or {}
    local options_panel = panels_snap[PANEL_OPTIONS] or {}
    local idx = options_panel.selected or 1
    local reason_panel = panels_snap[PANEL_REASON] or {}
    local message = reason_panel.text
    if message == "" then message = nil end
    close_with(idx, message)
  end)

  smelt.win.on_event(win_id, "dismiss", function()
    -- "no" is always option 2 (yes/no come first).
    close_with(2, nil)
  end)
end
