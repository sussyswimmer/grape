/**
 * The glyph icons a shortcut can carry (docs/design/m10-shortcuts-wheel.md
 * §6). Grape's own drawings: plain pictograms on a 24 × 24 grid, each a
 * list of SVG paths meant to be stroked with round caps and joins and never
 * filled. Pure data: no DOM.
 *
 * The ids and their order are `GLYPHS` in
 * `apps/desktop/src-tauri/src/events.rs`. Rust sends an id; an id missing
 * here would draw nothing, so the two lists must stay the same.
 *
 * Paths use absolute commands only (M L H V C Q A Z) with every number
 * inside the grid, which `glyphs.test.ts` checks.
 */

/** The side of the grid every glyph is drawn on. */
export const GLYPH_GRID = 24;

export const GLYPHS = {
  // A ring with a stem and two teeth.
  key: [
    "M 12 16 A 4 4 0 1 1 4 16 A 4 4 0 1 1 12 16 Z",
    "M 10.8 13.2 L 20 4",
    "M 17 7 L 19.5 9.5",
    "M 14 10 L 16 12",
  ],
  // A capital T standing on a short foot.
  text: ["M 5 5 H 19", "M 12 5 V 19", "M 9 19 H 15"],
  // A board, its clip and two lines.
  clipboard: [
    "M 8 5 H 7 A 2 2 0 0 0 5 7 V 19 A 2 2 0 0 0 7 21 H 17 A 2 2 0 0 0 19 19 V 7 A 2 2 0 0 0 17 5 H 16",
    "M 9 3 H 15 V 7 H 9 Z",
    "M 9 12 H 15",
    "M 9 16 H 13",
  ],
  // Four tiles.
  app: [
    "M 4 4 H 10 V 10 H 4 Z",
    "M 14 4 H 20 V 10 H 14 Z",
    "M 4 14 H 10 V 20 H 4 Z",
    "M 14 14 H 20 V 20 H 14 Z",
  ],
  // A sheet with one corner folded.
  file: [
    "M 14 3 H 7 A 2 2 0 0 0 5 5 V 19 A 2 2 0 0 0 7 21 H 17 A 2 2 0 0 0 19 19 V 8 Z",
    "M 14 3 V 8 H 19",
  ],
  // A folder with its tab on the left.
  folder: [
    "M 3 7 A 2 2 0 0 1 5 5 H 9 L 11 8 H 19 A 2 2 0 0 1 21 10 V 17 A 2 2 0 0 1 19 19 H 5 A 2 2 0 0 1 3 17 Z",
  ],
  // Two hooks joined by a bar.
  link: [
    "M 11 7 L 13 5 A 4.25 4.25 0 0 1 19 11 L 17 13",
    "M 13 17 L 11 19 A 4.25 4.25 0 0 1 5 13 L 7 11",
    "M 10 14 L 14 10",
  ],
  // A lightning stroke.
  bolt: ["M 13 3 L 5 14 H 11 L 10 21 L 19 10 H 13 Z"],
  // Two angle brackets around a slash.
  script: ["M 8 7 L 3 12 L 8 17", "M 16 7 L 21 12 L 16 17", "M 13.5 5 L 10.5 19"],
  // A screen with a prompt and a cursor.
  terminal: [
    "M 5 5 H 19 A 2 2 0 0 1 21 7 V 17 A 2 2 0 0 1 19 19 H 5 A 2 2 0 0 1 3 17 V 7 A 2 2 0 0 1 5 5 Z",
    "M 7 10 L 10 12.5 L 7 15",
    "M 12 15 H 16",
  ],
  // A frame with a title bar, split in two halves.
  window: [
    "M 5 4 H 19 A 2 2 0 0 1 21 6 V 18 A 2 2 0 0 1 19 20 H 5 A 2 2 0 0 1 3 18 V 6 A 2 2 0 0 1 5 4 Z",
    "M 3 9 H 21",
    "M 12 9 V 20",
  ],
  // A hub, a rim and eight teeth.
  gear: [
    "M 14.5 12 A 2.5 2.5 0 1 1 9.5 12 A 2.5 2.5 0 1 1 14.5 12 Z",
    "M 18 12 A 6 6 0 1 1 6 12 A 6 6 0 1 1 18 12 Z",
    "M 12 6 V 3",
    "M 12 18 V 21",
    "M 18 12 H 21",
    "M 6 12 H 3",
    "M 16.24 7.76 L 18.36 5.64",
    "M 7.76 7.76 L 5.64 5.64",
    "M 7.76 16.24 L 5.64 18.36",
    "M 16.24 16.24 L 18.36 18.36",
  ],
  // A dome on a rim, with its clapper.
  bell: ["M 6 17 V 11 A 6 6 0 0 1 18 11 V 17", "M 4 17 H 20", "M 10.5 20 A 1.6 1.6 0 0 0 13.5 20"],
  // A cone and two waves.
  speaker: [
    "M 4 10 H 8 L 13 6 V 18 L 8 14 H 4 Z",
    "M 16 9.5 A 3.5 3.5 0 0 1 16 14.5",
    "M 18.5 7 A 7 7 0 0 1 18.5 17",
  ],
  // A dial with two hands.
  clock: ["M 21 12 A 9 9 0 1 1 3 12 A 9 9 0 1 1 21 12 Z", "M 12 7 V 12 L 15.5 14"],
  // Three sheets, one on the other.
  stack: ["M 12 3 L 21 8 L 12 13 L 3 8 Z", "M 3 12 L 12 17 L 21 12", "M 3 16 L 12 21 L 21 16"],
  // Five points.
  star: [
    "M 12 3 L 14.35 9.36 L 21.13 9.63 L 15.8 13.84 L 17.64 20.37 L 12 16.6 L 6.36 20.37 L 8.2 13.84 L 2.87 9.63 L 9.65 9.36 Z",
  ],
  // Two lobes that meet in a point.
  heart: [
    "M 12 20 C 5 15 3 11.5 3 8.5 A 4.6 4.6 0 0 1 12 7.5 A 4.6 4.6 0 0 1 21 8.5 C 21 11.5 19 15 12 20 Z",
  ],
  // A roof, walls and a door.
  home: ["M 3 11 L 12 3 L 21 11", "M 5 9.5 V 20 H 19 V 9.5", "M 10 20 V 14 H 14 V 20"],
  // A lens and its handle.
  search: ["M 17 10.5 A 6.5 6.5 0 1 1 4 10.5 A 6.5 6.5 0 1 1 17 10.5 Z", "M 15.2 15.2 L 21 21"],
  // An envelope and its flap.
  mail: ["M 3 6 H 21 V 18 H 3 Z", "M 3 7 L 12 13 L 21 7"],
  // Two notes under one beam.
  music: [
    "M 9 17 V 5 L 19 3 V 15",
    "M 9 17 A 2.5 2.5 0 1 1 4 17 A 2.5 2.5 0 1 1 9 17 Z",
    "M 19 15 A 2.5 2.5 0 1 1 14 15 A 2.5 2.5 0 1 1 19 15 Z",
  ],
  // A body with a hump and a lens.
  camera: [
    "M 3 9 A 2 2 0 0 1 5 7 H 8 L 9.5 4.5 H 14.5 L 16 7 H 19 A 2 2 0 0 1 21 9 V 17 A 2 2 0 0 1 19 19 H 5 A 2 2 0 0 1 3 17 Z",
    "M 15.5 13 A 3.5 3.5 0 1 1 8.5 13 A 3.5 3.5 0 1 1 15.5 13 Z",
  ],
  // A body, a shackle and a keyhole.
  lock: [
    "M 6 11 H 18 A 1 1 0 0 1 19 12 V 20 A 1 1 0 0 1 18 21 H 6 A 1 1 0 0 1 5 20 V 12 A 1 1 0 0 1 6 11 Z",
    "M 8 11 V 8 A 4 4 0 0 1 16 8 V 11",
    "M 12 15 V 17",
  ],
} as const satisfies Readonly<Record<string, readonly string[]>>;

/** A glyph id the page can draw. */
export type GlyphId = keyof typeof GLYPHS;

/** Every glyph id, in Rust's order. */
export const GLYPH_IDS: readonly GlyphId[] = Object.keys(GLYPHS) as GlyphId[];

/** True when `id` names a glyph (an own key: "constructor" is not one). */
export function isGlyphId(id: string): id is GlyphId {
  return Object.hasOwn(GLYPHS, id);
}

/** The paths of glyph `id`, or `null` for an id the page doesn't know. */
export function glyphPaths(id: string): readonly string[] | null {
  return isGlyphId(id) ? GLYPHS[id] : null;
}
