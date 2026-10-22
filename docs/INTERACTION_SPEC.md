# Interaction spec — the drag → key → wheel → drop flow

This is the heart of Grape. Every rule below is tagged:
- **[observed]** came from Tangerine's demo via REA (see `REA_FINDINGS.md`).
- **[Grape]** is our own design choice.

## 1. State machine

```
Idle
 └─(OS file drag begins)──────────────▶ Dragging        (no UI yet)
Dragging
 ├─(exactly one wheel modifier held)──▶ WheelOpen(mode)
 ├─(drag ends)────────────────────────▶ Idle            (OS handles the drop normally)
WheelOpen(mode)
 ├─(a different single modifier)──────▶ WheelOpen(new mode)   wheel re-renders in place
 ├─(cursor over a segment)────────────▶ highlight segment + play tick
 ├─(drop on a segment)────────────────▶ Running(job) ─▶ Idle
 ├─(drop off-ring / on empty wheel)───▶ Idle            (cancel, nothing happens)
 └─(Esc)──────────────────────────────▶ Idle            (overlay hides; OS drag continues)
```

## 2. Opening the wheel

- **[observed]** The wheel mode is chosen only when **exactly one** of Shift / Meta / Control (/ Alt tracked but unmapped) is held. Two or more modifiers at once mean "no mode", so nothing changes.
- **[observed]** Shift → conversions, Meta (Command) → tools, Control → actions.
- **[observed]** Pressing a *different* single modifier while the wheel is open swaps the wheel's contents. The wheel stays anchored where it was.
- **[observed]** The user can release the key and keep dragging. The wheel stays open until drop or cancel.
- **[Grape]** On Windows the defaults are Shift / Ctrl / Alt, because the Windows key opens the Start menu.
- **[Grape]** The wheel opens on **key-down** during an active drag. The user can tap (press and release) or keep holding; both work. Some apps, like Explorer, change the drag's drop effect while a modifier is held. That stops mattering because the drop then lands on Grape's overlay (see `PLATFORM_WINDOWS.md`).
- **[observed]** The wheel only counts as active after the pointer has moved more than 4 px since mouse-down. Grape uses 4 px on macOS and `GetSystemMetrics(SM_CXDRAG/SM_CYDRAG)` on Windows.

## 3. Anchor and placement

- **[observed]** The wheel's center (the *anchor*) is the cursor position at the moment the mode first activated. It does **not** follow the cursor.
- **[observed]** The anchor is clamped so the whole wheel plus a 14 px margin stays on screen.
- **[Grape]** Clamp to the bounds of the display under the cursor, excluding the menu bar or taskbar (use `visibleFrame` / the monitor work area).
- **[observed]** Switching modes re-renders the wheel at the same anchor.

## 4. Geometry

These are Grape's values, in points/DIPs at 1× scale. The site's values were ≈ 48.6 inner, ≈ 119.5 outer, gap 4. Ours are deliberately our own round numbers.

| Constant        | Value | Meaning                                              |
|-----------------|-------|------------------------------------------------------|
| `INNER_R`       | 48    | Dead zone radius; nothing is selected inside it      |
| `OUTER_R`       | 120   | Outer edge of the segments                           |
| `BACKDROP_R`    | 126   | Glass backdrop radius                                |
| `GAP`           | 4     | Visual and hit-test gap between segments             |
| `CORNER`        | 20    | Outer corner rounding of each segment                |
| `LABEL_R`       | ≈ 84  | Radius where labels and icons sit                    |
| `DENSE_AT`      | 7     | At ≥ 7 items use the compact label style             |

- **[observed]** There are `n` equal segments. The center angle of segment `i` is `start(n) − 2π·i/n`, with angles measured counter-clockwise from +x, so segments advance clockwise. `start(n) = π/2` (item 0 at the **top**), except `start(3) = π` (item 0 on the **left**).
- **[observed]** **Hit test**: let `d` be the distance from the anchor. If `d < INNER_R` or `d > OUTER_R`, nothing is selected. Otherwise pick the nearest segment by angle. If the pointer's angular distance from that segment's center exceeds `π/n − asin(GAP/2 / d)`, it's in a gap, so nothing is selected.
- **[observed]** The UI then also checks that the pointer is inside the drawn path (`isPointInFill`), so the rounded corners are honored. Grape does the same.
- **[Grape]** Hit testing runs in `grape-core` (pure function, unit-tested). The UI may re-check against its rendered path.

## 5. Feedback

- **[observed]** The hovered segment highlights. A center label shows the hovered item's title, or hides when nothing is hovered.
- **[observed]** Moving onto a *new* segment plays a short highlight sound. Leaving all segments cancels any pending sound.
- **[observed]** If the wheel has no items, it shows a message instead of segments:
  - Conversions or Actions empty: "No conversions for this format. Tap {actions key} for file actions."
  - Tools empty: "No editing tools for this format. Tap {conversions key} to convert or {actions key} for file actions."
  - **[Grape]** Use the user's *current* key names in these messages. Write the copy in our own words.
- **[Grape]** On macOS, also trigger a light trackpad haptic on segment change (`NSHapticFeedbackManager`). Respect the "sounds" setting.
- **[Grape]** Honor Reduce Motion: no scale or opacity animation, the wheel just appears.

## 6. Dropping

- **[observed]** Drop on a highlighted segment → run that conversion, tool or action.
- **[observed]** Drop while the wheel is open but nothing is highlighted → cancel. **[Grape]** The overlay swallows the drop, so the file does *not* land in whatever app was underneath. That's why the overlay covers the whole display while the wheel is open (see `ARCHITECTURE.md`).
- **[Grape]** Esc hides the overlay *without* ending the OS drag, so the user can still drop normally.
- **[observed]** While a conversion runs, a placeholder shows "Making {name}…". When it finishes, a confirmation appears.
- **[Grape]** In the real OS, show this as a small toast near the anchor plus the OS's own file appearance. The demo's ≈ 680 ms is fake latency; real jobs report progress.

## 7. Multiple files

- **[observed]** Tools depend on selection size. Some tools are hidden for multi-selections, and some only make sense with several files (Create PDF, Merge PDFs, Join Videos, Collage). See `TOOLS_AND_ACTIONS.md`.
- **[observed]** File actions work for folders, mixed selections, and files with no conversions.
- **[Grape]** **Conversions** on a multi-selection show the *intersection* of each file's targets, and each file is converted on its own. If the intersection is empty, show the empty-state message.
- **[Grape]** **Folders** only get the Actions wheel; Conversions and Tools show their empty state. Archives created from folders are v2.

## 8. Keyboard and touch (demo-only, informational)

The demo also supports Enter-to-open, arrow keys, Home/End and Space/Enter-to-apply, plus long-press on touch. These were **[observed] on the website only**. Whether the real app supports them in Finder is **unknown**: Enter in Finder renames a file. Grape v1 ignores this; consider a global hotkey in v2.

## 9. Things Grape must handle that the demo doesn't

- Drags that aren't files (text, images from browsers, URL drags): if no file paths arrive, close the wheel and don't block the drop.
- Huge selections (> 200 files): cap the wheel, show a count, and queue jobs.
- Read-only source folder: write outputs to the fallback location (Settings), then notify.
- Grape crashing mid-drag: the overlay must never stay up. A watchdog hides it after 30 s with no events.
