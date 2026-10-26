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

- Pin one version for both OSes (e.g. 7.1.x) and record it in DECISIONS.
- Build flags: `--enable-gpl --enable-version3 --enable-libx264 --enable-libmp3lame --enable-libopus --enable-libvorbis --enable-libvpx --enable-libdav1d --enable-libsvtav1`, plus platform hardware encoders: `--enable-videotoolbox` (mac); `--enable-mediafoundation --enable-nvenc --enable-amf --enable-libvpl` (win). GPL is fine under rule 2.
- Use a reputable static build (e.g. BtbN for Windows, evermeet/martin-riedl style for macOS) or our own CI build. Record the source and checksum.
- Ship as a Tauri sidecar via `bundle.externalBin` with target-triple suffixes (`ffmpeg-aarch64-apple-darwin`, `ffmpeg-x86_64-pc-windows-msvc.exe`, …). Also ship `ffprobe`.
- **The pinned builds are in `scripts/sidecars.lock`** (the pin is D22): each archive's URL, SHA-256 and size, and each file's path inside it. `scripts/fetch-sidecars.ps1` (Windows) and `scripts/fetch-sidecars.sh` (macOS) download and check them into `apps/desktop/src-tauri/binaries/` (D25; `PLATFORM_WINDOWS.md` and `PLATFORM_MACOS.md` → "Sidecars"). Change a pin in the lock, never in the scripts. No bundle carries them until `licenses/ffmpeg/` and the notices entry exist (rule 3).

### Target arguments (starting points; tune in M3)

| Target | Video | Audio | Notes |
|---|---|---|---|
| mp4 | `h264_videotoolbox` (mac) / hw or `libx264 -crf 20 -preset medium` | `aac -b:a 192k` | `-movflags +faststart -pix_fmt yuv420p` |
| mov | same as mp4 | `aac` | |
| mkv | `-c copy` when the codecs fit, else as mp4 | | try remux first |
| webm | `libvpx-vp9 -crf 32 -b:v 0 -row-mt 1` | `libopus 128k` | |
| avi | `mpeg4 -q:v 3` | `libmp3lame` | |
| wmv | `wmv2 -q:v 3` | `wmav2` | |
| gif | `fps=12,scale=min(iw\,720):-1:flags=lanczos,split[a][b];[a]palettegen[p];[b][p]paletteuse` | — | cap long inputs: warn above 30 s |
| mp3 | — | `libmp3lame -q:a 2` | from video: `-vn` |
| m4a | — | `aac 256k` | |
| wav | — | `pcm_s16le` (keep 24-bit if the source is) | |
| flac | — | `flac -compression_level 8` | compress(wav) → flac |
| ogg | — | `libvorbis -q:a 6` | |
| opus | — | `libopus 128k` | |
| aiff | — | `pcm_s16be` | |
| wma | — | `wmav2 192k` | |

Run FFmpeg with `-nostdin -hide_banner -y`, write to a temp name, parse `-progress pipe:1` for the progress bar, and kill the child process on cancel.

## Subtitles

Grape's own parser and writers (`grape-engines/src/subtitle/`, DECISIONS D20). No crate, no sidecar.

