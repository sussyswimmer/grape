# FFmpeg and ffprobe

Grape runs `ffmpeg` and `ffprobe` as separate programs. Grape builds them itself from source, with only the parts it uses (DECISIONS D24). This folder holds their license texts, and this file says exactly what they're built from and where their complete source is.

| | |
|---|---|
| Programs | `ffmpeg` and `ffprobe` (`ffmpeg.exe` and `ffprobe.exe` on Windows) |
| Version | FFmpeg 9.0.2. `ffmpeg -version` says `9.0.2-grape`: `--extra-version=grape` marks Grape's build |
| License | GNU General Public License, version 2 or later (GPL-2.0-or-later): FFmpeg with `--enable-gpl`, for x264. Not `--enable-version3`, not `--enable-nonfree`. On Windows, the few `libgcc` helpers in the exe come under the GCC Runtime Library Exception, which leaves that license as it is |
| Built by | `scripts/ffmpeg/build.sh <target>` in Grape's repository, from the tarballs pinned in `scripts/ffmpeg/sources.json`, each checked by size and SHA-256 before it's unpacked |
| Targets | `windows-x86_64`: cross-compiled on Linux with mingw-w64 (GCC, win32 threads), static, only Windows' own DLLs. `macos-arm64` and `macos-x86_64`: Xcode's clang, macOS 11.0 or later, joined into one universal file |
| Source | `grape-ffmpeg-sources.tar`, on the same GitHub Release as Grape's installers; for an installer or DMG from a CI run (not a release), in that run's `ffmpeg-source` artifact. See "The source" below |

FFmpeg itself is LGPL-2.1-or-later with optional GPL parts (`ffmpeg-LICENSE.md`). This build turns on the GPL parts and links x264, so the programs as a whole are under the GPL, version 2 or later: `LICENSE.txt`.

## The files

