---
description: Walk through the manual drag-and-drop QA checklist on scratch copies
---

# Manual drag QA

Guide Maxwell through the manual checklist in `docs/TESTING.md` on **this** machine, one item at a time, and prove that no source file was touched. Phase 1 is Windows: run the unmarked items and the **win:** items, and skip the **mac:** items until M8.

## Never use real files
Every drag in this session uses **scratch copies**. Never point Grape at the user's own documents, Desktop or Downloads.

1. Make a scratch folder: `%TEMP%\grape-qa-<yyyyMMdd-HHmmss>` on Windows (`$TMPDIR/grape-qa-<timestamp>` on macOS).
2. Copy `fixtures/` into it. Before `fixtures/` exists (before M3), create a small set instead: a `.txt`, a `.png`, a file whose name has spaces, a unicode name (`ảnh chụp.png`), and an existing `name 2.png` to test collisions.
3. For the "different drive" Shift-drop check, also put a copy on a second drive or a USB stick, if there is one.

## Baseline
Before the first drag, record the SHA-256, size and last-write time of every scratch file:

```powershell
Get-ChildItem -Recurse -File $qa | ForEach-Object {
  [pscustomobject]@{ Path = $_.FullName; Hash = (Get-FileHash -Algorithm SHA256 $_.FullName).Hash; Size = $_.Length; Mtime = $_.LastWriteTimeUtc }
} | Export-Csv "$qa-baseline.csv" -NoTypeInformation
```

Keep the baseline file **outside** the scratch folder, so it isn't part of what's being tested.

## Run the checklist
1. Make sure Grape is running (`pnpm tauri dev` in `apps/desktop`, or the installed build) and is **not** paused in the tray.
2. Read out one item from `docs/TESTING.md` at a time, in order: "Drag + wheel", then "Actions", then "Robustness". Skip items for features that aren't built yet, and say which ones you skipped.
3. Wait for Maxwell to try each item, then record **pass / fail / skipped** with a short note.
4. Do **"Shift-drop never deletes the source" 10 times**, including once from a different drive. After every Shift-drop, check that the source file is still there.
5. When an output appears, check its name against `docs/FORMAT_MATRIX.md` → "Output naming" (including the ` 2` collision rule).

## Check the sources again
Re-hash every baseline file and compare it with the baseline:
- **Any source missing, moved, or with a changed hash, size or mtime → STOP.** That breaks golden rule 2. Report it first, with the exact steps that caused it. The only exception is files moved by the **Move** action on purpose.
- New files are fine, as long as they're the expected outputs.

## Report
In the chat, give:
- the OS, Grape build and date
- a table of checklist items with their results
- source integrity: OK, or the list of files that changed
- the failures, each with steps to reproduce

Offer to add any new QA lines the run showed a need for. Delete the scratch folder **only after Maxwell confirms**.
