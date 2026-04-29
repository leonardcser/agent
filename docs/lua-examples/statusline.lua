-- Three statusline sources: a cwd label, a git branch pill, and a
-- clock. Each registered independently; items are appended to the
-- Rust-side built-in spans (slug, vim mode, model, cost, position, …).
--
-- Sources render left-to-right in registration order. The optional
-- third arg sets a default `align` for items the source returns
-- without an explicit `align_right`; per-item `align_right` still
-- wins when set.

local function git_branch()
  local f = io.popen("git rev-parse --abbrev-ref HEAD 2>/dev/null")
  if not f then return nil end
  local branch = f:read("*l")
  f:close()
  return branch
end

smelt.statusline.register("cwd", function()
  local cwd = os.getenv("PWD") or ""
  local home = os.getenv("HOME") or ""
  if home ~= "" and cwd:sub(1, #home) == home then
    cwd = "~" .. cwd:sub(#home + 1)
  end
  return { text = " " .. cwd .. " ", bold = true, fg = 75, priority = 0, truncatable = true }
end)

smelt.statusline.register("git_branch", function()
  local branch = git_branch()
  if not branch then return nil end
  return { text = " " .. branch .. " ", fg = 114, priority = 1, group = true }
end)

smelt.statusline.register("clock", function()
  return { text = os.date("%H:%M"), fg = 245, priority = 2 }
end, { align = "right" })