| File | What it is |
|---|---|
| `LICENSE.txt` | The GPL, version 2: FFmpeg 9.0.2's `COPYING.GPLv2`, the license of the programs |
| `LICENSE-GPLv3.txt` | The GPL, version 3: FFmpeg 9.0.2's `COPYING.GPLv3`. The programs are under version 2 or later, so this is a version they can also be taken under; and on Windows it's the license of the `libgcc` helpers in the exe, with the exception in `gcc-runtime-exception.txt` |
| `ffmpeg-LICENSE.md` | FFmpeg 9.0.2's `LICENSE.md`: which parts of FFmpeg are under which license |
| `ffmpeg-notices.txt` | Every license notice in the source files the builds compile, and the headers those include, that isn't already word for word in a library's license text here: BSD, MIT, ISC, zlib, IJG and Boost notices in FFmpeg's own files (MIPS's fixed-point code, WebRTC's iLBC, musl's `sscanf`, the IJG's DCT, the Ogg demuxer, x86inc.asm and more), Opus's and dav1d's notices file by file, x86inc.asm's in x264, dav1d and libvpx, libvorbis's LPC code (Jutta Degener and Carsten Bormann's terms, TU-Berlin-1.0) and zlib's own. Each verbatim, with the files it's in and the builds that compile them. Then the other things those files say about licenses (code from the public domain or from another project, a license named without its text), each with the upstream license text it needs (the next four files, or a library's text here) or why it needs nothing more. `scripts/ffmpeg/build.sh` checks it against every build: see "License notices" below |
| `dsd2pcm.txt` | dsd2pcm's `LICENSE.txt` (BSD-2-Clause-Views): FFmpeg's DSD decoder (`libavcodec/dsd.c`, `dsd.h`, `dsddec.c`) is based on it. See "Code from other projects" below |
| `sha2-gifford.txt` | Aaron D. Gifford's notice for his SHA-2 code (BSD-3-Clause), which FFmpeg's `libavutil/sha.c` and `sha512.c` are based on. From Ruby's copy of his `sha2.c`: see below |
| `glumpy.txt` | Glumpy's `LICENSE.txt` (BSD-3-Clause): some of `libswscale/filters.c` derives from Glumpy |
| `libwebp.txt` | libwebp's `COPYING` (BSD-3-Clause): libvpx's `vpx_util/vpx_thread.c`, `vpx_thread.h` and `vpx_pthread.h` come from libwebp and name its `COPYING`, which libvpx doesn't have |
| `x264.txt` | x264's `COPYING` (GPL-2.0) |
| `lame.txt`, `lame-LICENSE.txt` | LAME's `COPYING` (the LGPL, version 2) and its `LICENSE` note, which asks to acknowledge LAME with a link: https://lame.sourceforge.io/ |
| `opus.txt` | Opus's `COPYING` (BSD-3-Clause, with the IETF patent grants it names) |
| `vorbis.txt`, `ogg.txt` | libvorbis's and libogg's `COPYING` (BSD-3-Clause) |
| `libvpx.txt`, `libvpx-PATENTS.txt` | libvpx's `LICENSE` (BSD-3-Clause) and `PATENTS` |
| `libvpx-x86inc.txt` | libvpx's `third_party/x86inc/LICENSE` (ISC) |
| `dav1d.txt`, `dav1d-PATENTS.txt` | dav1d's `COPYING` (BSD-2-Clause) and `doc/PATENTS` (the Alliance for Open Media patent license) |
| `zlib.txt` | zlib's `LICENSE` |
| `mingw-w64-runtime.txt` | Windows only. `COPYING.MinGW-w64-runtime.txt` from mingw-w64 v11.0.1, the runtime of the Ubuntu 24.04 toolchain that built the exe. The exe links parts of that runtime statically: its startup code (`libmingw32`) and from `libmingwex` math functions, `gdtoa` (reading and printing numbers) and its `printf`. This file holds the notices those parts ask binaries to carry. The runtime's source is in the source bundle: see "The source" |
| `gcc-runtime-exception.txt` | Windows only. The GCC Runtime Library Exception 3.1, GCC 13.2.0's `COPYING.RUNTIME`. The exe links a few helpers from GCC's `libgcc` statically (stack probing, 64-bit division, popcount), which is GPL-3.0-or-later WITH GCC-exception-3.1 (the GPLv3 is `LICENSE-GPLv3.txt`). The exception lets the result be distributed under any terms, so it doesn't change the programs' license and asks nothing more; the texts are here so that can be checked |

Every text is byte for byte the file from the pinned tarball (or, for `mingw-w64-runtime.txt` and `gcc-runtime-exception.txt`, from mingw-w64 v11.0.1 and GCC 13.2.0, and for the four texts of code from other projects, from the sources in "Code from other projects"), except `ffmpeg-notices.txt`, whose excerpts are each verbatim from the file named. `sha2-gifford.txt` is the first 35 lines of the file it's from, byte for byte; `dsd2pcm.txt` keeps its CRLF line endings.

FFmpeg's `libavcodec/jfdctfst.c`, `jfdctint_template.c` and `jrevdct.c` come from the Independent JPEG Group's software, and all three are compiled in (the MPEG-4 encoder and the decoders use them). Their terms ask binaries' documentation to say: this software is based in part on the work of the Independent JPEG Group. `ffmpeg-notices.txt` and `THIRD_PARTY_NOTICES.md` say it.

Windows: `ffmpeg.exe` and `ffprobe.exe` import only `KERNEL32.dll`, `SHELL32.dll`, `bcrypt.dll` and `msvcrt.dll`, all part of Windows. macOS (not built yet): the programs should link only macOS's own `libSystem` dynamically, plus the compiler's builtins from Xcode's clang (compiler-rt, Apache-2.0 WITH LLVM-exception, which asks nothing of binaries). The first macOS build should confirm it with `otool -L`.

## The sources

| Source | Version | License (SPDX) | Tarball | SHA-256 | Texts here |
|---|---|---|---|---|---|
| FFmpeg | 9.0.2 | GPL-2.0-or-later in this build | https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz | `8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e` | `LICENSE.txt`, `ffmpeg-LICENSE.md` |
| x264 | stable branch, commit `b35605ace3ddf7c1a5d67a2eb553f034aef41d55` (2025-06-08, X264_BUILD 165) | GPL-2.0-or-later | https://code.videolan.org/videolan/x264/-/archive/b35605ace3ddf7c1a5d67a2eb553f034aef41d55/x264-b35605ace3ddf7c1a5d67a2eb553f034aef41d55.tar.bz2 | `6eeb82934e69fd51e043bd8c5b0d152839638d1ce7aa4eea65a3fedcf83ff224` | `x264.txt` |
| LAME | 4.0 | LGPL-2.0-or-later | https://downloads.sourceforge.net/project/lame/lame/4.0/lame-4.0.tar.gz | `3df5124d5ad3a98312ffd7ba6a9b36230e4f8a3e66d3ce0f425e336c32d216eb` | `lame.txt`, `lame-LICENSE.txt` |
| Opus | 1.6.1 | BSD-3-Clause | https://downloads.xiph.org/releases/opus/opus-1.6.1.tar.gz | `6ffcb593207be92584df15b32466ed64bbec99109f007c82205f0194572411a1` | `opus.txt` |
| libogg | 1.3.6 | BSD-3-Clause | https://github.com/xiph/ogg/releases/download/v1.3.6/libogg-1.3.6.tar.xz | `5c8253428e181840cd20d41f3ca16557a9cc04bad4a3d04cce84808677fa1061` | `ogg.txt` |
| libvorbis | 1.3.7 | BSD-3-Clause | https://github.com/xiph/vorbis/releases/download/v1.3.7/libvorbis-1.3.7.tar.xz | `b33cc4934322bcbf6efcbacf49e3ca01aadbea4114ec9589d1b1e9d20f72954b` | `vorbis.txt` |
| libvpx | 1.17.0 (tag `v1.17.0`, commit `6df3ec34557879fff673706f4a1d9fbd0f3a6f0e`) | BSD-3-Clause | https://github.com/webmproject/libvpx/archive/refs/tags/v1.17.0.tar.gz | `1020f184046187baa2985dbde38e0691f49c44088bca7a1842b0236c6081dc0a` | `libvpx.txt`, `libvpx-PATENTS.txt`, `libvpx-x86inc.txt` |
| dav1d | 1.5.4 | BSD-2-Clause | https://downloads.videolan.org/pub/videolan/dav1d/1.5.4/dav1d-1.5.4.tar.xz | `686616b7c69eb88d44459391ab25cac13b6647a3b288835c5784e71c1514a5c5` | `dav1d.txt`, `dav1d-PATENTS.txt` |
| zlib | 1.3.2 | Zlib | https://zlib.net/fossils/zlib-1.3.2.tar.gz | `bb329a0a2cd0274d05519d61c667c062e06990d72e125ee2dfa8de64f0119d16` | `zlib.txt` |
| mingw-w64 runtime (Windows; not built by the script, see "The source") | 11.0.1 | ZPL-2.1 and the notices in its text | https://downloads.sourceforge.net/project/mingw-w64/mingw-w64/mingw-w64-release/mingw-w64-v11.0.1.tar.bz2 | `3f66bce069ee8bed7439a1a13da7cb91a5e67ea6170f21317ac7f5794625ee10` | `mingw-w64-runtime.txt` |

`scripts/ffmpeg/sources.json` has the same pins with their sizes, and notes how each download was checked. `ffmpeg-notices.txt` has the notices inside all of them.

How each library is built (all static, into a private prefix that pkg-config alone looks at):

- **zlib:** `./configure --static`.
- **libogg, libvorbis, Opus, LAME:** their `./configure --disable-shared --enable-static`; libvorbis without its docs, examples and ogg test, Opus without docs and extra programs, LAME with `--disable-frontend --disable-decoder --disable-analyzer-hooks --disable-gtktest`.
- **x264:** the library only, 8-bit: `--enable-static --disable-cli --bit-depth=8`, with OpenCL, AviSynth, swscale, lavf, FFMS, GPAC and L-SMASH input off.
- **libvpx:** the VP9 encoder only: `--disable-vp8 --disable-vp9-decoder --enable-vp9-encoder`, no examples, tools, docs, tests, WebM I/O or libyuv. On Windows it's made to use Win32 threads instead of winpthreads.
- **dav1d:** `meson` release build, static, without tools, tests, examples or docs.

`scripts/ffmpeg/build.sh` has every option, per target.

## Code from other projects

FFmpeg and libvpx say some of their code came from other projects under BSD licenses whose texts their files don't carry. These are those texts, each from the project's own release where it could be reached:

| File | Project | From | SHA-256 of the file here |
|---|---|---|---|
| `dsd2pcm.txt` | dsd2pcm, Sebastian Gesemann's DSD to PCM converter (2011). FFmpeg's DSD decoder names it ("based on BSD licensed dsd2pcm by Sebastian Gesemann"), with his copyright line, which matches this text | His Mercurial repository on Google Code, kept in the Google Code Archive: https://storage.googleapis.com/google-code-archive-source/v2/code.google.com/dsd2pcm/source-archive.zip (SHA-256 `f4eb2d60533af425df3a28beb9d2237906cfb81f323f1963e10e8d0d5190baf8`). Its only changeset, `00f9f2ffa7b98b53242a06a3a2b55308ff74030c` (2011-11-30, by Sebastian Gesemann, "Updated version of initial release to include a LICENSE file"), file `LICENSE.txt`. The archive's `info.txt`, signed by him, says the code is under the simplified BSD license in `LICENSE.txt` | `75fef0191d689dcdab5fa67165db8816a2ecd0fbb06e08a5cc7a02c39a6cf5e2` |
| `sha2-gifford.txt` | Aaron D. Gifford's SHA-2 code, sha2-1.0.1 (its `sha2.c` is v1.1, 2001-11-08) | The release, http://www.aarongifford.com/computers/sha2-1.0.1.tgz, couldn't be reached from the machine that wrote this, nor its copy in the Wayback Machine. So this is Ruby's copy of his `sha2.c`, lines 1 to 35 (the header comment): https://github.com/ruby/ruby/blob/v3_4_11/ext/digest/sha2/sha2.c, tag `v3_4_11`, commit `592f1ffdb36153e8be83603ade3c2e9ab6138a77`, blob `21d5acbe964287cbccf4fb73f45cbeeb1f85887c` (the whole file's SHA-256: `4be9489312b9790bdff56295c8140bd68f53663e5796922eefdc7d082aa92ccc`). Ruby took it from sha2-1.0.1 in 2002 and kept his notice word for word; its last three lines are Ruby's (`$OrigId` is his file's own version line). ldns's copy (https://github.com/NLnetLabs/ldns, commit `2867a37d5534aa1bb49c38e590420b7917657fc0`, `sha2.c`, blob `e54cc262a28c4d79684e61c36207a4862b9119a4`) has the same notice word for word, with his version line as it was and a note of its own changes added in the middle | `b5a56d1f02c6aa8bc81eda610ce7bac9e42431cb6df093839d776f3f289e0eb9` |
| `glumpy.txt` | Glumpy, Nicolas P. Rougier's. FFmpeg's `filters.c` quotes `glumpy/library/build-spatial-filters.py`: "Copyright (c) 2009-2016 Nicolas P. Rougier. All rights reserved. Distributed under the (new) BSD License." | https://github.com/glumpy/glumpy, tag `v1.2.1`, commit `4cd745b879fc4333a924a60b204ac681b6f883a7` (2023-12-04), file `LICENSE.txt` (unchanged since it was added in 2014; blob `8c12816ead575b0203201283091c6d7e64bb47e2`) | `1c14dba0558af52a4b03c5592e0ae169930481e7b83392e1baa2fa364586dc23` |
| `libwebp.txt` | libwebp, the WebP library. libvpx's `vpx_thread.h` says "Original source: https://chromium.googlesource.com/webm/libwebp" | https://chromium.googlesource.com/webm/libwebp, tag `v1.6.0`, commit `4fa21912338357f89e4fd51cf2368325b59e9bd9` (2025-07-07), file `COPYING` (unchanged since libwebp's first commit in 2010; blob `7a6f99547d4d6b4c5e6d5321acfaeba313e08de3`) | `5aec868f669e384a22372a4e8a1a6cd7d44c64cd451f960ca69cc170d1e13acf` |

