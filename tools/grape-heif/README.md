# grape-heif

`grape-heif.exe` is Grape's HEIC helper on Windows (DECISIONS D33). Grape starts it as `grape-heif.exe --grape-heif-worker` for one image at a time, in a Job Object, and speaks the protocol in `crates/grape-heif-proto` with it: one JSON request on stdin, JSON lines on stdout, pixels through files in a private temp folder. It decodes HEIC with libheif and libde265 and encodes it with libheif and x265, all linked statically from vcpkg, with libheif's plugin loading compiled out. macOS uses ImageIO in Grape's own worker instead (`grape_platform::macos::heif`).

The program is **GPL-3.0-or-later as a whole** (x265 is GPL), so it ships only as this separate executable, never inside Grape's own binaries (golden rule 6), with its license texts and source bundle: `licenses/grape-heif/`. The code in this folder is MIT, like the rest of Grape.

It's its own Cargo workspace with its own `Cargo.lock`, so `cargo test --workspace` at the root never needs vcpkg.

## Files

```
Cargo.toml, Cargo.lock    the crate (version = libheif's + "-grape.N"); every crate passes D9
.cargo/config.toml        +crt-static: no Visual C++ runtime, and vcpkg-rs picks x64-windows-static
src/main.rs               SetDefaultDllDirectories(System32) + SetErrorMode, then --grape-heif-worker or -version
src/codec.rs              LibheifCodec: grape_heif_proto::Codec on libheif (security limits, decode, encode)
src/banner.rs             the -version banner the bundling gate and CI read
src/bin/grape-heif-fixtures.rs   the HEIC test fixtures (CI only, never shipped)
vcpkg.lock.json           the vcpkg commit, triplet, install spec and the three source archives' hashes
vcpkg/ports/libheif/      the overlay port: vcpkg e456309's libheif recipe taken to 1.23.7, plugin loading off
build.ps1                 builds it (CI and the PC alike)
source-bundle.sh          the GPL source bundle CI ships with every build
deny.toml, about.toml, about.hbs   license checks and licenses/grape-heif/rust-crates.md
```

## Build

On Windows with git, Rust 1.99 and the Visual Studio Build Tools' C++ workload (docs/PLATFORM_WINDOWS.md → Dev setup):

```
.\tools\grape-heif\build.ps1 -VcpkgRoot C:\gh-vcpkg -BinaryCache C:\gh-vcpkg-cache -Install
```

The first build takes 30 minutes or more and about 3 GB. `-Install` copies the result to `apps\desktop\src-tauri\binaries\grape-heif-x86_64-pc-windows-msvc.exe`, which `pnpm tauri dev --config src-tauri/tauri.sidecars.windows.conf.json` (or a build with it) picks up; that overlay lists FFmpeg, ffprobe and bsdtar too, so they must be in `binaries\` as well. Or take `grape-heif.exe` from a CI run's `grape-heif-x86_64-pc-windows-msvc` artifact.

CI's `.github/workflows/grape-heif.yml` runs `build.ps1`, then `cargo test`, fmt and clippy here, checks the banner, that no plugin loader is in the exe, the DLLs it imports and the license texts, writes the fixtures and the source bundle, and uploads all three.

## Checks that run on Linux

The crate can't link without vcpkg's libraries, but it type-checks for Windows from any OS:

```
CARGO_TARGET_DIR=target/grape-heif cargo clippy --manifest-path tools/grape-heif/Cargo.toml --release --locked --all-targets --target x86_64-pc-windows-msvc -- -D warnings
cargo deny --manifest-path tools/grape-heif/Cargo.toml --config tools/grape-heif/deny.toml --locked check licenses bans sources
cargo about generate --locked --fail --manifest-path tools/grape-heif/Cargo.toml -c tools/grape-heif/about.toml -o licenses/grape-heif/rust-crates.md tools/grape-heif/about.hbs
```

## Bumping

- **libheif** (likely: it has frequent security releases, taken under D9's override): the overlay's `vcpkg.json` version and `portfile.cmake` SHA512, `vcpkg.lock.json`'s entry, `Cargo.toml`'s version (the codec refuses any other libheif), `licenses/grape-heif/libheif.txt` if `COPYING` changed, and `SOURCE.md`.
- **vcpkg, libde265 or x265:** the commit in `vcpkg.lock.json` (D9's 14 days), the hashes, `SOURCE.md` and the banner checks in the workflow.
- **The protocol:** any change bumps `grape_heif_proto::PROTOCOL` and rebuilds this in the same commit.
- **A crate:** pin it to the root `Cargo.lock`'s version when both have it, check D9, then regenerate `rust-crates.md`.
