# licenses/

License texts for the third-party programs and libraries Grape ships as separate files: sidecars like FFmpeg, ffprobe and `bsdtar`, the DLLs some of them need, PDFium, the PDF library Grape's PDF helper loads, and `grape-heif`, Grape's own image helper for HEIC and AVIF on Windows, which is GPL because of the libraries built into it. This is rule 3 in `docs/ENGINES.md` → Licensing rules.

PDFium is the first to ship (`pdfium/`, DECISIONS D29). `ffmpeg/` has the texts for Grape's own FFmpeg build (D24), and `libarchive/` those of libarchive's `bsdtar`, which Grape reads RAR archives with (D27). Both ship in every installer and DMG, with the per-OS sidecars overlays, from CI's builds of the programs in the same run (D25). `grape-heif.exe` has its texts and its source's whereabouts in `grape-heif/` (D33, D41); it ships in the Windows installer through the same Windows overlay.

The Rust crates and JavaScript compiled into Grape itself aren't covered here. Their notices are in `THIRD_PARTY_NOTICES.md` at the repo root, which cargo-about generates (DECISIONS D23). That includes rav1e, the AVIF encoder compiled into Grape's macOS build (D41): its AV1 patent license (`PATENTS`, the Alliance for Open Media Patent License 1.0) and the notices of the libaom and dav1d code in it are a hand-written section of `about.hbs`, "rav1e (AVIF on macOS)", because cargo-about takes only a crate's license files.

## The rule

Every bundled third-party binary gets:

1. **A folder here**, named after the program: `licenses/ffmpeg/`, `licenses/grape-heif/`. Programs from one build share a folder (ffprobe goes in `ffmpeg/`). A DLL from another project gets its own folder. Libraries linked statically into a program go in that program's folder (`grape-heif/libheif.txt`).
2. **Its license text** as `LICENSE.txt`, exactly as the project publishes it for that version. For a GPL build that's the GPL text the build is under. Line endings may become LF (git stores text that way here); nothing else changes.
3. **The license of every library built into it**, when that's a different text, one file per library: `x264.txt`, `lame.txt`, `libvpx.txt`. A static FFmpeg build carries a dozen. Its configure line (`ffmpeg -version`) says which. When the build ships its own folder of these texts (PDFium's archive has `licenses/`), take them all, under the names it gives them, so a version bump can be compared file by file. A library the build uses but has no text for (libc++ in PDFium) is named in `SOURCE.md`.
4. **A `SOURCE.md`** with the program's name, exact version, license, where the binary came from (the URL and SHA-256 in `scripts/sidecars.lock`, which the fetch scripts use, DECISIONS D25; or, for a program Grape builds itself, the build script and its pinned sources, D24) and, for GPL and LGPL programs, the exact source: a link to the source at that version (a tag or a commit, never a branch) and the build's configure line. The source archive also goes on the GitHub Release (M7).
5. **An entry in `THIRD_PARTY_NOTICES.md`.** That file is generated, so don't edit it. Add the entry to the template, `about.hbs`, in a section of its own before "## Rust crates", then regenerate it with the command at the top of the file. CI fails if the two differ.
6. **A line in `bundle.resources`**, so the folder ships: `"../../../licenses/ffmpeg/": "licenses/ffmpeg/"`. It goes in the overlay that bundles the program itself (`tauri.sidecars.windows.conf.json` for FFmpeg, bsdtar and grape-heif, `tauri.sidecars.macos.conf.json` for FFmpeg and bsdtar, `tauri.pdfium.windows.conf.json` and `tauri.pdfium.macos.conf.json` for PDFium), so the texts ship exactly when the program does. It lands in the install folder on Windows and in `Grape.app/Contents/Resources/` on macOS, next to `THIRD_PARTY_NOTICES.md`.
7. **A DECISIONS entry** with its name, version, license and why it's needed (rule 4).

## Layout

```
licenses/
  README.md          this file (doesn't ship)
  ffmpeg/            FFmpeg and ffprobe, one build
    LICENSE.txt      GPL-2.0, the license of the build
    SOURCE.md        version, how it's built, the source pins, configure line
    x264.txt         one text per library built in, when it differs
    lame.txt
    ...
  grape-heif/        Grape's image helper on Windows, HEIC and AVIF: libheif, libde265, x265,
                     dav1d and libaom linked statically, so GPL as a whole (D33, D41)
    LICENSE.txt      GPL-3.0, the license of the program as a whole (gnu.org's gpl-3.0.txt)
    SOURCE.md        version, vcpkg commit, the five source archives and their hashes,
                     the two overlay ports, how it's built, where the source bundle is
    libheif.txt      libheif's COPYING (LGPL-3.0-or-later)
    libde265.txt     libde265's COPYING (LGPL-3.0-or-later)
    libde265-win32cond.txt   the DOC notice of libde265's Windows condition variables
    x265.txt         x265's COPYING (GPL-2.0-or-later)
    dav1d.txt        dav1d's COPYING (BSD-2-Clause)
    dav1d-PATENTS.txt   dav1d's doc/PATENTS (the Alliance for Open Media Patent License 1.0)
    dav1d-x86inc.txt the ISC notice of dav1d's x86inc.asm
    aom.txt          vcpkg's copyright file for libaom: its LICENSE (BSD-2-Clause) and PATENTS,
                     and the fastfeat, vector and x86inc licenses
    rust-crates.md   its Rust crates (cargo-about from tools/grape-heif/Cargo.lock)
  libarchive/        bsdtar, which Grape reads RAR with (D27)
    LICENSE.txt      BSD-2-Clause, the tarball's COPYING
    SOURCE.md        version, tarball URL and SHA-256, patches, build lines, notes
    copyright-lines.txt   the copyright lines of the compiled sources (generated
                     and checked by scripts/bsdtar/build.*)
    regents-bsd-3-clause.txt, netbsd-bsd-2-clause.txt, postgresql.txt,
    blake2.txt, unicode.txt, openbsd-isc.txt   the parts under other terms
  pdfium/            PDFium, the library the PDF helper loads (D29)
    LICENSE.txt      MIT, pdfium-binaries' packaging (the archive's LICENSE)
    SOURCE.md        version, binary URLs and SHA-256s, PDFium commit, patches, notes
    pdfium.txt       BSD-3-Clause (and Apache-2.0), PDFium itself
    abseil.txt ...   the archive's licenses/ folder, 17 files in all
```
