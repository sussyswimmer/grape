# Format matrix, tools per format, output naming

The source of truth for *what Grape can do to which file*. `grape-core` encodes it, and its tests assert the totals below.

**Provenance:** reproduced from the conversion function in Tangerine's demo script (`REA_FINDINGS.md`, artifact sha256 `24a7b6d5…86d3fd`). The reproduction yields exactly **275 pairs across 45 inputs**, which matches the "275 conversion options" Tangerine advertises. The tier column (v1 / v2 / later) is **Grape's** roadmap choice and was not observed.

## Rules [observed]

1. Each input format maps to a fixed target list (tables below). A format never converts to itself.
2. **Input-only formats** are never offered as targets: `rar`, `ppt`, `xls`, `rtf`, `rtfd`, `textClipping`.
3. Formats are identified by the lowercased extension. `.tgz` / `.tar.gz` are treated as `gz` for naming. **[Grape]** Sniff magic bytes when the extension is missing or lies, and use `jpeg` → `jpg`, `tif` → `tiff`, `aif` → `aiff`, `htm` → `html`, `markdown` → `md`.
4. Display names are uppercase extensions, except `gz` displays as **GZIP**.
5. Converting to `gz` writes `name.tar.gz`. A single file or folder is tarred first.

## Category of an input [observed]

audio → `mp3 m4a wav flac ogg opus aiff wma` · video → `mp4 mov mkv webm avi wmv gif` · document → `pdf ppt pptx docx pages md rtf rtfd textClipping txt html epub` · spreadsheet → `xlsx xls ods csv tsv` · archive → `zip tar gz rar` · subtitle → `srt vtt` · image → `jpg png webp heic tiff svg avif bmp`

**[Grape]** Category drives the icon, the job runner and which engine is used.

## Conversion targets

### Images

| Input | # | Targets | Tier |
|---|---|---|---|
| `jpg` | 7 | png webp heic tiff avif bmp pdf | v1 |
| `png` | 7 | jpg webp heic tiff avif bmp pdf | v1 |
| `webp` | 7 | jpg png heic tiff avif bmp pdf | v1 |
| `heic` | 7 | jpg png webp tiff avif bmp pdf | v1 |
| `tiff` | 7 | jpg png webp heic avif bmp pdf | v1 |
| `svg` | 8 | jpg png webp heic tiff avif bmp pdf | v1 |
| `avif` | 7 | jpg png webp heic tiff bmp pdf | v1 |
| `bmp` | 7 | jpg png webp heic tiff avif pdf | v1 |

### Audio

| Input | # | Targets | Tier |
|---|---|---|---|
| `mp3` | 7 | m4a wav flac ogg opus aiff wma | v1 |
| `m4a` | 7 | mp3 wav flac ogg opus aiff wma | v1 |
| `wav` | 7 | mp3 m4a flac ogg opus aiff wma | v1 |
| `flac` | 7 | mp3 m4a wav ogg opus aiff wma | v1 |
| `ogg` | 7 | mp3 m4a wav flac opus aiff wma | v1 |
| `opus` | 7 | mp3 m4a wav flac ogg aiff wma | v1 |
| `aiff` | 7 | mp3 m4a wav flac ogg opus wma | v1 |
| `wma` | 7 | mp3 m4a wav flac ogg opus aiff | v1 |

### Video

| Input | # | Targets | Tier |
|---|---|---|---|
| `mp4` | 7 | mov mkv webm avi wmv gif mp3 | v1 |
| `mov` | 7 | mp4 mkv webm avi wmv gif mp3 | v1 |
| `mkv` | 7 | mp4 mov webm avi wmv gif mp3 | v1 |
| `webm` | 7 | mp4 mov mkv avi wmv gif mp3 | v1 |
| `avi` | 7 | mp4 mov mkv webm wmv gif mp3 | v1 |
| `wmv` | 7 | mp4 mov mkv webm avi gif mp3 | v1 |
| `gif` | 6 | mp4 mov mkv webm avi wmv | v1 |

### PDF

| Input | # | Targets | Tier |
|---|---|---|---|
| `pdf` | 9 | docx<sup>v2</sup> pages<sup>later</sup> pptx<sup>v2</sup> xlsx<sup>v2</sup> jpg png epub<sup>v2</sup> txt md | v1/v2/later |

### Slides

| Input | # | Targets | Tier |
|---|---|---|---|
| `ppt` | 3 | pdf<sup>v2</sup> jpg<sup>v2</sup> png<sup>v2</sup> | v2 |
| `pptx` | 3 | pdf<sup>v2</sup> jpg<sup>v2</sup> png<sup>v2</sup> | v2 |

### Spreadsheets

