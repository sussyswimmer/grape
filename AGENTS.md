# AGENTS.md

This repo is built by both Claude Code and Codex. **`CLAUDE.md` is the canonical instruction file.** Read it first and follow its golden rules, then read the nested `CLAUDE.md` in whichever directory you're working in. They apply to every agent, not just Claude.

The six rules you must not break are summarized here (full text in `CLAUDE.md`):

1. Clean room: no Tangerine code, assets, wording, colors or name.
2. Never modify, overwrite, move or delete source files, except the explicit Move action.
3. Offline only: no network calls in the conversion path.
4. Never break the user's OS drag. Esc always closes the wheel.
5. One shared behavior spec (`docs/`); only drag detection, overlay and file actions differ per OS.
6. No GPL code linked into Grape's binaries. GPL tools ship only as separate sidecar executables, with licenses and source links.

**Current phase: Windows first** (`docs/DECISIONS.md` D8). Don't write macOS platform code until milestone M8.

Work lanes and milestone ownership are in `docs/ROADMAP.md` → "Agent lanes". Stay in your lane's crates unless the task says otherwise, so the two agents don't edit the same files at once.
