# grape-heif-proto

The protocol between Grape and its image helper process (DECISIONS D33; protocol 2, D41: HEIC and AVIF): the request, the JSON-line messages, the raw pixel file, the `ftyp` sniff and the worker loop `serve`. **Lane A (Claude Code) owns this crate.** Three programs share it: grape-engines' `heif.rs` (the parent), the app's `main.rs` on macOS (`Grape --grape-heif-worker`, ImageIO) and `tools/grape-heif` (`grape-heif.exe`, libheif, built by path from outside the workspace).

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
  lib.rs           PROTOCOL (2), WORKER_FLAG, MAX_REQUEST_BYTES, MAX_LINE_BYTES, SNIFF_BYTES, WORKER_HEAP_BYTES (2 GiB, not on the wire), names (pixels.rgba, input.rgba, out.heic, out.avif; output(format))
  format.rs        HeifFormat {Heic, Avif}, "heic"/"avif" on the wire
  request.rs       Request {protocol, workDir, op, limits}, Op {probe, decode {source}, encode {format, quality?}}, Limits::DEFAULT, Limits::for_encode(format) (16,888 a side for HEIC, 16,384 for AVIF)
  message.rs       Message {image, progress, done, error}, ImageInfo, Done, Caps {heic, avif, codec}, FormatCaps {decode, encode}, Failure, FailKind
  pixels.rs        the raw pixel file: PixelWriter, read_pixels (exact length, limits before allocating)
  sniff.rs         Sniff {Heic, Avif, OtherHeif, NotHeif} from the ftyp box, Sniff::format
  serve.rs         Codec (decode and encode take the HeifFormat), PixelSink, serve (checks, job thread, sink, lines), worker_main
  serve/tests.rs   serve with a fake codec
```

## Tests you must keep green
- The exact JSON strings of heic-spec §3.2 and §3.3 and avif-spec's (D41) at protocol 2, both ways, and every unknown field or message refused (`{"kind":"probe","x":1}` included, and an `encode` without `format`, protocol 1's).
- The sniff table, in D41's order: every HEVC brand, a major `avif`/`avis` (Avif, even with a compatible HEVC brand), structural brands with HEVC (Heic, which wins a tie) or AV1 (Avif), a structural brand with no codec brand (`mif1` alone: Heic, for the helper to decide, D33's 2026-10-26 note), another codec's brand (OtherHeif), `isom` with a compatible `avif` (OtherHeif), MP4 and MOV, size 0 and 1, truncated boxes.
- The pixel file: round trips, lengths ±1, overflow, channels 2 and 5, the limits.
- `serve` with the fake: path, quality, limit and protocol checks, a panic, bad rows, the sink's failure winning, no file ever replaced, the sniffed format reaching the codec, `out.avif` made new for AVIF, and an output that sniffs as another format refused.
