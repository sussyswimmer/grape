# FFmpeg and ffprobe

Grape runs `ffmpeg` and `ffprobe` as separate programs. Grape builds them itself from source, with only the parts it uses (DECISIONS D24). This folder holds their license texts, and this file says exactly what they're built from and where their complete source is.

| | |
|---|---|
| Programs | `ffmpeg` and `ffprobe` (`ffmpeg.exe` and `ffprobe.exe` on Windows) |
| Version | FFmpeg 9.0.2. `ffmpeg -version` says `9.0.2-grape`: `--extra-version=grape` marks Grape's build |
| License | GNU General Public License, version 2 or later (GPL-2.0-or-later): FFmpeg with `--enable-gpl`, for x264. Not `--enable-version3`, not `--enable-nonfree` |
| Built by | `scripts/ffmpeg/build.sh <target>` in Grape's repository, from the tarballs pinned in `scripts/ffmpeg/sources.json`, each checked by size and SHA-256 before it's unpacked |
| Targets | `windows-x86_64`: cross-compiled on Linux with mingw-w64 (GCC, win32 threads), static, only Windows' own DLLs. `macos-arm64` and `macos-x86_64`: Xcode's clang, macOS 11.0 or later, joined into one universal file |
| Source | `grape-ffmpeg-sources.tar`, on the same GitHub Release as Grape's installers. See "The source" below |

FFmpeg itself is LGPL-2.1-or-later with optional GPL parts (`ffmpeg-LICENSE.md`). This build turns on the GPL parts and links x264, so the programs as a whole are under the GPL, version 2 or later: `LICENSE.txt`.

## The files

| File | What it is |
|---|---|
| `LICENSE.txt` | The GPL, version 2: FFmpeg 9.0.2's `COPYING.GPLv2`, the license of the programs |
| `ffmpeg-LICENSE.md` | FFmpeg 9.0.2's `LICENSE.md`: which parts of FFmpeg are under which license |
| `ffmpeg-ijg.txt` | The Independent JPEG Group's notice. FFmpeg's `libavcodec/jfdctfst.c`, `jfdctint_template.c` and `jrevdct.c` come from the IJG's software, and all three are compiled in (the MPEG-4 encoder and the decoders use them). Their terms ask binaries' documentation to say: this software is based in part on the work of the Independent JPEG Group. `THIRD_PARTY_NOTICES.md` says it too. Grape builds those files exactly as FFmpeg 9.0.2 has them |
| `x264.txt` | x264's `COPYING` (GPL-2.0) |
| `lame.txt`, `lame-LICENSE.txt` | LAME's `COPYING` (the LGPL, version 2) and its `LICENSE` note, which asks to acknowledge LAME with a link: https://lame.sourceforge.io/ |
| `opus.txt` | Opus's `COPYING` (BSD-3-Clause, with the IETF patent grants it names) |
| `vorbis.txt`, `ogg.txt` | libvorbis's and libogg's `COPYING` (BSD-3-Clause) |
| `libvpx.txt`, `libvpx-PATENTS.txt` | libvpx's `LICENSE` (BSD-3-Clause) and `PATENTS` |
| `libvpx-x86inc.txt` | libvpx's `third_party/x86inc/LICENSE` (ISC) |
| `dav1d.txt`, `dav1d-PATENTS.txt` | dav1d's `COPYING` (BSD-2-Clause) and `doc/PATENTS` (the Alliance for Open Media patent license) |
| `zlib.txt` | zlib's `LICENSE` |
| `x86inc.txt` | The ISC notice of `x86inc.asm`, x264's assembly macro layer, which FFmpeg, x264, dav1d and libvpx each carry a copy of and assemble into the x86_64 builds (Windows, Intel Mac): the four notices verbatim, which differ only in their years |
| `mingw-w64-runtime.txt` | Windows only. `COPYING.MinGW-w64-runtime.txt` from mingw-w64 v11.0.1, the runtime of the Ubuntu 24.04 toolchain that built the exe. The exe links parts of that runtime statically: its startup code (`libmingw32`) and from `libmingwex` math functions, `gdtoa` (reading and printing numbers) and its `printf`. This file holds the notices those parts ask binaries to carry |
| `gcc-runtime-exception.txt` | Windows only. The GCC Runtime Library Exception 3.1, GCC 13.2.0's `COPYING.RUNTIME`. The exe links a few helpers from GCC's `libgcc` statically (stack probing, 64-bit division, popcount), which is GPL-3.0-or-later WITH GCC-exception-3.1. The exception lets the result be distributed under any terms, so it doesn't change the programs' license and asks nothing more; the text is here so that can be checked |