`ffmpeg-notices.txt` names the text each statement needs, and why the others need nothing more.

## License notices

`scripts/ffmpeg/build.sh` checks `ffmpeg-notices.txt` against every build, before it copies the programs out. `scripts/ffmpeg/notices.py` lists every file the build compiled, from the build's own dependency records (and the headers they include), reads every comment in them, and stops the build when it finds a license notice, or another statement about a license, that `ffmpeg-notices.txt` doesn't have, or a file that has one but isn't listed under it, or a statement with no decision in `notices.py` (the upstream license text it needs, which must be in this folder, or why it needs nothing more). So a new FFmpeg version or a new component can't bring in a notice unnoticed, or a statement unread. `scripts/ffmpeg/README.md` → "License notices" says what counts and how the file is written.

`ffmpeg-notices.txt` is the union of the `linux-x86_64` and `windows-x86_64` builds of 2026-10-26: 3,220 and 3,227 files, the same but for a few of each OS's own, like Windows' thread code (Linux isn't shipped, but its build is the one the tests run). The macOS builds compile the same sources with Apple's clang, but `macos-arm64` compiles AArch64 code instead of the x86 assembly. That build hasn't run yet, so those sources were read instead (`notices.py scan`): FFmpeg's `aarch64` folders, dav1d's `src/arm`, libvpx's Arm code, x264's `common/aarch64`, Opus's `celt/arm` and `silk/arm`. That found the zlib-license notice of FFmpeg's `libavcodec/aarch64/fdctdsp_neon.S` (libjpeg-turbo's authors), and the BSD notices of dav1d's and Opus's Arm files, file by file. Their entries say `macos-arm64 (read, not built yet)`; the first Mac build checks them for real.

