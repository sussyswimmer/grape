# grape-engines

Executes `JobPlan`s from `grape-core`: images, FFmpeg sidecar, PDF, archives, subtitles, and v2 documents. **Lane B (Codex) owns this crate.**

## Hard rules
- **Never touch the source.** Open inputs read-only. Write to `{dest_dir}/.grape-tmp-{uuid}.{ext}`, fsync, then atomically rename to the final name from the plan, **never replacing an existing file** (no-clobber rename; re-plan the name if it was taken meanwhile). On error or cancel, delete the temp file. Every test hashes the source before and after.
- **No network.** The engine code makes no HTTP calls at all.
- **Sidecars come from `SidecarPaths`** injected by the app. Never use `PATH` lookups, which would pick up whatever FFmpeg the user has installed.
- **Licensing:** read `docs/ENGINES.md` → Licensing rules before adding any crate or binary. Log it in `docs/DECISIONS.md`.
- **Cancellation:** every engine takes a `CancelToken`. FFmpeg children get killed. Long Rust loops check the token at least every 50 ms of work.
- **Progress:** call `run_job`'s `progress` callback with fractions in 0.0..=1.0 (it may never be called). FFmpeg uses `-progress pipe:1` + `ffprobe` duration.

## Layout
```
src/
  lib.rs            supports, run_job(job, ctx, progress) -> JobOutcome; engine_for picks the engine
  publish.rs        shared by every engine: destination folder, no-clobber finalize, user-message names
  output.rs         temp files and the no-clobber hard link
  cancel.rs         Cancellable reader/writer
  image.rs          image crate (raster); later ravif, resvg. publish_pixels and pixels_fit serve heif.rs
  headers.rs        header readers that bound what the image decoders keep
  subtitle/         srt/vtt/txt: decode, parse, write (pure) + convert (D20)
  media/            FFmpeg: probe + args (pure, exact-argv tests), run (child process), convert (D22)
  pdf/              PDF in a worker process: PDFium, lopdf (D29; lane A's module by agreement)
  heif.rs           HEIC through a helper process (D33; lane A's module by agreement)
  helper.rs         helper-process plumbing, WorkDir and the containment hook set_contain (lane A's)
  archive/          zip/tar/gz convert + extract, and RAR input through Grape's bsdtar sidecar (archive/rar/: header preflight, spawn/pump/watchdog, tap) (D27; lane A's module by agreement): one name check for every entry, Limits, never writes a link
  docs.rs           v2, behind feature "v2"
tests/              real conversions on fixtures generated in each test (no fixtures/ folder);
                    feature "engines-it" adds the ones that need the FFmpeg sidecar
                    feature "rar-it" runs the RAR tests (unit tests in src/archive/rar/it.rs) against a real bsdtar (GRAPE_TEST_BSDTAR)
```

## FFmpeg argument builders
Pure functions in `media/args.rs`: `passes(src, to, &Probe, out, palette) -> Result<Vec<Pass>, Missing>`, from what `ffprobe` said (`media/probe.rs`). Unit-test the exact argv, and keep the invariants: both whitelists, `file:` URLs, a forced `-f`, and only the temp path as output. The arguments are in `docs/ENGINES.md`. Hardware encoders come later: then an `EncoderCaps`, probed once with a test encode, picks them, with x264 as the fallback.

## Gotchas
- Apply EXIF orientation before re-encoding images. Removing metadata must not rotate photos.
- PDF → images: 300 DPI; one page writes a file, several pages write a folder named per the naming rules.
- GIF output: two-pass palette. Warn for clips over 30 s, because GIFs get huge.
- Archives: reject entries with `..` or absolute paths (zip-slip). Cap total extracted size (decompression bombs).
- Windows paths: use `\\?\` long-path forms when a path is over 260 chars.