Every text is byte for byte the file from the pinned tarball (or, for the last two, from mingw-w64 v11.0.1 and GCC 13.2.0). `ffmpeg-ijg.txt` and `x86inc.txt` are excerpts: a short note, then the notices verbatim.

Windows: `ffmpeg.exe` and `ffprobe.exe` import only `KERNEL32.dll`, `SHELL32.dll`, `bcrypt.dll` and `msvcrt.dll`, all part of Windows. macOS (not built yet): the programs should link only macOS's own `libSystem` dynamically, plus the compiler's builtins from Xcode's clang (compiler-rt, Apache-2.0 WITH LLVM-exception, which asks nothing of binaries). The first macOS build should confirm it with `otool -L`.

## The sources

| Source | Version | License (SPDX) | Tarball | SHA-256 | Texts here |
|---|---|---|---|---|---|
| FFmpeg | 9.0.2 | GPL-2.0-or-later in this build | https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz | `8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e` | `LICENSE.txt`, `ffmpeg-LICENSE.md`, `ffmpeg-ijg.txt` |
| x264 | stable branch, commit `b35605ace3ddf7c1a5d67a2eb553f034aef41d55` (2025-06-08, X264_BUILD 165) | GPL-2.0-or-later | https://code.videolan.org/videolan/x264/-/archive/b35605ace3ddf7c1a5d67a2eb553f034aef41d55/x264-b35605ace3ddf7c1a5d67a2eb553f034aef41d55.tar.bz2 | `6eeb82934e69fd51e043bd8c5b0d152839638d1ce7aa4eea65a3fedcf83ff224` | `x264.txt` |
| LAME | 4.0 | LGPL-2.0-or-later | https://downloads.sourceforge.net/project/lame/lame/4.0/lame-4.0.tar.gz | `3df5124d5ad3a98312ffd7ba6a9b36230e4f8a3e66d3ce0f425e336c32d216eb` | `lame.txt`, `lame-LICENSE.txt` |
| Opus | 1.6.1 | BSD-3-Clause | https://downloads.xiph.org/releases/opus/opus-1.6.1.tar.gz | `6ffcb593207be92584df15b32466ed64bbec99109f007c82205f0194572411a1` | `opus.txt` |
| libogg | 1.3.6 | BSD-3-Clause | https://github.com/xiph/ogg/releases/download/v1.3.6/libogg-1.3.6.tar.xz | `5c8253428e181840cd20d41f3ca16557a9cc04bad4a3d04cce84808677fa1061` | `ogg.txt` |
| libvorbis | 1.3.7 | BSD-3-Clause | https://github.com/xiph/vorbis/releases/download/v1.3.7/libvorbis-1.3.7.tar.xz | `b33cc4934322bcbf6efcbacf49e3ca01aadbea4114ec9589d1b1e9d20f72954b` | `vorbis.txt` |
| libvpx | 1.17.0 (tag `v1.17.0`, commit `6df3ec34557879fff673706f4a1d9fbd0f3a6f0e`) | BSD-3-Clause | https://github.com/webmproject/libvpx/archive/refs/tags/v1.17.0.tar.gz | `1020f184046187baa2985dbde38e0691f49c44088bca7a1842b0236c6081dc0a` | `libvpx.txt`, `libvpx-PATENTS.txt`, `libvpx-x86inc.txt` |
| dav1d | 1.5.4 | BSD-2-Clause | https://downloads.videolan.org/pub/videolan/dav1d/1.5.4/dav1d-1.5.4.tar.xz | `686616b7c69eb88d44459391ab25cac13b6647a3b288835c5784e71c1514a5c5` | `dav1d.txt`, `dav1d-PATENTS.txt` |
| zlib | 1.3.2 | Zlib | https://zlib.net/fossils/zlib-1.3.2.tar.gz | `bb329a0a2cd0274d05519d61c667c062e06990d72e125ee2dfa8de64f0119d16` | `zlib.txt` |