## The configure line

This is FFmpeg's configure line, exactly as `ffmpeg -version` prints it after `configuration:`.

Windows (`windows-x86_64`):

```
--pkg-config=pkg-config --pkg-config-flags=--static --enable-gpl --enable-static --disable-shared --disable-autodetect --disable-network --disable-doc --disable-ffplay --disable-debug --disable-everything --enable-protocol='file,pipe' --enable-demuxer='mov,matroska,avi,asf,mp3,flac,ogg,wav,aiff,gif,image_png_pipe' --enable-muxer='mp4,mov,matroska,webm,avi,asf,gif,mp3,ipod,wav,flac,ogg,opus,aiff,image2pipe,null' --enable-decoders --disable-decoder='libopus,libvorbis,libvpx_vp8,libvpx_vp9' --enable-parsers --enable-bsfs --enable-encoder='libx264,aac,libvpx_vp9,libopus,mpeg4,libmp3lame,wmv2,wmav2,gif,png,pcm_s16le,pcm_s24le,pcm_s16be,pcm_s24be,flac,libvorbis,alac,pcm_s32le,pcm_f32le' --enable-filter='scale,fps,palettegen,paletteuse,format,aformat,aresample,null,anull,transpose,hflip,vflip,rotate,crop,trim,atrim,premultiply_dynamic,sine,testsrc,pan,ebur128,volume,alimiter' --enable-indev=lavfi --enable-libx264 --enable-libmp3lame --enable-libopus --enable-libvorbis --enable-libvpx --enable-libdav1d --enable-zlib --target-os=mingw32 --arch=x86_64 --cross-prefix=x86_64-w64-mingw32- --enable-cross-compile --enable-w32threads --extra-cflags=-I../../prefix/include --extra-ldflags=-L../../prefix/lib --extra-ldflags=-static --extra-ldflags='-Wl,--no-insert-timestamp' --extra-version=grape
```

