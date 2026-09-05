//! Prompt-input and shell-escape surfaces. The prompt is the chrome
//! users actually type into; `Block::Exec` is the resulting transcript
//! block when `!cmd` runs. Both share rendering primitives, so they
//! live together.

use crate::app_story;

// ── Prompt input variants ─────────────────────────────────────────

app_story!(prompt_typing_renders_into_input, |ctx| {
    ctx.set_viewport(40, 8);
    ctx.type_prompt("hello prompt");
    ctx.assert_snapshot();
});

app_story!(prompt_multiline_input, |ctx| {
    // Multi-line prompt: pin the chrome that auto-grows when the
    // buffer wraps onto extra rows (different from the single-line
    // chrome captured above).
    ctx.set_viewport(50, 10);
    ctx.type_prompt("first line\nsecond line\nthird line");
    ctx.assert_snapshot();
});

app_story!(transcript_live_search_preview, |ctx| {
    ctx.set_viewport(58, 14);
    ctx.push_user_turn("first question");
    ctx.push_assistant_text("ordinary answer");
    ctx.push_user_turn("the release needle appears in this question");
    ctx.push_assistant_text("ordinary response");
    ctx.push_user_turn("latest question");
    ctx.push_assistant_text("tail answer");
    ctx.preview_transcript_search("release needle");
    ctx.assert_snapshot();
});

app_story!(prompt_slash_command_completion, |ctx| {
    // Typing `/` triggers the slash-command completion popup over
    // the prompt. The aux tip row remains reserved but is covered by
    // the prompt-docked picker.
    ctx.set_viewport(50, 14);
    ctx.type_prompt("/");
    ctx.assert_snapshot();
});

app_story!(prompt_slash_command_refilter_keeps_best_on_bottom, |ctx| {
    // Slash command filtering resets to the best match instead of preserving
    // the previously selected command at its new ranked position. In the
    // prompt-docked picker, logical best renders on the bottom row and covers
    // the reserved aux tip row.
    ctx.set_viewport(50, 10);
    ctx.run_lua(
        r#"
      smelt.cmd.register("aaacx", function() end, { desc = "old first" })
      smelt.cmd.register("acx", function() end, { desc = "best match" })
    "#,
    );
    ctx.type_prompt("/acx");
    ctx.assert_snapshot();
});

app_story!(prompt_slash_completion_shrinks_for_short_terminal, |ctx| {
    // When the terminal is very short the prompt-docked picker must
    // shrink so it never overlaps the prompt input. The aux tip row stays
    // reserved but is covered by the picker.
    ctx.set_viewport(50, 8);
    ctx.type_prompt("/");
    ctx.assert_snapshot();
});

app_story!(prompt_slash_completion_two_rows_when_cramped, |ctx| {
    // Extreme height: the picker collapses to just a couple of rows
    // rather than painting over the prompt chrome. The bottom picker row
    // covers the reserved aux tip row.
    ctx.set_viewport(50, 6);
    ctx.type_prompt("/");
    ctx.assert_snapshot();
});

app_story!(prompt_modal_picker_overlays_tip, |ctx| {
    // A modal prompt-docked picker covers the reserved aux tip row, so the
    // transcript and prompt chrome do not shift.
    ctx.set_viewport(50, 10);
    ctx.run_lua(
        r#"
      smelt.cmd.register_picker("tipless", {
        items = {
          { label = "alpha", description = "first" },
          { label = "beta", description = "second" },
        },
      })
    "#,
    );
    ctx.run_command("/tipless");
    ctx.assert_snapshot();
});

app_story!(slash_command_accent_survives_lua_reload, |ctx| {
    // A successful reload swaps the live prompt command catalog. Persisted
    // command semantics keep the transcript accent independent of that catalog.
    ctx.set_viewport(40, 10);
    ctx.reload_lua();
    ctx.push_command_turn("/permissions");
    ctx.type_prompt("/permissions");
    ctx.assert_snapshot();
});

app_story!(prompt_shell_escape_prefix, |ctx| {
    // `!cmd` switches the prompt into shell-escape rendering: accent
    // `!` prefix, panel chrome shared with the exec block.
    ctx.set_viewport(50, 8);
    ctx.type_prompt("!ls -la");
    ctx.assert_snapshot();
});

// ── Prompt top-bar chrome (queued + stash) ────────────────────────

app_story!(prompt_stash_row, |ctx| {
    // Ctrl+S stashes the current buffer. The top bar grows by one row
    // with the `◌ Stashed (ctrl+s to unstash)` cue.
    ctx.set_viewport(50, 8);
    ctx.type_prompt("draft note");
    ctx.stash_prompt();
    ctx.assert_snapshot();
});

