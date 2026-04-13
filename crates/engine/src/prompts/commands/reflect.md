---
description: step back and rethink recent changes before moving on
---

# Reflect

You just finished a task. Before moving on, step back and honestly evaluate what
you did.

The goal is to catch cases where you fought friction instead of fixing its root
cause — band-aid fixes, workarounds, growing complexity that signals a wrong
abstraction. This is your chance to course-correct before the debt compounds.

## Rules

- **Do NOT edit any files.** Only read code and report back.
- **Do NOT launch subagents.** Do all the analysis yourself.

## What to do

1. Run `git diff` (or `git diff HEAD` if there are staged changes) to see
   everything that changed.

2. For each change, ask:
   - **Did I work around a problem instead of fixing it?** A conditional that
     shouldn't need to exist, a special case bolted onto a general path, a flag
     added to bypass something broken.
   - **Did I add complexity because the existing structure fought me?** If
     slotting in a feature required touching many files, adding parameters, or
     threading state through layers — maybe the structure is wrong, not the
     feature.
   - **Is there a simpler design I dismissed too early?** Sometimes the "harder"
     refactor is actually less total work than the "quick" fix plus all its
     follow-on patches.
   - **Did I duplicate logic or patterns that already exist?** Search the
     codebase for similar utilities, helpers, or conventions I could have
     reused.
   - **Would I be comfortable seeing this code in a year without remembering the
     context?**

3. Report back with a numbered list of concrete suggestions. For each one:
   - What the current code does and why it smells
   - What you'd do instead
   - Why it's worth the effort (skip anything that isn't)

Be blunt and be brief. An empty list is a valid answer — not every task leaves
debt. Don't pad the list to look thorough.
