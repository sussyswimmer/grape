# apps/desktop/ui

The TypeScript UI for Grape's windows: the wheel **overlay**, **settings**, the **toast**, the **float** shelf and the tool **panel**. **Lane A (Claude Code) owns this.**

## Stack
- TypeScript with `strict: true`, Vite, and one HTML entry per window (`overlay.html`, `settings.html`, `toast.html`, `float.html`, `panel.html`), each listed in `vite.config.ts`.
- Vanilla DOM and SVG for the wheel. No framework there. Settings may use one only if it earns its place, and that needs a `docs/DECISIONS.md` entry.
- Tests: Vitest.

## The UI never decides behavior
- Rust owns the state machine (`docs/INTERACTION_SPEC.md` §1), the hit test and the job runner. The UI renders what Rust sends and reports drops back.
- Events in: `wheel://open`, `wheel://highlight`, `wheel://close`, `toast://show`, `toast://hide`, as specified in `docs/design/m1-drag-wheel.md` §C, the shelf's `float://view` and `float://drag-ended`, and the tool panel's `panel://open` and `panel://close`. The golden JSON in `src/shared/fixtures/` is shared with the Rust tests. Out: the `wheel_painted(seq)` command after each wheel render (`docs/DECISIONS.md` D15), `toast_clicked(id)` when a revealable toast is clicked, the shelf's `float_*` commands, and the panel's `panel_current`, `panel_run` and `panel_cancel`. Types and runtime guards live in `src/shared/events.ts`; every payload is guarded before use.
- Settings (`src/settings/`): each change is one `settings_*` command, and Rust answers with the whole `SettingsView` (or rejects with a short sentence the page shows). `isSettingsView` in `src/settings/model.ts` guards every answer against the golden `settings-view-*.json`. Rust swaps keys, opens the folder pickers and stores everything; the page never sends a path and has no Save button. The welcome card shows while the view says `welcome`, and only its "Got it" (`settings_dismiss_welcome`) hides it; its illustrations (`src/settings/welcome-art.ts`) are our own SVGs, colored and animated from `settings.css`, and still under `prefers-reduced-motion`.
- Float shelf (`src/float/`, Windows and macOS): Rust keeps the paths, the window and every drag; the page gets ids and display names. It pulls `float_view` once on load, then redraws from each `float://view`, keying rows by id so focus survives. Its commands are `float_remove({ id })`, `float_clear()`, `float_reveal({ id })` and `float_drag_out({ ids })`. A press on a row or "Drag all" that moves 4 px with the button held sends one `float_drag_out` while the button is still down; Rust runs the OS drag (copy or shortcut, never a move), and the page ignores pointer gestures until `float://drag-ended` (or a 10 s fallback), because WebView2, and WKWebView as far as is known, send no mouseup after an OS drag. Nothing is HTML-draggable and every `dragstart` is cancelled. Only the header's title is a `data-tauri-drag-region`: a file drag can't start from a window-move region.
- Tool panel (`src/panel/`, D31): Compress, Audio Channels, Normalize Volume and Merge PDFs show their options before they run. Rust keeps the paths and the window; the page gets a request `seq`, item ids (1..n in drop order) and display names, and answers by seq and ids only: `panel_run({ seq, order, choice })` or `panel_cancel({ seq })`. It asks `panel_current` once on load and shows the answer only if it's newer than every seq an event already brought. Enter runs (unless a panel button is the target), Esc cancels (or only ends a reorder gesture), Tab stays inside the panel. On Windows the panel opens without the keyboard, so it dims and its hint reads "Click here to use the keyboard." until the window gains focus; then the main control takes it. After Run or Cancel the panel takes nothing until `panel://close` (Rust hides it first) or a rejection: a refused run's sentence shows in the hint, and a refused cancel (a mouse button was down, so its Esc belonged to a drag) just takes input again. Compress and Channels are native radios in a segmented control, Normalize a native range, Merge a listbox with `aria-activedescendant` plus Move up/Move down and Alt+↑/↓; its pointer reorder uses pointer capture and the row math in `model.ts`, never layout reads. Nothing is HTML-draggable and every `dragstart` and `contextmenu` is cancelled. Its fixtures are `panel-*.json`: `panel-open-*` and `panel-close` are events in `events.test.ts`'s `CASES`; `panel-run-*` and `panel-cancel` are command args (UI → Rust), outside `CASES`, tested in `src/panel/model.test.ts`.
- **No pointer events arrive during an OS drag.** Highlighting is driven by `wheel://highlight`, which comes from the drop target's `Over` positions. Don't use DOM `pointermove` in the overlay. (The shelf uses pointer events only before its drag starts.) The UI can't re-check the rounded corners with `isPointInFill`, so `grape_core::hit_test` honors them itself (§4); `src/wheel/geometry.ts` draws the same shape.
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
Cover: segment path generation, label placement, empty-state messages, and key-name substitution. These are pure functions, so they don't need a DOM environment. The renderers (wheel, toast, settings, float shelf, tool panel) run against the small stand-in DOM in `src/test-support/fake-dom.ts`.

## Commands
```bash
pnpm -C apps/desktop/ui dev        # Vite dev server on :1420 (started by `pnpm tauri dev`)
pnpm -C apps/desktop/ui build      # dist/ for release builds
pnpm -C apps/desktop/ui typecheck
pnpm -C apps/desktop/ui test
```