app_story!(prompt_queued_messages, |ctx| {
    // While a turn is active, Enter-on-prompt queues the message for the
    // next turn. Empty Enter promotes the oldest item into the next-request
    // queue, giving request and turn rows different markers.
    ctx.set_viewport(50, 10);
    ctx.push_queued_message("first follow-up");
    ctx.push_queued_message("second follow-up");
    ctx.promote_next_queued_message();
    ctx.assert_snapshot();
});

app_story!(
    prompt_queued_messages_collapse_when_tall_queue_short_terminal,
    |ctx| {
        // A large queue on a short terminal must collapse oldest messages
        // into a "+N more" row so the transcript keeps at least two rows.
        ctx.set_viewport(50, 10);
        for i in 1..=12 {
            ctx.push_queued_message(&format!("follow-up {}", i));
        }
        ctx.assert_snapshot();
    }
);

app_story!(
    prompt_queued_messages_with_stash_collapse_on_short_terminal,
    |ctx| {
        // Stash plus a tall queue on a short terminal: the stash row must
        // stay visible and the queue still collapses to "+N more".
        ctx.set_viewport(50, 10);
        ctx.type_prompt("draft note");
        ctx.stash_prompt();
        for i in 1..=10 {
            ctx.push_queued_message(&format!("follow-up {}", i));
        }
        ctx.assert_snapshot();
    }
);

app_story!(
    prompt_slash_completion_with_stash_shrinks_for_short_terminal,
    |ctx| {
        // A prompt-docked picker must still avoid overlapping the prompt
        // when the top bar has grown by an extra stash row.
        ctx.set_viewport(50, 8);
        ctx.type_prompt("draft note");
        ctx.stash_prompt();
        ctx.type_prompt("/");
        ctx.assert_snapshot();
    }
);

app_story!(
    prompt_queued_messages_with_notification_collapse_on_short_terminal,
    |ctx| {
        // A notification uses the stable aux row above the prompt; the queue
        // still collapses and the stash/indicator rows stay visible on a short
        // terminal.
        ctx.set_viewport(50, 10);
        ctx.notify("this is a notification", None);
        for i in 1..=10 {
            ctx.push_queued_message(&format!("follow-up {}", i));
        }
        ctx.assert_snapshot();
    }
);

app_story!(
    prompt_slash_completion_with_notification_shrinks_for_short_terminal,
    |ctx| {
        // A prompt-docked picker covers a visible toast and anchors directly
        // above the prompt chrome, so the layout does not shift.
        ctx.set_viewport(50, 8);
        ctx.notify("this is a notification", None);
        ctx.type_prompt("/");
        ctx.assert_snapshot();
    }
);

app_story!(prompt_slash_completion_with_live_notification, |ctx| {
    // A live toast is covered by prompt-docked picker rows; the picker
    // stays anchored immediately above the prompt chrome.
    ctx.set_viewport(50, 8);
    ctx.type_prompt("/");
    ctx.notify("slash notice", None);
    ctx.assert_snapshot();
});

app_story!(prompt_path_completion_on_tab, |ctx| {
    // Plain path completion is manual and directory-scoped: Tab lists entries
    // for the typed directory instead of fuzzy-searching the workspace.
    ctx.set_viewport(50, 8);
    ctx.write_fixture("src/main.rs", "fn main() {}\n");
    ctx.write_fixture("src/lib.rs", "pub fn lib() {}\n");
    ctx.write_fixture("src/bin/app.rs", "fn main() {}\n");
    ctx.type_prompt("src/");
    ctx.press_tab();
    ctx.assert_snapshot();
});

