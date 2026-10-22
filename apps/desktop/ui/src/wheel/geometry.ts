// Wheel geometry for drawing. Rust (grape-core) owns the numbers: the
// segments and GEOMETRY arrive in wheel://open, and this file only turns
// them into SVG path data. Angles are math-style radians (counter-clockwise
// from +x) and screen y grows down, so a point at radius r and angle a is
// (r·cos a, −r·sin a) from the anchor (INTERACTION_SPEC §4).

/** grape-core's GEOMETRY constants, in logical pixels. */
export interface Geometry {
  innerR: number;
  outerR: number;
  backdropR: number;
  gap: number;
  corner: number;
  labelR: number;
  denseAt: number;
  screenMargin: number;
}

/** One segment from grape-core's wheel_layout. */
export interface Segment {
  index: number;
  center: number;
  start: number;
  end: number;
  labelX: number;
  labelY: number;
}

/** A point in logical pixels, y down. */
export interface Point {
  x: number;
  y: number;
}

/** Which wheel is open. */
export type WheelMode = "conversions" | "tools" | "actions";

/** What a drop on a segment does. */
export interface WheelEntry {
  kind: "convert" | "tool" | "action";
  key: string;
}

/** A segment with its title and item (`docs/design/m1-drag-wheel.md` §C). */
export interface UiSegment extends Segment {
  title: string;
  entry: WheelEntry;
}

/** Which empty-state message to show. */
export type EmptyState = "noConversions" | "noTools";

/** What the wheel shows. */
export type WheelContent =
  | { kind: "pending" }
  | { kind: "entries"; dense: boolean; segments: UiSegment[] }
  | { kind: "empty"; state: EmptyState };

/** The user's key names, e.g. "Shift". */
export interface KeyNames {
  conversions: string;
  tools: string;
  actions: string;
}

/** The wheel://open payload. */
export interface WheelOpen {
  seq: number;
  mode: WheelMode;
  /** The wheel's center, in logical pixels from the overlay's top-left. */
  anchor: Point;
  scale: number;
  geometry: Geometry;
  keys: KeyNames;
  sounds: boolean;
  content: WheelContent;
}

/** The wheel://highlight payload. */
export interface WheelHighlight {
  seq: number;
  index: number | null;
}

/** The wheel://close payload. */
export interface WheelClose {
  seq: number;
  reason: string;
}

/** The point at radius `r` and math angle `angle`, as a screen offset. */
export function polar(r: number, angle: number): Point {
  return { x: r * Math.cos(angle), y: -r * Math.sin(angle) };
}

/**
 * How far a segment's edge sits inside its boundary ray at radius `r`, as an
 * angle: the gap is a constant `gap` pixels wide, so the angle shrinks
 * outwards. Matches grape-core's hit test.
 */
export function edgeInset(r: number, gap: number): number {
  return Math.asin(Math.min(1, gap / 2 / r));
}

function fmt(n: number): string {
  // Three decimals keep paths short; `+ 0` turns -0 into 0.
  return (Math.round(n * 1000) / 1000 + 0).toString();
}

function pt(p: Point): string {
  return `${fmt(p.x)} ${fmt(p.y)}`;
}

/**
 * SVG path data for one segment: an annular sector between `innerR` and
 * `outerR`, inset by half the gap on each side. Corners are square for the
 * M1 placeholder; rounding by `corner` comes with the real wheel in M3.
 * Returns "" when the gap leaves nothing to draw.
 */
export function segmentPath(segment: Segment, geometry: Geometry): string {
  const { innerR, outerR, gap } = geometry;
  const outerInset = edgeInset(outerR, gap);
  const innerInset = edgeInset(innerR, gap);
  const outerFrom = segment.start - outerInset;
  const outerTo = segment.end + outerInset;
  const innerFrom = segment.end + innerInset;
  const innerTo = segment.start - innerInset;
  if (outerFrom <= outerTo || innerTo <= innerFrom) {
    return "";
  }
  const outerLarge = outerFrom - outerTo > Math.PI ? 1 : 0;
  const innerLarge = innerTo - innerFrom > Math.PI ? 1 : 0;
  // Screen y is down, so decreasing math angles run clockwise (sweep 1).
  return [
    `M ${pt(polar(outerR, outerFrom))}`,
    `A ${fmt(outerR)} ${fmt(outerR)} 0 ${outerLarge} 1 ${pt(polar(outerR, outerTo))}`,
    `L ${pt(polar(innerR, innerFrom))}`,
    `A ${fmt(innerR)} ${fmt(innerR)} 0 ${innerLarge} 0 ${pt(polar(innerR, innerTo))}`,
    "Z",
  ].join(" ");
}

/** The words in the wheel's center while nothing is hovered (M1 only). */
export function modeCaption(mode: WheelMode): string {
  switch (mode) {
    case "conversions":
      return "Convert";
    case "tools":
      return "Tools";
    case "actions":
      return "File actions";
  }
}

/** The empty-state message, naming the user's own keys. */
export function emptyMessage(state: EmptyState, keys: KeyNames): string {
  switch (state) {
    case "noConversions":
      return `Nothing to convert here. Tap ${keys.actions} for file actions.`;
    case "noTools":
      return `No tools fit this selection. Tap ${keys.conversions} to convert or ${keys.actions} for file actions.`;
  }
}
