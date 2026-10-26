# licenses/

License texts for the third-party programs Grape ships as separate files: sidecars like FFmpeg, ffprobe and `heif-enc`, and the DLLs some of them need. This is rule 3 in `docs/ENGINES.md` → Licensing rules.

`ffmpeg/` has the texts for Grape's own FFmpeg build (DECISIONS D24). Nothing ships them yet: an installer carries FFmpeg only once CI builds it and the bundle step takes the sidecars (D25).

The Rust crates and JavaScript compiled into Grape itself aren't covered here. Their notices are in `THIRD_PARTY_NOTICES.md` at the repo root, which cargo-about generates (DECISIONS D23).

## The rule

Every bundled third-party binary gets:

1. **A folder here**, named after the program: `licenses/ffmpeg/`, `licenses/heif-enc/`. Programs from one build share a folder (ffprobe goes in `ffmpeg/`). A DLL from another project gets its own folder (`libheif/`, `libde265/`).
2. **Its license text** as `LICENSE.txt`, exactly as the project publishes it for that version. For a GPL build that's the GPL text the build is under.
3. **The license of every library built into it**, when that's a different text, one file per library: `x264.txt`, `lame.txt`, `libvpx.txt`. A static FFmpeg build carries a dozen. Its configure line (`ffmpeg -version`) says which.
4. **A `SOURCE.md`** with the program's name, exact version, license, where the binary came from (the URL and SHA-256 in `scripts/sidecars.lock`, which the fetch scripts use, DECISIONS D25; or, for a program Grape builds itself, the build script and its pinned sources, D24) and, for GPL and LGPL programs, the exact source: a link to the source at that version (a tag or a commit, never a branch) and the build's configure line. The source archive also goes on the GitHub Release (M7).
5. **An entry in `THIRD_PARTY_NOTICES.md`.** That file is generated, so don't edit it. Add the entry to the template, `about.hbs`, in a section of its own before "## Rust crates", then regenerate it with the command at the top of the file. CI fails if the two differ.
6. **A line in `apps/desktop/src-tauri/tauri.conf.json` → `bundle.resources`**, so the folder ships: `"../../../licenses/ffmpeg/": "licenses/ffmpeg/"`. It lands in the install folder on Windows and in `Grape.app/Contents/Resources/` on macOS, next to `THIRD_PARTY_NOTICES.md`.
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
  heif-enc/          heif-enc with x265 (GPL)
    LICENSE.txt
    SOURCE.md
    x265.txt
  libheif/           a DLL Grape loads on Windows (LGPL)
    LICENSE.txt
    SOURCE.md
```
