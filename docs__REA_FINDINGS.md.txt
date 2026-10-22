# REA findings: what was learned from Tangerine, and how

The behavior spec in `docs/` was recovered with **REA** (`rea-agents` 6.7.0) on 2026-10-19. This file is the evidence ledger, so later work can tell **observed** facts from **inferences**, **unknowns** and Grape's own **decisions**.

## Target and method

- **Target:** Tangerine's public marketing site, `https://tangerineformac.com/`. It contains a fully interactive, *simulated* demo of the app ("All operations on this page are simulated", per the page).
- **Not analyzed:** the Tangerine app itself. It isn't installed, and decompiling it isn't needed or wanted (DECISIONS D3).
- **Browser:** a throwaway headless Chrome 155 profile, attached via CDP. It used none of the user's own Chrome profile or sign-ins.
- **REA operations:**
  - `list_browser_targets`: evidence `ev_d9a9db77aba6c7f091f6f17eec1f93fd061ef21b908fd9dac175f9c7eabc8eef`
  - `analyze_web_bundle`: evidence `ev_66e7839c5018837a86fd28ef4f40d85222bbffe53ae80201125f7ef53fab9fed`. 3 scripts observed, 3 analyzed, 0 parse failures, completeness `complete`, no unknowns.
- **Main artifact:** `/assets/home-BU_Sezi2.js`, 42,765 bytes, sha256 `24a7b6d5d14087d1afb576161c013f8ce4559903ad3fc8ed5d6d3e098f86d3fd`. Re-downloaded separately and the hash matched REA's capture. Read for behavior only; **not copied into Grape**.

## Observed (directly in the demo script or page)

| # | Finding | Used in |
|---|---|---|
| O1 | Conversion matrix for 45 input formats. Reproduced, it yields **275 pairs**, equal to the advertised "275 conversion options" | FORMAT_MATRIX |
| O2 | Input-only formats: rar, ppt, xls, rtf, rtfd, textClipping | FORMAT_MATRIX |
| O3 | 26 tool keys with titles and names. Per-format tool lists for single vs multi selection, the multi-allowed set, the combine set and the opens-UI set | TOOLS_AND_ACTIONS, FORMAT_MATRIX |
| O4 | 6 file actions and their purposes | TOOLS_AND_ACTIONS |
| O5 | Output naming: `stem.ext`, `stem 2.ext`; tool suffixes ("Compressed", "No Metadata", …); wav compress → flac; gz → `.tar.gz` | FORMAT_MATRIX |
| O6 | Mode = exactly one modifier: Shift → conversions, Meta → tools, Control → actions. Switching swaps in place | INTERACTION_SPEC §2 |
| O7 | Anchor = cursor at activation, clamped on screen; the wheel doesn't follow the cursor | INTERACTION_SPEC §3 |
| O8 | Ring geometry and hit-test math (inner/outer radius, gap, angular test, item 0 at the top except n=3 on the left) | INTERACTION_SPEC §4 |
| O9 | Highlight sound on segment change. Center label. Empty-state messages | INTERACTION_SPEC §5 |
| O10 | Drop on a segment runs it. Otherwise cancel. 4 px movement threshold | INTERACTION_SPEC §2, §6 |
| O11 | Page copy: "File actions do not use your conversion allowance" → Tangerine has a usage quota | (Grape is free: D2) |
| O12 | Page copy: PDF pages export at 300 DPI; Apple silicon, macOS 13+ | ENGINES, PLATFORM_MACOS |

## Inferred (reasonable, not proven)

| # | Inference | Basis |
|---|---|---|
| I1 | The demo's matrix and tool lists mirror the real app | Exact match with the advertised counts (275 / 26 / 6) |
| I2 | The "opens UI" set means the tool shows an options or editor panel before running | Set membership: instant ops like Mute/Split/Extract are excluded |
| I3 | The real app detects drags without Accessibility permission | It ships sandboxed on the Mac App Store |
| I4 | DOCX/RTF/RTFD support may use `NSAttributedString` | Those are exactly the formats AppKit reads natively |

## Unknown (the website can't answer these)

- U1: How the real app detects a Finder drag and modifier taps (Grape's approach: PLATFORM_* docs).
- U2: Which engines the real app uses (FFmpeg? AVFoundation? its own?).
- U3: How `.pages` is read or written.
- U4: Whether Enter-to-open and arrow keys exist in the real app (Finder uses Enter for rename).
- U5: Real-app naming for multi-file "combine" tools, and progress UI.
- U6: Exact quality settings (compression presets, bitrates).

## Decided by Grape (not from Tangerine)

Everything tagged **[Grape]** in the docs: the Windows port, key defaults, geometry constants, tiers, engines and licensing, overlay architecture, settings, naming for combine tools, branding.

## Reproducing this

```bash
# headless Chrome with a throwaway profile, then:
npx -y rea-agents@6.7.0 list-browser-targets http://127.0.0.1:9333 --format json
npx -y rea-agents@6.7.0 analyze-web-bundle http://127.0.0.1:9333 <targetId> --observation-ms 3000 --format json
```
The site may change. Compare the artifact hash above before trusting a re-run.
