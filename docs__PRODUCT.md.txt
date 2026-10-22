# Grape — Product

## One line

Drag a file, tap a key, drop on a wheel segment, and the file is converted next to the original, offline.

## Price

**Free and open source** (`DECISIONS.md` D2). There's no quota, no account and no paid tier. Unlike Tangerine, which has a "conversion allowance".

## Who it's for

People who convert files often and hate online converters: students, creators, devs, anyone with screenshots, recordings and PDFs. They want the conversion to happen inside the drag they were already making. No app to open, no upload, no save dialog.

## The three wheels

Each wheel opens when the user taps one modifier key mid-drag. Keys are remappable in Settings.

| Wheel            | macOS key | Windows key | What it holds                                                                       |
|------------------|-----------|-------------|-------------------------------------------------------------------------------------|
| **Conversions**  | Shift     | Shift       | Target formats for the dragged file, e.g. PNG → JPG/WebP/HEIC/…/PDF                  |
| **Tools**        | Command   | Ctrl        | Edits like Compress, Remove Metadata, Trim, Crop, Split/Merge PDF                    |
| **File actions** | Control   | Alt         | Share (AirDrop or Nearby Share), Float, Copy, Move, Copy Path, Terminal              |

Full behavior is in `INTERACTION_SPEC.md`. Formats are in `FORMAT_MATRIX.md`, and tools and actions are in `TOOLS_AND_ACTIONS.md`.

## Parity target (Tangerine, as advertised and as observed in its demo)

- **275** conversion pairs across 45 input formats. Verified by reproducing the demo's matrix; see `REA_FINDINGS.md`.
- **26** advanced file tools.
- **6** file actions.
- 100% local processing.

## Scope

**v1 ("core")**
- Conversions: images, audio, video (+ GIF, + MP3 extraction), subtitles, archives (ZIP/TAR/GZIP, RAR input), PDF → JPG/PNG/TXT/MD, images → PDF.
- Tools: Compress, Remove Metadata, Mute (remove audio), Normalize Volume, Audio Channels (mono), Extract Archive, Split PDF, Merge PDFs, Create PDF.
- All 6 file actions.
- Settings: key remapping, output location, launch at login, sounds on/off.

**v2 ("documents + editors")**
- DOCX/MD/TXT/RTF/HTML/EPUB conversions, spreadsheets, slides → PDF/images, PDF → DOCX/PPTX/XLSX/EPUB.
- Tools that need an editor panel: Trim (audio/video), Crop (image/video), Edit Photo, Annotate, Add BG, Redact (image/video), Bleep, Speed, Snapshots, Split Video, Join Videos, Collage, Organize PDF, Audio Visualizer.

**Later / unknown feasibility**
- Apple **Pages** read/write. No open library exists, and how Tangerine does it is unknown. See `DECISIONS.md` O2.

## Non-goals

- Cloud features, accounts or sync.
- A full editor app. Editor panels are small and single-purpose.
- Linux, for now.

## Success criteria for v1

- From key tap to a fully drawn wheel takes under 100 ms.
- A typical image conversion (12 MP JPG → PNG) finishes in under 1 s on Apple silicon and a mid-range Windows laptop.
- Zero source files are modified across the full QA run in `TESTING.md`.
- If Grape fails to detect a drag, the user's normal drop still works.

## Branding

- **Name:** Grape. The accent color is a deep grape purple. The wheel uses frosted glass in Grape's own palette. The highlight sound and iconography are original.
- **Don't** mimic Tangerine's orange, layout or copy. See golden rule 1 in `CLAUDE.md`.
