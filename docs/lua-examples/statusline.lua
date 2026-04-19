-- Custom status bar showing cwd path and git branch.
-- Drop this into ~/.config/smelt/init.lua to try.

local function git_branch()
  local f = io.popen("git rev-parse --abbrev-ref HEAD 2>/dev/null")
  if not f then return nil end
  local branch = f:read("*l")
  f:close()
  return branch
end

smelt.statusline(function()
  local cwd = os.getenv("PWD") or ""
  local home = os.getenv("HOME") or ""
  if home ~= "" and cwd:sub(1, #home) == home then
    cwd = "~" .. cwd:sub(#home + 1)
  end

  local branch = git_branch()

  local items = {
    { text = " " .. cwd .. " ", bold = true, fg = 75, priority = 0, truncatable = true },
  }

  if branch then
    items[#items + 1] = {
      text = " " .. branch .. " ",
      fg = 114,
      priority = 1,
      group = true,
    }
  end

  items[#items + 1] = {
    text = os.date("%H:%M"),
    fg = 245,
    priority = 2,
    align_right = true,
  }

  return items
end)
