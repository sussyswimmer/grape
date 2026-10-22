import { describe, expect, it } from "vitest";

// tokens.css: "Every color in the UI comes from here." This test reads every
// other stylesheet, blanks out comments, and fails on any color literal, so
// a palette change in tokens.css reaches every window. The one exception is
// overlay.css's near-invisible hit fill, which design note D.2 pins there.

const TOKENS = "./tokens.css";

const sheets = import.meta.glob<string>("./*.css", {
  query: "?raw",
  import: "default",
  eager: true,
});

/** Hex colors, color functions, and the named colors worth catching. */
const COLOR_LITERAL =
  /#[0-9a-f]{3,8}(?![\w-])|(?<![\w-])(?:rgba?|hsla?|hwb|lab|lch|oklab|oklch|color|color-mix)\(|(?<![\w-])(?:white|black)(?![\w-])/i;

/** The single literal allowed outside tokens.css (D.2, D15). */
const ALLOWED = /^\s*--overlay-hit-fill:\s*rgba\(0, 0, 0, 0\.004\);\s*$/;

/** Returns "line: text" for every line that writes a color outside tokens.css. */
function colorLiterals(css: string): string[] {
  const lines = css.split("\n");
  const code = css.replace(/\/\*[\s\S]*?\*\//g, (text) => text.replace(/[^\n]/g, " ")).split("\n");
  const hits: string[] = [];
  code.forEach((line, index) => {
    if (COLOR_LITERAL.test(line) && !ALLOWED.test(line)) {
      hits.push(`${index + 1}: ${(lines[index] ?? "").trim()}`);
    }
  });
  return hits;
}

describe("colorLiterals", () => {
  it("flags hex colors, color functions and named colors", () => {
    const samples = [
      "  fill: #fff;",
      "  --x: #1b0c2e;",
      "  box-shadow: 0 1px 4px rgba(0, 0, 0, 0.4);",
      "  color: rgb(1 2 3);",
      "  color: hsl(270 50% 40%);",
      "  color: color-mix(in srgb, red, blue);",
      "  stroke: white;",
      "  background: Black;",
    ];
    for (const sample of samples) {
      expect(colorLiterals(sample), sample).toHaveLength(1);
    }
  });

  it("ignores tokens, comments, ids and look-alike words", () => {
    const samples = [
      "  fill: var(--glass-fill-solid);",
      "  /* rgba(0, 0, 0, 0.45) used to be here; #fff too */",
      "#wheel-root > .wheel {",
      "#toast-root.is-revealable {",
      "  white-space: nowrap;",
      "  background: transparent;",
      "  --overlay-hit-fill: rgba(0, 0, 0, 0.004);",
    ];
    for (const sample of samples) {
      expect(colorLiterals(sample), sample).toEqual([]);
    }
  });
});

describe("stylesheets", () => {
  const files = Object.entries(sheets).filter(([path]) => path !== TOKENS);

  it("include the sheets this test is meant to scan", () => {
    const paths = Object.keys(sheets);
    expect(paths).toContain(TOKENS);
    expect(paths).toContain("./overlay.css");
    expect(paths).toContain("./toast.css");
    for (const [, css] of files) {
      expect(css.length).toBeGreaterThan(0);
    }
  });

  it.each(files)("%s takes every color from tokens.css", (_path, css) => {
    expect(colorLiterals(css)).toEqual([]);
  });

  it("keeps the overlay's hit fill where D.2 puts it", () => {
    const overlay = sheets["./overlay.css"] ?? "";
    expect(overlay.split("\n").some((line) => ALLOWED.test(line))).toBe(true);
  });
});
