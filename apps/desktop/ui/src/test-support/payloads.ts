/**
 * Builders for test payloads on top of the golden fixtures. Test code only.
 */

import type {
  Geometry,
  ShortcutSegment,
  UiSegment,
  WheelIcon,
  WheelMode,
  WheelOpenPayload,
} from "../shared/events";
import { filletRadius } from "../wheel/geometry";
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

/**
 * A Shortcuts `wheel://open` payload with `n` segments laid out like
 * grape-core's `wheel_layout(n)`, built on the golden shortcuts fixture.
 * Segments are named `Item 0`… with no icon unless told; `groups` lists
 * the indexes that are groups.
 */
export function shortcutsPayload(
  n: number,
  overrides: {
    seq?: number;
    sounds?: boolean;
    trail?: readonly string[];
    titles?: readonly string[];
    icons?: readonly WheelIcon[];
    groups?: readonly number[];
  } = {},
): WheelOpenPayload {
  const base = openFixture("wheel-open-shortcuts.json");
  const g = base.geometry;
  const start = n === 3 ? Math.PI : Math.PI / 2;
  const segments: ShortcutSegment[] = Array.from({ length: n }, (_, index) => {
    const center = start - (2 * Math.PI * index) / n;
    return {
      index,
      center,
      start: center + Math.PI / n,
      end: center - Math.PI / n,
      labelX: g.labelR * Math.cos(center),
      labelY: -g.labelR * Math.sin(center),
      title: overrides.titles?.[index] ?? `Item ${index}`,
      icon: overrides.icons?.[index] ?? { kind: "none" },
      group: overrides.groups?.includes(index) ?? false,
    };
  });
  return {
    ...base,
    seq: overrides.seq ?? base.seq,
    sounds: overrides.sounds ?? base.sounds,
    content: {
      kind: "shortcuts",
      dense: n >= g.denseAt,
      trail: [...(overrides.trail ?? [])],
      segments,
    },
  };
}

/**
 * A mirror of grape-core's `hit_test(dx, dy, n)`: the ring, the gaps and
 * the rounded outer corners. One item is a full ring with no gap.
 */
export function coreHitTest(dx: number, dy: number, n: number, g: Geometry): number | null {
  const d = Math.hypot(dx, dy);
  if (n === 0 || !(d >= g.innerR && d <= g.outerR)) return null;
  if (n === 1) return 0;
  if (Math.PI / n - Math.asin(g.gap / 2 / g.innerR) <= 0) return null;
  const theta = Math.atan2(-dy, dx);
  const step = (2 * Math.PI) / n;
  const start = n === 3 ? Math.PI : Math.PI / 2;
  const k = Math.round((start - theta) / step);
  const index = Math.min(((k % n) + n) % n, n - 1);
  const center = start - step * index;
  const wrapped = ((((theta - center + Math.PI) % (2 * Math.PI)) + 2 * Math.PI) % (2 * Math.PI)) - Math.PI;
  const offset = Math.abs(wrapped);
  if (offset > Math.PI / n - Math.asin(g.gap / 2 / d)) return null;
  const rho = filletRadius(n, g);
  if (rho > 0) {
    // In the frame of the nearer boundary ray: u along it, w toward the segment.
    const alpha = Math.PI / n - offset;
    const u = d * Math.cos(alpha);
    const w = d * Math.sin(alpha);
    const off = g.gap / 2 + rho;
    const uc = Math.sqrt((g.outerR - rho) ** 2 - off ** 2);
    const phi = Math.atan2(off, uc);
    if (alpha < phi && u > uc && Math.hypot(u - uc, w - off) > rho) return null;
  }
  return index;
}
