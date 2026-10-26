# libarchive (bsdtar)

Grape reads RAR archives with `bsdtar`, the tar program from [libarchive](https://www.libarchive.org/). It's used only as `bsdtar -c -P -f - --format pax @-`: Grape writes the RAR into its stdin through a pipe, and it writes a tar to stdout, which Grape checks and re-packs. It never writes to disk. It ships as a separate program: `bsdtar.exe` next to `Grape.exe` on Windows, and `Grape.app/Contents/MacOS/bsdtar` on macOS. DECISIONS D27 has why and how.

- **Version:** libarchive 3.8.10 (released 2026-10-14; tag `v3.8.10` is commit `0c48412b`) plus Grape's patch series below. Its version line is `bsdtar 3.8.10-grape.1 - libarchive 3.8.10 libb2/bundled ` on Windows and `bsdtar 3.8.10-grape.1 - libarchive 3.8.10 CommonCrypto/system libb2/bundled ` on macOS (trailing space included).
- **License:** BSD-2-Clause, with a few parts under other permissive terms (the table below). Nothing in it is GPL or LGPL, and it links no other library.

## The files here

The texts come from the pinned tarball, with LF line endings. The notices taken from source files are copied without their C comment markers.

| File | What it covers | License |
|---|---|---|
| `LICENSE.txt` | libarchive and bsdtar: the tarball's `COPYING`, verbatim, with its summary of the per-file licenses | BSD-2-Clause |
| `copyright-lines.txt` | Every distinct copyright line of the source files compiled into bsdtar. `COPYING`'s notice reads `Copyright (c) 2003-2018 <author(s)>`, a template, so this list names the holders (below) | BSD-2-Clause, mostly |
| `regents-bsd-3-clause.txt` | The University of California's code: the compress filters, strmode (in `archive_entry.c`), umask and setmode | BSD-3-Clause |
| `netbsd-bsd-2-clause.txt` | The NetBSD Foundation's code: the red-black tree and device numbers | BSD-2-Clause |
| `postgresql.txt` | The Windows error-code table, from PostgreSQL's `win32error.c` (`archive_windows.c`, `tar/bsdtar_windows.c`). It's compiled into the Windows build only; the folder is the same for both OSes | PostgreSQL License |
| `blake2.txt` | BLAKE2s, bundled (libb2 isn't linked) | CC0-1.0, which Grape takes, or OpenSSL or Apache-2.0 |
| `unicode.txt` | The composition tables in `archive_string_composition.h`, generated from Unicode 6.0's `UnicodeData.txt`. Kept as a courtesy | Unicode-3.0 |
| `openbsd-isc.txt` | OpenBSD's arc4random (`archive_random.c`) and readpassphrase (`libarchive_fe/passphrase.c`). Each is compiled only where the system lacks its own (Windows uses its own code for both, macOS has both), so the Windows and macOS builds normally contain neither. The files are compiled, so the notices are kept | ISC |

Parts that need no notice:

- **Public domain:** PPMd (`archive_ppmd7.c`, `archive_ppmd8.c`; Igor Pavlov and Dmitry Shkarin) and the date parser (`archive_parse_date.c`).
- **0BSD:** the branch/call/jump filters from XZ Embedded and XZ Utils in the 7-Zip reader (`archive_read_support_format_7zip.c`).
- **The Visual C++ runtime:** the Windows build links it statically (`/MT`), which needs no notice under Visual Studio's redistribution terms.
- **macOS** links only `/usr/lib/libSystem.B.dylib`; CommonCrypto is part of it.

### How `copyright-lines.txt` is made

`scripts/bsdtar/build.sh` (`copyright_lines`) and `build.ps1` (`Get-CopyrightLines`) regenerate it from the extracted, patched source in every full build, and fail when it differs from this folder's copy byte for byte: the pin moved and the notices didn't. Both do exactly this:

1. **The files:** those `libarchive/CMakeLists.txt` lists in `include_HEADERS` and `libarchive_SOURCES` (its `SET(…)` block plus every `LIST(APPEND …)`), without `archive_disk_acl_*` (ACLs are off), and those `tar/CMakeLists.txt` lists in `bsdtar_SOURCES` (which brings in `libarchive_fe/` and, on Windows, `tar/bsdtar_windows.c`): `.c` and `.h` files only, each read as UTF-8.
2. **The lines:** after any leading comment marker (`/*`, `*`, `//`, `#`) and spaces, a line counts when it starts with `Copyright` or `Portions Copyright`, a space, then `(c)`, `(C)`, `©` or a digit. Trailing spaces and `*/` are dropped.
3. **The file:** a one-line heading, a blank line, then the distinct lines sorted by code point, with LF line endings.

A holder written on the line after its `Copyright` line (the Regents, Todd C. Miller) is named in the notice file for that code.

## The source

| What | Where | SHA-256 | Size (bytes) |
|---|---|---|---|
| libarchive 3.8.10 | [`libarchive-3.8.10.tar.gz`](https://github.com/libarchive/libarchive/releases/download/v3.8.10/libarchive-3.8.10.tar.gz), the GitHub release; [libarchive.org/downloads](https://www.libarchive.org/downloads/libarchive-3.8.10.tar.gz) has the same file (its `.sums.txt` matches) | `a1bffbe5152cfe869751179e7b16648c87c461cb8c46eee75062efbe164cc57c` | 9,301,407 |

`scripts/bsdtar/sources.json` pins it, and the build scripts check its size and SHA-256 before anything is extracted. 3.8.10 carries the fix for the silent truncation of a RAR5 multi-volume file (PR #3414, `dec66c38` on `patch/3.8`) and the RAR5 bid above the seekable-ZIP reader (#3412, `ac1aad27`).

### Grape's patches

In `scripts/bsdtar/patches/`, applied in this order with `git apply`. Each SHA-256 is checked against `sources.json` first. The numbering skips 0004 on purpose (below).

| File | What | From | SHA-256 |
|---|---|---|---|
| `0001-b59b9e16-tar-check-pathname-guard-outside-regex-if.patch` | bsdtar skips an entry whose name can't be converted, with a warning, instead of crashing on a NULL name | upstream b59b9e16 (PR #3696, merged as `8c4d7d77` on 2026-10-18), **changed:** its guard sits outside `#if HAVE_REGEX_H`, which compiled it out of MSVC builds | `621028e08cfc17dc5f02f66797c88ae831304ed5d72969590c1f415c575f0546` |
| `0002-777dc1a0-string-report-EILSEQ-on-windows-conversions.patch` | Windows wide-to-narrow name conversions report EILSEQ instead of failing badly (#3681, #3695) | upstream 777dc1a (PR #3696, merged 2026-10-18), **changed:** the library hunk only; its four test-file hunks are dropped (tests aren't built) | `3e55fd226e80fc824348565544c0506085dc63383a8bf90a8a55715753a6e63a` |
| `0003-ef743732-rar-bound-stored-symlink-target-size.patch` | A RAR4 symlink target is at most 1 MiB | upstream ef743732 (PR #3709, merged as `6066a428` on 2026-10-18), unchanged | `780ea7e11d6056e8946513c065d9250896bb36a4b84d6e4c0ece7d3f02489dc5` |
| `0005-grape-tar-utf8-active-code-page-manifest.patch` | `activeCodePage` UTF-8 in bsdtar's Windows manifest, so every RAR name converts | Grape | `775bbb5498c2fd03f72dea1c6dc562703bf7b81a5f83059386c4820e1b304288` |
| `0006-grape-tar-at-archive-rar-and-rar5-only.patch` | `@archive` reads only RAR and RAR5: no other format, no decompression filter (none can start a program) and no `mtree:checkfs` (no input can make bsdtar open a local file) | Grape | `d58dee199634da6c4d941a3d4ebf34cbe57bec22b8633f2f9cdcede3e4f68268` |
| `0007-grape-tar-version-marker.patch` | The version line reads `bsdtar 3.8.10-grape.1 - …`, the marker Grape's bundling gate looks for. `.1` goes up with every change to the series, here and in `sources.json` | Grape | `f3a399c95fc03ff135d56c9c027bead689cfe23141f2ebe352523d6295f63bd2` |

**Not carried:** upstream e1d79210 (PR #3707, merged as `d44270eb` on 2026-10-21), which reports `@archive` data warnings. No RAR path reaches it once 0006 is in: neither RAR reader returns a warning from its data reads in 3.8.10. It would have been 0004.

## How it's built

`scripts/bsdtar/build.ps1` (Windows) and `build.sh` (macOS, and Linux for local tests) extract the tarball, apply the patches and build only the `bsdtar` target, with every optional library off. The exact CMake lines:

Windows x64 (MSVC, the newest Visual Studio CMake finds, static CRT; `build.ps1` passes the same switches):

```
cmake -S <src> -B <build> -A x64 \
  -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded -DMSVC_USE_STATIC_CRT=ON \
  -DBUILD_SHARED_LIBS=OFF -DENABLE_TAR=ON -DENABLE_TAR_SHARED=OFF \
  -DENABLE_CPIO=OFF -DENABLE_CAT=OFF -DENABLE_UNZIP=OFF \
  -DENABLE_TEST=OFF -DENABLE_INSTALL=OFF -DENABLE_COVERAGE=OFF -DENABLE_WERROR=OFF \
  -DENABLE_OPENSSL=OFF -DENABLE_MBEDTLS=OFF -DENABLE_NETTLE=OFF -DENABLE_CNG=OFF -DENABLE_MD=OFF \
  -DENABLE_ZLIB=OFF -DENABLE_BZip2=OFF -DENABLE_LZMA=OFF -DENABLE_ZSTD=OFF \
  -DENABLE_LZ4=OFF -DENABLE_LZO=OFF -DENABLE_LIBB2=OFF \
  -DENABLE_LIBXML2=OFF -DENABLE_BSDXML=OFF -DENABLE_EXPAT=OFF -DENABLE_WIN32_XMLLITE=OFF \
  -DENABLE_PCREPOSIX=OFF -DENABLE_PCRE2POSIX=OFF -DPOSIX_REGEX_LIB=NONE -DENABLE_LIBGCC=OFF \
  -DENABLE_ICONV=OFF -DENABLE_ACL=OFF -DENABLE_XATTR=OFF \
  -DWINDOWS_VERSION=WIN10
cmake --build <build> --config Release --target bsdtar
```

macOS, once for `arm64` and once for `x86_64`, then merged with `lipo -create`; all three files are signed ad hoc:

```
cmake -S <src> -B <build-arch> \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_OSX_ARCHITECTURES=<arch> -DCMAKE_OSX_DEPLOYMENT_TARGET=13.0 \
  -DCMAKE_FIND_FRAMEWORK=NEVER -DCMAKE_FIND_APPBUNDLE=NEVER \
  -DCMAKE_IGNORE_PREFIX_PATH="/opt/homebrew;/usr/local" \
  -DBUILD_SHARED_LIBS=OFF -DENABLE_TAR=ON -DENABLE_TAR_SHARED=OFF \
  -DENABLE_CPIO=OFF -DENABLE_CAT=OFF -DENABLE_UNZIP=OFF \
  -DENABLE_TEST=OFF -DENABLE_INSTALL=OFF -DENABLE_COVERAGE=OFF -DENABLE_WERROR=OFF \
  -DENABLE_OPENSSL=OFF -DENABLE_MBEDTLS=OFF -DENABLE_NETTLE=OFF -DENABLE_MD=OFF \
  -DENABLE_ZLIB=OFF -DENABLE_BZip2=OFF -DENABLE_LZMA=OFF -DENABLE_ZSTD=OFF \
  -DENABLE_LZ4=OFF -DENABLE_LZO=OFF -DENABLE_LIBB2=OFF \
  -DENABLE_LIBXML2=OFF -DENABLE_BSDXML=OFF -DENABLE_EXPAT=OFF \
  -DENABLE_PCREPOSIX=OFF -DENABLE_PCRE2POSIX=OFF -DPOSIX_REGEX_LIB=LIBC -DENABLE_LIBGCC=OFF \
  -DENABLE_ICONV=OFF -DENABLE_ACL=OFF -DENABLE_XATTR=OFF
cmake --build <build-arch> --target bsdtar -j <cores>
```

Each CI artifact carries a `BUILDINFO.txt` with the toolchain it was built with (CMake, the compiler, the SDK and the OS), the hashes of `sources.json` and every patch, and a `SHA256SUMS` of the binaries.

## Where the binary comes from

CI's `bsdtar` workflow (`.github/workflows/bsdtar.yml`) builds it in the same run as the installer or DMG that bundles it, checks it (its imports, its manifest or architectures, the exact version line, these copyright lines) and smoke-tests it in Grape's exact environment. There is no GitHub Release of it until M7; from then on, each Grape release carries the libarchive tarball, the patches and the build scripts next to the installer.
