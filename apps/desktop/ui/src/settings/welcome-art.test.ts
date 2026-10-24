import { describe, expect, it } from "vitest";
import settingsCss from "../styles/settings.css?raw";
import { descendants, type FakeElement, fakeRoot } from "../test-support/fake-dom";
import type { WelcomeArt } from "./model";
import { welcomeArt } from "./welcome-art";

const ARTS: readonly WelcomeArt[] = ["drag", "tap", "drop"];

/** The classes CSS animates. Each must sit on a group with no `transform` attribute of its own. */
const ANIMATED = ["art-drag-move", "art-press", "art-ripple", "art-drop-move", "art-target"];

function draw(art: WelcomeArt): FakeElement {
  const { doc } = fakeRoot();
  return welcomeArt(doc as unknown as Document, art) as unknown as FakeElement;
}

function classes(element: FakeElement): string[] {
  return element.classList.toString().split(" ").filter(Boolean);
}

/** `settings.css` without its comments. */
const css = settingsCss.replace(/\/\*[\s\S]*?\*\//g, "");

/** The text between the `{` at or after `from` and its matching `}`. */
function block(text: string, from: number): { start: number; end: number } {
  const start = text.indexOf("{", from);
  let depth = 0;
  for (let index = start; index < text.length; index += 1) {
    if (text[index] === "{") depth += 1;
    if (text[index] === "}") depth -= 1;
    if (depth === 0) return { start, end: index };
  }
  throw new Error("unbalanced braces");
}

describe("welcomeArt", () => {
  it.each(ARTS)("draws %s as a decorative SVG", (art) => {
    const svg = draw(art);
    expect(svg.localName).toBe("svg");
    expect(svg.namespaceURI).toBe("http://www.w3.org/2000/svg");
    expect(svg.getAttribute("viewBox")).toBe("0 0 96 64");
    expect(svg.getAttribute("aria-hidden")).toBe("true");
    expect(svg.getAttribute("focusable")).toBe("false");
    expect(classes(svg)).toEqual(["welcome-art", `welcome-art-${art}`]);
    expect(descendants(svg).length).toBeGreaterThan(2);
  });

  it.each(ARTS)("takes every color from CSS, and has no words in %s", (art) => {
    for (const node of [draw(art), ...descendants(draw(art))]) {
      for (const name of ["fill", "stroke", "style", "color", "opacity"]) {
        expect(node.hasAttribute(name), `${node.localName} ${name}`).toBe(false);
      }
      expect(["text", "tspan", "image", "use"]).not.toContain(node.localName);
    }
  });

  it.each(ARTS)("draws only finite, non-empty shapes in %s", (art) => {
    for (const node of descendants(draw(art))) {
      for (const name of ["d", "transform", "x", "y", "cx", "cy", "r", "width", "height"]) {
        const value = node.getAttribute(name);
        if (value === null) continue;
        expect(value, `${node.localName} ${name}`).not.toMatch(/NaN|undefined|Infinity/);
        expect(value.trim(), `${node.localName} ${name}`).not.toBe("");
      }
    }
  });

  it("never animates an element that places itself with a transform attribute", () => {
    const animated = ARTS.flatMap((art) => descendants(draw(art))).filter((node) =>
      classes(node).some((name) => ANIMATED.includes(name)),
    );
    expect(animated.length).toBe(ANIMATED.length);
    for (const node of animated) {
      expect(node.hasAttribute("transform"), classes(node).join(" ")).toBe(false);
    }
  });

  it("drops the file on one of four wheel segments", () => {
    const nodes = descendants(draw("drop"));
    const segments = nodes.filter((node) => classes(node).includes("art-segment"));
    expect(segments).toHaveLength(4);
    const outlines = segments.map((node) => node.getAttribute("d") ?? "");
    expect(new Set(outlines).size).toBe(4);
    const targets = nodes.filter((node) => classes(node).includes("art-target"));
    expect(targets).toHaveLength(1);
    expect(targets[0]?.getAttribute("d")).toBe(outlines[1]);
  });

  it("styles every part it draws", () => {
    const used = new Set(
      ARTS.flatMap((art) => descendants(draw(art)))
        .flatMap(classes)
        .filter((name) => name.startsWith("art-")),
    );
    expect(used.size).toBeGreaterThan(0);
    for (const name of used) {
      expect(css, name).toMatch(new RegExp(`\\.${name}(?![\\w-])`));
    }
  });
});

describe("the illustrations' motion", () => {
  const gate = "@media (prefers-reduced-motion: no-preference)";

  it("runs only for users who haven't asked for reduced motion", () => {
    const at = css.indexOf(gate);
    expect(at).toBeGreaterThanOrEqual(0);
    expect(css.indexOf(gate, at + 1)).toBe(-1);
    const { start, end } = block(css, at);
    const uses = [...css.matchAll(/(?<![\w-])animation(?:-name)?\s*:/g)];
    expect(uses.length).toBe(ANIMATED.length);
    for (const use of uses) {
      expect(use.index > start && use.index < end, `animation at ${use.index}`).toBe(true);
    }
  });

  it("names only keyframes the sheet defines, each a loop", () => {
    const { start, end } = block(css, css.indexOf(gate));
    const declarations = [...css.slice(start, end).matchAll(/animation:\s*([\w-]+)[^;]*;/g)];
    expect(declarations).toHaveLength(ANIMATED.length);
    for (const [declaration, name] of declarations) {
      expect(css, String(name)).toContain(`@keyframes ${String(name)} {`);
      expect(declaration).toContain("infinite");
    }
  });
});
