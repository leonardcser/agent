//! Statusline width and priority stories.

use crate::app_story;
use protocol::{EngineAskErrorKind, EngineEvent};

app_story!(quota_pause, |ctx| {
    ctx.set_viewport(80, 12);
    ctx.run_lua(
        r#"
        smelt.settings.auto_continue = "always"
        smelt.time.now_ms = function() return 1704110400000 end
        smelt.time.format = smelt.time.format_utc
    "#,
    );
    ctx.push_user_turn("Finish the current task.");
    ctx.push_queued_message("Then run the tests.");
    ctx.engine(EngineEvent::TurnError {
        message: "quota exceeded".into(),
        kind: Some(EngineAskErrorKind::Quota),
        retry_at_ms: Some(1704119400000),
    });
    ctx.assert_snapshot_named("scheduled");
    ctx.set_viewport(36, 12);
    ctx.assert_snapshot_named("narrow");
    ctx.set_viewport(80, 12);
    ctx.run_lua(r#"smelt.settings.auto_continue = "off""#);
    ctx.assert_snapshot_named("manual");
});

app_story!(statusline_width_ladder, |ctx| {
    // Pin the statusline compositor's hide/show stages: a truncatable slug,
    // optional middle indicators, a required mode, and a right-aligned cursor.
    ctx.run_lua(
        r#"
        local statusline = require("smelt.statusline")
        statusline.remove("core")
        statusline.add("width_story", function()
          return {
            {
              text = " project-alpha-long-name ",
              style = { fg = "Comment" },
              priority = 5,
              truncatable = true,
            },
            {
              text = " INSERT ",
              style = { hl_group = "SmeltVimInsert" },
              priority = 3,
            },
            {
              text = " normal ",
              style = { hl_group = "SmeltModeDefault" },
              priority = 1,
            },
            {
              text = " 42.0 tok/s",
              style = { fg = "Comment" },
              priority = 4,
              separated = true,
            },
            {
              text = "permission pending",
              style = { fg = "SmeltAccent", bold = true },
              priority = 2,
              separated = true,
            },
            {
              text = "2 procs",
              style = { fg = "SmeltProcess" },
              priority = 2,
              separated = true,
            },
            {
              text = "12:34 56%",
              style = { fg = "Comment" },
              priority = 0,
              align_right = true,
            },
          }
        end)
        "#,
    );

    for width in [72, 40, 28, 18, 12] {
        ctx.set_viewport(width, 8);
        ctx.assert_snapshot();
    }
});
