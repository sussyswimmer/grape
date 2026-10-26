# Engines, codecs and licensing

## Licensing rules (Grape is free and open source; see DECISIONS D2)

1. **Grape's own binaries** (the Tauri app and our crates) may link only permissive code (MIT/Apache/BSD/ISC/Zlib) or LGPL libraries that are *dynamically* linked. CI checks the crates with cargo-deny against `deny.toml` (DECISIONS D23).
2. **GPL/AGPL tools may ship only as separate sidecar executables** that Grape runs as a child process. Never link them into our binary.
3. Every bundled third-party binary ships with its license text in `licenses/` and an entry in `THIRD_PARTY_NOTICES.md`. GPL sidecars also need the exact source version and a link to the source (or a source archive on the GitHub Release). `THIRD_PARTY_NOTICES.md` is generated, so the entry goes in its template, `about.hbs`. `licenses/README.md` has the steps.
4. Record every new library or sidecar in `DECISIONS.md`: name, version, license and why it's needed.
5. Skip anything with *restrictive non-OSS* terms, like the `unrar` freeware license. Use libarchive (BSD) for RAR reading instead, through Grape's own bsdtar build (D27).

## Engine map

| Family | v1 engine | Notes |
|---|---|---|
| **Raster images** (jpg png webp tiff bmp avif) | Rust `image` crate + `ravif` (AVIF encode) + `jpeg-encoder`/`mozjpeg` for quality | Keep EXIF orientation: apply it, then strip or keep per tool |
| **HEIC** | A helper process on both OSes, one protocol (`crates/grape-heif-proto`, D33). Windows: `grape-heif.exe`, Grape's own sidecar on libheif 1.23.7 + libde265 1.1.3 + x265 4.3, linked statically, plugin loading compiled out (GPL-3.0-or-later as a whole, rule 2; "HEIC build" below). macOS: `Grape --grape-heif-worker` with ImageIO, through `grape-platform` | Primary image only, upright, 8-bit; ICC kept; encode ≤ 35.6 MP; libheif quality 50 / ImageIO 0.8 |
| **SVG input** | `resvg` + `usvg` | Rasterize; see the size rule in FORMAT_MATRIX |
| **Images → PDF / Create PDF** | `printpdf` or `lopdf` embedding JPEG/PNG streams directly | One page per image, page size = image aspect at 72 dpi-equivalent |
| **Audio & video** | **FFmpeg sidecar** (static build, pinned version) | Arguments per target below |
| **PDF → JPG/PNG** | `pdfium-render` 0.9.4 + bundled PDFium (BSD-3-Clause; "PDFium build" below), in Grape's PDF worker process (DECISIONS D29) | 300 DPI, every page; a page over 100 megapixels at the DPI that fits, and the toast says how many. Folder for > 1 page |
| **PDF → TXT / MD** | PDFium text extraction, in the worker (D29). MD = paragraphs + headings by font-size heuristics | Scanned PDFs: OCR is v2 (Apple Vision on mac, `Windows.Media.Ocr` on Windows) |
| **PDF split / merge / metadata** | `lopdf` 0.45.0 with Grape's own page copier, in the worker; PDFium's page import when lopdf can't open a file (D29) | Split and merge drop the outline, form fields and tags. Encrypted PDFs are refused |
| **PDF compress** (v2) | Downsample embedded images via pdfium + lopdf rewrite | No Ghostscript |
| **Subtitles** | Our own parser in `grape-engines` (srt ⇄ vtt ⇄ txt) | Tiny, fully tested. Rules in "Subtitles" below (D20) |
| **Archives** | `zip`, `tar`, `flate2` (D27); RAR input through Grape's own `bsdtar` sidecar, built from libarchive 3.8.10 (BSD-2-Clause) with Grape's patches (D27; "bsdtar build" below) | Zip-slip guard on extract: reject `..` and absolute paths. RAR: a Rust header preflight refuses encrypted, multi-part, solid RAR4, big-dictionary and self-extracting archives before bsdtar runs. Extract leaves out every link; a conversion keeps a symlink that stays inside the archive and leaves out escaping ones and hard links (D27). Both leave out special files and RAR5 file copies. What's left out is counted, and the toast says how many (`Note::LeftOut`) |
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

## PDFium build

- bblanchon/pdfium-binaries, release `chromium/8086` (PDFium 157.0.8086.0): no V8, no XFA. Windows x64 and a universal macOS build, pinned in `scripts/sidecars.lock` → `libraries` with each archive's and each library's SHA-256 (D29). The fetch scripts put it in `apps/desktop/src-tauri/binaries/` (`-Only pdfium` / `--only pdfium` for just that).
- It ships as a library, not a sidecar: `pdfium.dll` next to `Grape.exe`, `Grape.app/Contents/Frameworks/libpdfium.dylib`. A build adds it with `--config src-tauri/tauri.pdfium.windows.conf.json` or `tauri.pdfium.macos.conf.json`. Its texts are in `licenses/pdfium/` and its entry in `about.hbs`.
- Only the PDF worker loads it: a separate process of Grape's own executable (`Grape --grape-pdf-worker`), one per job. It binds the library by absolute path, never from the system or `PATH`. The app's main process never loads it.
- It has to export every function pdfium-render binds with `pdfium_7881`. Check the exports again when either one moves (D29 says how it was checked).

