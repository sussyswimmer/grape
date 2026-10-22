---
description: Add or change a conversion pair (or a new format) end to end
argument-hint: "<from> -> <to>, or a new format"
---

# Add a conversion: $ARGUMENTS

A conversion touches the spec, the core model, an engine, a fixture and the QA list. Do all of it in one change, so the docs and code never disagree.

## 0. Check first
- Read `docs/FORMAT_MATRIX.md` (rules, tables, naming), `docs/ENGINES.md` (engine map, licensing rules) and `crates/grape-core/CLAUDE.md`.
- If the pair already exists, say so and stop.
- The matrix counts (**275** total, **178** v1) are the Tangerine parity numbers. **Changing them needs Maxwell's OK.** A new pair is a Grape addition: mark it **[Grape]** in the doc.
- Pick the tier (v1 / v2 / later) and say why.

## 1. Spec: `docs/FORMAT_MATRIX.md`
- Add the target to the input's row in its table. Update the `#` column, add a `<sup>` tier tag if it isn't v1, and update the row's Tier column.
- Update the **Total** line (total and per-tier counts).
- A new format also needs: its category line, any alias (`jpeg` → `jpg` style), and its display name.
- If the output name isn't plain `{stem}.{ext}`, add it to "Output naming".

## 2. Core: `crates/grape-core`
- New format: a `Format` variant with its key, display name, category and output extension. Aliases go in `Format::from_extension` only, never in the matrix.
- Add the target to the input's row in the matrix table, with its tier.
- Update the count tests (total, per tier, the row's count) and any naming tests.
- `cargo test -p grape-core` must pass, including the doc-sync test that compares the tables in `docs/FORMAT_MATRIX.md` with the code.

## 3. Engine: `crates/grape-engines`
- Implement the path in the right engine module (image, media, pdf, archive, subtitle, or docs for v2).
- FFmpeg targets: add or adjust the argument builder (a pure function, with an exact-argv unit test), and add the row to `docs/ENGINES.md` → "Target arguments".
- A new crate or binary: check `docs/ENGINES.md` → Licensing rules, and add a `docs/DECISIONS.md` line (name, version, license, why). GPL code only ships as a separate sidecar.

## 4. Fixture and integration test
- Add a fixture to `fixtures/` if the input format doesn't have one yet: at most 200 KB, license-free, with its origin listed in `fixtures/README.md`.
- Add an `engines-it` test that converts it and validates the output: magic bytes, it decodes, dimensions or duration within tolerance.
- The test **must** assert the source's hash, mtime and path are unchanged, and that no temp files are left behind.

## 5. QA and wrap-up
- Add a manual QA line to `docs/TESTING.md` if the conversion has a visible quirk (multi-page output, a slow encode, a size warning).
- Run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p grape-core` and the engine tests for the OS you're on.
- Commit the spec and core change, then the engine and fixture, each with an imperative subject. Say which OS the engine tests ran on.
