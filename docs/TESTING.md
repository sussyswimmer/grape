# Testing

## Layers

| Layer | Where | Runs in CI | What |
|---|---|---|---|
| Unit | `grape-core` | always | matrix counts, naming, tools table, geometry/hit test, planning |
| Engine integration | `grape-engines/tests` (feature `engines-it`) | macOS + Windows jobs | convert each fixture through each v1 pair, then validate the output |
| Platform | `grape-platform` | Windows + macOS jobs | lint both `cfg`s; logic behind traits gets mock tests; clipboard and move tests run on the real OS (never a dialog or AirDrop) |
| UI | `apps/desktop/ui` (Vitest) | always | segment path generation, label placement, empty states, key-name substitution |
| Manual QA | checklist below | before every release | real drags on real OSes |

## Fixtures
- `fixtures/` holds one small, license-free file per input format (≤ 200 KB each; generate media with FFmpeg's `testsrc`/`sine`). Add a `fixtures/README.md` listing each file's origin.
- Fixtures with metadata: a JPEG with GPS EXIF, an MP3 with ID3 tags, and a PDF with Author/Title, used to test Remove Metadata.
- Tricky names: spaces, unicode (`ảnh chụp.jpg`), `.tar.gz`, no extension, uppercase extension, and an existing `name 2.png` to test collisions.

## Output validation (engine tests)
- The output exists, is non-empty, and has the expected extension.
- `ffprobe` / image decode / pdfium open succeeds, and the format matches the target, checked by magic bytes and not just the extension.
- Dimensions or duration are preserved within tolerance, unless a tool changes them on purpose.
- **The source's bytes, mtime and path are unchanged.** Hash before and after. This assertion is in *every* engine test.
- No temp files are left behind after success, failure or cancel.

## Manual QA checklist (both OSes unless marked)

In Phase 1 (Windows first), run every unmarked item and every **win:** item on the Windows PC. The **mac:** items need a Mac with the DMG from CI (`PLATFORM_MACOS.md` → "Running a CI build"); the Mac port started early (D18).

App shell
- [ ] win: the tray menu shows Settings…, Pause Grape and Quit. Settings opens the settings window; Pause toggles its check mark and the tooltip.
- [ ] win: Quit ends the process (no Grape process left in Task Manager).
- [ ] win: the overlay never gets a taskbar button or an Alt-Tab entry (debug build: tray → "Debug: flash overlay").
- [ ] win: a second launch adds no second tray icon; it opens Settings in the running instance.
- [ ] win: the debug overlay flash doesn't steal focus (the window you were typing in keeps the keyboard), and the log shows the overlay's ex-style with `TOOLWINDOW|NOACTIVATE|TOPMOST`.

Drag + wheel, M1 spike (win; use scratch copies of files)
- [ ] win: drag a PNG from Explorer, tap Shift → a ring appears at the cursor, then fills in with PNG's targets (JPG, WEBP, HEIC, TIFF, AVIF, BMP, PDF; JPG at the top). The log shows `wheel 1 open: Conversions at (x, y)…`, `1 item(s) entered; 7 segment(s)` and `overlay drop target on N of M window(s)`.
- [ ] win: hover the segments → each one highlights with a soft tick, and the middle shows its title. Drop on JPG (the top) → the log shows `drop on the Conversions wheel: segment 0 (Convert(Jpg)) …` with the file's full path, and a toast under the wheel says "Making photo.jpg…", then "Done / photo.jpg", and fades after a few seconds. The original is still where it was (the conversion checks are in the next section).
- [ ] win: the toast never takes the keyboard from the window you were in, has no taskbar or Alt-Tab entry, and dropping a file on it does nothing.
- [ ] win: tap Ctrl, then Alt, mid-drag → the wheel swaps in place to the tools (COMPRESS, METADATA) and then the six file actions.
- [ ] win: drag a folder and tap Shift → no segments; the wheel says there's nothing to convert and names the Alt key for file actions.
- [ ] win: drop in the middle circle or off the ring, including far from the wheel on the same monitor → the log says `no segment (cancel)`, nothing moves, and the file doesn't land in the window underneath.
- [ ] win: drop a scratch file onto the Settings window with Shift held → nothing happens (no drop cursor), and the file stays where it was.
- [ ] win: Esc mid-drag → the ring hides; Explorer cancels its drag too. The file is untouched.
- [ ] win: press Esc mid-drag before tapping any key, keep holding and tap Shift → no ring.
- [ ] win: hold Shift *before* starting the drag → no ring, and dropping into another folder moves the scratch file as Explorer always does.
- [ ] win: drag from a browser, an editor or the taskbar → no ring.
- [ ] win: the log's `painted N ms after the key` stays under 100 ms.
- [ ] win: the log never shows `wry's drop handler saw` (that would mean Grape's drop target missed a window).

Wheel look and image conversions, M3 first slice (win; use scratch copies of files)
- [ ] win: switch Windows between light and dark mode → the wheel's glass, segments and labels and the toast follow the theme. No colour looks borrowed from another app.
- [ ] win: Settings → Accessibility → Visual effects → Animation effects off → the wheel appears with no scale or fade, and the working toast's dot doesn't pulse.
- [ ] win: the tick is audible on the very first wheel after launch, with no click first. If it's silent, nothing else breaks.
- [ ] win: hover the rounded outer corners of a segment → the highlight ends where the drawn corner ends; dropping in the cut-off corner area selects nothing (the log says `no segment (cancel)`).
- [ ] win: a selection with 7 or more segments uses the smaller label style, and labels stay inside their segments.
- The app runs `grape-engines` jobs for the 20 raster pairs; every other conversion and every tool shows an info toast instead:
- [ ] win: Shift-drop `photo.png` on JPG → the toast reads "Making photo.jpg…" then "Done / photo.jpg"; `photo.jpg` appears next to the original and opens in Photos. `Get-FileHash` of `photo.png` is the same before and after.
- [ ] win: do it again → `photo 2.jpg`; the first `photo.jpg` is untouched.
- [ ] win: convert between every pair of JPG, PNG, WEBP, TIFF and BMP (20 pairs) → each output opens and has the same width and height.
- [ ] win: a PNG with a transparent background → JPG has a white background. A phone photo that displays rotated → its PNG is upright.
- [ ] win: drop on HEIC, AVIF or PDF → an info toast says it isn't in this build yet, and nothing is written.
- [ ] win: a source folder you can't write to → the output lands in `Downloads\Grape` (created only now, if it didn't exist), and the toast's title says "Saved in Downloads\Grape".
- [ ] win: with hidden files shown, no `.grape-tmp-…` or `.grape-probe-…` file is left in any folder you converted in, also after a failed or cancelled conversion.

File actions, M5 (win; use scratch copies of files)
- [ ] win: drag two scratch files, tap Alt and drop on COPY → a toast says "Copied 2 items". Ctrl+V in another folder → copies appear there, and the originals are still where they were.
- [ ] win: Alt-drop one file on COPY PATH → the toast says "Copied the path" and shows it. Paste into Notepad → the full path, unquoted. With three files → three lines.
- [ ] win: with clipboard history on (Win+V), `cargo test -p grape-platform` passes a few runs in a row (the clipboard tests retry while the history reads what they wrote).
- [ ] win: Alt-drop a file on TERMINAL → Windows Terminal opens in the file's folder. Do the same with a folder → it opens in that folder. Without Windows Terminal, or with a `;` in the folder name, PowerShell opens there instead.
- [ ] win: Alt-drop two scratch files on MOVE → a folder picker titled "Move 2 items to…" opens in front of Explorer, starting in their folder. (If it only flashes in the taskbar, note it.) Pick another folder → both move there, the toast says "Moved 2 items / to {folder}", and clicking the toast shows the first one selected in Explorer.
- [ ] win: right after a Move, Ctrl+Z in Explorer → note whether the files come back. Grape asks the shell for an undo record (`FOFX_ADDUNDORECORD`), but nobody has checked yet whether Explorer's undo picks it up.
- [ ] win: Move, then close the picker → nothing moves and no toast appears.
- [ ] win: Move into the folder the files are already in → the toast says "Already there", and nothing moves.
- [ ] win: Move onto a folder that already has a file with the same name → Windows asks what to do. Skip → that file stays put, and the toast says "Moved 1 of 2 items". Keep both → the toast says the move is done, and clicking it selects the renamed file ("name (2)"), or at least opens the folder. It must never select the file that was already there.
- [ ] win: Move a folder into a folder that already holds a folder of the same name with one clashing file, and Skip the clash → the toast says "Some of it may not have moved" and shows the destination. It never says "Nothing moved".
- [ ] win: Move a junction to a folder on another drive, then cancel part-way → the toast says "Some of it may not have moved" (or nothing moved, and no toast appears if you cancelled before anything copied).
- [ ] win: Move into a folder whose name has a space and a comma (`a b,c`), then click the toast → Explorer opens that folder with the moved file selected.
- [ ] win: make a junction to another drive in PowerShell (`New-Item -ItemType Junction -Path C:\scratch\other -Target D:\scratch`), then Move a file from C: into `C:\scratch\other` → Grape asks "Move to another drive?" first.
- [ ] win: Move to a folder on another drive (a USB stick) → Grape first asks "Move to another drive?" with No as the default. No → nothing moves. Yes → the items move.
- [ ] win: Share and Float → a "Not in this build yet" toast, and nothing else happens.

macOS, first build (D18; mac; use scratch copies of files)
- [ ] mac: Grape shows only in the menu bar: no Dock icon, no Cmd-Tab entry. Opening Grape.app again from Applications opens Settings.
- [ ] mac: no permission prompt appears at launch or during a drag (no Accessibility, Input Monitoring, Screen Recording or "paste from" prompt). Note any "access files in your Desktop/Documents folder" prompt and what triggered it.
- [ ] mac: drag a PNG from Finder, tap Shift → a ring appears at the cursor, then fills in with PNG's targets. The log (`~/Library/Logs/io.github.sussyswimmer.grape/`) shows `wheel 1 open…`, `1 item(s) entered` and the overlay's `level 101`.
- [ ] mac: hover the segments → each highlights with a tick, and a Force Touch trackpad gives a light haptic.
- [ ] mac: after the wheel closes, Finder is still the active app (its menu bar still shows), and the ring never covers the dragged file's image.
- [ ] mac: Shift-drop on JPG → "Making photo.jpg…" then "Done"; `photo.jpg` is next to the original, and Finder didn't move or copy the original anywhere. One click on the toast → Finder shows `photo.jpg` selected.
- [ ] mac: hold Command, drop on a tool, then hold Control and drop on COPY → each runs exactly once. Note whether the log says it ran "on release because no drop arrived" (AppKit refused the drop) or shows a normal drop.
- [ ] mac: Esc mid-drag → the ring hides within about a quarter second, Finder cancels its drag, and the file is untouched. Moving on and releasing reopens nothing.
- [ ] mac: drop in the middle or off the ring, including over a Finder window under the overlay → nothing happens, and the file doesn't land in that window.
- [ ] mac: drag selected text from Safari or TextEdit, or an image from Safari, and tap Shift → no ring. Drag a window by its title bar with Shift held → no ring.
- [ ] mac: a quick flick: press on a file, flick it and stop, then tap Shift while the pointer rests → the ring opens without another move.
- [ ] mac: during a long conversion (a big TIFF, or several images at once), start another drag and open a wheel over the toast → the wheel covers the toast, stays open, and the segment under the toast highlights.
- [ ] mac: hold Control, highlight COPY, press Esc and let go at once → the wheel closes and the clipboard is unchanged.
- [ ] mac: press Esc mid-drag before tapping any key, keep holding and tap Shift → either no ring, or an empty ring that closes when you move or let go. Nothing is dropped.
- [ ] mac: open the wheel on one display, drag onto another and press Esc → the drag cancels, and the ring closes when you let go.
- [ ] mac: hold Command and release in the middle or off the ring → nothing runs, the wheel closes, and the file stays where it was.
- [ ] mac: open the wheel on one display and drag onto another → the wheel stays where it was with nothing highlighted and doesn't close; dragging back highlights segments again.
- [ ] mac: drag slowly around the open wheel, including over the menu bar and the Dock → it never closes by itself.
- [ ] mac: two displays (one Retina, one not; also with the second one left of or above the main one) → the ring appears at the cursor on each, and the highlighted segment is the one under the cursor.
- [ ] mac: a full-screen app in front, another Space, and Stage Manager on → the ring appears over it and drops work.
- [ ] mac: Control-drop two files on COPY → "Copied 2 items"; Cmd+V in a Finder window pastes copies. COPY PATH → pasting into TextEdit gives two lines.
- [ ] mac: Control-drop a file on TERMINAL → Terminal opens in its folder, also for a folder named `a b "c" $(whoami)`.
- [ ] mac: Control-drop two files on MOVE → a "Move 2 items to…" folder picker comes to the front (note if it opens behind Finder). Pick a folder → both move, and the toast says "Moved 2 items". Close the picker → nothing moves and no toast.
- [ ] mac: MOVE onto a folder holding a file with the same name → that file stays where it was, nothing is overwritten, and the toast says "Moved 1 of 2 items".
- [ ] mac: MOVE to a USB stick → "Move to another drive?" with Cancel as the default (Return cancels). Move → the items move.
- [ ] mac: Control-drop on SHARE → the AirDrop window comes to the front with the files (note if it opens behind Finder). Close it or send → Finder is active again.
- [ ] mac: after a Move, and after cancelling one, the Finder window you dragged from is active again: arrow keys move its selection without clicking first.
- [ ] mac: drag a USB stick's or a mounted DMG's icon from the Desktop onto MOVE → it isn't moved and nothing on it is erased; the toast says nothing moved.
- [ ] mac: Move a large folder to a USB stick and pull the stick out halfway → the toast says "Couldn't move" or "Some of it may not have moved", never "Moved …", and the original folder is still complete in its old place.
- [ ] mac (needs Xcode): quit Grape, then start it from Terminal with Xcode's Main Thread Checker: `DYLD_INSERT_LIBRARIES=/Applications/Xcode.app/Contents/Developer/usr/lib/libMainThreadChecker.dylib` followed by the path of the Grape executable in `Grape.app/Contents/MacOS/`. Drag files for a minute → Terminal shows no "Main Thread Checker: UI API called on a background thread" report.
- [ ] mac: Quit from the menu bar → no Grape process is left in Activity Monitor.

Drag + wheel
- [ ] Drag a file from Finder/Explorer → tap the conversions key → the wheel appears at the cursor, clamped on screen.
- [ ] Switch keys mid-drag → the wheel swaps contents in place.
- [ ] Release the key, keep dragging → the wheel stays.
- [ ] Hover segments → highlight, center label, tick (and haptic on mac).
- [ ] Drop on a segment → output appears next to the source with the right name.
- [ ] Drop in the dead zone, a gap, or off-ring → nothing happens; the source didn't move.
- [ ] Esc → the wheel hides and the drag is cancelled; the file stays where it was.
- [ ] Two modifiers held at once → no mode change.
- [ ] Non-file drag (selected text, a browser image) → no wheel, or the wheel closes harmlessly.
- [ ] Multi-select of 3 JPGs → Tools shows Create PDF (and Collage, which is v2-only, once v2 tools are enabled); Conversions converts all three.
- [ ] Folder → only Actions has items; the others show empty states naming the right keys.
- [ ] Second monitor at a different scale → correct position and hit testing.
- [ ] mac: full-screen app in front; Stage Manager on.
- [ ] win: **Shift-drop never deletes the source** (repeat 10×, including dragging from a different drive).
- [ ] win: elevated Explorer → Grape stays out of the way.
- [ ] win: OneDrive cloud-only file → hydrates or fails gracefully, never hangs.

Actions
- [ ] Share opens AirDrop (mac) or the Share sheet (win) with the files.
- [ ] Float: the shelf appears; dragging out of it into another app works; pin to edge.
- [ ] Copy → paste in Finder/Explorer creates copies.
- [ ] Move → moves to the chosen folder; undo works on Windows.
- [ ] Copy Path → correct absolute paths, one per line.
- [ ] Terminal → opens at the folder (the parent for files).

Robustness
- [ ] Kill Grape mid-drag → the overlay disappears with the process; the system drag is unaffected.
- [ ] 300-file selection → capped wheel, queued jobs, responsive UI.
- [ ] Read-only source folder → output goes to the fallback folder, with a toast explaining it.
- [ ] Pause Grape from the tray → drags behave as if Grape weren't installed.