| Input | # | Targets | Tier |
|---|---|---|---|
| `xlsx` | 4 | ods<sup>v2</sup> csv<sup>v2</sup> tsv<sup>v2</sup> pdf<sup>v2</sup> | v2 |
| `xls` | 5 | xlsx<sup>v2</sup> ods<sup>v2</sup> csv<sup>v2</sup> tsv<sup>v2</sup> pdf<sup>v2</sup> | v2 |
| `ods` | 4 | xlsx<sup>v2</sup> csv<sup>v2</sup> tsv<sup>v2</sup> pdf<sup>v2</sup> | v2 |
| `csv` | 4 | xlsx<sup>v2</sup> ods<sup>v2</sup> tsv<sup>v2</sup> pdf<sup>v2</sup> | v2 |
| `tsv` | 4 | xlsx<sup>v2</sup> ods<sup>v2</sup> csv<sup>v2</sup> pdf<sup>v2</sup> | v2 |

### Documents

| Input | # | Targets | Tier |
|---|---|---|---|
| `docx` | 8 | pages<sup>later</sup> pdf<sup>v2</sup> txt<sup>v2</sup> md<sup>v2</sup> html<sup>v2</sup> epub<sup>v2</sup> png<sup>v2</sup> jpg<sup>v2</sup> | v2/later |
| `pages` | 8 | docx<sup>later</sup> pdf<sup>later</sup> txt<sup>later</sup> md<sup>later</sup> html<sup>later</sup> epub<sup>later</sup> png<sup>later</sup> jpg<sup>later</sup> | later |
| `md` | 8 | docx<sup>v2</sup> pages<sup>later</sup> pdf<sup>v2</sup> txt<sup>v2</sup> html<sup>v2</sup> epub<sup>v2</sup> png<sup>v2</sup> jpg<sup>v2</sup> | v2/later |
| `rtf` | 9 | docx<sup>v2</sup> pages<sup>later</sup> pdf<sup>v2</sup> txt<sup>v2</sup> md<sup>v2</sup> html<sup>v2</sup> epub<sup>v2</sup> png<sup>v2</sup> jpg<sup>v2</sup> | v2/later |
| `rtfd` | 9 | docx<sup>v2</sup> pages<sup>later</sup> pdf<sup>v2</sup> txt<sup>v2</sup> md<sup>v2</sup> html<sup>v2</sup> epub<sup>v2</sup> png<sup>v2</sup> jpg<sup>v2</sup> | v2/later |
| `textClipping` | 9 | docx<sup>v2</sup> pages<sup>later</sup> pdf<sup>v2</sup> txt<sup>v2</sup> md<sup>v2</sup> html<sup>v2</sup> epub<sup>v2</sup> png<sup>v2</sup> jpg<sup>v2</sup> | v2/later |
| `epub` | 6 | pdf<sup>v2</sup> docx<sup>v2</sup> pages<sup>later</sup> txt<sup>v2</sup> md<sup>v2</sup> html<sup>v2</sup> | v2/later |
| `txt` | 8 | docx<sup>v2</sup> pages<sup>later</sup> pdf<sup>v2</sup> md<sup>v2</sup> html<sup>v2</sup> epub<sup>v2</sup> jpg<sup>v2</sup> png<sup>v2</sup> | v2/later |

### Subtitles

| Input | # | Targets | Tier |
|---|---|---|---|
| `srt` | 2 | vtt txt | v1 |
| `vtt` | 2 | srt txt | v1 |

### Archives

| Input | # | Targets | Tier |
|---|---|---|---|
| `zip` | 2 | tar gz | v1 |
| `tar` | 2 | zip gz | v1 |
| `gz` | 2 | zip tar | v1 |
| `rar` | 3 | zip tar gz | v1 |

**Total: 275 pairs** — v1: 178, v2: 81, later: 16.

Notes:
- `svg` is input-only within images. Rasterize at its intrinsic size; **[Grape]** use 2× for very small viewBoxes (< 256 px).
- `video → mp3` extracts the audio track. `video → gif` uses a palette (see `ENGINES.md`).
- `gif → video` is offered, but `gif` gets no image targets **[observed]**.
- PDF → images exports **every page at 300 DPI** **[observed: site copy]**. Multiple pages go into a new folder.
- **[Grape]** A page over 100 megapixels, or over 65,535 px on a side, is drawn at the highest DPI that fits (only that page), and the toast says how many pages were (D29).
- `heic` output needs an HEVC encoder. **[Grape]** macOS: ImageIO in Grape's HEIC worker; a Mac without HEVC encoding says "This Mac can't write HEIC files." Windows: Grape's own `grape-heif.exe` (libheif + x265; GPL, so a separate executable under ENGINES.md licensing rule 2; D33). Only the primary image of a HEIC converts, and HEIC output is at most 35,651,584 pixels for now.
- **[Grape]** `rar` (RAR 4 and 5) is read through Grape's own bsdtar (D27). Refused with a toast:
  - password-protected archives;
  - one part of a multi-part set;
  - solid RAR 4;
  - RAR 5 dictionaries over 64 MiB;
  - self-extracting archives;
  - RAR 7-only features.

  RAR 5 junctions and file copies are left out and counted. `rar → tar` writes a re-packed GNU tar, not bsdtar's pax.

## Tools per input [observed]

Tool keys are defined in `TOOLS_AND_ACTIONS.md`. "Multi" means more than one file is selected.

