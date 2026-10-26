# Engines, codecs and licensing

## Licensing rules (Grape is free and open source; see DECISIONS D2)

1. **Grape's own binaries** (the Tauri app and our crates) may link only permissive code (MIT/Apache/BSD/ISC/Zlib) or LGPL libraries that are *dynamically* linked. CI checks the crates with cargo-deny against `deny.toml` (DECISIONS D23).
2. **GPL/AGPL tools may ship only as separate sidecar executables** that Grape runs as a child process. Never link them into our binary.
3. Every bundled third-party binary ships with its license text in `licenses/` and an entry in `THIRD_PARTY_NOTICES.md`. GPL sidecars also need the exact source version and a link to the source (or a source archive on the GitHub Release). `THIRD_PARTY_NOTICES.md` is generated, so the entry goes in its template, `about.hbs`. `licenses/README.md` has the steps.
4. Record every new library or sidecar in `DECISIONS.md`: name, version, license and why it's needed.
5. Skip anything with *restrictive non-OSS* terms, like the `unrar` freeware license. Use libarchive (BSD) for RAR reading instead.

## Engine map

| Family | v1 engine | Notes |
|---|---|---|
| **Raster images** (jpg png webp tiff bmp avif) | Rust `image` crate + `ravif` (AVIF encode) + `jpeg-encoder`/`mozjpeg` for quality | Keep EXIF orientation: apply it, then strip or keep per tool |
| **HEIC** | macOS: ImageIO. Windows: libheif + libde265 (LGPL DLLs) to decode; `heif-enc` sidecar with x265 (GPL, rule 2) to encode | |
| **SVG input** | `resvg` + `usvg` | Rasterize; see the size rule in FORMAT_MATRIX |
| **Images → PDF / Create PDF** | `printpdf` or `lopdf` embedding JPEG/PNG streams directly | One page per image, page size = image aspect at 72 dpi-equivalent |
| **Audio & video** | **FFmpeg sidecar** (static build, pinned version) | Arguments per target below |
| **PDF → JPG/PNG** | `pdfium-render` + bundled PDFium (BSD/Apache) | 300 DPI, every page; folder for > 1 page |
| **PDF → TXT / MD** | PDFium text extraction. MD = paragraphs + headings by font-size heuristics | Scanned PDFs: OCR is v2 (Apple Vision on mac, `Windows.Media.Ocr` on Windows) |
| **PDF split / merge / metadata** | `lopdf` | |
| **PDF compress** (v2) | Downsample embedded images via pdfium + lopdf rewrite | No Ghostscript |
| **Subtitles** | Our own parser in `grape-engines` (srt ⇄ vtt ⇄ txt) | Tiny, fully tested. Rules in "Subtitles" below (D20) |
| **Archives** | `zip`, `tar`, `flate2`; RAR read via `compress-tools` (libarchive) | Zip-slip guard on extract: reject `..` and absolute paths |
| **Documents** (v2) | `pandoc` sidecar (GPL, rule 2) for docx/md/html/epub/txt/rtf. macOS can also use `NSAttributedString` for docx/rtf/rtfd | → PDF: render HTML in the system webview and print to PDF (WKWebView `createPDF`, WebView2 `PrintToPdfAsync`). No LaTeX |
| **Spreadsheets** (v2) | `calamine` (read xlsx/xls/ods), `rust_xlsxwriter` (xlsx), `spreadsheet-ods` (ods), `csv` | → PDF via HTML table + webview print |
| **Slides** (v2) | LibreOffice headless sidecar (MPL) for ppt/pptx → pdf, then pdfium → images | Big download (~300 MB). Make it an **optional add-on download**, which needs a DECISIONS entry since it's a network call |
| **PDF → DOCX/PPTX/XLSX/EPUB** (v2) | LibreOffice `--infilter=writer_pdf_import` for docx; others TBD | Quality is hard. Spike before promising |
| **Pages** | none | O2 in DECISIONS |

## FFmpeg build

- **Pinned: FFmpeg 9.0.2** (released 2026-09-18) on both OSes, recorded with its sources and SHA-256s in DECISIONS D22.
  - Windows x64: a BtbN month-end GPL build (`autobuild-2026-09-30-13-08`, `n9.0.2-17`); month-end builds stay downloadable for about two years.
  - macOS: martin-riedl.de 9.0.2 for arm64 and amd64, joined with `lipo` into one universal file and re-signed.
  - Later (M7): our own trimmed CI build, for much smaller executables and a source bundle on Grape's own release.
