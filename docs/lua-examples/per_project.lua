-- Per-project config: if $PWD/.smelt/init.lua exists, source it after
-- the user-level config has loaded.

local cwd = os.getenv("PWD")
if cwd then
  local path = cwd .. "/.smelt/init.lua"
  local f = io.open(path, "r")
  if f then
    f:close()
    dofile(path)
    smelt.notify("loaded project config: " .. path)
  end
end