| Input(s) | Single file | Multi-selection |
|---|---|---|
| `jpg png webp heic tiff` | compress, removeMetadata, editImage, annotateImage, frameImage, cropImage, redactImage | compress, removeMetadata, cropImage, createPDF, createCollage |
| `svg` | — (empty) | createPDF, createCollage |
| `mp3 m4a wav flac` | compress, removeMetadata, normalizeAudio, audioToVideo, trimAudio, audioChannels, redactAudio | compress, removeMetadata, normalizeAudio |
| `mp4 mov mkv` | compress, removeMetadata, muteVideo, trimVideo, cropVideo, changeVideoSpeed, videoSnapshots, splitVideo, redactVideo | compress, removeMetadata, muteVideo, joinVideos |
| `gif` | removeMetadata | removeMetadata |
| `pdf` | compress, removeMetadata, splitPDF, organizePDF | compress, removeMetadata, splitPDF, mergePDF |
| `zip tar gz rar` | extractArchive | extractArchive |
| everything else | — (empty) | — (empty) |

How "multi" is computed **[observed]**: take the multi list for that format, then keep only tools in the **multi-allowed set** = {compress, removeMetadata, cropImage, normalizeAudio, muteVideo, splitPDF, extractArchive, createPDF, mergePDF, joinVideos, createCollage}. That's why trimVideo, cropVideo and changeVideoSpeed vanish for multi-video even though they're in its raw list, and why images keep compress, removeMetadata, cropImage, createPDF and createCollage.

**[Grape]** Formats observed with no tools (`ogg opus aiff wma webm avi wmv avif bmp`) get none in v1, to match. Revisit in v2. FFmpeg could handle them trivially.

## Output naming

### Conversions [observed]
- `stem` = filename minus the last extension. `.tar.gz` / `.tgz` are stripped as one unit.
- Output = `{stem}.{ext}`. For `gz` the ext is `tar.gz`.
- If that name exists (on disk, or already planned in this batch): `{stem} 2.{ext}`, `{stem} 3.{ext}`, …
- **[Grape]** The collision check ignores case: an existing `A.PNG` blocks `a.png`.
- **[Grape]** A name with no dot (`Makefile`), only a leading dot (`.env`) or a trailing dot (`notes.`) has no extension, so the whole name is the stem: `Makefile` → `Makefile.png`.
- **[Grape]** Document → `jpg`/`png` with more than one page writes a new folder `{stem}` (same collision rule) holding `{stem} {n}.{ext}`, with `n` counting from 1 and zero-padded to the width of the page count (`report 01.png` … `report 12.png`). One page writes `{stem}.{ext}` as usual.

### Tools [observed]

Output = `{stem} {Suffix}.{ext}`, with the same ` 2`, ` 3` collision rule. A folder output has no extension.

| Tool | Suffix | Output ext |
|---|---|---|
| compress | Compressed | same; **wav → flac** |
| removeMetadata | No Metadata | same |
| editImage | Edited | same |
| annotateImage | Annotated | png |
| frameImage | Background | same |
| cropImage / cropVideo | Cropped | same |
| redactImage / redactVideo | Redacted | same |
| redactAudio | Bleeped | same |
| normalizeAudio | Normalized | same |
| muteVideo | Muted | same |
| trimVideo / trimAudio | Trimmed | same |
| changeVideoSpeed | Speed {N}x | same |
| videoSnapshots | Snapshot | png |
| splitVideo | Clips | folder |
| splitPDF | Pages | folder |
| organizePDF | Organized | pdf |
| extractArchive | Extracted | folder |
| audioToVideo | Visualizer | mp4 |
| audioChannels | Mono (or Stereo) | same |
| createPDF / mergePDF / joinVideos / createCollage | **[Grape]** first file's stem + ` Combined` / ` Merged` / ` Joined` / ` Collage` | pdf / pdf / first file's container / png |

The multi-file tools' naming was not shown in the demo, so those names are Grape's choice.

- **[Grape]** "same" means the source's literal extension, case included: `Photo.JPG` → `Photo Compressed.JPG`. A source with no extension gives an output with none. Compress's **wav → flac** goes by the detected format (rule 3), so WAV audio named `take.mp3` gives `take Compressed.flac`.
- **[Grape]** `{N}` in `Speed {N}x` is the factor with at most two decimals and no trailing zeros: `Speed 2x`, `Speed 1.5x`, `Speed 0.25x`.

### Location [Grape]
- Default: the same folder as the source.
- If that isn't writable, or Settings says so: the fallback folder (default `~/Downloads/Grape`, `%USERPROFILE%\Downloads\Grape`).
- Never overwrite. Never touch the source.

## Test assertions (put these in `grape-core`)
- `pairs().len() == 275` once full parity is enabled, and `== 178` with only v1 enabled.
- No `(x, x)` pairs. No input-only format appears as a target.
- Naming table above: one test per row, plus collision and `.tar.gz` cases.
- Tools table above: one test per row for single and multi.