**Reading**
- **Size:** files over 16 MiB are refused unread, and files of over 1,000,000 lines, or with a line of over 64 KiB once read as UTF-8, before anything is written ("… is too large to convert"). A Windows-1252 or UTF-16 line can be shorter than that in the file. A line that a WebVTT `&#10;` or `&#13;` breaks counts once for each line it makes.
- **Encoding:** a BOM first (UTF-8, UTF-16LE, UTF-16BE), then valid UTF-8, else Windows-1252 (WHATWG table). NUL characters are removed.
- **Line breaks:** `\r\n`, `\n` or a lone `\r`. `\r\r\n` is one break, never a blank line: it is what a CRLF file's line ends become when converted again, and it counts as `\r\n` for the output.
- **Format by content:** a file that starts with `WEBVTT` is WebVTT; anything else is read as SRT. Its header lines up to the first blank line are skipped (`Kind:`, `Language:`, `X-TIMESTAMP-MAP=`), and so are `NOTE`, `STYLE` and `REGION` blocks, up to their blank line or to a timing line, which starts a cue.
- **Timing lines** are `start --> end`; cue settings or SRT coordinates after the end are dropped. Timestamps take optional hours, `.` or `,` before the fraction, and 1–3+ fraction digits read as a decimal (`,5` is 500 ms). Minutes or seconds over 59 make the line invalid, and its cue is skipped. In SRT a line with `-->` is a timing line only when it starts with a digit after any spaces, invisible marks (a stray BOM, zero-width and direction marks) and one `-`. So text such as `Go this way -->` or `Route 66 -->` stays text, while a negative timing line from a shifted SRT is still a timing line, and its cue is skipped as invalid. In WebVTT, whose cue text can't hold `-->`, every such line is one.
- **Cues:** a timing line always starts a new cue, even with no blank line before it. A digits-only line just before it is that cue's number. A block with no timing line joins the previous cue's text, unless it is only digits and that cue already has text: then it is a stray number and dropped. Known limit: so a cue with no text, followed by the next cue's number on its own between blank lines (`1`, timing, blank, `2`, blank, timing), shows that number as its text.
- **Styling** is read into italic, bold and underline: `<i> <b> <u>` (with WebVTT classes) and SRT's `{\i1}`/`{\i0}`-style codes. Other `{\…}` codes, `<font>`, `<c>`, `<v>`, `<lang>`, `<ruby>` and inline timestamps are dropped, keeping their text; `<rt>` text goes in parentheses. WebVTT character references are decoded as HTML reads them: the 252 HTML 4 names, case-sensitive (`&eacute;` is é, `&Eacute;` is É), plus `&apos;` and the uppercase `&AMP;`, `&COPY;`, `&GT;`, `&LT;`, `&QUOT;`, `&REG;`, and numbers (`&#233;`, `&#xE9;`). The 106 names of HTML 3.2 (those of U+00A0–U+00FF, such as `&nbsp`, `&copy` and `&eacute`, and `&amp`, `&lt`, `&gt`, `&quot`, `&AMP`, `&LT`, `&GT`, `&QUOT`, `&COPY`, `&REG`) need no `;` when no ASCII letter or digit follows: `&copy 2024` is © 2024, and `&copyé` is ©é. HTML also reads `&copy2024` as ©2024; Grape keeps that as text. Numbers 128–159 are the Windows-1252 characters they stand for there, as in HTML (`&#150;` is –; the five that Windows-1252 leaves undefined stay C1 controls). `&#10;` and `&#13;` break the line; `&#0;`, unknown names, other names without their `;` and numbers without one stay as text. SRT text is taken as is.
- **Normalizing:** cues are sorted by start time, and cues that start together keep their file order; a cue that ends at or before its start gets 1 ms; blank lines and empty cues are dropped. No cue left → "… has no subtitles in it".

**Writing** (UTF-8; line endings follow the source)
- **WebVTT:** `WEBVTT`, then cues numbered 1…n, `HH:MM:SS.mmm --> HH:MM:SS.mmm`, and text with `&`, `<` and `>` escaped. No BOM.
- **SRT:** cues numbered 1…n, `HH:MM:SS,mmm --> HH:MM:SS,mmm`, and raw text; a `-->` inside text is written `->`. A UTF-8 BOM unless the source was BOM-less UTF-8.
- **Styling:** `<i> <b> <u>`, balanced within each cue.
- **TXT:** only the text, one line per subtitle line, with no timings or blank lines; a line identical to the one before is written once. No BOM.

**Known limits**
- **Cancelling:** after a cancel, the job can take longer to return than the 50 ms allowed between checks. Freeing a large parsed file, an fsync already under way and removing a large temp file add up to about 150 ms on the largest files. Measured in a release build: up to 147–177 ms for 16 MiB files dense with tags or a conversion with an 80 MiB output, and 57–81 ms for a realistic 16 MiB SRT.

## Metadata removal
- Images: decode and re-encode without EXIF/XMP/IPTC. Apply the orientation first, so stripping it doesn't rotate the picture.
- Media: `-map_metadata -1 -map_chapters -1 -c copy`.
- PDF: clear the `Info` dictionary and XMP `Metadata` stream with lopdf.
- Report what was removed (GPS, camera, author…) in the toast.
