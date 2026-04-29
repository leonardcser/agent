-- Built-in plan mode plugin.
--
-- Registers the `exit_plan_mode` tool and the plan-mode system prompt
-- section when the user switches to Plan mode. Unregisters them when
-- leaving Plan mode.

local PLAN_PROMPT = [[
# Plan mode
You are in PLAN mode. You must NOT make any edits, write files, or run any non-readonly tools.

You may only use read-only tools: read_file, glob, grep, bash (read-only commands only).

## Workflow

### Phase 1: Understand
- Understand the user's request by reading code and asking questions
- Search for existing functions, utilities, and patterns that can be reused
- Avoid proposing new code when suitable implementations already exist

### Phase 2: Design
- Design the implementation based on your findings
- Consider multiple approaches and their trade-offs
- Identify critical files and code paths

### Phase 3: Review
- Read critical files to deepen understanding
- Ensure the design aligns with the user's original request
- Use ask_user_question to clarify any remaining questions

### Phase 4: Finalize
- Call exit_plan_mode with your plan as the plan_summary argument
- The plan should include:
  - Context: why the change is being made and the intended outcome
  - The recommended approach (not all alternatives)
  - Paths of critical files to modify
  - Existing functions and utilities to reuse
  - How to verify/test the changes
- Keep it concise enough to scan quickly, but detailed enough to execute

## Rules
- Your turn should only end with ask_user_question OR exit_plan_mode
- Use ask_user_question ONLY to clarify requirements or choose between approaches
- Use exit_plan_mode to submit the plan for approval -- do NOT ask about plan approval via text
- Don't make large assumptions about user intent -- ask first]]

local ADJECTIVES = {
  "amber", "ancient", "azure", "blazing", "bold", "brave", "bright", "broad",
  "calm", "carved", "clear", "clever", "cold", "cool", "coral", "crisp",
  "crystal", "dark", "deep", "deft", "dry", "eager", "endless", "fair",
  "fallen", "fast", "fierce", "fine", "firm", "fleet", "flowing", "flying",
  "foggy", "free", "frozen", "gentle", "gilded", "glad", "glass", "gold",
  "grand", "green", "grey", "hidden", "hollow", "humble", "hushed", "iron",
  "ivory", "keen", "kind", "last", "late", "lean", "light", "little", "lone",
  "long", "lost", "lucky", "lucid", "mild", "misty", "mossy", "muted",
}

local NOUNS = {
  "anchor", "arch", "ash", "aurora", "basin", "bay", "beacon", "beam", "bell",
  "birch", "blade", "bloom", "bluff", "branch", "breeze", "bridge", "brook",
  "cairn", "canyon", "cape", "cedar", "chalk", "cliff", "cloud", "coast",
  "coral", "cove", "crane", "creek", "crest", "crown", "crystal", "dale",
  "dawn", "delta", "dew", "dove", "drift", "dune", "dusk", "eagle", "echo",
  "edge", "elm", "ember", "falcon", "feather", "fern", "field", "fjord",
  "flame", "flint", "forge", "fox", "frost", "garden", "gate", "glade",
}

local VERBS = {
  "arcing", "blazing", "bowing", "braiding", "calling", "carving", "chasing",
  "climbing", "coiling", "crossing", "curving", "dancing", "dashing", "dipping",
  "diving", "drifting", "ebbing", "facing", "fading", "falling", "flowing",
  "folding", "forging", "forming", "gliding", "growing", "guiding", "holding",
  "humming", "jumping", "keeping", "landing", "leading", "leaning", "leaping",
}

local function generate_plan_name()
  local t = os.time()
  local adj = ADJECTIVES[(t % #ADJECTIVES) + 1]
  local noun = NOUNS[(math.floor(t / #ADJECTIVES) % #NOUNS) + 1]
  local verb = VERBS[(math.floor(t / (#ADJECTIVES * #NOUNS)) % #VERBS) + 1]
  return adj .. "-" .. noun .. "-" .. verb
end

local function save_plan(summary)
  local session_dir = smelt.session.dir()
  if session_dir == "" then
    return nil, "no session directory"
  end
  local plans_dir = session_dir .. "/plans"
  os.execute('mkdir -p "' .. plans_dir .. '"')

  local base = generate_plan_name()
  local path = plans_dir .. "/" .. base .. ".md"
  local n = 2
  while io.open(path, "r") do
    path = plans_dir .. "/" .. base .. "-" .. n .. ".md"
    n = n + 1
  end

  local f, err = io.open(path, "w")
  if not f then
    return nil, err
  end
  f:write(summary)
  f:close()
  return path
end

local function activate()
  smelt.prompt.set_section("plan_mode", PLAN_PROMPT)

  smelt.tools.register({
    name = "exit_plan_mode",
    description = "Signal that planning is complete and ready for user approval. Call this when your plan is finalized.",
    modes = { "plan" },
    parameters = {
      type = "object",
      properties = {
        plan_summary = {
          type = "string",
          description = "A concise summary of the implementation plan for the user to approve.",
        },
      },
      required = { "plan_summary" },
    },
    execute = function(args)
      local summary = args.plan_summary or ""

      -- Open the confirm dialog and wait for the user's answer.
      -- `dialog.open` yields the task coroutine; `result` is
      -- `{ action, option_index, inputs }`.
      local result = smelt.ui.dialog.open({
        title  = "plan",
        blocks_agent = true,
        panels = {
          { kind = "markdown", text = summary },
          { kind = "options", items = {
            {
              label = "yes, and auto-apply",
              action = "approve",
              on_select = function() smelt.engine.set_mode("apply") end,
            },
            { label = "yes", action = "approve" },
            { label = "no",  action = "deny"    },
          }},
        },
      })

      if result.action ~= "approve" then
        return { content = "Plan not approved.\n\n" .. summary, is_error = true }
      end

      local path, err = save_plan(summary)
      if path then
        return "Plan saved to " .. path .. "\n\n" .. summary
            .. "\n\nThe user approved this plan. Proceed with the implementation now."
      else
        return { content = "Failed to save plan: " .. (err or "unknown") .. "\n\n" .. summary, is_error = true }
      end
    end,
  })
end

local function deactivate()
  smelt.prompt.remove_section("plan_mode")
  smelt.tools.unregister("exit_plan_mode")
end

-- React to mode changes.
smelt.on("mode_change", function()
  local mode = smelt.engine.mode()
  if mode == "plan" then
    activate()
  else
    deactivate()
  end
end)

-- If we're already in plan mode at session start, activate.
smelt.on("session_start", function()
  if smelt.engine.mode() == "plan" then activate() end
end)
