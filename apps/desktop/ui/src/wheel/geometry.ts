/**
 * Wheel segment outlines (docs/design/m1-drag-wheel.md §D.1). Pure: no DOM.
 *
 * Coordinates are CSS px centered on the wheel's anchor with y growing
 * down, the same space as grape-core's `hit_test(dx, dy, n)`. Angles are
 * math-style radians (counter-clockwise from +x), so a math angle θ at
 * radius r is the screen point (r·cos θ, −r·sin θ).
 *
 * Every number comes from Rust: the segment's `center` angle from
 * `wheel_layout(n)` and the radii from `GEOMETRY`. Nothing is hard-coded.
 *
 * Shape of one segment (n ≥ 2), with half-width h = π/n:
 * - Each radial edge is a straight line parallel to the boundary ray at
 *   β = center ± h, offset by gap/2 toward the segment. Two neighbours are
 *   therefore exactly `gap` apart everywhere, which is the core hit test's
 *   `|Δθ| ≤ π/n − asin(gap/2 / d)` drawn as a shape.
 * - The two outer corners are rounded with radius ρ = min(corner, ρmax);
 *   ρmax keeps the two fillets of a narrow segment from crossing.
 * - The inner corners stay square.
 */

import type { Geometry, UiSegment } from "../shared/events";

/** A point in wheel space: CSS px from the anchor, y down. */
export interface Vec {
  readonly x: number;
  readonly y: number;
}

/** The only part of a segment the outline needs: its center angle from Rust. */
export type SegmentAngle = Pick<UiSegment, "center">;

/** The corner points along one radial edge of a segment. */
export interface EdgeCorners {
  /** The boundary ray's math angle β (center + h for `start`, center − h for `end`). */
  readonly beta: number;
  /** Where the outer arc meets the fillet (on the outer circle). */
  readonly outer: Vec;
  /** The fillet's center. */
  readonly filletCenter: Vec;
  /** Where the fillet meets the straight edge (on the gap line). */
  readonly edge: Vec;
  /** The square inner corner: the gap line meets the inner circle. */
  readonly inner: Vec;
}

/** Every vertex of a segment outline, for tests and the path builder. */
export interface SegmentCorners {
  /** The outer corner radius actually used. */
  readonly rho: number;
  /** The counter-clockwise edge (math angle center + π/n). */
  readonly start: EdgeCorners;
  /** The clockwise edge (math angle center − π/n). */
  readonly end: EdgeCorners;
}

/** Binary-search steps for ρmax (§D.1). */
const RHO_SEARCH_STEPS = 20;
/** The fillets must stop this far (radians) short of the segment's center line. */
const RHO_ANGLE_MARGIN = 1e-6;

/**
 * True when `n` segments of this geometry can be drawn: a positive whole
 * number of segments, finite radii that nest, a gap that fits inside the
 * dead zone, and (for n ≥ 2) segments wide enough that the inner corners
 * don't cross.
 */
export function canDraw(n: number, g: Geometry): boolean {
  if (!Number.isSafeInteger(n) || n < 1) {
    return false;
  }
  const { innerR, outerR, gap, corner } = g;
  const finite = [innerR, outerR, gap, corner].every((value) => Number.isFinite(value));
  if (!finite || gap < 0 || corner < 0 || innerR <= gap / 2 || outerR <= innerR) {
    return false;
  }
  return n === 1 || Math.PI / n - Math.asin(gap / 2 / innerR) > 0;
}

/**
 * The angle between a boundary ray and the point where a fillet of radius
 * `rho` meets the outer circle (asin(gap/2 / outerR) when `rho` is 0).
 */
export function filletAngle(rho: number, g: Geometry): number {
  const inset = g.outerR - rho;
  const offset = g.gap / 2 + rho;
  return Math.atan2(offset, Math.sqrt(inset * inset - offset * offset));
}

/**
 * The outer corner radius for `n` segments: `g.corner`, or less when the
 * segments are too narrow for two fillets of that size. A candidate ρ fits
 * when the outer tangent point stays at most `h − 1e-6` from its boundary
 * ray (so the fillets never cross the center line) and the fillet still
 * starts outside the square inner corner. 0 when nothing fits.
 */
export function filletRadius(n: number, g: Geometry): number {
  if (!canDraw(n, g) || n === 1 || g.corner <= 0) {
    return 0;
  }
  const h = Math.PI / n;
  const half = g.gap / 2;
  const uInner = Math.sqrt(g.innerR * g.innerR - half * half);
  const fits = (rho: number): boolean => {
    const inset = g.outerR - rho;
    const offset = half + rho;
    if (inset <= offset) {
      return false;
    }
    const u = Math.sqrt(inset * inset - offset * offset);
    return filletAngle(rho, g) <= h - RHO_ANGLE_MARGIN && u >= uInner;
  };
  if (fits(g.corner)) {
    return g.corner;
  }
  let lo = 0;
  let hi = g.corner;
  for (let step = 0; step < RHO_SEARCH_STEPS; step += 1) {
    const mid = (lo + hi) / 2;
    if (fits(mid)) {
      lo = mid;
    } else {
      hi = mid;
    }
  }
  return lo;
}

