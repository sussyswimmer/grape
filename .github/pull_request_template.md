## What changed

<!-- One concern per PR. Link the milestone issue by hand, e.g. "Part of #2" or "Closes #4". -->

## OS it was actually run on

<!-- ROADMAP → Definition of done. If you couldn't run it on the OS it targets, say so, put the code behind a feature flag, and add a manual QA item (CLAUDE.md). -->

- [ ] Windows
- [ ] macOS
- [ ] Linux / CI only

## Checklist

- [ ] Tests added or updated. `cargo fmt --all`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace` and `pnpm -C apps/desktop/ui typecheck` are clean.
- [ ] Docs updated in this PR if behavior changed. If code and docs disagree, the docs win.
- [ ] No source file was modified during the run (golden rule 2).
- [ ] Touched drop handling on Windows? Re-ran "Shift-drop never deletes the source" by hand 10×.
- [ ] No network calls in the conversion path (golden rule 3).
- [ ] New crate or binary? Logged in `docs/DECISIONS.md` with its license and why. No GPL code linked into Grape's binaries (golden rule 6, `docs/ENGINES.md`).
- [ ] Clean room: no Tangerine code, assets, wording, colors or name (golden rule 1).

## grape-core API change?

<!-- grape-core's public API is the contract between lane A and lane B (ROADMAP → Agent lanes). A change needs a DECISIONS.md note and a heads-up here. -->

- [ ] No
- [ ] Yes. DECISIONS entry and what changed:
