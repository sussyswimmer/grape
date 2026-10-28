# grape-heif-proto

The protocol between Grape and its HEIC helper process (DECISIONS D33): the request, the JSON-line messages, the raw pixel file, the `ftyp` sniff and the worker loop `serve`. **Lane A (Claude Code) owns this crate.** Three programs share it: grape-engines' `heif.rs` (the parent), the app's `main.rs` on macOS (`Grape --grape-heif-worker`, ImageIO) and `tools/grape-heif` (`grape-heif.exe`, libheif, built by path from outside the workspace).

## Hard rules
- **Pure, no OS, no unsafe.** `#![forbid(unsafe_code)]`. `std` only: `std::fs`, `std::io`, `std::thread` and `std::process`. No OS-specific crate, and no `cfg` except the Unix parent watchdog (`std::os::unix::process::parent_id`, heic-spec J3). The wire types, `sniff` and the pixel file do no I/O, so they test without files.
- **Dependencies: `serde` (derive) and `serde_json`, nothing else.** This crate is built into `grape-heif.exe`, a GPL-3.0-or-later sidecar, so nothing from grape-engines or the app may come in (J1).
- **Any protocol change bumps `PROTOCOL`** and rebuilds `tools/grape-heif` in the same commit. That covers the wire types, a field, a `FailKind`, the pixel file's layout and the fixed `names`. The parent and the helpers must never disagree.
- **Both sides refuse unknown fields** (`deny_unknown_fields` on every type). An internally tagged enum's variant without fields is written `Probe {}`, never `Probe`, because serde reads a unit variant without checking for unknown keys.
- **No message holds a path, a file name or file contents.** A `detail` names what failed and an `io::ErrorKind`; serde_json errors are reported as `bad request (<category> at line:col)`, never quoted.
- **The manifest spells its fields out** (no `*.workspace = true`; J2), because `tools/grape-heif` builds it from outside the workspace. `[lints]` copies the workspace's three, and `publish = false` keeps it out of the notices.

## Layout
```
src/
  lib.rs           PROTOCOL, WORKER_FLAG, MAX_REQUEST_BYTES, MAX_LINE_BYTES, SNIFF_BYTES, names (pixels.rgba, input.rgba, out.heic)
  request.rs       Request {protocol, workDir, op, limits}, Op {probe, decode, encode}, Limits::DEFAULT
  message.rs       Message {image, progress, done, error}, ImageInfo, Done, Caps, Failure, FailKind
  pixels.rs        the raw pixel file: PixelWriter, read_pixels (exact length, limits before allocating)
  sniff.rs         Sniff {Heic, OtherHeif, NotHeif} from the ftyp box
  serve.rs         Codec, PixelSink, serve (checks, job thread, sink, lines), worker_main
  serve/tests.rs   serve with a fake codec
```

## Tests you must keep green
- The exact JSON strings of heic-spec §3.2 and §3.3, both ways, and every unknown field or message refused (`{"kind":"probe","x":1}` included).
- The sniff table: every HEVC brand, structural brands, a structural brand with no codec brand (`mif1` alone: Heic, for the helper to decide, D33's 2026-10-26 note), another codec's brand (OtherHeif), MP4 and MOV, size 0 and 1, truncated boxes.
- The pixel file: round trips, lengths ±1, overflow, channels 2 and 5, the limits.
- `serve` with the fake: path, quality, limit and protocol checks, a panic, bad rows, the sink's failure winning, no file ever replaced.
