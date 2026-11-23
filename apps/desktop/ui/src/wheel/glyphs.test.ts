import { describe, expect, it } from "vitest";
import { GLYPH_GRID, GLYPH_IDS, glyphPaths, GLYPHS, isGlyphId } from "./glyphs";

// `GLYPHS` in apps/desktop/src-tauri/src/events.rs, copied by hand. Rust
// sends these ids; one that is missing here would silently draw nothing.
const RUST_ORDER = [
  "key",
  "text",
  "clipboard",
  "app",
  "file",
  "folder",
  "link",
  "bolt",
  "script",
  "terminal",
  "window",
  "gear",
  "bell",
  "speaker",
  "clock",
  "stack",
  "star",
  "heart",
  "home",
  "search",
  "mail",
  "music",
  "camera",
  "lock",
];

/** How many numbers each allowed command takes. */
const ARITY: Readonly<Record<string, number>> = { M: 2, L: 2, H: 1, V: 1, C: 6, Q: 4, A: 7, Z: 0 };

interface Command {
  readonly letter: string;
  readonly numbers: readonly number[];
}

/**
 * Parses a path made of absolute commands and plain decimals, one space
 * between tokens. Throws on whatever else it meets.
 */
function parse(d: string): Command[] {
  const commands: Command[] = [];
  const tokens = d.split(" ");
  let at = 0;
  while (at < tokens.length) {
    const letter = tokens[at] ?? "";
    const arity = ARITY[letter];
    if (arity === undefined || !Object.hasOwn(ARITY, letter)) {
      throw new Error(`"${letter}" is not a command in "${d}"`);
    }
    const numbers = tokens.slice(at + 1, at + 1 + arity).map((token) => {
      if (!/^\d+(\.\d+)?$/.test(token)) {
        throw new Error(`"${token}" is not a plain number in "${d}"`);
      }
      return Number(token);
    });
    if (numbers.length !== arity) {
      throw new Error(`${letter} is cut short in "${d}"`);
    }
    commands.push({ letter, numbers });
    at += 1 + arity;
  }
  return commands;
}

const all = Object.entries(GLYPHS).flatMap(([id, paths]) =>
  paths.map((d, index) => [`${id}[${index}]`, d] as const),
);

describe("the parser these tests rely on", () => {
  it("reads absolute commands and their numbers", () => {
    expect(parse("M 1 2 L 3.5 4 Z")).toEqual([
      { letter: "M", numbers: [1, 2] },
      { letter: "L", numbers: [3.5, 4] },
      { letter: "Z", numbers: [] },
    ]);
  });

  it("refuses relative commands, signs, exponents, commas and other letters", () => {
    for (const bad of ["m 1 2", "M 1 2 l 3 4", "M -1 2", "M 1e1 2", "M 1,2", "M 1 2 S 3 4 5 6", "M 1", "M 1 2 3", "M  1 2", ""]) {
      expect(() => parse(bad), bad).toThrow();
    }
  });
});

describe("GLYPHS", () => {
  it("lists Rust's 24 ids in Rust's order", () => {
    expect(RUST_ORDER).toHaveLength(24);
    expect(Object.keys(GLYPHS)).toEqual(RUST_ORDER);
    expect(GLYPH_IDS).toEqual(RUST_ORDER);
  });

  it("has no id twice", () => {
    expect(new Set(GLYPH_IDS).size).toBe(GLYPH_IDS.length);
    expect(new Set(RUST_ORDER).size).toBe(RUST_ORDER.length);
  });

  it("gives every id at least one path", () => {
    for (const id of GLYPH_IDS) {
      expect(GLYPHS[id].length, id).toBeGreaterThan(0);
    }
  });

  it("draws no two glyphs the same", () => {
    const drawings = GLYPH_IDS.map((id) => GLYPHS[id].join("|"));
    expect(new Set(drawings).size).toBe(drawings.length);
  });

  it.each(all)("%s uses only M L H V C Q A Z, starting with M", (_name, d) => {
    const commands = parse(d);
    expect(commands[0]?.letter).toBe("M");
    expect(commands.length).toBeGreaterThan(1);
  });

  it.each(all)("%s keeps every number on the grid", (_name, d) => {
    for (const { numbers } of parse(d)) {
      for (const value of numbers) {
        expect(value).toBeGreaterThanOrEqual(0);
        expect(value).toBeLessThanOrEqual(GLYPH_GRID);
      }
    }
  });

  it.each(all)("%s leaves room for the stroke at the grid's edge", (_name, d) => {
    // Points only: an arc's radii, rotation and flags aren't places.
    for (const { letter, numbers } of parse(d)) {
      const points = letter === "A" ? numbers.slice(5) : numbers;
      for (const value of points) {
        expect(value).toBeGreaterThanOrEqual(2);
        expect(value).toBeLessThanOrEqual(GLYPH_GRID - 2);
      }
    }
  });

  it.each(all)("%s writes arcs with flags of 0 or 1 and no rotation", (_name, d) => {
    for (const { letter, numbers } of parse(d)) {
      if (letter !== "A") continue;
      const [rx, ry, rotation, large, sweep] = numbers;
      expect(rx).toBeGreaterThan(0);
      expect(ry).toBe(rx);
      expect(rotation).toBe(0);
      expect([0, 1]).toContain(large);
      expect([0, 1]).toContain(sweep);
    }
  });

  it.each(all)("%s gives every arc a radius that reaches its end point", (_name, d) => {
    // A radius under half the chord would be scaled up by the browser.
    let x = 0;
    let y = 0;
    for (const { letter, numbers } of parse(d)) {
      const n = numbers;
      if (letter === "H") {
        x = n[0] ?? x;
      } else if (letter === "V") {
        y = n[0] ?? y;
      } else if (letter !== "Z") {
        const toX = n[n.length - 2] ?? x;
        const toY = n[n.length - 1] ?? y;
        if (letter === "A") {
          expect(n[0] ?? 0).toBeGreaterThanOrEqual(Math.hypot(toX - x, toY - y) / 2);
        }
        x = toX;
        y = toY;
      }
    }
  });
});

describe("glyphPaths", () => {
  it("finds every listed glyph", () => {
    for (const id of RUST_ORDER) {
      expect(isGlyphId(id)).toBe(true);
      expect(glyphPaths(id)).toBe(GLYPHS[id as keyof typeof GLYPHS]);
    }
  });

  it("answers null for an id the page doesn't know", () => {
    for (const id of ["rocket", "", "Key", "key ", "constructor", "toString", "__proto__", "hasOwnProperty"]) {
      expect(isGlyphId(id), id).toBe(false);
      expect(glyphPaths(id), id).toBeNull();
    }
  });
});
