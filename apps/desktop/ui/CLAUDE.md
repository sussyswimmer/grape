# apps/desktop/ui

The TypeScript UI for Grape's windows: the wheel **overlay**, **settings**, and later the **float** shelf and **toast**. **Lane A (Claude Code) owns this.**

## Stack
- TypeScript with `strict: true`, Vite, and one HTML entry per window (`overlay.html`, `settings.html`, …).
- Vanilla DOM and SVG for the wheel. No framework there. Settings may use one only if it earns its place, and that needs a `docs/DECISIONS.md` entry.
- Tests: Vitest.

## The UI never decides behavior
- Rust owns the state machine (`docs/INTERACTION_SPEC.md` §1), the hit test and the job runner. The UI renders what Rust sends and reports drops back.
- Events in: `wheel://open {anchor, mode, segments, labels}`, `wheel://highlight {index|null}`, `wheel://close`, `job://progress`, `job://done`.
- **No pointer events arrive during an OS drag.** Highlighting is driven by `wheel://highlight`, which comes from `DragDropEvent::Over` positions. Don't use DOM `pointermove`. `isPointInFill` is only a second check that honors the rounded corners (§4).
- Geometry comes from `grape-core`: the segments in `wheel://open`, and the exported `GEOMETRY` constants. Never hard-code radii, gaps or angles here.

## Wheel rendering
- Draw `n` segments with the gap and corner rounding from `GEOMETRY`. At `DENSE_AT` items or more, use the compact label style.
- The center label shows the hovered item's title, and hides when nothing is hovered.
- Empty wheel: show a message instead of segments. Write it in **our own words**, and use the user's **current** key names from settings, never hard-coded "Shift"/"Ctrl".
- Tick sound: a short, original sound, preloaded and played through WebAudio when the highlight moves to a new segment. Cancel any pending tick when the pointer leaves all segments. Respect the "sounds" setting.
- Honor `prefers-reduced-motion`: no scale or opacity animation, the wheel just appears.
- The glass follows the OS light/dark theme. Add accessible labels (`aria-label`, `<title>`) to segments.

## Rules
- **Clean room** (golden rule 1): only Grape's own palette (see `src/styles/tokens.css`), Lucide icons (ISC) or our own SVGs, and our own copy. No SF Symbols, no Tangerine colors, layout or wording.
- **Offline** (golden rule 3): no CDN scripts, web fonts or remote images. Everything is bundled. The CSP in `src-tauri/tauri.conf.json` enforces this; don't loosen it without a DECISIONS entry.
- **No `any`**: no `: any`, `as any` or `<any>`. `src/no-any.test.ts` fails the build if one appears.
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
