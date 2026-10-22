import { describe, expect, it } from "vitest";
import {
  edgeInset,
  modeCaption,
  polar,
  segmentCaption,
  segmentPath,
  type Geometry,
  type Segment,
} from "./geometry";

// The values grape-core sends (INTERACTION_SPEC §4).
const GEOMETRY: Geometry = {
  innerR: 48,
  outerR: 120,
  backdropR: 126,
  gap: 4,
  corner: 20,
  labelR: 84,
  denseAt: 7,
  screenMargin: 14,
};

/** grape-core's wheel_layout(n), for test input. */
function layout(n: number): Segment[] {
  const step = (2 * Math.PI) / n;
  const half = Math.PI / n;
  const start = n === 3 ? Math.PI : Math.PI / 2;
  return Array.from({ length: n }, (_, index) => {
    const center = start - step * index;
    return {
      index,
      center,
      start: center + half,
      end: center - half,
      labelX: 84 * Math.cos(center),
      labelY: -84 * Math.sin(center),
    };
  });
}

/** The numbers in a path, in order. */
function numbers(path: string): number[] {
  return (path.match(/-?\d+(\.\d+)?/g) ?? []).map(Number);
}

describe("polar", () => {
  it("puts angle π/2 straight up, because screen y grows down", () => {
    const top = polar(10, Math.PI / 2);
    expect(top.x).toBeCloseTo(0);
    expect(top.y).toBeCloseTo(-10);
    const right = polar(10, 0);
    expect(right.x).toBeCloseTo(10);
    expect(right.y).toBeCloseTo(0);
  });
});

describe("edgeInset", () => {
  it("is the half-gap angle the hit test uses", () => {
    expect(edgeInset(120, 4)).toBeCloseTo(Math.asin(2 / 120));
    expect(edgeInset(48, 4)).toBeGreaterThan(edgeInset(120, 4));
  });

  it("never asks asin for more than 1", () => {
    expect(edgeInset(1, 4)).toBeCloseTo(Math.PI / 2);
  });
});

describe("segmentPath", () => {
  it("draws a closed annular sector with two arcs", () => {
    const [top] = layout(6);
    expect(top).toBeDefined();
    if (top === undefined) return;
    const path = segmentPath(top, GEOMETRY);
    expect(path.startsWith("M ")).toBe(true);
    expect(path.endsWith("Z")).toBe(true);
    expect(path.match(/ A /g)).toHaveLength(2);
    expect(path).toContain("A 120 120 0 0 1");
    expect(path).toContain("A 48 48 0 0 0");
  });

  it("puts every corner on the inner or outer radius", () => {
    for (const segment of layout(6)) {
      const n = numbers(segmentPath(segment, GEOMETRY));
      // M x y | A rx ry rot large sweep x y | L x y | A rx ry rot large sweep x y
      const corners: [number, number][] = [
        [n[0] ?? NaN, n[1] ?? NaN],
        [n[7] ?? NaN, n[8] ?? NaN],
        [n[9] ?? NaN, n[10] ?? NaN],
        [n[16] ?? NaN, n[17] ?? NaN],
      ];
      const radii = corners.map(([x, y]) => Math.hypot(x, y));
      expect(radii[0]).toBeCloseTo(120, 2);
      expect(radii[1]).toBeCloseTo(120, 2);
      expect(radii[2]).toBeCloseTo(48, 2);
      expect(radii[3]).toBeCloseTo(48, 2);
    }
  });

  it("leaves a gap exactly `gap` pixels wide between neighbours", () => {
    const segments = layout(6);
    const [a, b] = segments;
    if (a === undefined || b === undefined) throw new Error("layout");
    const na = numbers(segmentPath(a, GEOMETRY));
    const nb = numbers(segmentPath(b, GEOMETRY));
    // a's clockwise outer corner and b's counter-clockwise outer corner.
    const aEnd = { x: na[7] ?? NaN, y: na[8] ?? NaN };
    const bStart = { x: nb[0] ?? NaN, y: nb[1] ?? NaN };
    expect(Math.hypot(aEnd.x - bStart.x, aEnd.y - bStart.y)).toBeCloseTo(4, 1);
  });

  it("uses a large arc only for a single segment", () => {
    const [only] = layout(1);
    if (only === undefined) throw new Error("layout");
    expect(segmentPath(only, GEOMETRY)).toContain("A 120 120 0 1 1");
  });

  it("is empty when the gap swallows the segment", () => {
    const [tiny] = layout(500);
    if (tiny === undefined) throw new Error("layout");
    expect(segmentPath(tiny, GEOMETRY)).toBe("");
  });
});

describe("captions", () => {
  it("name each wheel in Grape's own words", () => {
    expect(modeCaption("conversions")).toBe("Convert");
    expect(modeCaption("tools")).toBe("Tools");
    expect(modeCaption("actions")).toBe("File actions");
  });

  it("number placeholder segments from 1", () => {
    expect(segmentCaption(0)).toBe("Segment 1");
    expect(segmentCaption(5)).toBe("Segment 6");
  });
});
