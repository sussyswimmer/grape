# Radial findings: what was learned from Radial, and how

The Shortcuts wheel (`DECISIONS.md` D47, `INTERACTION_SPEC.md` §12) is modeled on the behavior of Radial, a pie-menu app for macOS by AppVerge. This file is its evidence ledger, the way `REA_FINDINGS.md` is Tangerine's: it keeps **observed** facts apart from **inferences**, **unknowns** and Grape's own **decisions**.

## Target and method

- **Target:** Radial's public website, read on 2026-11-23: `https://radial.appverge.net/` (the site names `getradial.app` as its canonical address) and its documentation pages under `/docs/`.
- **Not analyzed:** the Radial app itself. It was not downloaded, installed, run or decompiled (D3 applies to it as it does to Tangerine). The `/download` page was read as a page only; its download link was not followed.
- **Method:** plain HTTPS fetches of the pages below, read as text. No browser session, no account, no REA run: the pages are static and say what the app does in prose.
- **Pages read**, with the first 16 hex digits of each page's SHA-256 as fetched:

| Page | sha256 (first 16) |
|---|---|
| `/` | `d85ae77e762385e6` |
| `/download` | `fb68d318b201393c` |
| `/features` | `10e3c929a5c1496e` |
| `/features/keyboard-shortcuts` | `1cd9b343adbc9e51` |
| `/features/context-aware-menus` | `c202fea28112f679` |
| `/features/sub-menus` | `7a4b2dc03bb94710` |
| `/docs/getting-started/basic-usage` | `32fd393a27b28df8` |
| `/docs/getting-started/first-shortcut` | `8864a3a901a3407c` |
| `/docs/guides/customization` | `9556cf237d66b7de` |
| `/docs/guides/app-specific-macros` | `3e4e310a5740a2bc` |
| `/docs/guides/sub-menus` | `f2142a90cbc6ad36` |

- Read for behavior only; **nothing was copied into Grape**: no code, icons, images, sounds, colors, wording or names. Grape's words for these things are its own (`docs/design/m10-shortcuts-wheel.md` §1).

## Observed (stated on the pages)

| # | Finding | Used in |
|---|---|---|
| R1 | A circular menu opens at the cursor from a keyboard trigger, with no drag. Each slice is something the user set up; picking one runs it and the menu closes | INTERACTION_SPEC §12 |
| R2 | Two ways to open it by keyboard: hold a modifier (the menu shows while held; letting go runs the item under the pointer or dismisses), or a recorded key combination that toggles it (then click, or Enter, picks) | INTERACTION_SPEC §12 |
| R3 | Other ways to open it: a screen corner, shaking the cursor, a mouse button. An activation delay and a double press can be set | ROADMAP M10 (not built) |
| R4 | Picking is by moving toward an item; items keep their place in the circle, so the hand learns them | INTERACTION_SPEC §12 |
| R5 | The menu depends on the app in front: an app can have its own menu, and a global one covers the rest. The docs say the global items show in apps that have no menu of their own | INTERACTION_SPEC §12 |
| R6 | An item is a name, an icon and a sequence of actions run in order | m10 design §7 |
| R7 | Action kinds listed: press a key combination; type text (with variables); clipboard copy, paste, clear; open an app, a file, a URL; run an Apple Shortcut, an AppleScript, a shell script, a Keyboard Maestro macro; an AI action; window alignment; system controls; a multi-window layout; a notification; a screen effect; speak text; wait; open another menu | m10 design §7 |
| R8 | A sub-menu is an item that opens another menu in place of the current one, and the user can go back to the parent | INTERACTION_SPEC §12 |
| R9 | Icons can be app icons, system symbols, emoji or the user's images | m10 design §6 (emoji and Grape's own glyphs only) |
| R10 | Settings for the look: shape, size, at the cursor or the screen's center, labels under icons, animation speed, a haptic on hover | ROADMAP M10 (placement only) |
| R11 | Whole menus can be shared and installed from an online library; the site suggests up to nine items for a shared menu | ROADMAP M10 (not built: needs the network) |
| R12 | An app switcher with window previews | ROADMAP M10 (not built) |
| R13 | macOS 15 or later; paid, with a trial | (Grape is free: D2) |

## Inferred (reasonable, not proven)

| # | Inference | Basis |
|---|---|---|
| RI1 | Pressing keys in other apps needs the Accessibility permission | macOS requires it for posted key events; the pages don't say |
| RI2 | The menu window doesn't take the keyboard from the app in front | The key combination is described as going to the active app |
| RI3 | A global menu and an app's menu don't show together in one ring | R5's wording; the marketing page is looser ("both live on the same hotkey") |

## Unknown (the website can't answer these)

- RU1: How the app reads its trigger keys, the pointer and Esc.
- RU2: How it keeps focus on the app in front while the menu is clicked.
- RU3: What happens when an action in a sequence fails.
- RU4: How deep sub-menus may nest, and how many items a menu may hold.
- RU5: What it stores, and where.

## Decided by Grape (not from Radial)

Everything tagged **[Grape]** in `INTERACTION_SPEC.md` §12 and all of `docs/design/m10-shortcuts-wheel.md`: one hotkey that works both held and tapped; Esc and Return read as temporary hot keys; the pointer read by polling; the ring's geometry and hit test (Grape's own, from §4); 12 items and two levels; an app's menu replacing Everywhere, with an optional Everywhere group; which step kinds exist and what each runs; no AI and no online library (golden rule 3); the file format; the starter shortcuts; every word of copy; the glyphs.

## Reproducing this

```bash
curl -sSL -o keyboard-shortcuts.html https://radial.appverge.net/features/keyboard-shortcuts
sha256sum keyboard-shortcuts.html
```
The site may change. Compare the hashes above before trusting a re-read.
