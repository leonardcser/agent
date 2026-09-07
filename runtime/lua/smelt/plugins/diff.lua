-- /diff: continuous, virtualized Git diff viewer. Layout, loading lifecycle,
-- sidebar synchronization and interaction policy live here; native documents
-- index patch bytes and materialize only the visible rows.

local M = {}
local active

local function display(text)
  return (text or ""):gsub("%c", function(c) return string.format("\\x%02x", c:byte()) end)
end

function M.open()
  if active then active.close() end

  local closed, loading, changing, overlay = false, nil, nil, nil
  local diff, selected, busy
  local stale = false
  local file_count = 0
  local focused = "preview"
  local tree, sidebar_count, folder_press
  local layout = smelt.ui.layout

  local function window(name, opts)
    local buf = smelt.buf.new()
    opts.name = "smelt.diff." .. name
    opts.region = "diff_overlay"
    opts.wrap = false
    local win = smelt.win.new(buf, opts)
    return win, buf
  end

  local sidebar, sidebar_buf = window("files", {
    surface = "list", kind = "list", pad_left = 1, pad_right = 1,
    hide_cursor = true, scrollbar = true,
  })
  local preview, preview_buf = window("preview", {
    surface = "readonly_text", vim_enabled = true, scrollbar = true,
    pad_left = 1, pad_right = 1,
  })

  local split = layout.split("horizontal", { size = 34, min_first = 26, min_second = 32 })
  local overlay_opts = { name = "smelt.diff", anchor = "center", width = "94%", height = "85%", modal = true, border = "none" }
  local function status()
    local label = busy or (stale and "stale - r refresh")
    local files_leaf = layout.leaf(sidebar, {
      border = { top = "Comment", bottom = "Comment", left = "Comment" },
      title = smelt.dialog.title(" files "),
    })
    overlay_opts.layout = split:layout(files_leaf,
      layout.leaf(preview, { border = { top = "Comment", bottom = "Comment", right = "Comment" },
        title = smelt.dialog.title(label and (" changes - " .. label .. " ") or " changes ") }))
    overlay = smelt.overlay.new(overlay_opts)
  end

  local function build_tree()
    sidebar_count, folder_press = nil, nil
    local collapsed = tree and tree:collapsed() or {}
    tree = diff and diff:tree(collapsed) or nil
    if tree then
      sidebar:document(tree:document())
    else
      sidebar:document(nil)
      sidebar_buf:lines({ "unavailable" })
    end
  end

  local function selected_node()
    return tree and tree:node(sidebar:cursor() or 0)
  end
  local function reveal_file(index)
    if tree then sidebar:cursor(tree:file_row(index, true) or 0) end
  end
  local function toggle_directory()
    if not tree then return end
    local row = tree:toggle(sidebar:cursor() or 0)
    if row then sidebar:cursor(row) end
  end

  local function cleanup()
    if closed then return end
    closed = true
    if loading then loading:remove(); loading = nil end
    if changing then changing:remove(); changing = nil end
    active = nil
    diff, tree, selected, folder_press = nil, nil, nil, nil
    file_count = 0
  end

  local function close()
    cleanup()
    if overlay then overlay:close() end
  end
  preview:on("close", cleanup)
  sidebar:on("close", cleanup)

  local function jump_file(index, sidebar_row)
    sidebar_count, folder_press = nil, nil
    if not diff or file_count == 0 then return end
    index = math.max(1, math.min(file_count, index))
    if not selected then preview:document(diff:document()) end
    selected = index
    if sidebar_row then sidebar:cursor(sidebar_row) else reveal_file(index) end
    local row = diff:file_row(index)
    preview:scroll(row)
    preview:cursor(row)
  end

  local function sync_sidebar()
    if closed or not diff or not selected or folder_press or focused ~= "preview" then return end
    local scroll = preview:scroll()
    if not scroll then return end
    local index = diff:file_at(math.max(scroll.top, preview:cursor() or 0))
    if index and index ~= selected then
      selected = index
      -- Following the preview never expands deliberately collapsed folders.
      local row = tree:file_row(index)
      if row then sidebar:cursor(row) end
    end
  end

  local function focus_pane(name)
    sidebar_count, folder_press = nil, nil
    focused = name
    if name == "preview" then preview:focus(); sync_sidebar() else sidebar:focus() end
  end
  sidebar:on("focus", function() focused = "files" end)
  sidebar:on("blur", function() sidebar_count, folder_press = nil, nil end)
  preview:on("focus", function() focused = "preview"; sync_sidebar() end)
  local function select_file()
    local node = selected_node()
    if node and node.index and node.index ~= selected then jump_file(node.index) end
  end
  local function pointer_row(event)
    local rect, scroll = sidebar:rect(), sidebar:scroll()
    if not rect or not scroll or event.row >= rect.height or event.col < 1 or event.col >= rect.width - 1 then return end
    return scroll.top + event.row
  end
  sidebar:on("press", function(event)
    sidebar_count, folder_press = nil, nil
    if event.button ~= "left" then return end
    local row = pointer_row(event)
    local node = tree and row and tree:node(row)
    if not node then return end
    focus_pane("files")
    sidebar:cursor(row)
    if node.children then
      folder_press = { tree = tree, row = row }
    elseif node.index then
      jump_file(node.index)
    end
  end)
  sidebar:on("release", function(event)
    local press = folder_press
    folder_press = nil
    if not press or event.button ~= "left" or press.tree ~= tree then return end
    local row = pointer_row(event)
    sidebar:cursor(press.row)
    if row == press.row then toggle_directory() end
  end)
  sidebar:on("scrolled", function() folder_press = nil end)
  sidebar:on("resized", function() folder_press = nil end)
  preview:on("scrolled", sync_sidebar)
  preview:on("selection_changed", sync_sidebar)

  local function nav_file(delta)
    local node = (focused == "files" or not selected) and selected_node() or nil
    if node and (node.children or node.group) then
      jump_file(delta > 0 and node.first or node.first - 1)
    else
      jump_file((node and node.index or selected or 1) + delta)
    end
  end
  local function nav_tree(delta)
    if not tree then return end
    local last = tree:document():row_count() - 1
    local current = sidebar:cursor() or 0
    local row = current + delta
    local separator = tree:section_row("staged") - 1
    if current < separator and row >= separator then
      row = row + 1
    elseif current > separator and row <= separator then
      row = row - 1
    end
    sidebar:cursor(math.max(0, math.min(last, row)))
    select_file()
  end
  local function open_node()
    local node = selected_node()
    if not node then return end
    if node.children then
      toggle_directory()
    elseif node.index then
      jump_file(node.index)
      focus_pane("preview")
    end
  end
  local function left()
    local node = selected_node()
    if not node then return end
    if node.children and node.expanded then
      toggle_directory()
    elseif node.parent then
      sidebar:cursor(node.parent)
    end
  end
  local function right()
    local node = selected_node()
    if node and node.children then
      if node.expanded then nav_tree(1) else toggle_directory() end
    else
      open_node()
    end
  end
  local function sidebar_key(key, action)
    sidebar:key(key, function()
      local count = sidebar_count or 1
      sidebar_count, folder_press = nil, nil
      action(count)
    end)
  end
  for digit = 0, 9 do
    sidebar:key(tostring(digit), function()
      folder_press = nil
      if tree and (sidebar_count or digit > 0) then
        sidebar_count = math.min((sidebar_count or 0) * 10 + digit, tree:document():row_count())
      end
    end)
  end
  sidebar_key("ctrl-j", function() nav_file(1) end)
  sidebar_key("ctrl-k", function() nav_file(-1) end)
  for _, binding in ipairs({
    { "j", 1 }, { "k", -1 }, { "down", 1 }, { "up", -1 },
    { "ctrl-n", 1 }, { "ctrl-p", -1 },
  }) do
    sidebar_key(binding[1], function(count) nav_tree(binding[2] * count) end)
  end
  for _, key in ipairs({ "g", "home" }) do
    sidebar_key(key, function() sidebar:cursor(0); select_file() end)
  end
  for _, key in ipairs({ "G", "end" }) do
    sidebar_key(key, function() if tree then sidebar:cursor(math.max(0, tree:document():row_count() - 1)); select_file() end end)
  end
  sidebar_key("enter", open_node)
  sidebar_key("space", open_node)
  sidebar_key("h", left)
  sidebar_key("left", left)
  sidebar_key("l", right)
  sidebar_key("right", right)
  for _, binding in ipairs({
    { "ctrl-u", -0.5 }, { "ctrl-d", 0.5 },
    { "ctrl-b", -1 }, { "ctrl-f", 1 }, { "pgup", -1 }, { "pgdn", 1 },
  }) do
    sidebar_key(binding[1], function(count)
      local height = (sidebar:rect() or {}).height or 10
      local delta = math.max(1, math.floor(height * math.abs(binding[2]))) * count
      nav_tree(binding[2] < 0 and -delta or delta)
    end)
  end

  local function hunk(forward)
    if not diff or not selected then return end
    local row = diff:hunk(preview:cursor() or 0, forward)
    if row then preview:scroll(row); preview:cursor(row); focus_pane("preview") end
  end
  local function fold()
    if not diff or not selected then return end
    local row = diff:toggle_fold(preview:cursor() or 0)
    if row then preview:scroll(row); preview:cursor(row) end
  end
  preview:key("enter", fold)
  preview:key("H", function() preview:pan(-4) end)
  preview:key("L", function() preview:pan(4) end)
  preview:key("<S-Left>", function() preview:pan(-4) end)
  preview:key("<S-Right>", function() preview:pan(4) end)

  local function install(result, keep, next_file, restore_position)
    local sidebar_scroll = tree and sidebar:scroll()
    local sidebar_row = sidebar:cursor() or 0
    local sidebar_offset = sidebar_scroll and (sidebar_row - sidebar_scroll.top)
    local scroll = preview:scroll() or {}
    local cursor, top
    if diff then cursor, top = result.diff:restore(diff, preview:cursor() or 0, scroll.top or 0) end
    local pan = scroll.left or 0
    diff, selected = result.diff, nil
    stale = false
    file_count = diff:file_count()
    build_tree()
    local index = keep and diff:find_file(keep.section, keep.key)
    local same_file = index ~= nil
    if not index and next_file then index = diff:find_file(next_file.section, next_file.key) end
    if not index then
      local first, last = diff:section_range(keep and keep.section or "unstaged")
      if first <= last then index = first elseif not keep and file_count > 0 then index = 1 end
    end
    if index then
      local hidden = same_file and restore_position and not tree:file_row(index)
      jump_file(index, hidden and math.min(sidebar_row, tree:document():row_count() - 1))
      if same_file and restore_position then
        if cursor then preview:cursor(cursor) end
        if top then preview:scroll(top) end
      end
      preview:pan(pan)
    else
      local section = keep and keep.section or "unstaged"
      sidebar:cursor(tree:section_row(section))
      preview:document(nil)
      preview_buf:lines({ "", file_count == 0 and "clean" or
        (section == "unstaged" and "all staged" or "nothing staged"), "r refresh" })
    end
    if sidebar_offset then
      sidebar:scroll(math.max(0, (sidebar:cursor() or 0) - sidebar_offset))
    end
  end

  local function refresh()
    sidebar_count, folder_press = nil, nil
    if changing then return end
    if loading then loading:remove() end
    busy = diff and "refreshing" or "loading"
    status()
    loading = smelt.spawn(function()
      local result, err = smelt.git.diff({ cwd = smelt.session.cwd() })
      if closed then return end
      loading, busy = nil, nil
      if not result then
        if diff then
          stale = true
          smelt.notify.error("refresh failed: " .. display(err))
        else
          preview:document(nil)
          preview_buf:lines({ "", "load failed", display(err or "git diff failed"), "r retry" })
          build_tree()
        end
      else
        local keep = selected and diff:file(selected)
        install(result, keep, nil, true)
      end
      status()
    end)
  end

  local function change_index(action)
    sidebar_count, folder_press = nil, nil
    if changing or loading or stale or not diff then return end
    if focused == "preview" and smelt.vim.mode() ~= "normal" then return end
    local node = focused == "files" and selected_node() or nil
    local index = focused == "files" and node and node.index or (focused == "preview" and selected)
    if not index then return end
    local keep = diff:file(index)
    if (action == "stage" and keep.section ~= "unstaged") or
       (action == "unstage" and keep.section ~= "staged") then return end
    reveal_file(index)
    local first, last = diff:section_range(keep.section)
    local next_file = index < last and diff:file(index + 1) or (index > first and diff:file(index - 1) or nil)
    local operation = keep.section == "unstaged" and "stage" or "unstage"
    busy = operation == "stage" and "staging" or "unstaging"
    status()
    changing = smelt.spawn(function()
      local result, err = smelt.git.index(diff, index, action)
      if closed then return end
      changing, busy = nil, nil
      if not result then
        stale = true
        smelt.notify.error(operation .. " failed: " .. display(err))
      elseif result.refresh_error then
        stale = true
        smelt.notify.error(operation .. " succeeded; refresh failed: " .. display(result.refresh_error))
      else
        install(result, keep, next_file)
      end
      status()
    end)
  end

  overlay_opts.keymaps = {
      { key = "q", on_press = close },
      { key = "esc", on_press = close },
      { key = "ctrl-c", on_press = close },
      { key = "r", on_press = refresh },
      { key = "ctrl-j", on_press = function() nav_file(1) end },
      { key = "ctrl-k", on_press = function() nav_file(-1) end },
      { key = "s", on_press = function() change_index("stage") end },
      { key = "u", on_press = function() change_index("unstage") end },
      { key = "-", on_press = function() change_index("toggle") end },
      { key = "tab", on_press = function() focus_pane(focused == "preview" and "files" or "preview") end },
      { key = "<S-Tab>", on_press = function() focus_pane(focused == "preview" and "files" or "preview") end },
      { key = "{", on_press = function() hunk(false) end },
      { key = "}", on_press = function() hunk(true) end },
      { key = "[", on_press = function() nav_file(-1) end },
      { key = "]", on_press = function() nav_file(1) end },
  }
  status()
  overlay_opts.keymaps = nil
  active = { close = close }
  preview_buf:lines({ "", "loading" })
  preview:focus()
  refresh()
end

smelt.cmd.register("diff", function() M.open() end, {
  desc = "view local Git changes in a continuous diff", busy = "run",
})

return M
