-- `/cmd <arg>` dim placeholder - shows a hint like `[arg1|arg2|…]` when the
-- prompt reads `/<cmd> ` with the cursor past the space and no argument typed
-- yet. Replaces the old picker-based arg completer.

if not smelt.prompt then return end

local ns = smelt.ns("cmd_arg_placeholder")

local function prompt_buf()
  local win = smelt.prompt.win()
  return win and win:buf()
end

local function find_cmd(text, cpos)
  if text:find("\n", 1, true) then return nil end
  local name = text:match("^/(%S+)%s+$")
  if not name then return nil end
  local prefix_len = 1 + #name + 1 -- "/" + name + " "
  if cpos < prefix_len then return nil end
  for _, c in ipairs(smelt.cmd.list()) do
    if c.name == name then return c end
  end
  return nil
end

local function command_args(cmd)
  if not cmd.args_fn then return cmd.args end
  local ok, args = pcall(cmd.args_fn)
  if not ok or type(args) ~= "table" then return cmd.args end
  for i = 1, #args do
    if type(args[i]) ~= "string" then return cmd.args end
  end
  return args
end

local function build_placeholder(args, max_width)
  if max_width <= 4 then return nil end

  -- Free-text hint: <question>, <focus>, etc.
  if #args == 1 then
    local a = args[1]
    if a:sub(1, 1) == "<" and a:sub(-1) == ">" then
      if #a <= max_width then return a end
      return nil
    end
  end

  -- Finite choice set: [low|medium|high]
  local full = "[" .. table.concat(args, "|") .. "]"
  if #full <= max_width then return full end
  for i = #args, 1, -1 do
    local truncated = "[" .. table.concat(args, "|", 1, i) .. "|…]"
    if #truncated <= max_width then return truncated end
  end
  return nil
end

local function update()
  local buf = prompt_buf()
  if not buf then return end
  buf:clear_ns(ns)

  local text = smelt.prompt.text()
  local cpos = smelt.prompt.cursor()
  local cmd = find_cmd(text, cpos)
  if not cmd then return end
  local args = command_args(cmd)
  if not args or #args == 0 then return end

  local win = smelt.prompt.win()
  local content_width = win and win:content_width()
  if not content_width then return end

  local available = math.max(0, content_width - #text)
  local placeholder = build_placeholder(args, available)
  if not placeholder then return end

  local line = buf:line(1) or ""
  buf:mark(ns, 1, #line, {
    virt_text = placeholder,
    virt_text_hl = "GhostText",
  })
end

local win = smelt.prompt.win()
win:on("text_changed", update)
win:on("resized", update)
smelt.signal.subscribe("model", update)
smelt.events.on("cmd_pre", function()
  local buf = prompt_buf()
  if buf then buf:clear_ns(ns) end
end)
