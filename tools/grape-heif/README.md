# grape-heif

`grape-heif.exe` is Grape's image helper on Windows, for HEIC and AVIF (DECISIONS D33, D41). Grape starts it as `grape-heif.exe --grape-heif-worker` for one image at a time, in a Job Object, and speaks the protocol in `crates/grape-heif-proto` with it: one JSON request on stdin, JSON lines on stdout, pixels through files in a private temp folder. It's libheif with libde265 (HEIC decode), x265 (HEIC encode), dav1d (AVIF decode) and libaom (AVIF encode, its encoder only), all linked statically from vcpkg, with libheif's plugin loading compiled out. macOS uses Grape's own worker instead: ImageIO, and rav1e for AVIF output (`grape_platform::macos::heif`, `grape_platform::avif`).

The program is **GPL-3.0-or-later as a whole** (x265 is GPL), so it ships only as this separate executable, never inside Grape's own binaries (golden rule 6), with its license texts and source bundle: `licenses/grape-heif/`. The code in this folder is MIT, like the rest of Grape.

It's its own Cargo workspace with its own `Cargo.lock`, so `cargo test --workspace` at the root never needs vcpkg.

## Files

```
Cargo.toml, Cargo.lock    the crate (version = libheif's + "-grape.N"); every crate passes D9
.cargo/config.toml        +crt-static: no Visual C++ runtime, and vcpkg-rs picks x64-windows-static
src/main.rs               SetDefaultDllDirectories(System32) + SetErrorMode, then --grape-heif-worker or -version
src/codec.rs              LibheifCodec: grape_heif_proto::Codec on libheif (security limits, decode, encode)
src/banner.rs             the -version banner the bundling gate and CI read
src/bin/grape-heif-fixtures.rs   the HEIC and AVIF test fixtures (CI only, never shipped)
vcpkg.lock.json           the vcpkg commit, triplet, install spec and the five source archives' hashes
vcpkg/ports/libheif/      overlay: vcpkg e456309's libheif recipe taken to 1.23.7, plugin loading off, dav1d on, aom encoder only
vcpkg/ports/aom/          overlay: vcpkg e456309's aom recipe taken to 3.15.2, from AOM's release tarball by SHA-512
build.ps1                 builds it (CI and the PC alike)
source-bundle.sh          the GPL source bundle CI ships with every build
deny.toml, about.toml, about.hbs   license checks and licenses/grape-heif/rust-crates.md
```

## Build

On Windows with git, Rust 1.99 and the Visual Studio Build Tools' C++ workload (docs/PLATFORM_WINDOWS.md → Dev setup):

```
.\tools\grape-heif\build.ps1 -VcpkgRoot C:\gh-vcpkg -BinaryCache C:\gh-vcpkg-cache -Install
```

The first build takes 45 minutes or more and about 5 GB (an estimate: libaom and dav1d build in debug and release, and vcpkg fetches Strawberry Perl, Python and meson for them). `-Install` copies the result to `apps\desktop\src-tauri\binaries\grape-heif-x86_64-pc-windows-msvc.exe`, which `pnpm tauri dev --config src-tauri/tauri.sidecars.windows.conf.json` (or a build with it) picks up; that overlay lists FFmpeg, ffprobe and bsdtar too, so they must be in `binaries\` as well. Or take `grape-heif.exe` from a CI run's `grape-heif-x86_64-pc-windows-msvc` artifact.

CI's `.github/workflows/grape-heif.yml` runs `build.ps1`, then `cargo test`, fmt and clippy here, checks the banner, the probe's caps, that no plugin loader is in the exe, the DLLs it imports and the license texts, decodes a HEIC and an AVIF through the worker, writes the HEIC and AVIF fixtures and the source bundle, and uploads all three. It caches vcpkg's binary archives, the build tools vcpkg downloaded (`downloads\tools`) and cargo's target.

## Checks that run on Linux

The crate can't link without vcpkg's libraries, but it type-checks for Windows from any OS:

```
CARGO_TARGET_DIR=target/grape-heif cargo clippy --manifest-path tools/grape-heif/Cargo.toml --release --locked --all-targets --target x86_64-pc-windows-msvc -- -D warnings
cargo deny --manifest-path tools/grape-heif/Cargo.toml --config tools/grape-heif/deny.toml --locked check licenses bans sources
cargo about generate --locked --fail --manifest-path tools/grape-heif/Cargo.toml -c tools/grape-heif/about.toml -o licenses/grape-heif/rust-crates.md tools/grape-heif/about.hbs
```

## Bumping

- **libheif** (likely: it has frequent security releases, taken under D9's override): the overlay's `vcpkg.json` version and `portfile.cmake` SHA512, `vcpkg.lock.json`'s entry, `Cargo.toml`'s version (the codec refuses any other libheif), `licenses/grape-heif/libheif.txt` if `COPYING` changed, and `SOURCE.md`.
- **vcpkg, libde265, x265 or dav1d:** the commit in `vcpkg.lock.json` (D9's 14 days), the hashes and commits, `SOURCE.md`, and the license texts if `COPYING`, `doc/PATENTS` or x86inc.asm's notice changed. The workflow reads the versions from the lock.
- **libaom** (security releases come often; D9's override): the aom overlay's `vcpkg.json` version and `portfile.cmake` SHA512, and the lock's url, sha512, sha256 and size, from the tarball in `storage.googleapis.com/aom-releases` (check its upload date for D9, never a `-rc`). The overlay's one change from vcpkg's recipe is that source; when vcpkg's commit moves, start again from its new recipe. `aom.txt` must stay vcpkg's `share/aom/copyright`.
- **The protocol:** any change bumps `grape_heif_proto::PROTOCOL` and rebuilds this in the same commit.
- **A crate:** pin it to the root `Cargo.lock`'s version when both have it, check D9, then regenerate `rust-crates.md`.
