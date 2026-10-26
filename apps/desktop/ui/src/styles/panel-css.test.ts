import { describe, expect, it } from "vitest";
import panelCss from "./panel.css?raw";

// What the fake DOM can't see: rules in panel.css that keep the panel
// readable. A real WebView still has to show them (TESTING.md → "Tool
// panels, M4").

/** `panel.css` without its comments. */
const css = panelCss.replace(/\/\*[\s\S]*?\*\//g, "");

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

/** The inside of the one `@media` block for `query`. */
function media(query: string): string {
  const at = css.indexOf(`@media ${query}`);
  expect(at, query).toBeGreaterThanOrEqual(0);
  const { start, end } = block(css, at);
  return css.slice(start + 1, end);
}

/** The sheet without its `@media` blocks: the rules every window gets. */
function topLevel(): string {
  let text = css;
  for (let at = text.indexOf("@media"); at >= 0; at = text.indexOf("@media")) {
    const { end } = block(text, at);
    text = text.slice(0, at) + text.slice(end + 1);
  }
  return text;
}

/** One selector's declarations, merged in order over every rule in `text` that lists it. */
function declarations(text: string, selector: string): Map<string, string> {
  const found = new Map<string, string>();
  for (const match of text.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    const selectors = (match[1] ?? "").split(",").map((part) => part.trim().replace(/\s+/g, " "));
    if (!selectors.includes(selector)) continue;
    for (const part of (match[2] ?? "").split(";")) {
      const colon = part.indexOf(":");
      if (colon > 0) found.set(part.slice(0, colon).trim(), part.slice(colon + 1).trim());
    }
  }
  return found;
}

describe("panel.css", () => {
  it("in a contrast theme, rings focus inside a Highlight fill in HighlightText", () => {
    const forced = media("(forced-colors: active)");
    // The checked segment and the selected row are Highlight already, so a
    // Highlight ring on them can't be seen.
    expect(declarations(forced, ".panel-segment.is-checked .panel-segment-text").get("background")).toBe("Highlight");
    expect(declarations(forced, ".panel-row.is-selected").get("background")).toBe("Highlight");
    for (const focused of [
      ".panel-segment.is-checked .panel-segment-input:focus-visible + .panel-segment-text",
      ".panel-list:focus-visible .panel-row.is-selected",
    ]) {
      const ring = declarations(forced, focused);
      expect(ring.get("outline-color"), focused).toBe("HighlightText");
      expect(ring.get("outline-offset"), focused).toBe("-4px");
    }
  });

  it("marks the selected row in a way a hovered row isn't", () => {
    const sheet = topLevel();
    const root = declarations(sheet, ":root");
    const look = (selector: string): string => {
      const fill = declarations(sheet, selector).get("background") ?? "";
      return fill.replace(/var\((--[\w-]+)\)/g, (_, name: string) => root.get(name) ?? name);
    };
    const mark = declarations(sheet, ".panel-row.is-selected::before");
    const marked = mark.get("content") !== undefined && mark.get("background") !== undefined;
    expect(look(".panel-row.is-selected") !== look(".panel-row:hover") || marked).toBe(true);
    expect(declarations(sheet, ".panel-row:hover::before").size).toBe(0);
  });

  it("gives touch and pen drags on the order list to the page's reorder", () => {
    expect(declarations(topLevel(), ".panel-list").get("touch-action")).toBe("none");
  });
});
