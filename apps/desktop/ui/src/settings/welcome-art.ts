/**
 * The welcome card's three small illustrations: a file being dragged, a key
 * being tapped, and a file dropped on a wheel segment. Grape's own
 * drawings. Every color comes from a class in `settings.css`, which reads
 * the tokens; the gentle looping animation lives there too and stops under
 * `prefers-reduced-motion`. The pictures are decorative (`aria-hidden`):
 * each step's text says the same thing.
 *
 * Coordinates are in a 96 × 64 viewBox. An element CSS animates carries no
 * `transform` attribute (the animation would replace it), so each one wraps
 * a group that does the placing.
 */

import type { Geometry } from "../shared/events";
import { segmentPath } from "../wheel/geometry";
import type { WelcomeArt } from "./model";

const SVG_NS = "http://www.w3.org/2000/svg";

/** A file with a folded corner, 22 × 28, its top-left at the origin. */
const FILE_BODY = "M 3 0 H 14 L 22 8 V 25 A 3 3 0 0 1 19 28 H 3 A 3 3 0 0 1 0 25 V 3 A 3 3 0 0 1 3 0 Z";
const FILE_FOLD = "M 14 0 V 5 A 3 3 0 0 0 17 8 H 22";
const FILE_LINES = "M 5 15 H 17 M 5 20 H 13";

/** A pointer, its tip at the origin. */
const POINTER = "M 0 0 V 15 L 4 11.5 L 6.6 17.4 L 9.2 16.3 L 6.6 10.5 H 11.5 Z";

/**
 * A small wheel for the drop step, drawn with the wheel's own segment
 * outline. Illustration sizes only: the live wheel's geometry always comes
 * from Rust.
 */
const SMALL_WHEEL: Geometry = {
  innerR: 8,
  outerR: 22,
  backdropR: 24,
  gap: 2.5,
  corner: 3.5,
  labelR: 15,
  denseAt: 7,
  screenMargin: 0,
};
const SMALL_WHEEL_SEGMENTS = 4;
/** The segment the file lands on: the right one (index 1, center angle 0). */
const TARGET_SEGMENT = 1;
/** Where the small wheel's center sits in the picture. */
const WHEEL_X = 46;
const WHEEL_Y = 36;

function svgElement<K extends keyof SVGElementTagNameMap>(
  doc: Document,
  tag: K,
  attributes: Readonly<Record<string, string>> = {},
): SVGElementTagNameMap[K] {
  const node = doc.createElementNS(SVG_NS, tag);
  for (const [name, value] of Object.entries(attributes)) {
    node.setAttribute(name, value);
  }
  return node;
}

function path(doc: Document, className: string, d: string): SVGPathElement {
  return svgElement(doc, "path", { class: className, d });
}

/** A group placed at (`x`, `y`), optionally scaled, holding `children`. */
function placed(doc: Document, x: number, y: number, scale: number, ...children: SVGElement[]): SVGGElement {
  const transform = scale === 1 ? `translate(${x} ${y})` : `translate(${x} ${y}) scale(${scale})`;
  const group = svgElement(doc, "g", { transform });
  group.append(...children);
  return group;
}

/** A group CSS animates with `className`. */
function moving(doc: Document, className: string, ...children: SVGElement[]): SVGGElement {
  const group = svgElement(doc, "g", { class: className });
  group.append(...children);
  return group;
}

function file(doc: Document): SVGElement[] {
  return [
    path(doc, "art-file", FILE_BODY),
    path(doc, "art-stroke", FILE_FOLD),
    path(doc, "art-stroke", FILE_LINES),
  ];
}

function pointer(doc: Document): SVGPathElement {
  return path(doc, "art-pointer", POINTER);
}

/** Drag: the file moves off with the pointer, leaving a dashed outline where it was (the original stays). */
function dragArt(doc: Document): SVGElement[] {
  return [
    placed(doc, 16, 24, 1, path(doc, "art-ghost", FILE_BODY)),
    moving(
      doc,
      "art-drag-move",
      placed(doc, 16, 24, 1, ...file(doc)),
      placed(doc, 31, 41, 1, pointer(doc)),
    ),
  ];
}

/** Tap: a key goes down once, and a ring spreads from it. */
function tapArt(doc: Document): SVGElement[] {
  return [
    svgElement(doc, "circle", { class: "art-ripple", cx: "48", cy: "33", r: "28" }),
    svgElement(doc, "rect", { class: "art-key-base", x: "28", y: "22", width: "40", height: "28", rx: "7" }),
    moving(
      doc,
      "art-press",
      svgElement(doc, "rect", { class: "art-key-cap", x: "28", y: "17", width: "40", height: "28", rx: "7" }),
      // A legend in the lower corner, where modifier keys carry theirs.
      path(doc, "art-stroke art-key-mark", "M 35 37 H 44"),
    ),
  ];
}

/** Drop: the file comes in with the pointer, the segment under it lights up, and the file goes in. */
function dropArt(doc: Document): SVGElement[] {
  const n = SMALL_WHEEL_SEGMENTS;
  // Item 0 at the top, then clockwise, as grape-core lays out four segments.
  const outline = (index: number): string =>
    segmentPath({ center: Math.PI / 2 - (2 * Math.PI * index) / n }, n, SMALL_WHEEL);
  const wheel = placed(
    doc,
    WHEEL_X,
    WHEEL_Y,
    1,
    svgElement(doc, "circle", { class: "art-glass", r: String(SMALL_WHEEL.backdropR) }),
    ...Array.from({ length: n }, (_, index) => path(doc, "art-segment", outline(index))),
    path(doc, "art-target", outline(TARGET_SEGMENT)),
  );
  // The pointer's tip rests on the target segment's middle, right of the center.
  const tipX = WHEEL_X + (SMALL_WHEEL.innerR + SMALL_WHEEL.outerR) / 2;
  const held = moving(
    doc,
    "art-drop-move",
    placed(doc, tipX - 3, WHEEL_Y - 4, 0.55, ...file(doc)),
    placed(doc, tipX, WHEEL_Y, 0.85, pointer(doc)),
  );
  return [wheel, held];
}

const DRAWINGS: Readonly<Record<WelcomeArt, (doc: Document) => SVGElement[]>> = {
  drag: dragArt,
  tap: tapArt,
  drop: dropArt,
};

/** The illustration for one welcome step, as a decorative inline SVG. */
export function welcomeArt(doc: Document, art: WelcomeArt): SVGSVGElement {
  const svg = svgElement(doc, "svg", {
    class: `welcome-art welcome-art-${art}`,
    viewBox: "0 0 96 64",
    "aria-hidden": "true",
    focusable: "false",
  });
  svg.append(...DRAWINGS[art](doc));
  return svg;
}
