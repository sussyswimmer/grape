import { describe, expect, it } from "vitest";
import toastCss from "./toast.css?raw";

// What the fake DOM can't see: the rules in toast.css that let an error's
// words show in full. A real WebView still has to show them (TESTING.md →
// "PDF, M3/M4": the scanned PDF's toast).

/** `toast.css` without its comments. */
const css = toastCss.replace(/\/\*[\s\S]*?\*\//g, "");

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

/** The sheet without its `@media` blocks: the rules every window gets. */
function topLevel(): string {
  let text = css;
  for (let at = text.indexOf("@media"); at >= 0; at = text.indexOf("@media")) {
    const { end } = block(text, at);
    text = text.slice(0, at) + text.slice(end + 1);
  }
  return text;
}

/** One selector's declarations, merged in order over every rule that lists it. */
function declarations(selector: string): Map<string, string> {
  const found = new Map<string, string>();
  for (const match of topLevel().matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    const selectors = (match[1] ?? "").split(",").map((part) => part.trim().replace(/\s+/g, " "));
    if (!selectors.includes(selector)) continue;
    for (const part of (match[2] ?? "").split(";")) {
      const colon = part.indexOf(":");
      if (colon > 0) found.set(part.slice(0, colon).trim(), part.slice(colon + 1).trim());
    }
  }
  return found;
}

/** `name`'s value on `selector`, falling back to `base`'s. */
function value(selector: string, base: string, name: string): string {
  const own = declarations(selector).get(name) ?? declarations(base).get(name);
  expect(own, `${selector} ${name}`).toBeDefined();
  return own ?? "";
}

const ALONE = '.toast[data-tone="error"] .toast-title:only-child';
const UNDER = '.toast[data-tone="error"] .toast-detail';
/** An info toast's detail: "Nothing to change" over a tool's own sentence. */
const INFO = '.toast[data-tone="info"] .toast-detail';

describe("toast.css", () => {
  it("keeps titles and details to one line, cut with an ellipsis", () => {
    for (const selector of [".toast-title", ".toast-detail"]) {
      expect(declarations(selector).get("white-space"), selector).toBe("nowrap");
      expect(declarations(selector).get("text-overflow"), selector).toBe("ellipsis");
    }
  });

  it("gives an error's title alone, an error's detail and an info toast's detail two lines", () => {
    for (const selector of [ALONE, UNDER, INFO]) {
      const rule = declarations(selector);
      expect(rule.get("display"), selector).toBe("-webkit-box");
      expect(rule.get("-webkit-box-orient"), selector).toBe("vertical");
      expect(rule.get("-webkit-line-clamp"), selector).toBe("2");
      expect(rule.get("white-space"), selector).toBe("normal");
      // A long name with no spaces still breaks.
      expect(rule.get("overflow-wrap"), selector).toBe("anywhere");
    }
  });

  it("fits an error's, or an info toast's, three lines in the pill", () => {
    // The window is 64 px (Rust sets it); the pill is 8 px shorter and has
    // a 1 px border.
    const pill = 64 - 8 - 2;
    const px = (text: string): number => Number.parseFloat(text);
    const line = (selector: string, base: string): number =>
      px(value(selector, base, "font-size")) * px(value(selector, base, "line-height"));
    const title = line(".toast-title", ".toast-title");
    const gap = px(declarations(".toast-text").get("gap") ?? "0");
    expect(2 * line(ALONE, ".toast-title")).toBeLessThanOrEqual(pill);
    expect(title + gap + 2 * line(UNDER, ".toast-detail")).toBeLessThanOrEqual(pill - 6);
    expect(title + gap + 2 * line(INFO, ".toast-detail")).toBeLessThanOrEqual(pill - 6);
  });
});
