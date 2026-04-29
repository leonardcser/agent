-- Built-in ask_user_question tool.
--
-- Registers a sequential plugin tool (blocks the LLM turn until the
-- user replies) that iterates one dialog per question in args.questions.
-- Each dialog has a markdown question, the LLM's options, and an
-- "Other" free-text input. Returns a plain-text answer list.
--
-- multiSelect in the schema is accepted for LLM compatibility but
-- treated as single-select for now.

smelt.tools.register({
  name = "ask_user_question",
  description = "Ask the user questions to gather preferences, clarify instructions, or get decisions on implementation choices. Present 1-4 questions with 2-4 options each.",
  execution_mode = "sequential",
  parameters = {
    type = "object",
    properties = {
      questions = {
        type = "array",
        minItems = 1,
        maxItems = 4,
        description = "Questions to ask the user (1-4 questions)",
        items = {
          type = "object",
          properties = {
            question = {
              type = "string",
              description = "The complete question to ask the user.",
            },
            header = {
              type = "string",
              description = "Very short label (max 12 chars).",
            },
            options = {
              type = "array",
              minItems = 2,
              maxItems = 4,
              description = "The available choices. An 'Other' free-text input is automatically offered alongside the options — do NOT include one yourself.",
              items = {
                type = "object",
                properties = {
                  label = {
                    type = "string",
                    description = "Display text (1-5 words).",
                  },
                  description = {
                    type = "string",
                    description = "Explanation of this option.",
                  },
                },
                required = { "label", "description" },
              },
            },
            multiSelect = {
              type = "boolean",
              description = "Allow multiple selections.",
            },
          },
          required = { "question", "header", "options", "multiSelect" },
        },
      },
    },
    required = { "questions" },
  },
  execute = function(args)
    local questions = args.questions or {}
    if #questions == 0 then
      return "no questions asked"
    end

    local parts = {}
    for _, q in ipairs(questions) do
      local options = q.options or {}
      local items = {}
      for _, opt in ipairs(options) do
        local label = opt.label or ""
        local desc = opt.description or ""
        local item_label
        if desc ~= "" and label ~= "" then
          item_label = label .. " — " .. desc
        else
          item_label = label
        end
        table.insert(items, { label = item_label })
      end

      local title = q.header
      if title == nil or title == "" then
        title = "question"
      end

      local result = smelt.ui.dialog.open({
        title = title,
        blocks_agent = true,
        panels = {
          { kind = "markdown", text = q.question or "" },
          { kind = "options",  items = items },
          { kind = "input",    name = "other", placeholder = "or type a custom answer..." },
        },
      })

      local answer
      local custom = (result.inputs and result.inputs.other) or ""
      if custom ~= "" then
        answer = "Other: " .. custom
      elseif result.action == "dismiss" or result.option_index == nil then
        smelt.engine.cancel()
        return { content = "user cancelled", is_error = true }
      else
        local picked = options[result.option_index]
        answer = (picked and picked.label) or "(unknown)"
      end

      table.insert(parts, string.format("Q: %s\nA: %s", q.question or "", answer))
    end

    return table.concat(parts, "\n\n")
  end,
})