app_story!(prompt_file_completion_with_error_notification, |ctx| {
    // `@file` uses the same prompt-docked picker plane as slash completion.
    // An error toast is covered by the picker rows when they coincide.
    ctx.set_viewport(50, 8);
    ctx.run_lua(
        r#"
        smelt.files.search = function(_, _)
          return {
            items = {
              { label = "src", path = "src", kind = "dir" },
              { label = "src/main.rs", path = "src/main.rs", kind = "file" },
            },
            status = "ready",
            ready = true,
          }
        end
        "#,
    );
    ctx.type_prompt("@");
    ctx.run_lua(r#"smelt.notify.error("file lookup failed", "story")"#);
    ctx.assert_snapshot();
});

app_story!(prompt_history_picker_with_notification, |ctx| {
    // Reverse-history search is a secondary prompt-docked picker opened by
    // `/history`; the picker covers the notification and anchors directly
    // above the prompt chrome.
    ctx.set_viewport(50, 8);
    ctx.push_history_entry("explain the renderer layering");
    ctx.push_history_entry("open the model picker next");
    ctx.run_command("/history");
    ctx.notify("history notice", None);
    ctx.assert_snapshot();
});

app_story!(prompt_model_picker_with_notification, |ctx| {
    // `/model` opens a persistent prompt-docked picker after the slash command
    // resolves. The picker covers the notification and anchors directly above
    // the prompt chrome.
    ctx.set_viewport(50, 8);
    ctx.seed_models(&[
        ("openai", "gpt-5", "openai"),
        ("anthropic", "claude-sonnet-4", "anthropic"),
        ("copilot", "gpt-4.1", "copilot"),
    ]);
    ctx.run_command("/model");
    ctx.notify("model notice", None);
    ctx.assert_snapshot();
});

app_story!(prompt_reasoning_picker_native_levels, |ctx| {
    ctx.set_viewport(60, 12);
    ctx.seed_models(&[("anthropic", "claude-opus-4-6", "anthropic")]);
    ctx.run_command("/model anthropic/claude-opus-4-6");
    ctx.run_command("/reasoning high");
    ctx.run_command("/reasoning");
    ctx.assert_snapshot();
});

app_story!(prompt_picker_with_queue_stash_and_notification, |ctx| {
    // Picker rows anchor directly above the prompt chrome and cover the
    // notification. Queue, stash, and the tip remain inside the prompt top bar.
    ctx.set_viewport(50, 12);
    ctx.type_prompt("draft note");
    ctx.stash_prompt();
    ctx.push_queued_message("first follow-up");
    ctx.push_queued_message("second follow-up");
    ctx.type_prompt("/");
    ctx.notify("queued notice", None);
    ctx.assert_snapshot();
});

app_story!(prompt_centered_picker_with_notification, |ctx| {
    // Generic `smelt.picker.open` uses the dialog z-plane. If it overlaps a
    // toast, the centered picker stays above the notification.
    ctx.set_viewport(50, 8);
    ctx.run_lua(
        r#"
      smelt.spawn(function()
        smelt.picker.open({
          items = {
            { label = "alpha", description = "first" },
            { label = "beta", description = "second" },
            { label = "gamma", description = "third" },
          },
        })
      end)
    "#,
    );
    ctx.pump_lua();
    ctx.notify("center notice", None);
    ctx.assert_snapshot();
});

app_story!(prompt_turn_queue_row_truncates, |ctx| {
    // Long next-turn queue entries stay on one prompt-above row, truncate with
    // a Unicode ellipsis, and leave right padding so they do not hit the edge.
    ctx.set_viewport(32, 8);
    ctx.push_queued_message("turn queue message that should be clipped at the right edge");
    ctx.assert_snapshot();
});

app_story!(prompt_request_queue_row_truncates, |ctx| {
    // Promoted next-request entries use the `»` marker and share the same
    // truncation/padding behavior as next-turn entries.
    ctx.set_viewport(32, 8);
    ctx.push_queued_message("request queue message that should be clipped at the right edge");
    ctx.promote_next_queued_message();
    ctx.assert_snapshot();
});

app_story!(prompt_stash_row_truncates, |ctx| {
    // The stash reminder is fixed text, but on narrow terminals it still needs
    // ellipsis truncation and right padding like the queue rows.
    ctx.set_viewport(24, 8);
    ctx.type_prompt("draft note");
    ctx.stash_prompt();
    ctx.assert_snapshot();
});

app_story!(prompt_compacting_keeps_context_token_counter, |ctx| {
    // Compaction recalculates the active context, but the latest provider
    // reading stays visible as a display-only usage indicator.
    ctx.set_viewport(28, 8);
    ctx.set_context_window(Some(25_000));
    ctx.set_context_tokens(19_500);
    ctx.push_context_recalculation("compacting");
    ctx.push_compaction_preview("");
    ctx.assert_snapshot();
});

app_story!(prompt_working_bar_width_ladder, |ctx| {
    // The top prompt bar degrades in stages as the viewport shrinks: full
    // label, compact spinner, then the token strip without orphan spacing.
    ctx.set_context_window(Some(25_000));
    ctx.set_context_tokens(19_500);
    ctx.run_lua("_G._busy_handle = smelt.work.busy('indexing')");

    for width in [48, 36, 28, 20, 16] {
        ctx.set_viewport(width, 8);
        ctx.assert_snapshot();
    }
});

app_story!(prompt_extended_reasoning_colors, |ctx| {
    ctx.set_viewport(48, 8);
    ctx.use_test_model();

    for effort in ["xhigh", "max", "ultra"] {
        ctx.run_lua(&format!("smelt.reasoning.set({effort:?})"));
        ctx.assert_snapshot_named(effort);
    }
});

// ── Exec block (`!cmd` shell escape) ──────────────────────────────

app_story!(exec_command_block_with_output, |ctx| {
    // `!ls` drives the real `start_exec` → `append_exec_output` →
    // `finish_exec` lifecycle. The snapshot captures the `!` accent prefix,
    // panel chrome shared with user blocks, and the wrapped output region.
    ctx.set_viewport(50, 12);
    ctx.exec_with_output("ls -1 src", "lib.rs\nmain.rs\nparser.rs");
    ctx.assert_snapshot();
});