/** Frame (u along the ray at β, v perpendicular, math orientation) → wheel space. */
function fromFrame(u: number, v: number, beta: number): Vec {
  const cos = Math.cos(beta);
  const sin = Math.sin(beta);
  return { x: u * cos - v * sin, y: -(u * sin + v * cos) };
}

/**
 * One edge's corners. `side` is −1 for the start edge (the segment lies
 * clockwise of its ray, at v < 0) and +1 for the end edge (mirrored).
 */
function edgeCorners(beta: number, side: -1 | 1, rho: number, g: Geometry): EdgeCorners {
  const half = g.gap / 2;
  const R = g.outerR;
  const inset = R - rho;
  const offset = half + rho;
  const uc = Math.sqrt(inset * inset - offset * offset);
  const scale = R / inset;
  const uInner = Math.sqrt(g.innerR * g.innerR - half * half);
  return {
    beta,
    outer: fromFrame(uc * scale, side * offset * scale, beta),
    filletCenter: fromFrame(uc, side * offset, beta),
    edge: fromFrame(uc, side * half, beta),
    inner: fromFrame(uInner, side * half, beta),
  };
}

/**
 * The outline's vertices for one of `n` segments (n ≥ 2), or `null` when
 * `n` is 1 (a full ring has no corners) or the geometry can't be drawn.
 */
export function segmentCorners(
  seg: SegmentAngle,
  n: number,
  g: Geometry,
): SegmentCorners | null {
  if (n === 1 || !canDraw(n, g) || !Number.isFinite(seg.center)) {
    return null;
  }
  const h = Math.PI / n;
  const rho = filletRadius(n, g);
  return {
    rho,
    start: edgeCorners(seg.center + h, -1, rho, g),
    end: edgeCorners(seg.center - h, 1, rho, g),
  };
}

/** A path number: six decimals at most (error ≤ 5e-7), no trailing zeros, no "-0". */
export function formatNumber(value: number): string {
  if (Math.abs(value) < 5e-7) {
    return "0";
  }
  return value.toFixed(6).replace(/\.?0+$/, "");
}

function point(p: Vec): string {
  return `${formatNumber(p.x)} ${formatNumber(p.y)}`;
}

function arc(r: number, largeArc: boolean, clockwise: boolean, to: Vec): string {
  const radius = formatNumber(r);
  return `A ${radius} ${radius} 0 ${largeArc ? 1 : 0} ${clockwise ? 1 : 0} ${point(to)}`;
}

/**
 * A full ring: the outer circle clockwise, the inner circle
 * counter-clockwise. Draw it with `fill-rule: evenodd`.
 */
function annulusPath(g: Geometry): string {
  const ring = (r: number, clockwise: boolean): string => {
    const top = { x: 0, y: -r };
    const bottom = { x: 0, y: r };
    return `M ${point(top)} ${arc(r, true, clockwise, bottom)} ${arc(r, true, clockwise, top)} Z`;
  };
  return `${ring(g.outerR, true)} ${ring(g.innerR, false)}`;
}

/**
 * The SVG path of one of `n` segments, centered on the anchor (y down).
 * `n === 1` gives a full ring (use `fill-rule: evenodd`). Returns "" when
 * the geometry can't be drawn, so a bad payload never yields NaN in the DOM.
 *
 * Order: outer arc (clockwise on screen) → fillet → end edge inward →
 * inner arc back (counter-clockwise) → start edge outward → fillet → close.
 */
export function segmentPath(seg: SegmentAngle, n: number, g: Geometry): string {
  if (n === 1) {
    return canDraw(1, g) ? annulusPath(g) : "";
  }
  const corners = segmentCorners(seg, n, g);
  if (corners === null) {
    return "";
  }
  const { rho, start, end } = corners;
  const h = Math.PI / n;
  const outerSpan = 2 * (h - filletAngle(rho, g));
  const innerSpan = 2 * (h - Math.asin(g.gap / 2 / g.innerR));
  const parts = [`M ${point(start.outer)}`, arc(g.outerR, outerSpan > Math.PI, true, end.outer)];
  if (rho > 0) {
    parts.push(arc(rho, false, true, end.edge));
  }
  parts.push(`L ${point(end.inner)}`, arc(g.innerR, innerSpan > Math.PI, false, start.inner));
  parts.push(`L ${point(start.edge)}`);
  if (rho > 0) {
    parts.push(arc(rho, false, true, start.outer));
  }
  parts.push("Z");
  return parts.join(" ");
}

/** Where a segment's label sits: Rust's `(labelX, labelY)`, unchanged. */
export function labelAnchor(seg: Pick<UiSegment, "labelX" | "labelY">): Vec {
  return { x: seg.labelX, y: seg.labelY };
}