macOS (`macos-arm64` and `macos-x86_64`; not built yet, so this is what the script will pass, see below):

```
--pkg-config=pkg-config --pkg-config-flags=--static --enable-gpl --enable-static --disable-shared --disable-autodetect --disable-network --disable-doc --disable-ffplay --disable-debug --disable-everything --enable-protocol='file,pipe' --enable-demuxer='mov,matroska,avi,asf,mp3,flac,ogg,wav,aiff,gif,image_png_pipe' --enable-muxer='mp4,mov,matroska,webm,avi,asf,gif,mp3,ipod,wav,flac,ogg,opus,aiff,image2pipe,null' --enable-decoders --disable-decoder='libopus,libvorbis,libvpx_vp8,libvpx_vp9' --enable-parsers --enable-bsfs --enable-encoder='libx264,aac,libvpx_vp9,libopus,mpeg4,libmp3lame,wmv2,wmav2,gif,png,pcm_s16le,pcm_s24le,pcm_s16be,pcm_s24be,flac,libvorbis,alac,pcm_s32le,pcm_f32le' --enable-filter='scale,fps,palettegen,paletteuse,format,aformat,aresample,null,anull,transpose,hflip,vflip,rotate,crop,trim,atrim,premultiply_dynamic,sine,testsrc,pan,ebur128,volume,alimiter' --enable-indev=lavfi --enable-libx264 --enable-libmp3lame --enable-libopus --enable-libvorbis --enable-libvpx --enable-libdav1d --enable-zlib --enable-pthreads --extra-cflags=-I../../prefix/include --extra-ldflags=-L../../prefix/lib --extra-cflags='-mmacosx-version-min=11.0' --extra-ldflags='-mmacosx-version-min=11.0' --extra-version=grape
```

