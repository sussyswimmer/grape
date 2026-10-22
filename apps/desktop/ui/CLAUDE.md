# apps/desktop/ui

The TypeScript UI for Grape's windows: the wheel **overlay**, **settings**, and later the **float** shelf and **toast**. **Lane A (Claude Code) owns this.**

## Stack
- TypeScript with `strict: true`, Vite, and one HTML entry per window (`overlay.html`, `settings.html`, …).
- Vanilla DOM and SVG for the wheel. No framework there. Settings may use one only if it earns its place, and that needs a `docs/DECISIONS.md` entry.
- Tests: Vitest.

## The UI never decides behavior
- Rust owns the state machine (`docs/INTERACTION_SPEC.md` §1), the hit test and the job runner. The UI renders what Rust sends and reports drops back.
- Events in: `wheel://open`, `wheel://highlight`, `wheel://close`, `toast://show`, `toast://hide`, as specified in `docs/design/m1-drag-wheel.md` §C. The golden JSON in `src/shared/fixtures/` is shared with the Rust tests. Out: the `wheel_painted(seq)` command after each wheel render (`docs/DECISIONS.md` D15), and `toast_clicked(id)` when a revealable toast is clicked. Types and runtime guards live in `src/shared/events.ts`; every payload is guarded before use.
- **No pointer events arrive during an OS drag.** Highlighting is driven by `wheel://highlight`, which comes from the drop target's `Over` positions. Don't use DOM `pointermove`. The UI can't re-check the rounded corners with `isPointInFill`, so `grape_core::hit_test` honors them itself (§4); `src/wheel/geometry.ts` draws the same shape.
- Geometry comes from `grape-core`: the segments in `wheel://open`, and the exported `GEOMETRY` constants. Never hard-code radii, gaps or angles here.

## Wheel rendering
- Draw `n` segments with the gap and corner rounding from `GEOMETRY`. At `DENSE_AT` items or more, use the compact label style.
- The center label shows the hovered item's title, and hides when nothing is hovered.
- Empty wheel: show a message instead of segments. Write it in **our own words**, and use the user's **current** key names from settings, never hard-coded "Shift"/"Ctrl".
- Tick sound: a short, original sound synthesized with WebAudio (`src/wheel/tick.ts`, no audio file), played when the highlight moves to a new segment. Cancel any pending tick when the pointer leaves all segments. Respect the "sounds" setting.
- Honor `prefers-reduced-motion`: no scale or opacity animation, the wheel just appears.
- The glass follows the OS light/dark theme. Add accessible labels (`aria-label`, `<title>`) to segments.

## Rules
- **Clean room** (golden rule 1): only Grape's own palette (see `src/styles/tokens.css`), Lucide icons (ISC) or our own SVGs, and our own copy. No SF Symbols, no Tangerine colors, layout or wording.
- **Offline** (golden rule 3): no CDN scripts, web fonts or remote images. Everything is bundled. The CSP in `src-tauri/tauri.conf.json` enforces this; don't loosen it without a DECISIONS entry.
- **No `any`**: no `: any`, `as any`, `<any>` or any other form of the type. `src/no-any.test.ts` fails the build if the word `any` appears in code (comments and strings are fine), so don't use it as a name either.
- **Every DOM query is null-checked**: use `mustGetElementById` from `src/shared/dom.ts` instead of a bare `getElementById`/`querySelector`.
- Webview permissions live in `src-tauri/capabilities/`. Ask for the narrowest permission that works.

## Tests (Vitest)
Cover: segment path generation, label placement, empty-state messages, and key-name substitution. These are pure functions, so they don't need a DOM environment.

## Commands
```bash
pnpm -C apps/desktop/ui dev        # Vite dev server on :1420 (started by `pnpm tauri dev`)
pnpm -C apps/desktop/ui build      # dist/ for release builds
pnpm -C apps/desktop/ui typecheck
pnpm -C apps/desktop/ui test
```
