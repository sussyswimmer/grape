# Tools (26) and file actions (6)

Keys, titles and which formats get which tool are **[observed]** (see `REA_FINDINGS.md`). Descriptions, engines, tiers and Windows behavior are **[Grape]**. Write all UI copy in our own words.

**Icons:** use Lucide (ISC license) or our own SVGs. Don't use SF Symbols: Apple's license limits them to Apple platforms, and we ship on Windows too. Tangerine's demo references SF Symbol names, which tells us nothing we need.

## Tool flags [observed]

- **Panel** = the tool is in the demo's "opens UI first" set, so it shows a small options or editor panel before running. That this set means "opens a panel" is an *inference*; the demo only shows a description card.
- **Combine** = the tool turns many inputs into one output (createPDF, mergePDF, joinVideos, createCollage).
- **Multi-OK** = the tool is allowed when more than one file is selected. See the rule in `FORMAT_MATRIX.md`.

## Tools

| Key | Wheel title | Name | Panel | Combine | Multi-OK | Tier | Engine (see ENGINES.md) |
|---|---|---|---|---|---|---|---|
| compress | COMPRESS | Compress | ✓ |  | ✓ | v1 (img/audio/video), v2 (pdf) | image (JPG, PNG) / FFmpeg (audio, video) / lopdf (v2) |
| removeMetadata | METADATA | Remove Metadata | ✓ |  | ✓ | v1 | container strip (JPG PNG WebP GIF TIFF HEIC) / FFmpeg `-map_metadata -1 -c copy` / lopdf |
| editImage | EDIT | Edit Photo | ✓ |  |  | v2 | image pipeline |
| annotateImage | ANNOTATE | Annotate Photo | ✓ |  |  | v2 | canvas in UI → PNG |
| frameImage | ADD BG | Add Background | ✓ |  |  | v2 | image pipeline |
| cropImage | CROP | Crop Image | ✓ |  | ✓ | v2 | image pipeline |
| redactImage | REDACT | Redact Photo | ✓ |  |  | v2 | image pipeline |
| normalizeAudio | NORMALIZE | Normalize Volume | ✓ |  | ✓ | v1 | FFmpeg `ebur128` + `volume` (+ oversampled `alimiter`) |
| trimAudio | TRIM | Trim Audio | ✓ |  |  | v2 | FFmpeg |
| audioChannels | CHANNELS | Audio Channels | ✓ |  |  | v1 | FFmpeg `pan` / `aresample` downmix |
| audioToVideo | VISUALIZE | Audio Visualizer | ✓ |  |  | v2 | FFmpeg `showwaves`/`showfreqs` |
| redactAudio | BLEEP | Bleep Audio | ✓ |  |  | v2 | FFmpeg `volume`+`sine` over ranges |
| muteVideo | MUTE | Remove Audio |  |  | ✓ | v1 | FFmpeg stream copy without the audio |
| trimVideo | TRIM | Trim Video | ✓ |  |  | v2 | FFmpeg (stream copy when keyframe-aligned) |
| cropVideo | CROP | Crop Video | ✓ |  |  | v2 | FFmpeg `crop` |
| changeVideoSpeed | SPEED | Change Speed | ✓ |  |  | v2 | FFmpeg `setpts` + `atempo` |
| videoSnapshots | SNAPSHOT | Video Snapshots | ✓ |  |  | v2 | FFmpeg frame extract → PNG |
| splitVideo | SPLIT | Split Video | ✓ |  |  | v2 | FFmpeg segment |
| joinVideos | JOIN | Join Videos | ✓ | ✓ | ✓ | v2 | FFmpeg concat |
| redactVideo | REDACT | Redact Video | ✓ |  |  | v2 | FFmpeg `boxblur`/`drawbox` over regions |
| splitPDF | SPLIT | Split PDF |  |  | ✓ | v1 | lopdf → one PDF per page in a folder |
| organizePDF | ORGANIZE | Organize PDF | ✓ |  |  | v2 | pdfium thumbnails + lopdf |
| mergePDF | MERGE PDF | Merge PDFs | ✓ | ✓ | ✓ | v1 | lopdf (panel = reorder list) |
| createPDF | CREATE PDF | Create PDF |  | ✓ | ✓ | v1 | Grape's own PDF writer: one page per image in selection order, mixed formats OK (JPG, PNG, WebP, TIFF; HEIC with Grape's image helper; SVG (a page of its own size, drawn at 300 DPI; Grape's SVG worker, D39); never AVIF, which has no tools in v1, D41). A file Grape can't read stops the whole PDF, and the toast names it (D37) |
| createCollage | COLLAGE | Create Collage | ✓ | ✓ | ✓ | v2 | image pipeline |
| extractArchive | EXTRACT | Extract Archive |  |  | ✓ | v1 | zip / tar / flate2; RAR through Grape's bsdtar (D27) |

26 tools ✓. This matches Tangerine's advertised count.

**[Grape] v1 panels.** Four tools open a small panel before they run; the rest run at once (`INTERACTION_SPEC.md` §11, `DECISIONS.md` D31):
- **Compress** is a three-way segmented control, Small / Balanced / High, not a slider: three named choices, all visible, one click each. It remembers the last preset that ran (`tools.compress`, Balanced at first). What each preset means per media is Grape's own choice (REA U6); lane B records the numbers in `ENGINES.md`.
- **Audio Channels** is Mono / Stereo, starting at Mono every time.
- **Normalize Volume** is a target loudness slider from −24 to −10 LUFS in steps of 1, −16 LUFS by default, and remembers the last target that ran (`tools.normalize_lufs`).
- **Merge PDFs** is an order list (pointer drag, Alt+↑/↓, Move up and Move down) showing the merged file's name; the first file names it and picks its folder. Its panel opens once Grape's PDF library is found (D35).
- **Remove Metadata** runs at once (although the table marks it Panel). Its toast says what it removed ("Removed author, title and 3 more", or "No document metadata found": page-level XMP and the images' own metadata stay, ENGINES.md → Metadata removal); on PDFs, on JPG, PNG, WebP, GIF, TIFF and HEIC (no helper needed; TIFF and HEIC D28), and on audio and video (MP3, M4A, WAV, FLAC, MP4, MOV, MKV; D26); images and media say "No metadata found" when they had none.

## File actions

Actions work on any selection: folders, mixed types, files with no conversions **[observed]**. **[observed]** Tangerine's copy says actions don't count against its "conversion allowance". That points to a usage quota. Grape is free with no quota (`DECISIONS.md` D2), so this doesn't apply.

| Key | Title | macOS | Windows | Tier |
|---|---|---|---|---|
| share | AIRDROP → **SHARE** on Windows | `NSSharingService(named: .sendViaAirDrop)` with the file URLs | Windows share sheet (`IDataTransferManagerInterop::ShowShareUIForWindow`, owned by an invisible window of Grape's) → Nearby Share and apps. Files only: folders, shortcuts and missing items are left out, and a toast says so (D21) | v1 |
| float | FLOAT | Floating shelf panel that can be pinned to a screen edge; files can be dragged *out* of it into other apps. The same shelf as on Windows, at the floating level under Grape's other windows, opened without taking focus; items drag out with an AppKit dragging session as a copy or an alias, never a move (D43) | A topmost tool window at the right edge of the screen that opens without taking focus. It holds up to 50 items and snaps to the left or right edge when left near one. Items drag out with OLE as a copy or a shortcut, never a move (D21) | v1 |
| copy | COPY | File URLs on `NSPasteboard.general` (paste in Finder copies files) | `CF_HDROP` + `Preferred DropEffect = COPY` on the clipboard | v1 |
| move | MOVE | `NSOpenPanel` (folders only) → move originals. **The only action allowed to move a source.** | `IFileOpenDialog` with `FOS_PICKFOLDERS` → `MoveFileExW` / `IFileOperation` (undo-able) | v1 |
| copyPath | COPY PATH | Absolute POSIX paths, one per line | Absolute Windows paths, one per line, unquoted | v1 |
| terminal | TERMINAL | Open the folder (or a file's parent) in Terminal; iTerm2 if the user picked it in Settings | Windows Terminal `wt -d <dir>` if installed, else PowerShell with `-NoExit -Command Set-Location` | v1 |

Move must use the platform's undo-capable API where one exists, and must refuse to move across volumes without confirming.

**Float never moves.** A drag out of the shelf offers copy or shortcut only, whatever keys are held, so the files stay where they are. Remove (× or Delete) and Close only take items off the shelf; nothing on disk changes.

## Adding a tool or action

Use `/add-conversion` for formats. For tools:
1. Add the key in `grape-core`.
2. Add it to the per-format tables in `FORMAT_MATRIX.md` and here.
3. Add a naming suffix.
4. Implement it in `grape-engines`.
5. Add a fixture test.
6. Add a QA line in `TESTING.md`.