## bsdtar build

Grape reads RAR with libarchive's `bsdtar`, built by Grape (D27). Not the OS's bsdtar (too old on Windows 10 and macOS 13–15.3, and neither decodes filtered RAR4), and not libarchive linked into Grape (a crash in the C decoder must end only bsdtar).

- **Source and patches:** `scripts/bsdtar/sources.json` pins libarchive 3.8.10's release tarball (URL, SHA-256, size) and the six patches in `scripts/bsdtar/patches/`, in order, each with its SHA-256: three upstream fixes (the NULL-name guard, Windows name conversions, the RAR4 symlink bound) and three of Grape's (a UTF-8 code page in the Windows manifest, `@archive` reading only RAR and RAR5, the version marker). **Change the pin in `sources.json`, never in the scripts**, and bump the marker with every change to the series.
- **Build scripts:** `scripts/bsdtar/build.ps1` (Windows x64, MSVC, static CRT) and `build.sh` (macOS arm64 + x86_64, minimum macOS 13.0, merged with `lipo` and signed ad hoc; Linux for local tests). Every optional library is off, so bsdtar links nothing but the OS: KERNEL32, ADVAPI32 and bcrypt on Windows, libSystem on macOS. `-Install` / `--install` copies the result into `apps/desktop/src-tauri/binaries/` for `tauri dev`. `licenses/libarchive/SOURCE.md` has the exact CMake lines.
- **The version line** is exactly `bsdtar 3.8.10-grape.1 - libarchive 3.8.10 libb2/bundled ` (macOS adds `CommonCrypto/system` before `libb2/bundled`). The build checks it exactly; the bundling gate (`scripts/check-sidecars.mjs`) checks the `-grape` marker, `licenses/libarchive/` and a `## libarchive` section in the notices.
- **CI:** `.github/workflows/bsdtar.yml`, a reusable workflow that `ci.yml` calls (and `release.yml` will). It builds per OS, checks the imports, the manifest or architectures, the version line and the copyright lines of the compiled sources, and runs `scripts/bsdtar/smoke.py`, a smoke test in Grape's exact environment on libarchive's own RAR test files. It's cached by the hash of `scripts/bsdtar/` and `licenses/libarchive/copyright-lines.txt` (a cache hit skips the copyright check, so a change to the file must miss) and uploads the binaries as artifacts for the same run. CI's `rar-it` jobs run the engine's RAR tests against them. No GitHub Release until M7, and `scripts/sidecars.lock` has no bsdtar.
- **Shipping:** a sidecar in `tauri.sidecars.conf.json`'s `externalBin`, next to FFmpeg, with `licenses/libarchive/`. No installer or DMG carries it until CI builds FFmpeg too (D24); then per-OS sidecar overlays take over (D27).
- **How Grape runs it** (`grape-engines` `archive/rar/`): only as `bsdtar -c -P -f - --format pax @-`, with the RAR written into a stdin pipe (never a file handle), in a new, empty, private folder, with only `PATH` (that folder), `TZ=UTC0`, `TAR_READER_OPTIONS=rar:hdrcharset=UTF-8`, plus `SystemRoot` (Windows) or `LC_ALL=en_US.UTF-8` (macOS), and `CREATE_NO_WINDOW` on Windows. A Rust preflight of the RAR's headers runs first. The output counts only with exit 0, an empty stderr, a tar end marker followed only by zeros, and entries matching the RAR's headers one to one by kind, size and CRC32. A 60 s stall watchdog, no total time cap.

## HEIC build

Grape converts HEIC in a helper process on both OSes (D33): a crash or a hang in a HEIF decoder must end only the helper, and neither libheif nor ImageIO can be cancelled partway through. The engine side is `grape-engines`' `heif.rs` and `helper.rs`; both helpers speak `crates/grape-heif-proto`.

