# grape-webp

libwebp's encoder, for Compress on WebP (DECISIONS D28). **Lane B owns this crate.** It holds the only `unsafe` code for images, so `grape-engines` keeps `#![forbid(unsafe_code)]`: everything that touches libwebp is here, behind one safe function.

## Hard rules
- **Encode only.** The API is `encode(Pixels, &Options, keep_going) -> Result<Vec<u8>, Error>` and `version()`. Never add a decoder, a demuxer or anything else that hands libwebp bytes from a file: no bitstream a user dropped may reach libwebp (CVE-2023-4863 was in its decoder). Grape reads WebP with image-webp, in Rust.
- **libwebp-sys stays `=0.14.4`, `default-features = false, features = ["std"]`** (libwebp 1.6.0, vendored and byte for byte the `v1.6.0` tag). A bump is a deliberate change under D9 (14 days old, or D9's override for a security release): check the vendored version, re-run the tests and the measurements below, and update `deny.toml`'s and `about.toml`'s checksums and `about.hbs`'s libwebp section. Never turn on `system-dylib` (whatever libwebp the system has), `generate-bindings`, `parallel` (cc's jobserver) or the SIMD features.
- **Never set `TARGET_CPU` for a build that ships.** libwebp-sys's build script passes `-march=$TARGET_CPU` to clang and gcc when it's set (`native` would tie the binary to the build machine's CPU), and it doesn't tell cargo it reads it, so changing it doesn't even rebuild libwebp. Unset, it tries `-march=<arch>`, which no compiler takes, so nothing is passed. CI's build and release jobs fail if it's set.
- **Every `unsafe` block has a `// SAFETY:` comment** (the workspace denies `undocumented_unsafe_blocks` and `unsafe_op_in_unsafe_fn`) that says why each pointer is valid for as long as libwebp holds it.
- **Every libwebp allocation has a drop guard**: `Picture` (`WebPPictureFree`) and `Writer` (`WebPMemoryWriterClear`), on every path, errors and cancels included. `tests/leak.rs` fails if either is left out.
- **The progress hook never unwinds into C.** It runs the caller's closure inside `catch_unwind`; a panic counts as "stop". libwebp calls the hook only on the thread that called `WebPEncode` (its worker threads' pictures have none), so the closure needs no `Send`.
- **No panics, no `unwrap`/`expect` outside tests.** Bad input is `Error::BadInput` or `Error::TooLarge` before libwebp sees it: sides of 1 to 16,383, data exactly `width × height × channels`, quality 0–100, method 0–6, level 0–9.
- **The ABI version is the encoder's** (`WEBP_ENCODER_ABI_VERSION`), passed to `WebPConfigInitInternal` and `WebPPictureInitInternal`. Never libwebp-sys's `WebPConfig::new`, `new_with_preset` or `WebPPictureInit`, which pass the decoder's. `WebPConfig` starts zeroed, because its initializer leaves `use_delta_palette` unset.
- **The settings are D28's** (ENGINES.md → Tools): lossy with `use_argb 0`, no sharp YUV, `exact 0`, alpha lossless (`alpha_compression 1`, `alpha_filtering 2`, `alpha_quality 100`), and `partition_limit` from `partition_limit()` for pictures over about 16.7 MP (so libwebp re-encodes fewer times to fit the first partition's 512 KiB; re-measure before changing it); lossless with `WebPConfigLosslessPreset(level)` and `exact 1`; `thread_level` from `Options::threads`. Grape's engine picks the quality, method and level.

## Layout
```
src/
  lib.rs     Pixels, Mode, Options, Error, MAX_SIDE, encode, version; the config, Picture and Writer guards, the progress trampoline
  tests.rs   RGB and RGBA, lossy and lossless, decoded back with image-webp; bad input, too large, progress, cancel, a panicking hook
tests/
  leak.rs    1,000 encodes through every way out, resident memory before and after (Linux)
```

## Checks
- `cargo test -p grape-webp` and `cargo clippy -p grape-webp --all-targets -- -D warnings`.
- The C compiles on every build: libwebp-sys's build script runs cc on 125 files (serially). From Linux, the Windows and Mac targets need a C compiler for them too: `docs/START_HERE.md` has the env for the cross-checks.
