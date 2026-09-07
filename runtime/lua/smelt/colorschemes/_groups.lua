-- Canonical manifest for highlight groups supplied by bundled colorschemes.
-- `role` describes which style channel a theme is expected to customize.
-- Tests keep this manifest, bundled theme definitions, production consumers,
-- and documented identifiers in sync.

return {
  { name = "Normal", role = "style", owner = "global", description = "Default text and terminal background." },
  { name = "Comment", role = "foreground", owner = "global", description = "Muted secondary text and comments." },
  { name = "Visual", role = "background", owner = "editor", description = "Selected text background." },
  { name = "Search", role = "style", owner = "editor", description = "Search match foreground and background." },
  { name = "YankFlash", role = "background", owner = "editor", description = "Transient yank confirmation background." },
  { name = "CursorLine", role = "background", owner = "editor", description = "Active row and modal action background." },
  { name = "ErrorMsg", role = "foreground", owner = "global", description = "Error and critical-state text." },
  { name = "WarningMsg", role = "foreground", owner = "global", description = "Warning-state text." },
  { name = "GhostText", role = "style", owner = "editor", description = "Inline prediction and suggestion text." },

  { name = "SmeltAccent", role = "foreground", owner = "global", description = "Primary smelt accent." },
  { name = "SmeltSlug", role = "style", owner = "statusline", description = "Task slug pill." },
  { name = "SmeltSuccess", role = "foreground", owner = "global", description = "Successful operation text." },
  { name = "SmeltHeading", role = "foreground", owner = "transcript", description = "Transcript and panel headings." },
  { name = "SmeltLink", role = "style", owner = "transcript", description = "Link destination text." },
  { name = "SmeltProcess", role = "foreground", owner = "processes", description = "Background process notices and counters." },

  { name = "SmeltGoalBanner", role = "style", owner = "goal", description = "Active goal banner row." },
  { name = "SmeltGoalBannerLabel", role = "style", owner = "goal", description = "Active goal label." },
  { name = "SmeltGoalBannerMode", role = "style", owner = "goal", description = "Goal continuation mode label." },
  { name = "SmeltGoalBannerPausedLabel", role = "style", owner = "goal", description = "Paused goal label." },
  { name = "SmeltGoalBannerBlockedLabel", role = "style", owner = "goal", description = "Blocked goal label." },

  { name = "SmeltStatusBg", role = "background", owner = "statusline", description = "Statusline row background." },
  { name = "SmeltUserBg", role = "background", owner = "transcript", description = "User and exec transcript block background." },
  { name = "SmeltUserTimestamp", role = "foreground", owner = "transcript", description = "Muted timestamp on a user-message panel." },
  { name = "SmeltScrollPillBg", role = "background", owner = "transcript", description = "Scroll position pill background." },
  { name = "SmeltCodeBlockBg", role = "background", owner = "transcript", description = "Fenced code block background." },
  { name = "SmeltSeparator", role = "foreground", owner = "chrome", description = "Prompt bars, borders, and inline separators." },
  { name = "SmeltResizeHandle", role = "foreground", owner = "prompt", description = "Active prompt resize handle." },
  { name = "SmeltScrollbarTrack", role = "background", owner = "editor", description = "Scrollbar track." },
  { name = "SmeltScrollbarThumb", role = "background", owner = "editor", description = "Scrollbar thumb." },

  { name = "SmeltToolPending", role = "foreground", owner = "tools", description = "Pending tool state." },
  { name = "SmeltReasonLow", role = "foreground", owner = "prompt", description = "Low reasoning effort." },
  { name = "SmeltReasonMedium", role = "foreground", owner = "prompt", description = "Medium reasoning effort." },
  { name = "SmeltReasonHigh", role = "foreground", owner = "prompt", description = "High reasoning effort." },
  { name = "SmeltReasonMax", role = "foreground", owner = "prompt", description = "Maximum reasoning effort." },
  { name = "SmeltReasonUltra", role = "foreground", owner = "prompt", description = "Ultra reasoning effort." },

  { name = "SmeltVimNormal", role = "style", owner = "statusline", description = "Vim normal mode pill." },
  { name = "SmeltVimInsert", role = "style", owner = "statusline", description = "Vim insert mode pill." },
  { name = "SmeltVimVisual", role = "style", owner = "statusline", description = "Vim visual mode pill." },
  { name = "SmeltModePlan", role = "style", owner = "statusline", description = "Plan agent mode pill." },
  { name = "SmeltModeApply", role = "style", owner = "statusline", description = "Apply agent mode pill." },
  { name = "SmeltModeYolo", role = "style", owner = "statusline", description = "Yolo agent mode pill." },
  { name = "SmeltModeDefault", role = "style", owner = "statusline", description = "Default and custom agent mode pill." },
  { name = "SmeltExecPrefix", role = "style", owner = "prompt", description = "Shell execution prefix." },

  { name = "SmeltDiffAddBg", role = "background", owner = "diff", description = "Added row background." },
  { name = "SmeltDiffDeleteBg", role = "background", owner = "diff", description = "Deleted row background." },
  { name = "SmeltDiffAddInlineBg", role = "background", owner = "diff", description = "Inline added-text background." },
  { name = "SmeltDiffDeleteInlineBg", role = "background", owner = "diff", description = "Inline deleted-text background." },
}
