# scripts/ffmpeg/

Grape builds its own FFmpeg (DECISIONS D24): `ffmpeg` and `ffprobe` with only what Grape uses, static, with networking compiled out, under GPL-2.0-or-later.

- `sources.json` pins every source tarball: URL, size, SHA-256, license. Change a pin here, never in the script.
- `build.sh <target>` downloads and checks the tarballs, builds the libraries and FFmpeg, checks the result and writes `dist/`.

## Targets

| Target | Runs on | Output |
|---|---|---|
| `windows-x86_64` | Linux, cross-compiled with mingw-w64 (win32 thread model) | `dist/windows-x86_64/ffmpeg.exe`, `ffprobe.exe`: static, only Windows' own DLLs |
| `macos-arm64` | an Apple silicon Mac, Xcode's clang | `dist/macos-arm64/ffmpeg`, `ffprobe`: macOS 11.0 or later |
| `macos-x86_64` | an Intel Mac, Xcode's clang | `dist/macos-x86_64/ffmpeg`, `ffprobe`: macOS 11.0 or later |
| `linux-x86_64` | x86_64 Linux | `dist/linux-x86_64/ffmpeg`, `ffprobe`: for local testing only (the engine tests); Grape doesn't ship for Linux |

Each run also writes `dist/<target>/BUILDINFO.txt` (the FFmpeg version, the configure line as `ffmpeg -version` prints it, every source's pin and SHA-256, the toolchain's versions and the outputs' SHA-256s) and `dist/grape-ffmpeg-sources.tar`: every tarball exactly as downloaded, this folder and `licenses/ffmpeg/`. That tar is the corresponding source the GPL asks for, and it goes on the GitHub Release with the binaries. It's the same for every target.

The macOS targets build natively, one per runner; the script doesn't cross-compile between Mac architectures. The two builds are joined with `lipo` into one universal file, as `scripts/fetch-sidecars.sh` does today.

## Running it

```
scripts/ffmpeg/build.sh windows-x86_64
```

from anywhere; it finds the repo from its own path. It deletes and remakes `build/ffmpeg/<target>/`, so a run always starts clean, and writes `dist/`. It touches nothing else, and it refuses to delete a `build/ffmpeg/<target>/` it didn't make.

| Variable | Default | |
|---|---|---|
| `GRAPE_FFMPEG_WORK` | `build/ffmpeg` | Build trees, about 500 MB per target. Must be a plain path: no spaces |
| `GRAPE_FFMPEG_DIST` | `dist` | Outputs |
| `GRAPE_FFMPEG_TARBALLS` | none | A folder that already holds the tarballs: a cache, or an unpacked source bundle's `tarballs/`. Files found there are copied instead of downloaded and checked the same way |
| `GRAPE_FFMPEG_JOBS` | the CPU count | Parallel jobs |

`build/` and `dist/` are in `.gitignore`. Each step's output is in `build/ffmpeg/<target>/logs/`; when a step fails, the script shows the end of its log.

## What each OS needs

**Linux** (tested on Ubuntu 24.04; CI should use the same image, so the Windows build's mingw-w64 runtime and GCC match `licenses/ffmpeg/`):

```
sudo apt-get install --no-install-recommends build-essential nasm meson ninja-build \
  pkg-config python3 curl ca-certificates xz-utils bzip2
```

For `windows-x86_64`, also:

```
sudo apt-get install --no-install-recommends gcc-mingw-w64-x86-64-win32 \
  binutils-mingw-w64-x86-64 mingw-w64-x86-64-dev
```

The compiler must use the win32 thread model, which keeps winpthreads (and its DLL) out of the exe. The script checks. If `x86_64-w64-mingw32-gcc` points at the posix variant: `sudo update-alternatives --set x86_64-w64-mingw32-gcc /usr/bin/x86_64-w64-mingw32-gcc-win32`.

**macOS:** the Xcode Command Line Tools (`xcode-select --install`), then `brew install nasm meson ninja pkgconf python`. nasm is for `macos-x86_64` only; the rest for both. curl, tar and bzip2 come with macOS.

The script needs nothing else, never uses `sudo`, and changes no settings. pkg-config only sees the private prefix it builds into (`PKG_CONFIG_LIBDIR`), so libraries installed on the machine can't sneak in.

## What it builds

The libraries, in order, each as a static library: zlib, libogg, libvorbis, Opus, LAME (no decoder, no frontend), x264 (8-bit, library only), libvpx (the VP9 encoder only), dav1d (library only). On Windows, libvpx is made to use Win32 threads instead of winpthreads (a `pthread.h` that doesn't compile, so its configure picks the path it uses with MSVC), and x264 and FFmpeg use Win32 threads by their own options. On macOS, libvorbis's `configure` loses `-force_cpusubtype_ALL`, an option of Apple's old linker; that's the only change the script makes to a source.

Then FFmpeg with `--disable-everything` and only these turned back on:

- **protocols:** `file` and `pipe`, and `--disable-network`, so the binary itself can't open a URL (golden rule 3);
- **demuxers:** the ten Grape reads (`mov matroska avi asf mp3 flac ogg wav aiff gif`) and `image_png_pipe` for the GIF palette;
- **muxers:** the fifteen Grape writes (`mp4 mov matroska webm avi asf gif mp3 ipod wav flac ogg opus aiff`) and `image2pipe`;
- **all decoders, parsers and bitstream filters**, because people drop every kind of file and copying a stream needs the bitstream filters; except the Opus, Vorbis and VPx decoders of the libraries, which FFmpeg's own decoders come before anyway;
- **encoders:** exactly the sixteen in `the_sidecar_has_everything_grape_uses_and_is_redistributable`, and `h263`, which the `mpeg4` encoder is built on;
- **filters:** the ones Grape passes (`scale fps palettegen paletteuse format aformat aresample null anull split`), the ones ffmpeg inserts by itself (`transpose hflip vflip rotate` turn rotated video upright, `crop` applies a container's crop, `trim atrim` cut, `premultiply_dynamic` converts alpha; FFmpeg's configure selects most of them for the ffmpeg program anyway), and `sine testsrc pan` with the `lavfi` input device for the engine tests, which make their sources with this ffmpeg. Grape's own argv can't reach `lavfi`: its `-format_whitelist` lists only file demuxers.

The script checks the result before writing anything to `dist/`: the generated component lists must match exactly, `CONFIG_NETWORK` must be 0, the license GPL without version3 or nonfree; on Linux and macOS `ffmpeg -version` must print the configure line from `config.h` and `ffmpeg -protocols` must list only `file` and `pipe`; on Windows every DLL the exe imports must be part of Windows.

## Measured

On a shared 4-core container (2026-10-26, Ubuntu 24.04, GCC 13.3, mingw-w64 GCC 13.2 with runtime 11.0.1), from tarballs already downloaded:

| Target | Whole run | FFmpeg alone | ffmpeg | ffprobe |
|---|---|---|---|---|
| `linux-x86_64` | 524 s | 305 s | 27,975,184 bytes | 27,762,128 bytes |
| `windows-x86_64` | 615 s | 386 s | 28,008,960 bytes (`ffmpeg.exe`) | 27,811,328 bytes (`ffprobe.exe`) |

The Windows exes are PE32+ x86-64 and import only `KERNEL32.dll`, `SHELL32.dll`, `bcrypt.dll` and `msvcrt.dll`. They ran under wine 9.0 (`-version`, `-protocols`, MP4 to WebM, MP3 and GIF with Grape's own arguments), not yet on Windows itself. The source bundle is 34.9 MB.

The macOS targets haven't run yet (no Mac here). Their settings are the Linux ones with Xcode's clang, `-mmacosx-version-min=11.0` and the `aarch64-apple-darwin`/`x86_64-apple-darwin` hosts for autotools; the first CI run is their test.

## Changing a pin

1. Pick the newest release at least 14 days old (D9), or a newer one for a security fix.
2. Download it and change its entry in `sources.json`: version, `released`, URL, file name, size, SHA-256 and the folder it unpacks to.
3. Build every target, run the `engines-it` tests against the Linux build, and compare the library's license files with `licenses/ffmpeg/`. Update the texts, `licenses/ffmpeg/SOURCE.md` and the FFmpeg section of `about.hbs` (then regenerate `THIRD_PARTY_NOTICES.md`).