`--disable-everything` turns every component off, and the options after it turn back on only what Grape uses. `--disable-autodetect` keeps out anything the build machine happens to have. `--disable-network` and the two protocols `file` and `pipe` leave no network code in the programs. `ffmpeg -version` and `-buildconf` print this line, and `dist/<target>/BUILDINFO.txt` from each build records it with the toolchain's versions and the programs' SHA-256s.

## The source

`grape-ffmpeg-sources.tar` is the complete corresponding source of these programs, as the GPL asks. It goes on the same GitHub Release as the installers that carry them, so it's always next to the binaries it belongs to. An installer or DMG that a CI run built, and no release carries, has its bundle in that run's `ffmpeg-source` artifact (with its SHA-256), from the same build jobs. It holds:

- `tarballs/`: every tarball in the table above, exactly as downloaded and checked, with mingw-w64's;
- `scripts/ffmpeg/`: the build script, the pins and their README;
- `licenses/ffmpeg/`: this folder.

To build the programs again from it: unpack it, and in its folder run `GRAPE_FFMPEG_TARBALLS=$PWD/tarballs scripts/ffmpeg/build.sh <target>` on the OS that `scripts/ffmpeg/README.md` names, with the tools it lists. Nothing is downloaded. On Windows the result is byte for byte the programs on the release: two builds of one commit in different folders come out the same (`SOURCE_DATE_EPOCH=0`, no link timestamp, no build folder in the binaries), so anyone can check the binaries against this source.

The compilers aren't in the archive. On Windows, parts of two of their runtimes end up in the exe, both as Ubuntu 24.04 packages them:

- **mingw-w64's C runtime, v11.0.1** (`mingw-w64-x86-64-dev` 11.0.1-3build1): its startup code and parts of `libmingwex`. Its source is in the archive, `tarballs/mingw-w64-v11.0.1.tar.bz2`: the upstream tarball, pinned in `sources.json` and checked like the others, but not built by the script, since the toolchain brings the runtime. It's byte for byte the `orig` tarball of Ubuntu's package (the same size and SHA-256 in its `.dsc`), and Ubuntu's patches change only build files and the tools, not the runtime's source. Its notices are `mingw-w64-runtime.txt`.
- **GCC 13.2.0's `libgcc`** (`gcc-mingw-w64-x86-64-win32` 13.2.0-6ubuntu1+26.1): a few helpers (stack probing, 64-bit division, popcount). They're GPL-3.0-or-later with the GCC Runtime Library Exception 3.1 (`gcc-runtime-exception.txt`, GCC's `COPYING.RUNTIME`; the GPLv3 is `LICENSE-GPLv3.txt`). The exception lets what GCC compiles, with them in it, be distributed under any terms, so the programs stay GPL-2.0-or-later and the helpers ask nothing more. Their source is GCC 13.2.0's: https://ftp.gnu.org/gnu/gcc/gcc-13.2.0/.

Each build's `BUILDINFO.txt` names the toolchain it used.

## Not tested yet

- The macOS builds haven't run: there's no Mac in the session that wrote this. Their first run is CI's (lane A). Until then the macOS configure line above is what the script will pass, not one read from a build.
- The Windows programs have run under wine 9.0 on Linux, not yet on Windows itself.
