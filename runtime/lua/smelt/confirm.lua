-- `smelt.confirm.open(handle_id)` — built-in tool-approval dialog.
--
-- Fired from the agent loop after registering a request in
-- `App::confirm_requests`. Plugins can override this function in
-- their init.lua to swap the default UI.
--
-- Panels (top → bottom, indices below match the keymap callbacks):
--   1 title    — bash-syntax-highlit ` tool: desc Allow?`
--   2 summary  — optional muted summary, hidden when empty
--   3 preview  — diff / notebook / file / bash body, hidden when empty
--   4 options  — yes / no + dynamic "always allow …" entries
--   5 reason   — optional message attached to the decision
--
-- Keys:
--   e                    focus the reason input
--   <S-Tab>              toggle app mode; auto-allow + close when the
--                        new mode covers this request
--   Enter                resolve with the focused option (+ reason text)
--   Esc / Ctrl-C         resolve as "no"
--
-- The preview panel is interactive — click it to focus, then scroll
-- with the wheel or vim motions (j/k, gg/G, Ctrl-D/Ctrl-U).

-- Tools whose preview is rendered into the preview buffer. The Lua
-- side dispatches by tool_name onto the matching renderer primitive.
local function fill_preview(buf, req)
  local tool = req.tool_name
  if tool == "edit_file" then
    smelt.diff.render(buf, {
      old  = req.args.old_string or "",
      new  = req.args.new_string or "",
      path = req.args.file_path or "",
    })
  elseif tool == "write_file" then
    smelt.syntax.render(buf, {
      content = req.args.content or "",
      path    = req.args.file_path or "",
    })
  elseif tool == "edit_notebook" then
    smelt.notebook.render(buf, req.args)
  elseif tool == "bash" and req.desc:find("\n") then
    smelt.bash.render(buf, req.desc)
  end
end

function smelt.confirm.open(handle_id)
  local req = smelt.confirm._get(handle_id)
  if not req then return end  -- registry entry vanished

  local title_buf   = smelt.buf.create()
  local summary_buf = smelt.buf.create()
  local preview_buf = smelt.buf.create()

  smelt.confirm._render_title(title_buf, handle_id)
  if req.summary and req.summary ~= "" then
    smelt.buf.set_lines(summary_buf, { " " .. req.summary })
  end
  fill_preview(preview_buf, req)

  local items = {}
  for i, label in ipairs(req.options) do
    items[i] = { label = label }
  end

  local panels = {
    { kind = "content", buf = title_buf,   height = "fit",  focusable = false, name = "title"   },
    { kind = "content", buf = summary_buf, height = "fit",  focusable = false, collapse_when_empty = true, name = "summary" },
    {
      kind                = "content",
      buf                 = preview_buf,
      height              = "fill",
      interactive         = true,
      collapse_when_empty = true,
      separator           = "dashed",
      name                = "preview",
    },
    { kind = "options", items = items, focus = true, name = "options" },
    { kind = "input", placeholder = "reason (optional)…", collapse_when_empty = true, name = "reason" },
  }

  local d = smelt.ui.dialog.open_handle({
    panels           = panels,
    blocks_agent     = true,
    placement        = "dock_bottom",
    placement_height = 100,
  })
  if not d then return end

  local resolved = false
  local function close_with(idx, message)
    if resolved then return end
    resolved = true
    smelt.confirm._resolve(handle_id, idx, message)
    d:close()
  end

  smelt.win.set_keymap(d.win, "e",         function() d.panels.reason:focus()      end)
  smelt.win.set_keymap(d.win, "s-tab",     function()
    if smelt.confirm._back_tab(handle_id) then
      resolved = true
      d:close()
    end
  end)

  smelt.win.on_event(d.win, "submit", function(ctx)
    local panels_snap = ctx.panels or {}
    local options_panel = panels_snap[d.panels.options.idx] or {}
    local idx = options_panel.selected or 1
    local reason_panel = panels_snap[d.panels.reason.idx] or {}
    local message = reason_panel.text
    if message == "" then message = nil end
    close_with(idx, message)
  end)

  smelt.win.on_event(d.win, "dismiss", function()
    -- "no" is always option 2 (yes/no come first).
    close_with(2, nil)
  end)
end
