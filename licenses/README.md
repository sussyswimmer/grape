# licenses/

License texts for the third-party programs and libraries Grape ships as separate files: sidecars like FFmpeg, ffprobe and `heif-enc`, the DLLs some of them need, and PDFium, the PDF library Grape's PDF helper loads. This is rule 3 in `docs/ENGINES.md` → Licensing rules.

PDFium is the first to ship (`pdfium/`, DECISIONS D29). The sidecars' texts come with them (D22).

The Rust crates and JavaScript compiled into Grape itself aren't covered here. Their notices are in `THIRD_PARTY_NOTICES.md` at the repo root, which cargo-about generates (DECISIONS D23).

## The rule

Every bundled third-party binary gets:

1. **A folder here**, named after the program: `licenses/ffmpeg/`, `licenses/heif-enc/`. Programs from one build share a folder (ffprobe goes in `ffmpeg/`). A DLL from another project gets its own folder (`libheif/`, `libde265/`).
2. **Its license text** as `LICENSE.txt`, exactly as the project publishes it for that version. For a GPL build that's the GPL text the build is under. Line endings may become LF (git stores text that way here); nothing else changes.
3. **The license of every library built into it**, when that's a different text, one file per library: `x264.txt`, `lame.txt`, `libvpx.txt`. A static FFmpeg build carries a dozen. Its configure line (`ffmpeg -version`) says which. When the build ships its own folder of these texts (PDFium's archive has `licenses/`), take them all, under the names it gives them, so a version bump can be compared file by file. A library the build uses but has no text for (libc++ in PDFium) is named in `SOURCE.md`.
4. **A `SOURCE.md`** with the program's name, exact version, license, where the binary came from (the URL and SHA-256 in `scripts/sidecars.lock`, which the fetch scripts use; DECISIONS D25) and, for GPL and LGPL programs, the exact source: a link to the source at that version (a tag or a commit, never a branch) and the build's configure line. The source archive also goes on the GitHub Release (M7).
5. **An entry in `THIRD_PARTY_NOTICES.md`.** That file is generated, so don't edit it. Add the entry to the template, `about.hbs`, in a section of its own before "## Rust crates", then regenerate it with the command at the top of the file. CI fails if the two differ.
6. **A line in `bundle.resources`**, so the folder ships: `"../../../licenses/ffmpeg/": "licenses/ffmpeg/"`. It goes in the overlay that bundles the program itself (`tauri.sidecars.conf.json` for FFmpeg, `tauri.pdfium.windows.conf.json` and `tauri.pdfium.macos.conf.json` for PDFium), so the texts ship exactly when the program does. It lands in the install folder on Windows and in `Grape.app/Contents/Resources/` on macOS, next to `THIRD_PARTY_NOTICES.md`.
7. **A DECISIONS entry** with its name, version, license and why it's needed (rule 4).

## Layout

```
licenses/
  README.md          this file (doesn't ship)
  ffmpeg/            FFmpeg and ffprobe, one build
    LICENSE.txt      GPL-3.0, the license of the build
    SOURCE.md        version, binary URL and SHA-256, source link, configure line
    x264.txt         one text per library built in, when it differs
    lame.txt
    ...
  heif-enc/          heif-enc with x265 (GPL)
    LICENSE.txt
    SOURCE.md
    x265.txt
  libheif/           a DLL Grape loads on Windows (LGPL)
    LICENSE.txt
    SOURCE.md
  pdfium/            PDFium, the library the PDF helper loads (D29)
    LICENSE.txt      MIT, pdfium-binaries' packaging (the archive's LICENSE)
    SOURCE.md        version, binary URLs and SHA-256s, PDFium commit, patches, notes
    pdfium.txt       BSD-3-Clause (and Apache-2.0), PDFium itself
    abseil.txt ...   the archive's licenses/ folder, 17 files in all
```
