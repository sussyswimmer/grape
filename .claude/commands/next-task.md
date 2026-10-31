---
description: Pick up the next unchecked roadmap item for a lane and build it
argument-hint: "A | B"
---

# Next task for lane $ARGUMENTS

Lane **A** is Claude Code (`grape-platform`, `apps/desktop/src-tauri`, `apps/desktop/ui`, packaging/CI). Lane **B** is Codex (`grape-core`, `grape-engines`, fixtures, `docs/FORMAT_MATRIX.md` tests). If no lane was given, ask which one before doing anything.

Codex can't run slash commands. Tell it: "Follow `.claude/commands/next-task.md` with lane B."

## 1. Find the task
1. Read `CLAUDE.md` (golden rules, current phase) and `docs/ROADMAP.md` → "Agent lanes".
2. Find the **earliest milestone that still has unchecked items for this lane**, and take its first unchecked item. Stay in the current phase: no macOS platform code before M8.
3. If the item depends on the other lane's work that isn't done yet (for example, the real wheel needs `grape-core`), stop and say what's blocking it. Don't build the other lane's part.
4. Restate the task and its **Accept** line in two or three sentences.

## 2. Read before you write
Use the "Which doc to read" table in `CLAUDE.md`, plus the nested `CLAUDE.md` of every directory you'll touch. The docs are the spec. If they're unclear or disagree with each other, say so and ask; don't guess.

## 3. Plan
- Platform code (drag detection, hooks, overlay windows, file actions) **always** gets plan mode first. Show the plan and wait for approval.
- Keep the slice small: one milestone item, or a few closely related ones. Stay inside this lane's crates.
- A new crate, npm package or binary needs a line in `docs/DECISIONS.md` (name, license, why). Check `docs/ENGINES.md` → Licensing rules first.
- A change to `grape-core`'s public API needs a `docs/DECISIONS.md` note and a heads-up in the PR description.

## 4. Build
- Write tests with the code: unit tests for logic, fixture tests for engines, mock-signal tests for the controller.
- Run everything that applies, and fix what fails:
  ```bash
  cargo fmt --all
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace
  pnpm -C apps/desktop/ui typecheck
  pnpm -C apps/desktop/ui test
  ```
- If you couldn't run the code on the OS it targets, put it behind a feature flag and add a manual QA line.

## 5. Update the docs
- Tick the finished items in `docs/ROADMAP.md`. Leave an **Accept** line unticked until it has really been checked on the target OS.
- If behavior changed on purpose, update the spec doc in the same change.
- Platform changes: add or refresh the matching manual QA lines in `docs/TESTING.md`.

## 6. Commit and report
- Small commits, imperative subject, one concern each.
- The summary says:
  - what was done, and the milestone items it covers
  - **which OS it was actually run on**
  - manual QA still pending (suggest `/qa-drag` after a milestone that touches the drag or the wheel)
  - anything left open, with the doc or decision it waits on