`scripts/ffmpeg/sources.json` has the same pins with their sizes, and notes how each download was checked.

How each library is built (all static, into a private prefix that pkg-config alone looks at):

- **zlib:** `./configure --static`.
- **libogg, libvorbis, Opus, LAME:** their `./configure --disable-shared --enable-static`; libvorbis without its docs, examples and ogg test, Opus without docs and extra programs, LAME with `--disable-frontend --disable-decoder --disable-analyzer-hooks --disable-gtktest`.
- **x264:** the library only, 8-bit: `--enable-static --disable-cli --bit-depth=8`, with OpenCL, AviSynth, swscale, lavf, FFMS, GPAC and L-SMASH input off.
- **libvpx:** the VP9 encoder only: `--disable-vp8 --disable-vp9-decoder --enable-vp9-encoder`, no examples, tools, docs, tests, WebM I/O or libyuv. On Windows it's made to use Win32 threads instead of winpthreads.
- **dav1d:** `meson` release build, static, without tools, tests, examples or docs.

`scripts/ffmpeg/build.sh` has every option, per target.

## The configure line

This is FFmpeg's configure line, exactly as `ffmpeg -version` prints it after `configuration:`.

Windows (`windows-x86_64`):

```
--pkg-config=pkg-config --pkg-config-flags=--static --enable-gpl --enable-static --disable-shared --disable-autodetect --disable-network --disable-doc --disable-ffplay --disable-debug --disable-everything --enable-protocol='file,pipe' --enable-demuxer='mov,matroska,avi,asf,mp3,flac,ogg,wav,aiff,gif,image_png_pipe' --enable-muxer='mp4,mov,matroska,webm,avi,asf,gif,mp3,ipod,wav,flac,ogg,opus,aiff,image2pipe' --enable-decoders --disable-decoder='libopus,libvorbis,libvpx_vp8,libvpx_vp9' --enable-parsers --enable-bsfs --enable-encoder='libx264,aac,libvpx_vp9,libopus,mpeg4,libmp3lame,wmv2,wmav2,gif,png,pcm_s16le,pcm_s24le,pcm_s16be,pcm_s24be,flac,libvorbis' --enable-filter='scale,fps,palettegen,paletteuse,format,aformat,aresample,null,anull,transpose,hflip,vflip,rotate,crop,trim,atrim,premultiply_dynamic,sine,testsrc,pan' --enable-indev=lavfi --enable-libx264 --enable-libmp3lame --enable-libopus --enable-libvorbis --enable-libvpx --enable-libdav1d --enable-zlib --target-os=mingw32 --arch=x86_64 --cross-prefix=x86_64-w64-mingw32- --enable-cross-compile --enable-w32threads --extra-cflags=-I../../prefix/include --extra-ldflags=-L../../prefix/lib --extra-ldflags=-static --extra-ldflags='-Wl,--no-insert-timestamp' --extra-version=grape
```

macOS (`macos-arm64` and `macos-x86_64`; not built yet, so this is what the script will pass, see below):

