# PDFium

Grape reads PDFs with PDFium, Google's PDF library (the one in Chrome). It ships as a separate library that Grape's PDF helper process loads: `pdfium.dll` next to `Grape.exe` on Windows, and `Grape.app/Contents/Frameworks/libpdfium.dylib` on macOS. Grape doesn't change it. DECISIONS D29 has why and how.

- **Version:** PDFium 157.0.8086.0, from the `chromium/8086` branch.
- **Build:** [bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries), release [`chromium/8086`](https://github.com/bblanchon/pdfium-binaries/releases/tag/chromium/8086) (2026-10-05). It's built without V8 (no JavaScript) and without XFA forms.
- **License:** PDFium is BSD-3-Clause. Some of its files are under Apache-2.0, so `pdfium.txt` has both texts. The build's own packaging is MIT (`LICENSE.txt`). Everything built into it is under a permissive license (the table below). Nothing in it is GPL or LGPL.

## The files here

These are the license files from the Windows archive, `pdfium-win-x64.tgz`: its `LICENSE` as `LICENSE.txt` and its `licenses/` folder. Only the line endings were changed, to LF. The Mac archive has the same texts, except that its `pdfium.txt` has `// ` at the start of every line (the builder's license step uses a `sed` pattern that BSD sed doesn't read the same way), so the Windows set is the one kept.

| File | What it covers | License |
|---|---|---|
| `LICENSE.txt` | pdfium-binaries, the build scripts and packaging (Benoit Blanchon) | MIT |
| `pdfium.txt` | PDFium itself | BSD-3-Clause, and Apache-2.0 for some files |
| `abseil.txt` | Abseil (C++ common libraries) | Apache-2.0 |
| `agg23.txt` | Anti-Grain Geometry 2.3 (vector rendering) | The AGG 2.3 notice: permissive, the notice stays with copies |
| `dragonbox-Apache2-LLVM.txt`, `dragonbox-Boost.txt` | Dragonbox (number formatting) | Apache-2.0 WITH LLVM-exception, or BSL-1.0. Grape takes Apache-2.0 WITH LLVM-exception |
| `fast_float.txt` | fast_float (number parsing) | MIT |
| `freetype.txt` | FreeType 2.14.3, Chromium's copy (fonts) | FTL. FreeType is FTL or GPL-2.0, and Grape takes the FTL |
| `harfbuzz.txt` | HarfBuzz (text shaping) | Old MIT |
| `icu.txt` | ICU (Unicode) | Unicode-3.0, with the notices of code ICU includes |
| `lcms.txt` | Little CMS (color management) | MIT |
| `libjpeg_turbo.md`, `libjpeg_turbo.ijg` | libjpeg-turbo (JPEG) | IJG, BSD-3-Clause and Zlib |
| `libopenjpeg.txt` | OpenJPEG (JPEG 2000) | BSD-2-Clause |
| `libpng.txt` | libpng (PNG) | PNG Reference Library License version 2 |
| `llvm-libc.txt` | LLVM's libc pieces | Apache-2.0 WITH LLVM-exception |
| `simdutf.txt` | simdutf (Unicode conversion) | MIT |
| `zlib.txt` | zlib 1.3.2.1, Chromium's copy | Zlib |

Three notes on those texts:

- **libc++** (LLVM's C++ standard library, Chromium's copy) is built into both binaries too, but the archive has no file for it: the builder's license step drops it. It's under the same Apache-2.0 WITH LLVM-exception text as `llvm-libc.txt`. The LLVM exception lifts the notice conditions for its code inside a compiled program, so it's listed here as a courtesy.
- **`icu.txt`** contains GPL-2.0-or-later and GPL-3.0-or-later texts with the Autoconf exception. They cover ICU4C's `aclocal.m4` and `config.guess`, scripts that configure ICU's own build. They aren't compiled into PDFium, which builds ICU with Chromium's build files.
- Two notices the licenses ask for in the documentation: Portions of this software are copyright © 2026 The FreeType Project (www.freetype.org). All rights reserved. This software is based in part on the work of the Independent JPEG Group.

## Where the binaries come from

`scripts/sidecars.lock` pins them, and `scripts/fetch-sidecars.ps1` and `.sh` check every hash below before anything is placed (DECISIONS D25, D29).

| Target | Archive | Archive SHA-256 | Size (bytes) | File in it | File SHA-256 |
|---|---|---|---|---|---|
| Windows x64 | [`pdfium-win-x64.tgz`](https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/8086/pdfium-win-x64.tgz) | `1fd8af952832dbb0eb16d9249f68fe09e5f5ebf7c3dd9f6066ea2720cc28487d` | 3,866,531 | `bin/pdfium.dll` | `1f2aee4286b8cf0413ab4562b7121aa454f9cfd9afd6ca8609bcef80927b3f80` |
| macOS (arm64 + x86_64) | [`pdfium-mac-univ.tgz`](https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/8086/pdfium-mac-univ.tgz) | `ceb8dff448db2a2b15ac52e1360427edc3461f2b51cb4579b1462f1a931087d5` | 7,115,223 | `lib/libpdfium.dylib` | `f45efe943c5041b8eed3299e455056820a695a3de0c06b624d611d6de1b205db` |

- The Mac library is signed ad hoc again by the fetch script, and again by the bundler when it goes into Grape.app, so the copy in the app doesn't have the file hash above. The Windows DLL is shipped as it is (until M7 signing, when the bundler signs it).
- **Attestation:** the release has `pdfium-attestation.json`, a SLSA provenance v1 statement. Both archive hashes above are among its subjects. It names the workflow `.github/workflows/build-all.yml` on `refs/heads/master` of bblanchon/pdfium-binaries, run [37320989355](https://github.com/bblanchon/pdfium-binaries/actions/runs/37320989355), at the builder commit [`5325fa6d0d9379329f10f98fdc4839b4e40f2e79`](https://github.com/bblanchon/pdfium-binaries/tree/5325fa6d0d9379329f10f98fdc4839b4e40f2e79). That commit is the build scripts, not PDFium.

## The source

- **PDFium:** [`2fd6cff57d9412cc42ef1a7e4e0a59b13a1e7cec`](https://pdfium.googlesource.com/pdfium/+/2fd6cff57d9412cc42ef1a7e4e0a59b13a1e7cec) on `chromium/8086` (2026-10-04 21:51 −07:00, "Roll Zlib from 285e94b8fa95 to 456ae730017b"). The builder checks out the branch's head at build time (`steps/02-checkout.sh`: `gclient sync -r origin/chromium/8086`) and doesn't record the commit. This was the head before the build ran (2026-10-05 16:53 UTC) and still is, so it's almost certainly the source. That is an inference, not a recorded fact.
- **Third-party code:** at the revisions in that commit's [`DEPS`](https://pdfium.googlesource.com/pdfium/+/2fd6cff57d9412cc42ef1a7e4e0a59b13a1e7cec/DEPS) (FreeType `4800589f76dc`, ICU `5aa526207171`, zlib `456ae730017b`, libpng `0179a807421c`, Abseil `a00d74bb55b3`, libc++ `97b436da4c33`, and so on).
- **The builder's patches** (`steps/03-patch.sh` at `5325fa6`): [`shared_library.patch`](https://github.com/bblanchon/pdfium-binaries/blob/5325fa6d0d9379329f10f98fdc4839b4e40f2e79/patches/shared_library.patch), [`public_headers.patch`](https://github.com/bblanchon/pdfium-binaries/blob/5325fa6d0d9379329f10f98fdc4839b4e40f2e79/patches/public_headers.patch) and [`clang_rt.patch`](https://github.com/bblanchon/pdfium-binaries/blob/5325fa6d0d9379329f10f98fdc4839b4e40f2e79/patches/clang_rt.patch) on every OS, plus [`win/build.patch`](https://github.com/bblanchon/pdfium-binaries/blob/5325fa6d0d9379329f10f98fdc4839b4e40f2e79/patches/win/build.patch) and a version resource on Windows, and [`mac/build.patch`](https://github.com/bblanchon/pdfium-binaries/blob/5325fa6d0d9379329f10f98fdc4839b4e40f2e79/patches/mac/build.patch) on macOS.
- **Build arguments** (`args.gn` in each archive): `is_component_build = false`, `is_debug = false`, `pdf_enable_v8 = false`, `pdf_enable_xfa = false`, `pdf_is_standalone = true`, `pdf_use_partition_alloc = false`, `treat_warnings_as_errors = false`; Windows adds `target_cpu = "x64"` and `target_os = "win"`, macOS `clang_use_chrome_plugins = false` and `target_os = "mac"`.

## Checked here (2026-10-26, on Linux, by reading the files)

- Both archives were downloaded again and match the hashes and sizes above.
- `pdfium.dll` is PE32+ x86-64 and imports only `KERNEL32`, `ADVAPI32`, `GDI32` and `USER32`: no network library, and no Visual C++ runtime to install.
- `libpdfium.dylib` is universal (x86_64 and arm64), install name `./libpdfium.dylib`, minimum macOS 13.0 (Grape's too), and links only AppKit, CoreFoundation, CoreGraphics, Foundation and libSystem.
- Both export every function that pdfium-render 0.9.4 binds with its `pdfium_7881` feature (451 of them). The 8086 headers only add functions to the 7881 ones; none is removed or changed.
