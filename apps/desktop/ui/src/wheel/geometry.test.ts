import { describe, expect, it } from "vitest";
import { type Geometry, isWheelOpenPayload, type UiSegment } from "../shared/events";
import {
  canDraw,
  filletAngle,
  filletRadius,
  formatNumber,
  labelAnchor,
  segmentCorners,
  segmentPath,
  type SegmentCorners,
  type Vec,
} from "./geometry";

// The geometry comes from the golden payload, so these tests use exactly
// the numbers Rust sends (grape-core's GEOMETRY).
const golden: unknown = JSON.parse(
  Object.values(
    import.meta.glob<string>("../shared/fixtures/wheel-open-entries.json", {
      query: "?raw",
      import: "default",
      eager: true,
    }),
  )[0] ?? "null",
);
if (!isWheelOpenPayload(golden)) {
  throw new Error("golden payload failed its guard");
}
const G: Geometry = golden.geometry;
const EPS = 1e-6;

// ---------------------------------------------------------------------------
// Test-only mirrors of grape-core, standing in for the data Rust sends.
// ---------------------------------------------------------------------------

/** grape-core `wheel_layout(n)`: item 0 at the top, or on the left for n = 3. */
function layout(n: number, g: Geometry = G): UiSegment[] {
  const start = n === 3 ? Math.PI : Math.PI / 2;
  return Array.from({ length: n }, (_, index) => {
    const center = start - (2 * Math.PI * index) / n;
    return {
      index,
      center,
      start: center + Math.PI / n,
      end: center - Math.PI / n,
      labelX: g.labelR * Math.cos(center),
      labelY: -g.labelR * Math.sin(center),
      title: `ITEM ${index}`,
      entry: { kind: "convert", key: `k${index}` },
    };
  });
}

/**
 * grape-core `hit_test(dx, dy, n)`: the ring, the gaps, and the rounded
 * outer corners (a point in a corner's cut-off, outside its fillet circle,
 * selects nothing). One item is a full ring with no gap.
 */