- **The pinned builds are in `scripts/sidecars.lock`** (the pin is D22): each archive's URL, SHA-256 and size, and each file's path inside it. `scripts/fetch-sidecars.ps1` (Windows) and `scripts/fetch-sidecars.sh` (macOS) download and check them into `apps/desktop/src-tauri/binaries/` (D25; `PLATFORM_WINDOWS.md` and `PLATFORM_MACOS.md` → "Sidecars"). Change a pin in the lock, never in the scripts. No bundle carries them until `licenses/ffmpeg/` and the notices entry exist (rule 3).
- What the build must have: `--enable-gpl --enable-version3` with `libx264`, `libmp3lame`, `libopus`, `libvorbis`, `libvpx` (VP9), `libdav1d`, plus the native `aac`, `flac`, `pcm_*`, `mpeg4`, `wmv2`, `wmav2`, `gif` and `png` encoders and the `palettegen`/`paletteuse` filters; hardware encoders (`--enable-mediafoundation --enable-nvenc --enable-amf --enable-libvpl` on Windows, `--enable-videotoolbox` on macOS) for a later slice. Never `--enable-nonfree`, which can't be redistributed. The `engines-it` test `the_sidecar_has_everything_grape_uses_and_is_redistributable` checks the encoders, muxers and demuxers Grape uses. GPL is fine under rule 2.
- Ship as a Tauri sidecar via `bundle.externalBin` with target-triple suffixes (`ffmpeg-x86_64-pc-windows-msvc.exe`, `ffmpeg-universal-apple-darwin`, …), plus `ffprobe`. The app finds them next to its own executable and passes them in `SidecarPaths`; the engine never looks on `PATH`.

### How Grape runs FFmpeg (`grape-engines/src/media/`)

- **Offline and contained.** `ffprobe` and `ffmpeg` both get `-protocol_whitelist file` and `-format_whitelist mov,matroska,avi,asf,mp3,flac,ogg,wav,aiff,gif`, and every path is a `file:` URL. So no source can reach the network or open other files: an HLS, DASH or `ffconcat` playlist renamed `.mp4` is refused before it reads anything, and no file name is ever taken for an option or a protocol (`pipe:1.ogg`, `-v error.wav`).
- **Probe first.** `ffprobe -of flat` gives the container, the duration, and for the first video stream that isn't cover art (`0:V:0`) and the first audio stream (`0:a:0`) their codec, size, pixel format, rotation, rate, channels and bit depth. Tags are never read. A source without the stream the target needs is refused before anything is written ("… has no sound to save as MP3", "… has no video to convert").
- **Only what the user sees.** The output keeps that video and that audio stream. Subtitles, data streams, further audio tracks and cover art are dropped. MP4 and MOV also get `-write_tmcd 0`: their muxer would otherwise rebuild a camera's timecode (GoPro, DJI, pro cameras) as a data track of its own.
- **Writing:** into a temp file next to the planned name (`-y` is safe: the temp name is new), with the output format forced by `-f`; the planned name never appears in the argv. Then `ffprobe` checks the output has the target's stream, the file is synced (reopened for writing: Windows only flushes a writable handle), and it's published no-clobber like every engine's.
- **Progress** comes from `-progress pipe:1`'s `out_time_us` against the source's duration (none when the duration is unknown), up to 99 % until the file is published. **Cancel** kills the child within about 50 ms and waits for it, then the temp files go. At most **two FFmpeg jobs** run at once; a job that waited for its turn checks again that its source is still there ("Grape couldn't read …" if not).
- **Process hygiene:** absolute program paths only, stdin empty, `FFREPORT` removed, and on Windows `CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS` (no console window flashes; encoding doesn't starve the desktop). `ffprobe` has 60 s.
- **Errors:** a source FFmpeg can't read, or one the whitelist refuses → "… isn't a readable MP4 video"; a sidecar that won't start → "Grape's media converter is missing. Reinstall Grape."; FFmpeg's own failures are judged by the end of its stderr: "Error opening input" with "No such file or directory", "Permission denied" or "Input/output error" (the source was moved, deleted or its drive went away) → "Grape couldn't read …"; "No space left", "Disk full", "Read-only file system", "Permission denied", "Input/output error" or "No such file or directory" (the output folder went away) → couldn't save; "Invalid data found" → not readable; anything else, a muxer refusing a copied stream included → couldn't write the format. An output gone by the time it's checked (its folder was removed while FFmpeg wrote into it) → couldn't save.

### Target arguments

Every re-encoded video stream except GIF's is scaled to even dimensions, at least 2×2: `scale='max(2\,trunc(iw/2)*2)':'max(2\,trunc(ih/2)*2)'` (yuv420p needs them; AVI's variant is in its row). A copied video stream keeps its size, odd or not.

