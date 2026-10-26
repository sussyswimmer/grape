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
 ├─(Esc)──────────────────────────────▶ Idle            (the source cancels the drag; no wheel until the button goes up)
 ├─(drag ends)────────────────────────▶ Idle            (OS handles the drop normally)
WheelOpen(mode)
 ├─(a different single modifier)──────▶ WheelOpen(new mode)   wheel re-renders in place
 ├─(cursor over a segment)────────────▶ highlight segment + play tick
 ├─(drop on a segment)────────────────▶ Running(job) ─▶ Idle
 ├─(drop on a panel tool)─────────────▶ Idle            (the wheel is done; the tool's panel opens, §11)
 ├─(drop off-ring / on empty wheel)───▶ Idle            (cancel, nothing happens)
 └─(Esc)──────────────────────────────▶ Idle            (overlay hides; on Windows and macOS the source cancels the drag)
```

## 2. Opening the wheel

- **[observed]** The wheel mode is chosen only when **exactly one** of Shift / Meta / Control (/ Alt tracked but unmapped) is held. Two or more modifiers at once mean "no mode", so nothing changes.
- **[observed]** Shift → conversions, Meta (Command) → tools, Control → actions.
- **[observed]** Pressing a *different* single modifier while the wheel is open swaps the wheel's contents. The wheel stays anchored where it was.
- **[observed]** The user can release the key and keep dragging. The wheel stays open until drop or cancel.
- **[Grape]** On Windows the defaults are Shift / Ctrl / Alt, because the Windows key opens the Start menu.
- **[Grape]** The wheel opens on **key-down** during an active drag. The user can tap (press and release) or keep holding; both work. Some apps, like Explorer, change the drag's drop effect while a modifier is held. That stops mattering because the drop then lands on Grape's overlay (see `PLATFORM_WINDOWS.md`).
- **[Grape]** Keys are remappable in Settings. The three wheels always use three different keys: choosing a key another wheel uses swaps the two (D19). Windows never offers the Windows key.
- **[Grape]** A key already held when the drag starts isn't a key-down: holding Shift before dragging keeps Explorer's "move" and opens no wheel. Releasing it and pressing it again during the drag does (`DECISIONS.md` D14).
- **[observed]** The wheel only counts as active after the pointer has moved more than 4 px since mouse-down. Grape uses `GetSystemMetrics(SM_CXDRAG/SM_CYDRAG)` on Windows. On macOS the drag counts as started once AppKit's own file drag session exists, which already needs the pointer to move (`PLATFORM_MACOS.md`).

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
- **[Grape]** Hit testing runs in `grape-core` (pure function, unit-tested) and honors the rounded outer corners itself, because the overlay receives no pointer events during an OS drag and can't re-check its drawn path. Each outer corner is rounded by a circle tangent to the gap edge and to the outer circle, with radius `min(CORNER, the largest that fits the segment)`. A pointer in the cut-off corner outside that circle selects nothing. The inner corners stay square. With one item the wheel is a full ring with no gap: any pointer with `INNER_R ≤ d ≤ OUTER_R` selects it. Segments too narrow to draw (their inner corners would cross, from 76 items with these constants) select nothing.

## 5. Feedback

- **[observed]** The hovered segment highlights. A center label shows the hovered item's title, or hides when nothing is hovered.
- **[observed]** Moving onto a *new* segment plays a short highlight sound. Leaving all segments cancels any pending sound.
- **[observed]** If the wheel has no items, it shows a message instead of segments:
  - Conversions or Actions empty: "No conversions for this format. Tap {actions key} for file actions."
  - Tools empty: "No editing tools for this format. Tap {conversions key} to convert or {actions key} for file actions."
  - **[Grape]** Use the user's *current* key names in these messages. Write the copy in our own words.
- **[Grape]** On macOS, also trigger a light trackpad haptic on segment change (`NSHapticFeedbackManager`). It has its own "haptics" setting (D19).
- **[Grape]** Honor Reduce Motion: no scale or opacity animation, the wheel just appears.

## 6. Dropping

- **[observed]** Drop on a highlighted segment → run that conversion, tool or action.
- **[observed]** Drop while the wheel is open but nothing is highlighted → cancel. **[Grape]** The overlay swallows the drop, so the file does *not* land in whatever app was underneath. That's why the overlay covers the whole display while the wheel is open (see `ARCHITECTURE.md`).
- **[Grape]** Esc hides the overlay. The drag sources Grape works with also cancel their drag on Esc (Explorer on Windows, AppKit on macOS), so the drag ends too and nothing is dropped (`DECISIONS.md` D14, D18). Esc before any wheel opens keeps Grape out of the way until the button goes up.
- **[Grape]** On macOS Grape can't read Esc without a permission prompt (D7). It sees the cancelled drag session as a `Leave` with the button still down and closes the wheel 200 ms later. That works while the cursor is over the wheel's display; elsewhere the ring stays until the button goes up. Esc before any wheel can't be seen there, so a key tap while still holding shows an empty ring that closes on the next move or on release (D18).
- **[Grape]** On macOS AppKit may refuse a drop while Command or Control is held. If the button goes up over a highlighted segment with either key held and no drop arrives within 750 ms, Grape runs that segment itself, once (D18).
- **[observed]** While a conversion runs, a placeholder shows "Making {name}…". When it finishes, a confirmation appears.
- **[Grape]** In the real OS, show this as a small toast near the anchor plus the OS's own file appearance. The demo's ≈ 680 ms is fake latency; real jobs report progress.
- **[Grape]** When a job left something out or removed metadata, the done toast's title says so ("2 links left out", "1 page drawn below 300 DPI", "Removed author and title", "No document metadata found") and the detail still names the output. A fallback that every output took, or a failure, takes the title instead; when only some outputs fell back, the detail says how many and the notes keep the title. Several outputs are "files", "folders" (Extract, Split) or "items" (mixed, or PDF → images, which may be either), and a PDF → images job is "Making doc…" while it runs, since one page gives `doc.jpg` and more a `doc` folder.
- **[Grape]** An error's words take up to two lines in the toast, so a reason such as "— it may be a scan." isn't cut off.
- **[Grape]** A drop this build can't run says "Not in this build yet" with "{FROM} to {TO} is coming." or "{Tool} on {FROM} is coming.". When a helper Grape ships is missing and is the only reason, it says "Grape's PDF library is missing" / "Reinstalling Grape brings it back." (D35).

## 7. Multiple files

- **[observed]** Tools depend on selection size. Some tools are hidden for multi-selections, and some only make sense with several files (Create PDF, Merge PDFs, Join Videos, Collage). See `TOOLS_AND_ACTIONS.md`.
- **[observed]** File actions work for folders, mixed selections, and files with no conversions.
- **[Grape]** **Conversions** on a multi-selection show the *intersection* of each file's targets, and each file is converted on its own. If the intersection is empty, show the empty-state message.
- **[Grape]** **Tools** on a multi-selection with mixed formats show the *intersection* of each format's tools. A tool must be enabled for every format in the selection, so in v1 a JPG plus a PDF drops Compress (it's v2 for PDF). If the intersection is empty, show the empty-state message.
- **[Grape]** **Folders** only get the Actions wheel; Conversions and Tools show their empty state. Archives created from folders are v2, but their targets and naming aren't specified yet, so folders stay Actions-only even with v2 enabled until they are.

## 8. Keyboard and touch (demo-only, informational)

The demo also supports Enter-to-open, arrow keys, Home/End and Space/Enter-to-apply, plus long-press on touch. These were **[observed] on the website only**. Whether the real app supports them in Finder is **unknown**: Enter in Finder renames a file. Grape v1 ignores this; consider a global hotkey in v2.

## 9. Things Grape must handle that the demo doesn't

- Drags that aren't files (text, images from browsers, URL drags): if no file paths arrive, close the wheel and don't block the drop.
- Huge selections (> 200 files): cap the wheel, show a count, and queue jobs.
- Read-only source folder: write outputs to the fallback location (Settings), then notify.
- Grape crashing mid-drag: the overlay must never stay up. A watchdog hides it after 30 s with no events.

## 10. The float shelf (Windows)

- **[Grape]** A drop on FLOAT puts the items on the shelf: a small panel at the right edge of the screen the drop was on, centered top to bottom. It opens without taking the keyboard from the app in front, and lists each item by name.
- **[Grape]** **Lifetime:** the shelf lives in memory. It stays until it's emptied or Grape quits, and quitting clears it.
- **[Grape]** **Adding to an open shelf:** another drop on FLOAT adds its items after the ones there, skipping any already on it. The shelf stays where it is and gets taller, up to six rows, then the list scrolls. It holds 50 items; the rest are left out, and a toast says the shelf is full.
- **[Grape]** **Closing:** a row's × (or Delete on a focused row) takes that item off. The header's × empties the shelf and closes it, and so does taking off the last item. Alt+F4 on the shelf (once a click made it the active window) is the header's ×: it empties the shelf and closes it. The shelf can't be minimized or maximized. None of this touches the files.
- **[Grape]** **Pin to an edge:** drag the shelf by its header. Let go within 48 px of the screen's left or right edge and it snaps flush to that edge; anywhere else it stays where you left it. It never goes past the screen's edges.
- **[Grape]** **Dragging out:** press an item, or "Drag all" from two items, and move. The drag is always a copy, or a shortcut where the target makes one, never a move, whatever keys are held. The files stay where they are, and the items stay on the shelf. An item that's gone shows "Missing" and can't be dragged. Esc during the drag cancels it. Double-click or Enter shows an item in Explorer; on an item that's gone it does nothing.
- **[Grape]** A drag out of the shelf never opens the wheel: Grape only watches drags that start in Explorer or on the Desktop (`DECISIONS.md` D14). Dropping anything onto the shelf does nothing.
- **[Grape]** On macOS FLOAT says "Not in this build yet" for now (D21).

## 11. Tool panels

- **[observed]** Some tools open a panel before they run (`REA_FINDINGS.md` I2; an inference from the demo's "opens UI first" set).
- **[Grape]** **Which tools:** a drop on Compress, Audio Channels, Normalize Volume or Merge PDFs opens a small panel; its Run starts the job with the chosen options. Remove Metadata, Mute, Split PDF, Create PDF and Extract Archive run at once, and conversions and file actions are unchanged (`TOOLS_AND_ACTIONS.md`, D31).
- **[Grape]** **A tool this build can't run opens no panel:** it shows the usual info toast instead ("Not in this build yet", §6). In this build Merge PDFs opens its panel when Grape's PDF library is found; Compress, Audio Channels and Normalize Volume wait for the media engine. For QA only, starting Grape with the environment variable `GRAPE_PREVIEW_PANELS=1` (exactly `1`) still opens those three; their Run meets the same check and shows the same toast, and nothing is written.
- **[Grape]** **Placement:** beside where the wheel was, on the same display, vertically centered on the wheel's center: right of the wheel's glass (142 logical px from its center), or left of it when there's no room on the right, or over it when the screen is too narrow for either; always inside the work area. 352 logical px wide, 224 to 430 tall. The drop point and the files around it stay uncovered.
- **[Grape]** **One at a time:** a new panel request replaces the open panel, which counts as cancelled. Losing the focus keeps it open: on Windows it starts unfocused, and a glance at Explorer or Finder must not lose a merge order. Run, Cancel, ×, Esc, Alt+F4, a new panel, Pause and Quit close it. Closing without Run runs nothing.
- **[Grape]** **Focus on Windows:** the panel opens without taking the keyboard; Explorer keeps it (Enter there still opens the dropped files), and the panel says "Click here to use the keyboard." The first click activates it. Until then, Esc closes it although Explorer has the keyboard, unless a mouse button is down or was just before (that Esc belongs to a drag). After that click Esc is the panel's own, while it has the keyboard: an Esc in another app (cancelling a rename in Explorer, closing a menu) is that app's, and the panel stays. Enter works only after the click. On close, an active panel hands the keyboard back to the window that was in front when it opened.
- **[Grape]** **Focus on macOS:** the panel comes to the front, becomes key, and Grape asks to be activated. If macOS grants it, Enter and Esc work at once; if not, the first click activates Grape and also reaches the control under it. Esc and Enter work only while the panel is key (D7: no key monitors); × and Cancel always work. An Esc the panel gets while a mouse button is down belongs to a drag (a file dragged out of a Finder window behind the panel can leave Grape active), so the panel stays open. On close, the app that was in front gets activation back, if the panel was key.
- **[Grape]** **Keys in the panel:** Enter runs (on a button, the button does its own thing: Enter on Cancel cancels); Esc cancels (during a pointer reorder it only aborts the reorder, and while a mouse button is down Rust refuses it, so the panel stays and takes keys again); Tab and Shift+Tab cycle inside the panel: × → the main control → Move up → Move down (Merge) → Cancel → Run.
- **[Grape]** **A drag while a panel is open:** the next drag still opens the wheel, on top of the panel (on Windows the overlay is lifted to the top of the topmost windows on every show; on macOS the panel's level is under the toast's and the overlay's). The panel stays open; a new panel-tool drop replaces it. Pause closes it and nothing runs; while paused no wheel opens, so no panel does. Quit forgets it, so a late Run is ignored.
- **[Grape]** **Contents** (Grape's own words; the page has the exact copy):
  - **Compress:** a three-way segmented control, Small / Balanced / High, with a note on what the selected one means. It starts at the last preset that ran (Balanced at first).
  - **Audio Channels:** Mono / Stereo, starting at Mono every time (the right choice depends on the file).
  - **Normalize Volume:** a target loudness slider from −24 to −10 LUFS in steps of 1, starting at the last target that ran (−16 LUFS at first).
  - **Merge PDFs:** the files in a reorderable list (pointer drag, Alt+↑/↓ on the selected row, Move up and Move down), with "Saves as {name}" for the merged file.
  - Compress and Normalize remember a choice only when its Run is accepted (`tools.compress`, `tools.normalize_lufs`, `ARCHITECTURE.md` → Settings).
- **[Grape]** **Merge order:** the list starts in the drag's order, which Explorer and Finder don't document. The first file in the list names the merged PDF ("{first} Merged.pdf") and picks its folder. The panel shows the plain name even when the output will get " 2".
- **[Grape]** **Drops on the panel do nothing:** on Windows it refuses them (D13); on macOS a file dropped on it is accepted as a copy and ignored (D18). It's never moved.
- **[Grape]** The page only ever sees a request number, item ids and display names, never a path (`ARCHITECTURE.md`).