```
--pkg-config=pkg-config --pkg-config-flags=--static --enable-gpl --enable-static --disable-shared --disable-autodetect --disable-network --disable-doc --disable-ffplay --disable-debug --disable-everything --enable-protocol='file,pipe' --enable-demuxer='mov,matroska,avi,asf,mp3,flac,ogg,wav,aiff,gif,image_png_pipe' --enable-muxer='mp4,mov,matroska,webm,avi,asf,gif,mp3,ipod,wav,flac,ogg,opus,aiff,image2pipe' --enable-decoders --disable-decoder='libopus,libvorbis,libvpx_vp8,libvpx_vp9' --enable-parsers --enable-bsfs --enable-encoder='libx264,aac,libvpx_vp9,libopus,mpeg4,libmp3lame,wmv2,wmav2,gif,png,pcm_s16le,pcm_s24le,pcm_s16be,pcm_s24be,flac,libvorbis' --enable-filter='scale,fps,palettegen,paletteuse,format,aformat,aresample,null,anull,transpose,hflip,vflip,rotate,crop,trim,atrim,premultiply_dynamic,sine,testsrc,pan' --enable-indev=lavfi --enable-libx264 --enable-libmp3lame --enable-libopus --enable-libvorbis --enable-libvpx --enable-libdav1d --enable-zlib --enable-pthreads --extra-cflags=-I../../prefix/include --extra-ldflags=-L../../prefix/lib --extra-cflags='-mmacosx-version-min=11.0' --extra-ldflags='-mmacosx-version-min=11.0' --extra-version=grape
```

`--disable-everything` turns every component off, and the options after it turn back on only what Grape uses. `--disable-autodetect` keeps out anything the build machine happens to have. `--disable-network` and the two protocols `file` and `pipe` leave no network code in the programs. `ffmpeg -version` and `-buildconf` print this line, and `dist/<target>/BUILDINFO.txt` from each build records it with the toolchain's versions and the programs' SHA-256s.

## The source

`grape-ffmpeg-sources.tar` is the complete corresponding source of these programs, as the GPL asks. It goes on the same GitHub Release as the installers that carry them, so it's always next to the binaries it belongs to. It holds:

- `tarballs/`: every tarball in the table above, exactly as downloaded and checked;
- `scripts/ffmpeg/`: the build script, the pins and their README;
- `licenses/ffmpeg/`: this folder.

To build the programs again from it: unpack it, and in its folder run `GRAPE_FFMPEG_TARBALLS=$PWD/tarballs scripts/ffmpeg/build.sh <target>` on the OS that `scripts/ffmpeg/README.md` names, with the tools it lists. Nothing is downloaded. On Windows the result is byte for byte the programs on the release: two builds of one commit in different folders come out the same (`SOURCE_DATE_EPOCH=0`, no link timestamp, no build folder in the binaries), so anyone can check the binaries against this source.

The compilers aren't in the archive, and neither are their runtimes. On Windows, parts of two runtimes end up in the exe: mingw-w64's C runtime, v11.0.1 (https://downloads.sourceforge.net/project/mingw-w64/mingw-w64/mingw-w64-release/mingw-w64-v11.0.1.tar.bz2, SHA-256 `3f66bce069ee8bed7439a1a13da7cb91a5e67ea6170f21317ac7f5794625ee10`), and GCC 13.2.0's `libgcc` (https://ftp.gnu.org/gnu/gcc/gcc-13.2.0/), both as Ubuntu 24.04 packages them (`mingw-w64-x86-64-dev` 11.0.1-3build1, `gcc-mingw-w64-x86-64-win32` 13.2.0-6ubuntu1+26.1). They're the compiler's own runtime: system libraries in the GPL's sense, which the corresponding source doesn't include (GPLv3 section 1; the programs are GPL-2.0-or-later). Their notices are in this folder, and each build's `BUILDINFO.txt` names the toolchain it used.

## Not tested yet

- The macOS builds haven't run: there's no Mac in the session that wrote this. Their first run is CI's (lane A). Until then the macOS configure line above is what the script will pass, not one read from a build.
- The Windows programs have run under wine 9.0 on Linux, not yet on Windows itself.