function hitTest(dx: number, dy: number, n: number, g: Geometry = G): number | null {
  const d = Math.hypot(dx, dy);
  if (n === 0 || !(d >= g.innerR && d <= g.outerR)) return null;
  if (n === 1) return 0;
  // Too narrow to draw (the inner corners would cross): nothing to select.
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

// ---------------------------------------------------------------------------
// Vector helpers
// ---------------------------------------------------------------------------

function dist(a: Vec, b: Vec): number {
  return Math.hypot(a.x - b.x, a.y - b.y);
}

function norm(a: Vec): number {
  return Math.hypot(a.x, a.y);
}

/** The screen direction of math angle β. */
function dir(beta: number): Vec {
  return { x: Math.cos(beta), y: -Math.sin(beta) };
}

/**
 * Signed distance of `p` from the line through the origin along `beta`,
 * positive counter-clockwise of the ray.
 */
function offsetFromRay(p: Vec, beta: number): number {
  const d = dir(beta);
  return p.x * d.y - p.y * d.x;
}

/** Distance along the ray. */
function alongRay(p: Vec, beta: number): number {
  const d = dir(beta);
  return p.x * d.x + p.y * d.y;
}

function near(actual: number, expected: number, label: string): void {
  expect(Math.abs(actual - expected), `${label}: ${actual} vs ${expected}`).toBeLessThan(EPS);
}

function corners(seg: UiSegment, n: number, g: Geometry = G): SegmentCorners {
  const found = segmentCorners(seg, n, g);
  if (found === null) throw new Error(`no corners for n=${n}`);
  return found;
}

// ---------------------------------------------------------------------------
// A tiny SVG path reader: M, L, A (circular, no rotation) and Z, flattened
// to polygons so the tests can check what the path string really draws.
// ---------------------------------------------------------------------------

type Command =
  | { op: "M" | "L"; to: Vec }
  | { op: "A"; r: number; large: boolean; sweep: boolean; to: Vec }
  | { op: "Z" };

function parsePath(d: string): Command[] {
  const tokens = d.trim().split(/\s+/);
  const out: Command[] = [];
  let i = 0;
  const num = (): number => {
    const token = tokens[i++];
    if (token === undefined || !/^-?\d+(\.\d+)?$/.test(token)) {
      throw new Error(`bad number "${String(token)}" in ${d}`);
    }
    return Number(token);
  };
  while (i < tokens.length) {
    const op = tokens[i++];
    if (op === "M" || op === "L") {
      out.push({ op, to: { x: num(), y: num() } });
    } else if (op === "A") {
      const rx = num();
      const ry = num();
      const rotation = num();
      const large = num();
      const sweep = num();
      if (rx !== ry || rotation !== 0 || (large !== 0 && large !== 1) || (sweep !== 0 && sweep !== 1)) {
        throw new Error(`unexpected arc in ${d}`);
      }
      out.push({ op, r: rx, large: large === 1, sweep: sweep === 1, to: { x: num(), y: num() } });
    } else if (op === "Z") {
      out.push({ op });
    } else {
      throw new Error(`unexpected command "${String(op)}" in ${d}`);
    }
  }
  return out;
}

interface ArcInfo {
  center: Vec;
  /** Start angle and signed sweep (screen space, positive = clockwise on screen). */
  theta: number;
  delta: number;
  r: number;
}

/** SVG arc endpoint → center parameterization (SVG 1.1 §F.6.5) for circles. */
function arcCenter(from: Vec, cmd: { r: number; large: boolean; sweep: boolean; to: Vec }): ArcInfo {
  let r = cmd.r;
  const x1 = (from.x - cmd.to.x) / 2;
  const y1 = (from.y - cmd.to.y) / 2;
  const lambda = (x1 * x1 + y1 * y1) / (r * r);
  if (lambda > 1) r *= Math.sqrt(lambda);
  const sign = cmd.large === cmd.sweep ? -1 : 1;
  const num = r * r * r * r - r * r * y1 * y1 - r * r * x1 * x1;
  const den = r * r * y1 * y1 + r * r * x1 * x1;
  const coef = sign * Math.sqrt(Math.max(0, num / den));
  const cx1 = coef * y1;
  const cy1 = -coef * x1;
  const center = { x: cx1 + (from.x + cmd.to.x) / 2, y: cy1 + (from.y + cmd.to.y) / 2 };
  const theta = Math.atan2((y1 - cy1) / r, (x1 - cx1) / r);
  let delta = Math.atan2((-y1 - cy1) / r, (-x1 - cx1) / r) - theta;
  if (cmd.sweep && delta < 0) delta += 2 * Math.PI;
  if (!cmd.sweep && delta > 0) delta -= 2 * Math.PI;
  return { center, theta, delta, r };
}

/** The path as closed polygons (one per subpath), arcs sampled every ~0.5°. */
function flatten(d: string): Vec[][] {
  const polygons: Vec[][] = [];
  let current: Vec[] = [];
  let at: Vec = { x: 0, y: 0 };
  for (const cmd of parsePath(d)) {
    if (cmd.op === "M") {
      if (current.length > 0) polygons.push(current);
      current = [cmd.to];
      at = cmd.to;
    } else if (cmd.op === "L") {
      current.push(cmd.to);
      at = cmd.to;
    } else if (cmd.op === "A") {
      const info = arcCenter(at, cmd);
      const steps = Math.max(2, Math.ceil(Math.abs(info.delta) / (Math.PI / 360)));
      for (let s = 1; s <= steps; s += 1) {
        const t = info.theta + (info.delta * s) / steps;
        current.push({ x: info.center.x + info.r * Math.cos(t), y: info.center.y + info.r * Math.sin(t) });
      }
      at = cmd.to;
    }
  }
  if (current.length > 0) polygons.push(current);
  return polygons;
}

interface Box {
  minX: number;
  maxX: number;
  minY: number;
  maxY: number;
}

const boxes = new WeakMap<Vec[], Box>();

function boundingBox(poly: Vec[]): Box {
  const cached = boxes.get(poly);
  if (cached !== undefined) return cached;
  const box = {
    minX: Math.min(...poly.map((v) => v.x)),
    maxX: Math.max(...poly.map((v) => v.x)),
    minY: Math.min(...poly.map((v) => v.y)),
    maxY: Math.max(...poly.map((v) => v.y)),
  };
  boxes.set(poly, box);
  return box;
}

/** Even-odd point-in-polygon over every subpath (matches `fill-rule: evenodd`). */
function insidePath(polygons: Vec[][], p: Vec): boolean {
  let inside = false;
  for (const poly of polygons) {
    // Cheap bounding-box reject first; the sampled hit-test sweep is large.
    const box = boundingBox(poly);
    if (p.x < box.minX || p.x > box.maxX || p.y < box.minY || p.y > box.maxY) continue;
    for (let i = 0, j = poly.length - 1; i < poly.length; j = i, i += 1) {
      const a = poly[i];
      const b = poly[j];
      if (a === undefined || b === undefined) continue;
      if (a.y > p.y !== b.y > p.y && p.x < ((b.x - a.x) * (p.y - a.y)) / (b.y - a.y) + a.x) {
        inside = !inside;
      }
    }
  }
  return inside;
}

/** Every vertex the path moves, lines or arcs to (not the arc samples). */
function vertices(d: string): Vec[] {
  return parsePath(d).flatMap((cmd) => (cmd.op === "Z" ? [] : [cmd.to]));
}

const NS = Array.from({ length: 12 }, (_, i) => i + 1);

// ---------------------------------------------------------------------------

describe("canDraw", () => {
  it("accepts the spec geometry for 1 to 12 segments", () => {
    for (const n of NS) expect(canDraw(n, G), `n=${n}`).toBe(true);
  });

  it("rejects bad counts and geometry", () => {
    expect(canDraw(0, G)).toBe(false);
    expect(canDraw(-1, G)).toBe(false);
    expect(canDraw(2.5, G)).toBe(false);
    expect(canDraw(Number.NaN, G)).toBe(false);
    expect(canDraw(4, { ...G, innerR: Number.NaN })).toBe(false);
    expect(canDraw(4, { ...G, outerR: G.innerR })).toBe(false);
    expect(canDraw(4, { ...G, gap: 2 * G.innerR })).toBe(false);
    expect(canDraw(4, { ...G, corner: -1 })).toBe(false);
    // So narrow that the inner corners would cross.
    expect(canDraw(100, G)).toBe(false);
  });
});

describe("filletRadius", () => {
  it("is the full corner while it fits (n = 2..14 with the spec numbers)", () => {
    for (let n = 2; n <= 14; n += 1) expect(filletRadius(n, G), `n=${n}`).toBe(G.corner);
  });

  it("shrinks once the fillets would cross (n ≥ 15 with the spec numbers)", () => {
    expect(filletRadius(15, G)).toBeLessThan(G.corner);
    expect(filletRadius(15, G)).toBeGreaterThan(0);
  });

  it("shrinks at n = 12 when the corner is too big for a twelfth of the ring", () => {
    const big = { ...G, corner: 40 };
    const rho = filletRadius(12, big);
    expect(rho).toBeLessThan(40);
    expect(rho).toBeGreaterThan(G.corner);
    expect(filletAngle(rho, big)).toBeLessThanOrEqual(Math.PI / 12 - 1e-6);
  });

  it("never increases with n, and the fillets never cross the center line", () => {
    let previous = Number.POSITIVE_INFINITY;
    for (let n = 2; n <= 40; n += 1) {
      if (!canDraw(n, G)) break;
      const rho = filletRadius(n, G);
      expect(rho, `n=${n}`).toBeLessThanOrEqual(previous);
      expect(filletAngle(rho, G), `n=${n}`).toBeLessThanOrEqual(Math.PI / n - 1e-6);
      previous = rho;
    }
  });

  it("is within the 20-step search tolerance of the largest fitting radius", () => {
    const rho = filletRadius(20, G);
    const tolerance = G.corner / 2 ** 20;
    expect(filletAngle(rho, G)).toBeLessThanOrEqual(Math.PI / 20 - 1e-6);
    expect(filletAngle(rho + 2 * tolerance, G)).toBeGreaterThan(Math.PI / 20 - 1e-6);
  });

  it("is 0 for a full ring, a zero corner or bad geometry", () => {
    expect(filletRadius(1, G)).toBe(0);
    expect(filletRadius(4, { ...G, corner: 0 })).toBe(0);
    expect(filletRadius(0, G)).toBe(0);
  });
});

describe("segmentCorners", () => {
  it("puts every vertex on its circle or gap line (n = 2..12, within 1e-6)", () => {
    for (let n = 2; n <= 12; n += 1) {
      for (const seg of layout(n)) {
        const c = corners(seg, n);
        for (const [name, edge, side] of [
          ["start", c.start, -1],
          ["end", c.end, 1],
        ] as const) {
          const where = `n=${n} i=${seg.index} ${name}`;
          const half = G.gap / 2;
          // On the circles.
          near(norm(edge.outer), G.outerR, `${where} outer`);
          near(norm(edge.inner), G.innerR, `${where} inner`);
          // On the gap line: gap/2 from the boundary ray, on the segment's side
          // (offsetFromRay is positive counter-clockwise of the ray).
          near(offsetFromRay(edge.edge, edge.beta), side * half, `${where} edge`);
          near(offsetFromRay(edge.inner, edge.beta), side * half, `${where} inner gap`);
          expect(alongRay(edge.edge, edge.beta), where).toBeGreaterThan(
            alongRay(edge.inner, edge.beta),
          );
          // The fillet touches both the edge and the outer circle.
          near(dist(edge.filletCenter, edge.edge), c.rho, `${where} fillet/edge`);
          near(dist(edge.filletCenter, edge.outer), c.rho, `${where} fillet/outer`);
          near(norm(edge.filletCenter), G.outerR - c.rho, `${where} fillet center`);
          near(offsetFromRay(edge.filletCenter, edge.beta), side * (half + c.rho), where);
        }
      }
    }
  });

  it("uses square inner corners at center ± (π/n − asin(gap/2 / innerR))", () => {
    for (let n = 2; n <= 12; n += 1) {
      const [seg] = layout(n);
      if (seg === undefined) throw new Error("no segment");
      const c = corners(seg, n);
      const expected = Math.PI / n - Math.asin(G.gap / 2 / G.innerR);
      const angleOf = (p: Vec): number => Math.atan2(-p.y, p.x);
      const wrap = (a: number): number => Math.atan2(Math.sin(a), Math.cos(a));
      expect(Math.abs(wrap(angleOf(c.start.inner) - seg.center) - expected)).toBeLessThan(EPS);
      expect(Math.abs(wrap(angleOf(c.end.inner) - seg.center) + expected)).toBeLessThan(EPS);
    }
  });

  it("keeps neighbours exactly one gap apart along each boundary ray", () => {
    for (let n = 2; n <= 12; n += 1) {
      const segs = layout(n);
      segs.forEach((seg, i) => {
        const next = segs[(i + 1) % n];
        if (next === undefined) throw new Error("no neighbour");
        const a = corners(seg, n).end;
        const b = corners(next, n).start;
        const where = `n=${n} between ${i} and ${next.index}`;
        // Both edges are parallel to the same ray…
        expect(Math.abs(Math.cos(a.beta - b.beta) - 1), where).toBeLessThan(EPS);
        // …and the matching points sit `gap` apart, straight across it.
        for (const [p, q] of [
          [a.inner, b.inner],
          [a.edge, b.edge],
        ] as const) {
          expect(Math.abs(dist(p, q) - G.gap), where).toBeLessThan(EPS);
          expect(Math.abs(alongRay(p, a.beta) - alongRay(q, a.beta)), where).toBeLessThan(EPS);
          expect(Math.abs(offsetFromRay(p, a.beta) + offsetFromRay(q, a.beta)), where).toBeLessThan(EPS);
        }
      });
    }
  });

  it("is null for a full ring or bad input", () => {
    const [seg] = layout(1);
    if (seg === undefined) throw new Error("no segment");
    expect(segmentCorners(seg, 1, G)).toBeNull();
    expect(segmentCorners({ center: Number.NaN }, 4, G)).toBeNull();
    expect(segmentCorners({ center: 0 }, 4, { ...G, outerR: 10 })).toBeNull();
  });
});

describe("segmentPath", () => {
  it("produces a valid path for n = 1..12, with no NaN", () => {
    for (const n of NS) {
      for (const seg of layout(n)) {
        const d = segmentPath(seg, n, G);
        expect(d, `n=${n}`).not.toMatch(/NaN|Infinity|e-|e\+/);
        expect(() => parsePath(d), `n=${n}`).not.toThrow();
      }
    }
  });

  it("draws n = 1 as a full ring: outer circle clockwise, inner counter-clockwise", () => {
    const [seg] = layout(1);
    if (seg === undefined) throw new Error("no segment");
    const cmds = parsePath(segmentPath(seg, 1, G));
    expect(cmds.map((c) => c.op).join("")).toBe("MAAZMAAZ");
    const arcs = cmds.filter((c) => c.op === "A");
    expect(arcs.map((c) => c.r)).toEqual([G.outerR, G.outerR, G.innerR, G.innerR]);
    expect(arcs.map((c) => c.sweep)).toEqual([true, true, false, false]);
    const rings = flatten(segmentPath(seg, 1, G));
    expect(insidePath(rings, { x: 0, y: 0 })).toBe(false);
    for (const angle of [0, 1, 2, 3, 4, 5, 6]) {
      const r = (G.innerR + G.outerR) / 2;
      expect(insidePath(rings, { x: r * Math.cos(angle), y: r * Math.sin(angle) })).toBe(true);
    }
  });

  it("follows the outline order with the corners as vertices (n = 2..12)", () => {
    for (let n = 2; n <= 12; n += 1) {
      for (const seg of layout(n)) {
        const d = segmentPath(seg, n, G);
        const cmds = parsePath(d);
        expect(cmds.map((c) => c.op).join(""), `n=${n}`).toBe("MAALALAZ");
        const c = corners(seg, n);
        const expected = [
          c.start.outer,
          c.end.outer,
          c.end.edge,
          c.end.inner,
          c.start.inner,
          c.start.edge,
          c.start.outer,
        ];
        const actual = vertices(d);
        expect(actual).toHaveLength(expected.length);
        actual.forEach((p, k) => {
          const want = expected[k];
          if (want === undefined) throw new Error("vertex count");
          expect(dist(p, want), `n=${n} i=${seg.index} vertex ${k}`).toBeLessThan(EPS);
        });
      }
    }
  });

  it("uses arcs centered where they should be (sweep and large-arc flags)", () => {
    for (let n = 2; n <= 12; n += 1) {
      for (const seg of layout(n)) {
        const c = corners(seg, n);
        const cmds = parsePath(segmentPath(seg, n, G));
        let at: Vec = { x: 0, y: 0 };
        const centers: Vec[] = [];
        for (const cmd of cmds) {
          if (cmd.op === "A") centers.push(arcCenter(at, cmd).center);
          if (cmd.op !== "Z") at = cmd.to;
        }
        const expected = [{ x: 0, y: 0 }, c.end.filletCenter, { x: 0, y: 0 }, c.start.filletCenter];
        centers.forEach((center, k) => {
          const want = expected[k];
          if (want === undefined) throw new Error("arc count");
          expect(dist(center, want), `n=${n} i=${seg.index} arc ${k}`).toBeLessThan(1e-4);
        });
      }
    }
  });

  it("places segment 0 from the Rust-provided center: top for n ≠ 3, left for n = 3", () => {
    for (let n = 2; n <= 12; n += 1) {
      const [seg] = layout(n);
      if (seg === undefined) throw new Error("no segment");
      const c = corners(seg, n);
      const mid = {
        x: (c.start.outer.x + c.end.outer.x) / 2,
        y: (c.start.outer.y + c.end.outer.y) / 2,
      };
      if (n === 3) {
        expect(mid.x, `n=${n}`).toBeLessThan(0);
        expect(Math.abs(mid.y), `n=${n}`).toBeLessThan(EPS);
      } else {
        expect(mid.y, `n=${n}`).toBeLessThan(0);
        expect(Math.abs(mid.x), `n=${n}`).toBeLessThan(EPS);
      }
    }
  });

  it("reads only the center angle, never the index", () => {
    const [seg] = layout(5);
    if (seg === undefined) throw new Error("no segment");
    const moved = { ...seg, index: 3 };
    expect(segmentPath(moved, 5, G)).toBe(segmentPath(seg, 5, G));
    expect(segmentPath({ ...seg, center: seg.center - 1 }, 5, G)).not.toBe(segmentPath(seg, 5, G));
  });

  it("agrees with grape-core's hit test everywhere, rounded corners and the full ring included", () => {
    for (let n = 1; n <= 12; n += 1) {
      const segs = layout(n);
      const shapes = segs.map((seg) => flatten(segmentPath(seg, n, G)));
      const half = G.gap / 2;
      // Every fillet circle: its arc is part of the drawn outline.
      const fillets = segs.flatMap((seg) => {
        const c = segmentCorners(seg, n, G);
        return c === null ? [] : [c.start.filletCenter, c.end.filletCenter];
      });
      const rho = filletRadius(n, G);
      let checked = 0;
      const mismatches: string[] = [];
      for (let x = -G.outerR - 2; x <= G.outerR + 2; x += 2.1) {
        for (let y = -G.outerR - 2; y <= G.outerR + 2; y += 2.1) {
          const p = { x, y };
          const d = norm(p);
          // Stay clear of every boundary by 0.25 px: the circles, the gap
          // lines and the fillet arcs.
          if (Math.abs(d - G.innerR) < 0.25 || Math.abs(d - G.outerR) < 0.25) continue;
          if (fillets.some((center) => Math.abs(dist(center, p) - rho) < 0.25)) continue;
          const nearGapLine =
            n > 1 &&
            segs.some((seg) =>
              [seg.center + Math.PI / n, seg.center - Math.PI / n].some(
                (beta) =>
                  Math.abs(Math.abs(offsetFromRay(p, beta)) - half) < 0.25 && alongRay(p, beta) > 0,
              ),
            );
          if (nearGapLine) continue;
          const hit = hitTest(x, y, n);
          shapes.forEach((shape, i) => {
            if (insidePath(shape, p) !== (hit === i)) {
              mismatches.push(`(${x.toFixed(1)}, ${y.toFixed(1)}) seg ${i} hit=${String(hit)}`);
            }
          });
          checked += 1;
        }
      }
      expect(mismatches, `n=${n}`).toEqual([]);
      expect(checked, `n=${n}`).toBeGreaterThan(5_000);
    }
  }, 60_000);

  it("returns an empty path for geometry it can't draw", () => {
    expect(segmentPath({ center: 0 }, 0, G)).toBe("");
    expect(segmentPath({ center: 0 }, 4, { ...G, gap: Number.NaN })).toBe("");
    expect(segmentPath({ center: 0 }, 1, { ...G, outerR: 1 })).toBe("");
  });
});

describe("labelAnchor", () => {
  it("is Rust's (labelX, labelY), untouched", () => {
    for (const n of NS) {
      for (const seg of layout(n)) {
        expect(labelAnchor(seg)).toEqual({ x: seg.labelX, y: seg.labelY });
      }
    }
    expect(labelAnchor({ labelX: 0, labelY: -84 })).toEqual({ x: 0, y: -84 });
  });
});

describe("formatNumber", () => {
  it("keeps six decimals at most and drops trailing zeros", () => {
    expect(formatNumber(120)).toBe("120");
    expect(formatNumber(-84)).toBe("-84");
    expect(formatNumber(1.5)).toBe("1.5");
    expect(formatNumber(1 / 3)).toBe("0.333333");
    expect(formatNumber(-2.0000004)).toBe("-2");
  });

  it("never writes -0 or exponents", () => {
    expect(formatNumber(-0)).toBe("0");
    expect(formatNumber(-1e-9)).toBe("0");
    expect(formatNumber(1e-7)).toBe("0");
    expect(formatNumber(2e-6)).toBe("0.000002");
  });
});