- **Windows: `grape-heif.exe`**, built from `tools/grape-heif` (its own Cargo workspace and `Cargo.lock`, outside Grape's): libheif-rs 3.0.0 over libheif **1.23.7** (a security release, taken under D9's override), libde265 **1.1.3** and x265 **4.3**, linked statically with the static CRT (`x64-windows-static`). GPL-3.0-or-later as a whole (rule 2): it ships only as this separate executable, with `licenses/grape-heif/` and its source bundle (rule 3).
- **The pin:** `tools/grape-heif/vcpkg.lock.json` holds vcpkg's commit (`e456309`, the last master commit before D9's cutoff), the triplet, the install spec `libheif[core,hevc]` and each source archive's URL and hashes. An overlay port, `tools/grape-heif/vcpkg/ports/libheif/`, takes that commit's libheif recipe to 1.23.7 (the release tarball, by SHA-512) and turns plugin loading off (`-DENABLE_PLUGIN_LOADING=OFF`): with it on, libheif scans its plugin folder even without `LIBHEIF_PLUGIN_PATH`.
- **Build:** `tools/grape-heif/build.ps1`, the same on CI and the PC. It clones vcpkg at the pinned commit (never the runner's), fetches and checks every source archive before it builds, so a binary-cache hit still has the sources for the bundle, then runs `cargo build --release --locked`. The first build takes 30 minutes or more and about 3 GB (`PLATFORM_WINDOWS.md`).
- **CI:** `.github/workflows/grape-heif.yml`, a reusable workflow that `ci.yml` and `release.yml` call (the release builds it without cache). It runs the tool's tests, fmt and clippy, checks the banner (`grape-heif version 1.23.7-grape.N`, `plugins=disabled`, `crt=static`), that no plugin loader is in the exe, the DLLs it imports and that the license texts match the built ports. It uploads the exe, the HEIC fixtures and the source bundle (`grape-heif-<version>-source.tar.gz`: the tool's code and the protocol crate, the vendored crates, the three archives, the vcpkg ports and the build notes). The installer job takes the exe from the same run; the release attaches the source bundle next to the installer.
- **Shipping:** `tauri.heif.windows.conf.json` adds `binaries/grape-heif` to `externalBin` and `licenses/grape-heif/` to the resources, and runs the bundling gate (`scripts/check-sidecars.mjs`: the `-grape` marker, the license folder and the notices section). Installed, it's `grape-heif.exe` next to `Grape.exe`. It isn't in `scripts/sidecars.lock`, and no GitHub Release carries it on its own until M7.
- **macOS: ImageIO**, through `objc2-image-io`, in `grape_platform::macos::heif`. The helper is Grape's own executable, started as `Grape --grape-heif-worker` (like the PDF worker), so nothing more is bundled. A Mac without a HEVC encoder (and GitHub's macOS VMs) can't write HEIC: "This Mac can't write HEIC files."
- **How Grape runs it** (`grape-engines` `heif.rs`): `<helper> --grape-heif-worker`, one image per process, its stdin and stdout piped and stderr discarded, `CREATE_NO_WINDOW` on Windows, `LIBHEIF_PLUGIN_PATH` and `LIBHEIF_SECURITY_LIMITS` removed from the environment it inherits, its current folder a new private `grape-heif-…` folder under the OS temp folder, where the raw pixels go both ways. On Windows the app's hook (`grape_engines::set_contain`) puts it in a Job Object before it gets its request: 2 GiB of committed memory, one process, killed when Grape exits, no error dialogs; a helper that can't be contained is killed and the job fails. Timeouts are 120 s plus 1 s per megapixel to decode and 3 s to encode; a cancel kills it within the 100 ms poll. Limits: a HEIC source of at most 512 MiB, at most 134,217,728 decoded pixels, the 1 GiB working set (`image::pixels_fit`, checked from the helper's `image` line before any pixel is read), and at most 35,651,584 pixels and 16,888 a side to encode (one HEVC level 6.2 picture; grid encoding is a follow-up).
- **Bumping:** `tools/grape-heif/README.md` → Bumping. A libheif or libde265 security release is likely to come again; take it under D9's override and say so in D33.

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
- **Cancelling:** after a cancel, the job can take longer to return than the 50 ms allowed between checks. Freeing a large parsed file, an fsync already under way and removing a large temp file add up to about 150–200 ms on the largest files, depending on the disk. Measured in release builds: up to 147–196 ms for 16 MiB files dense with tags or a conversion with an 80 MiB output, and 57–99 ms for a realistic 16 MiB SRT (once 244 ms).
- **SRT text that looks like a timing line:** a text line with `-->` that starts with a digit, or with `-` and a digit (`10 degrees --> brr`, `-10 degrees --> brr`), is read as a timing line. Its timing is invalid, so it and the rest of its cue are dropped, and so is a line of only digits right before it, taken for its cue number. A cue whose text starts there is dropped whole. It follows from the rules under Timing lines.

## Metadata removal
- Images: decode and re-encode without EXIF/XMP/IPTC. Apply the orientation first, so stripping it doesn't rotate the picture.
- Media: `-map_metadata -1 -map_chapters -1 -c copy`.
- PDF: clear the `Info` dictionary and XMP `Metadata` stream with lopdf, and write the whole file again so old revisions go too (D29). Page-level XMP and image metadata stay.
- Report what was removed (GPS, camera, author…) in the toast (`JobOutcome::notes` → `Note::MetadataRemoved`; the app words it, PDF first).
