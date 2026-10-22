# grape-core

The pure model of Grape: formats, the conversion matrix, tools per format, output naming, wheel geometry and hit testing, and job planning. **Lane B (Codex) owns this crate.** Claude Code may read it, but API changes need a `DECISIONS.md` note.

## Hard rules
- **No I/O, no OS, no Tauri, no FFmpeg.** Path handling works on strings and `Path` values only. "Does this file exist?" comes in as a `&HashSet<String>` argument. If you need anything else, the code belongs in `grape-engines` or `grape-platform`.
- **The spec is the docs.** `docs/FORMAT_MATRIX.md` and `docs/INTERACTION_SPEC.md` §4 are authoritative. Encode the tables as data (`const` arrays / `match` arms), not scattered `if`s, so a diff against the doc is easy.
- **Deterministic.** Same input → same output. No randomness, clocks or env reads.
- Allowed deps: `serde`, `thiserror`, `strum`; `proptest` for dev only. Anything else needs a DECISIONS entry.

## Public API (the contract with lane A)
```rust
pub fn targets(f: Format) -> &'static [Format];
pub fn tools(f: Format, selection_len: usize) -> Vec<ToolKey>;
pub fn actions() -> &'static [ActionKey; 6];
pub fn output_name(source: &str, op: &Operation, taken: &HashSet<String>) -> String;
pub fn wheel_layout(n: usize) -> Vec<Segment>;          // center angle, start/end, label point
pub fn hit_test(dx: f64, dy: f64, n: usize) -> Option<usize>; // dx,dy from anchor, screen coords (y down)
pub fn plan(sel: &Selection, op: &Operation, s: &Settings) -> Result<JobPlan, PlanError>;
```
Feature gating: `Features { v2: bool, later: bool }` is passed into `targets`/`tools` callers. Keep the full matrix in the data and filter it at the edge.

## Tests you must keep green
- `full_matrix_has_275_pairs`, `v1_matrix_has_178_pairs`.
- `no_self_pairs`, `input_only_never_target`.
- One test per row of the tools table and the naming table in `FORMAT_MATRIX.md`.
- `hit_test` property tests: `None` inside `INNER_R`, outside `OUTER_R` and in gaps. Item 0 is at the top for n≠3 and on the left for n=3.
- Collision naming: `a.png` + taken{`a.png`} → `a 2.png`; `.tar.gz` stems.

## Gotchas
- Screen y grows **down**. The spec's angles are math-style (counter-clockwise from +x), so convert with `(cos θ · r, −sin θ · r)`.
- `gz` displays as "GZIP" and names as `.tar.gz`.
- `jpeg` is an alias of `jpg`, `tif` of `tiff`. Aliases are resolved in `Format::from_extension`, never in the matrix.
