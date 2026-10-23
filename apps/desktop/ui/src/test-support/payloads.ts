/**
 * Builders for test payloads on top of the golden fixtures. Test code only.
 */

import type { UiSegment, WheelMode, WheelOpenPayload } from "../shared/events";
import { openFixture } from "./fixtures";

/**
 * A `wheel://open` payload with `n` entry segments laid out like
 * grape-core's `wheel_layout(n)` (item 0 at the top, or left for n = 3),
 * built on the golden entries fixture.
 */
export function entriesPayload(
  n: number,
  overrides: { seq?: number; mode?: WheelMode; sounds?: boolean; titles?: readonly string[] } = {},
): WheelOpenPayload {
  const base = openFixture("wheel-open-entries.json");
  const g = base.geometry;
  const start = n === 3 ? Math.PI : Math.PI / 2;
  const segments: UiSegment[] = Array.from({ length: n }, (_, index) => {
    const center = start - (2 * Math.PI * index) / n;
    return {
      index,
      center,
      start: center + Math.PI / n,
      end: center - Math.PI / n,
      labelX: g.labelR * Math.cos(center),
      labelY: -g.labelR * Math.sin(center),
      title: overrides.titles?.[index] ?? `FMT${index}`,
      entry: { kind: "convert", key: `fmt${index}` },
    };
  });
  return {
    ...base,
    seq: overrides.seq ?? base.seq,
    mode: overrides.mode ?? base.mode,
    sounds: overrides.sounds ?? base.sounds,
    content: { kind: "entries", dense: n >= g.denseAt, segments },
  };
}
