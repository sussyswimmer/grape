# Testing

## Layers

| Layer | Where | Runs in CI | What |
|---|---|---|---|
| Unit | `grape-core` | always | matrix counts, naming, tools table, geometry/hit test, planning |
| Engine integration | `grape-engines/tests` (feature `engines-it`) | macOS + Windows jobs | convert each fixture through each v1 pair, then validate the output |
| Platform | `grape-platform` | build only | compile both `cfg`s; logic behind traits gets mock tests |
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

In Phase 1 (Windows first), run every unmarked item and every **win:** item on the Windows PC. Skip the **mac:** items until M8.

App shell
- [ ] win: the tray menu shows Settings…, Pause Grape and Quit. Settings opens the settings window; Pause toggles its check mark and the tooltip.
- [ ] win: Quit ends the process (no Grape process left in Task Manager).
- [ ] win: the overlay never gets a taskbar button or an Alt-Tab entry (debug build: tray → "Debug: flash overlay").
- [ ] win: a second launch adds no second tray icon; it opens Settings in the running instance.
- [ ] win: the debug overlay flash doesn't steal focus (the window you were typing in keeps the keyboard), and the log shows the overlay's ex-style with `TOOLWINDOW|NOACTIVATE|TOPMOST`.

Drag + wheel, M1 spike (win; use scratch copies of files)
- [ ] win: drag a PNG from Explorer, tap Shift → a ring appears at the cursor, then fills in with PNG's targets (JPG, WEBP, HEIC, TIFF, AVIF, BMP, PDF; JPG at the top). The log shows `wheel 1 open: Conversions at (x, y)…`, `1 item(s) entered; 7 segment(s)` and `overlay drop target on N of M window(s)`.
- [ ] win: hover the segments → each one highlights with a soft tick, and the middle shows its title. Drop on JPG (the top) → the log shows `drop on the Conversions wheel: segment 0 (Convert(Jpg)) …` with the file's full path, a toast under the wheel says "Not in this build yet / PNG to JPG is coming." and fades after a few seconds, and the file is still where it was.
- [ ] win: the toast never takes the keyboard from the window you were in, has no taskbar or Alt-Tab entry, and dropping a file on it does nothing.
- [ ] win: tap Ctrl, then Alt, mid-drag → the wheel swaps in place to the tools (COMPRESS, METADATA) and then the six file actions.
- [ ] win: drag a folder and tap Shift → no segments; the wheel says there's nothing to convert and names the Alt key for file actions.
- [ ] win: drop in the middle circle or off the ring, including far from the wheel on the same monitor → the log says `no segment (cancel)`, nothing moves, and the file doesn't land in the window underneath.
- [ ] win: drop a scratch file onto the Settings window with Shift held → nothing happens (no drop cursor), and the file stays where it was.
- [ ] win: Esc mid-drag → the ring hides; Explorer cancels its drag too. The file is untouched.
- [ ] win: hold Shift *before* starting the drag → no ring, and dropping into another folder moves the scratch file as Explorer always does.
- [ ] win: drag from a browser, an editor or the taskbar → no ring.
- [ ] win: the log's `painted N ms after the key` stays under 100 ms.
- [ ] win: the log never shows `wry's drop handler saw` (that would mean Grape's drop target missed a window).

Drag + wheel
- [ ] Drag a file from Finder/Explorer → tap the conversions key → the wheel appears at the cursor, clamped on screen.
- [ ] Switch keys mid-drag → the wheel swaps contents in place.
- [ ] Release the key, keep dragging → the wheel stays.
- [ ] Hover segments → highlight, center label, tick (and haptic on mac).
- [ ] Drop on a segment → output appears next to the source with the right name.
- [ ] Drop in the dead zone, a gap, or off-ring → nothing happens; the source didn't move.
- [ ] Esc → the wheel hides; finishing the drop on a folder works normally.
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