| Target | Video | Audio | Notes |
|---|---|---|---|
| mp4 | `libx264 -crf 20 -preset medium -pix_fmt yuv420p` | `aac -b:a 192k` | `-movflags +faststart -write_tmcd 0`. **Copied** (`-c copy`) when the source is H.264 yuv420p, not rotated, not from an AVI or WMV, with no audio or AAC/MP3 |
| mov | same as mp4 | same as mp4 | same as mp4 |
| mkv | copy H.264/HEVC/VP8/VP9/AV1 that isn't rotated or from an AVI or WMV, else as mp4 | copy AAC/MP3/Opus/Vorbis/FLAC/AC-3/E-AC-3, else `aac -b:a 192k` | each stream decided on its own; rotated video is re-encoded upright |
| webm | `libvpx-vp9 -crf 32 -b:v 0 -row-mt 1 -deadline good -cpu-used 4 -pix_fmt yuv420p` | `libopus -b:a 128k`, `-af aformat=channel_layouts=mono\|stereo\|3.0\|quad\|5.0\|5.1\|6.1\|7.1` | the layout filter as for opus |
| avi | `mpeg4 -q:v 3 -tag:v XVID` | `libmp3lame -b:a 192k` (constant bitrate stays in sync in AVI) | MPEG-4 Part 2 refuses 8192 px or more on a side, so the even-size filter also scales a larger frame down to fit 8190×8190, keeping its aspect: `scale='max(2\,trunc(min(iw\,iw*8190/max(iw\,ih))/2)*2)':'max(2\,trunc(min(ih\,ih*8190/max(iw\,ih))/2)*2)'` |
| wmv | `wmv2 -q:v 3` | `wmav2 -b:a 192k`, `-ac 2` above stereo, `-ar 48000` above 48 kHz | wmav2 refuses more |
| gif | pass 1: `fps=12:eof_action=pass,scale='min(iw\,720)':-1:flags=lanczos,palettegen=stats_mode=diff`, one PNG via `image2pipe`, with `-abort_on empty_output`; pass 2: the same frames `paletteuse`d with that palette | — | two passes, so a long clip isn't held in memory; `image2pipe` because `image2` expands `%` in paths; `eof_action=pass` keeps the frame of a clip shorter than 1/12 s, and a palette pass that writes nothing fails as "couldn't write a GIF file". Long inputs aren't warned about yet |
| mp3 | — | `libmp3lame -q:a 2 -id3v2_version 3` | ID3v2.3 is what Explorer and Windows Media Player read best |
| m4a | — | `aac -b:a 256k` | `-f ipod -movflags +faststart` |
| wav | — | `pcm_s16le`, or `pcm_s24le` from a lossless source over 16 bits | `-rf64 auto` for files over 4 GiB |
| flac | — | `flac -compression_level 8`, `-sample_fmt s16` or `s32` (24-bit) by the source's depth | a lossy source counts as 16-bit |
| ogg | — | `libvorbis -q:a 6`, the layout filter as for opus, `-ar 192000` above 200 kHz | libvorbis would write other layouts with the wrong speaker positions, and has no setup above 200 kHz (DXD's 352.8 kHz, 384 kHz) |
| opus | — | `libopus -b:a 128k`, `-af aformat=channel_layouts=mono\|stereo\|3.0\|quad\|5.0\|5.1\|6.1\|7.1` | libopus takes only these Vorbis-order layouts and doesn't convert on its own, so FFmpeg remixes to the nearest: 5.1(side) (AC-3, E-AC-3, DTS, 6-channel FLAC) → 5.1, 2.1 → stereo, 4.0 → 5.0, 5.0(side) → 5.0, more than 8 channels → at most 7.1 |
| aiff | — | `pcm_s16be` or `pcm_s24be`, as wav | AIFF's sizes are 32-bit and FFmpeg wraps them past 4 GiB, so a source whose duration says the samples won't fit (24-bit stereo 48 kHz past about 4 h 8 min) is refused before anything is written ("… is too long to save as AIFF."), and an output past the limit fails its check |
| wma | — | `wmav2 -b:a 192k`, with wmv's limits | |

Video from an AVI or WMV (ASF) source is never copied: those containers keep only decode times, so copied B-frames would play out of order, and FFmpeg writes that without an error. Their audio follows the rules above.

A copied conversion that FFmpeg rejects (a stream can fit by its codec and still not mux) is retried once, re-encoding everything. Only FFmpeg's own refusal is retried, a failure judged "not readable" or "couldn't write the format" (Errors above): a write failure (disk full, no permission, a read-only drive, the folder gone) is reported at once as "couldn't save", a source FFmpeg can no longer open as "couldn't read", and a cancelled job is never retried. Either would otherwise cost a whole encode, or quietly publish a re-encode where a copy was due.

**Known limits** (later slices): only the first audio track is kept; HDR or 10-bit video comes out washed out in yuv420p without tone mapping; GIFs over 30 s aren't warned about; hardware encoders (Media Foundation, NVENC, AMF, QSV, VideoToolbox) aren't used yet.

## Subtitles

Grape's own parser and writers (`grape-engines/src/subtitle/`, DECISIONS D20). No crate, no sidecar.

**Reading**
- **Encoding:** a BOM first (UTF-8, UTF-16LE, UTF-16BE), then valid UTF-8, else Windows-1252 (WHATWG table). NUL characters are removed. Files over 64 MiB are refused unread.
- **Format by content:** a file that starts with `WEBVTT` is WebVTT; anything else is read as SRT. Its header lines up to the first blank line are skipped (`Kind:`, `Language:`, `X-TIMESTAMP-MAP=`), and so are `NOTE`, `STYLE` and `REGION` blocks.
- **Timing lines** are `start --> end`; cue settings or SRT coordinates after the end are dropped. Timestamps take optional hours, `.` or `,` before the fraction, and 1–3+ fraction digits read as a decimal (`,5` is 500 ms). Minutes or seconds over 59 make the line invalid, and its cue is skipped.
- **Cues:** a timing line always starts a new cue, even with no blank line before it. A digits-only line just before it is that cue's number. A block with no timing line joins the previous cue's text.
- **Styling** is read into italic, bold and underline: `<i> <b> <u>` (with WebVTT classes) and SRT's `{\i1}`/`{\i0}`-style codes. Other `{\…}` codes, `<font>`, `<c>`, `<v>`, `<lang>`, `<ruby>` and inline timestamps are dropped, keeping their text; `<rt>` text goes in parentheses. WebVTT entities (`&amp;`, `&lt;`, `&#233;` …) are decoded; SRT text is taken as is.
- **Normalizing:** cues are sorted by start time; a cue that ends at or before its start gets 1 ms; blank lines and empty cues are dropped. No cue left → "… has no subtitles in it".

**Writing** (UTF-8; line endings follow the source)
- **WebVTT:** `WEBVTT`, then cues numbered 1…n, `HH:MM:SS.mmm --> HH:MM:SS.mmm`, and text with `&`, `<` and `>` escaped. No BOM.
- **SRT:** cues numbered 1…n, `HH:MM:SS,mmm --> HH:MM:SS,mmm`, and raw text; a `-->` inside text is written `->`. A UTF-8 BOM unless the source was BOM-less UTF-8.
- **Styling:** `<i> <b> <u>`, balanced within each cue.
- **TXT:** only the text, one line per subtitle line, with no timings or blank lines; a line identical to the one before is written once. No BOM.

## Metadata removal
- Images: decode and re-encode without EXIF/XMP/IPTC. Apply the orientation first, so stripping it doesn't rotate the picture.
- Media: `-map_metadata -1 -map_chapters -1 -c copy`.
- PDF: clear the `Info` dictionary and XMP `Metadata` stream with lopdf.
- Report what was removed (GPS, camera, author…) in the toast.
