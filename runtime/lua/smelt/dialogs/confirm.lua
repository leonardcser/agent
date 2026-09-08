-- Built-in tool-approval dialog. Override `smelt.confirm.open` in init.lua to
-- swap the default UI. Tool `preview` callbacks live in each tool's Lua definition.

local label_value = smelt.label_value or require("smelt.label_value")

-- Build option labels and decision strings from the request payload.
local function build_options(req)
  local labels, decisions = {}, {}
  local function push(label, decision)
    labels[#labels + 1] = label
    decisions[#decisions + 1] = decision
  end

  push("allow once", "yes")
  push("deny", "no")

  for _, option in ipairs(req.grant_options or {}) do
    if option.label and option.id then
      push(option.label, option.id)
    end
  end

  return labels, decisions
end

local NS_HEADER_TOOL = smelt.ns("smelt.confirm.header.tool")
local NS_HEADER_DESC = smelt.ns("smelt.confirm.header.desc")

local function render_bash_header(buf, tool_name, command, desc, width)
  local plain = label_value.plain_lines(tool_name, command, width, { separator = ": " })
  if type(desc) == "string" and desc ~= "" then
    plain[#plain + 1] = desc
  end

  buf:lines(plain)
     :clear_ns(NS_HEADER_TOOL)
     :clear_ns(NS_HEADER_DESC)
  buf:mark(NS_HEADER_TOOL, 1, 0, { end_col = #tool_name, hl_group = "SmeltAccent" })
  if type(desc) == "string" and desc ~= "" then
    buf:mark(NS_HEADER_DESC, #plain, 0, { end_col = #desc, dim = true })
  end
end

local function summary_text(summary_lines)
  local out = {}
  for _, line in ipairs(summary_lines or {}) do
    local parts = {}
    for _, span in ipairs(line) do
      if not span.title_suffix then
        parts[#parts + 1] = tostring(span.text or "")
      end
    end
    out[#out + 1] = table.concat(parts)
  end
  return table.concat(out, "\n")
end

local function render_label_value_header(buf, tool_name, summary_lines, desc, width)
  local value = summary_text(summary_lines)
  if value == "" then
    local lines = { {
      { text = tool_name, style = { hl = "SmeltAccent" } },
      { text = ":" },
    } }
    if type(desc) == "string" and desc ~= "" then
      lines[#lines + 1] = { { text = desc, style = { dim = true } } }
    end
    buf:styled(lines)
    return
  end

  local lines = {}
  for _, row in ipairs(label_value.rows(tool_name, value, width, { separator = ": " })) do
    local spans
    if row.is_first then
      spans = {
        { text = tool_name, style = { hl = "SmeltAccent" } },
        { text = ": " },
      }
    else
      spans = { { text = row.label } }
    end
    spans[#spans + 1] = { text = row.value }
    lines[#lines + 1] = spans
  end

  if type(desc) == "string" and desc ~= "" then
    lines[#lines + 1] = { { text = desc, style = { dim = true } } }
  end

  buf:styled(lines)
end

-- Compose the body header: `tool_name: ` followed by the tool's
-- `summary(args)` output. Continuation lines are indented to align under the
-- first value column. Bash command text keeps a specialized renderer so it can
-- preserve command syntax highlighting and its timeout suffix.
local function render_header(buf, req, width)
  local tool_name = req.tool_name or ""
  local command = req.args and req.args.command
  local desc = req.args and req.args.description
  if tool_name == "bash" and type(command) == "string" then
    render_bash_header(buf, tool_name, command, desc, width)
    return
  end

  render_label_value_header(buf, tool_name, req.summary or {}, desc, width)
end

-- Drive the bundled tool-permission confirm dialog for `handle_id`.
-- Reads the matching request out of the `confirm_requested` signal, builds
-- the header + preview + option leaves, dispatches the user's choice
-- through the bundled host bridge. Bails when no matching request is active
-- (e.g. a newer prompt has superseded it). The host calls this Advanced
-- override point; plugins may replace it but should not invoke it directly.
---@advanced
---@type fun(handle_id: string): nil
function smelt.confirm.open(handle_id)
  -- Bail if the signal doesn't match this handle; a newer request may have
  -- replaced it before this dialog opened.
  local req = smelt.signal.get("confirm_requested")
  if not req or req.handle_id ~= handle_id then return end

  local header_buf  = smelt.buf.new()
  local preview_buf = smelt.buf.new({ readonly = true })
  render_header(header_buf, req, label_value.initial_dialog_width())
  __smelt_internal.confirm.__render_preview(preview_buf, handle_id)
  local first_preview = preview_buf:line(1)
  local has_preview = first_preview ~= nil and first_preview ~= ""

  local labels, decisions = build_options(req)

  local header_leaf  = smelt.dialog.content({ buf = header_buf, wrap = true })
  header_leaf:on("resized", function(ctx)
    render_header(header_buf, req, (ctx and ctx.content_width) or header_leaf:content_width())
  end)
  local preview_leaf = smelt.dialog.content({
    buf         = preview_buf,
    surface     = "selectable_text",
    readonly    = true,
    wrap        = false,
    pad_left    = 0,
    pad_right   = 0,
  })
  local allow_leaf, allow_buf = smelt.dialog.content({ wrap = false })
  allow_buf:styled({ { { text = "Allow?", style = { dim = true } } } })

  -- Reason input is built first so the options menu's on_submit can read
  -- its buffer when the user dismisses with text already typed.
  local reason_leaf, reason_buf =
      smelt.dialog.input("press tab to add a reason…", { pad_left = 2, wrap = true })
  local typed_reason = false
  reason_leaf:on("text_changed", function() typed_reason = true end)

  local resolved = false
  local function close_with(idx, message)
    if resolved then return end
    resolved = true
    __smelt_internal.confirm.__resolve(handle_id, decisions[idx] or "no", message)
  end

  local function current_reason()
    if not typed_reason then return nil end
    local line = reason_buf:line(1) or ""
    if line == "" then return nil end
    return line
  end

  local options_leaf, options_ctrl = smelt.dialog.menu(labels, {
    wrap = true,
    on_submit = function(ctx)
      close_with(ctx.index, current_reason())
      ctx.close()
    end,
  })

  -- Empty 1-row spacer panel that visually separates the options list from
  -- the reason input.
  local spacer_leaf = smelt.dialog.content({ text = "", wrap = false })

  local handle = smelt.dialog.new({
    blocks_agent = true,
    max_height   = "fill",
    min_height   = 0,
    panels = {
      { leaf = header_leaf,  height = "fit"                              },
      { leaf = preview_leaf, height = "fit", collapse_when_empty = true,
        border = has_preview and { style = "dashed", top = "Comment", bottom = "Comment" } or nil },
    },
    bottom_panels = {
      { leaf = allow_leaf,   height = "fit"                              },
      { leaf = options_leaf, height = "fit"                              },
      { leaf = spacer_leaf,  height = "fit"                              },
      { leaf = reason_leaf,  height = "fit", collapse_when_empty = true  },
    },
    focus = options_leaf,
    keymaps = {
      { key = "s-tab", on_press = function(ctx)
          if __smelt_internal.confirm.__back_tab(handle_id) then
            resolved = true
            ctx.close()
          end
        end },
    },
    on_dismiss = function(ctx)
      close_with(2, nil) -- "no" is always option 2
      ctx.close()
    end,
  })

  -- Tab from the options leaf jumps focus into the reason input; Esc inside
  -- the reason input pops focus back to the options leaf (instead of
  -- dismissing the dialog - that still works from the options leaf). Enter
  -- in the reason input routes through the menu's submit path so the
  -- highlighted option still drives the decision. Scoping all three
  -- keymaps per-leaf means typing literal Tab/Esc/Enter characters in the
  -- input would only ever do the configured action.
  options_leaf:key("tab", function() reason_leaf:focus() end)
  reason_leaf:key("esc", function() options_leaf:focus() end)
  reason_leaf:key("enter", function() options_ctrl:submit() end)

  return handle
end
