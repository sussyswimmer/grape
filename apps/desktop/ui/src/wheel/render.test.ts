import { describe, expect, it, vi } from "vitest";
import { WHEEL_PAINTED } from "../shared/events";
import {
  byClass,
  byTag,
  type FakeElement,
  fakeRoot,
  oneByClass,
} from "../test-support/fake-dom";
import type { WheelIcon } from "../shared/events";
import overlayCss from "../styles/overlay.css?raw";
import { openFixture } from "../test-support/fixtures";
import { coreHitTest, entriesPayload, shortcutsPayload } from "../test-support/payloads";
import { GLYPH_GRID, GLYPHS } from "./glyphs";
import { mountWheel, type WheelView, type WheelViewOptions } from "./render";
import { TEXT } from "./scene";
import type { SoundSink } from "./tick";

interface Harness {
  root: FakeElement;
  view: WheelView;
  frames: (() => void)[];
  invoke: ReturnType<typeof vi.fn>;
  sounds: string[];
}

function harness(options: Partial<WheelViewOptions> = {}): Harness {
  const { root } = fakeRoot();
  const frames: (() => void)[] = [];
  const invoke = vi.fn(() => Promise.resolve());
  const sounds: string[] = [];
  const sound: SoundSink = {
    opened: (on) => sounds.push(`opened:${String(on)}`),
    play: () => sounds.push("play"),
    cancel: () => sounds.push("cancel"),
  };
  const view = mountWheel(root as unknown as HTMLElement, {
    invoke,
    requestFrame: (callback) => frames.push(callback),
    prefersReducedMotion: () => false,
    sound,
    ...options,
  });
  return { root, view, frames, invoke, sounds };
}

function svgOf(root: FakeElement): FakeElement {
  expect(root.children).toHaveLength(1);
  const [svg] = root.children;
  if (svg === undefined) throw new Error("no svg");
  return svg;
}

function texts(element: FakeElement): string[] {
  return byTag(element, "tspan").map((span) => span.textContent);
}

const pending = openFixture("wheel-open-pending.json"); // seq 1, sounds on
const entries = openFixture("wheel-open-entries.json"); // seq 2, sounds off
const empty = openFixture("wheel-open-empty.json"); // seq 3
const shortcuts = openFixture("wheel-open-shortcuts.json"); // seq 4, sounds on, inside "Edit"
const noShortcuts = openFixture("wheel-open-shortcuts-empty.json"); // seq 5

/** A number attribute of `element`; NaN when it is missing. */
function numberOf(element: FakeElement | undefined, name: string): number {
  return Number(element?.getAttribute(name) ?? "NaN");
}

/** The square a drawn icon covers: its center and side, read back from the DOM. */
function iconSquare(icon: FakeElement): { x: number; y: number; size: number } {
  if (icon.localName === "text") {
    return {
      x: numberOf(icon, "x"),
      y: numberOf(icon, "y"),
      size: numberOf(icon, "font-size") / TEXT.emojiScale,
    };
  }
  const match = /^translate\((\S+) (\S+)\) scale\((\S+)\)$/.exec(icon.getAttribute("transform") ?? "");
  if (match === null) throw new Error("no transform");
  const size = Number(match[3]) * GLYPH_GRID;
  return { x: Number(match[1]) + size / 2, y: Number(match[2]) + size / 2, size };
}

describe("mountWheel", () => {
  it("empties the root on mount", () => {
    const { doc, root } = fakeRoot();
    root.append(doc.createElement("p"));
    mountWheel(root as unknown as HTMLElement, { requestFrame: () => undefined });
    expect(root.children).toEqual([]);
  });
});

describe("open", () => {
  it("draws svg#wheel at anchor − backdropR, sized 2·backdropR, centered viewBox", () => {
    const { root, view } = harness();
    view.open(pending);
    const svg = svgOf(root);
    expect(svg.namespaceURI).toBe("http://www.w3.org/2000/svg");
    expect(svg.localName).toBe("svg");
    expect(svg.getAttribute("id")).toBe("wheel");
    expect(svg.classList.contains("wheel")).toBe(true);
    expect(svg.style.getPropertyValue("position")).toBe("absolute");
    expect(svg.style.getPropertyValue("left")).toBe("274px");
    expect(svg.style.getPropertyValue("top")).toBe("174px");
    expect(svg.getAttribute("width")).toBe("252");
    expect(svg.getAttribute("height")).toBe("252");
    expect(svg.getAttribute("viewBox")).toBe("-126 -126 252 252");
    expect(oneByClass(svg, "wheel-backdrop").getAttribute("r")).toBe("126");
  });

  it("labels the wheel for assistive tech", () => {
    const { root, view } = harness();
    view.open(pending);
    const svg = svgOf(root);
    expect(svg.getAttribute("role")).toBe("img");
    expect(svg.getAttribute("aria-label")).toBe("Convert wheel, loading");
    expect(svg.children[0]?.localName).toBe("title");
    expect(svg.children[0]?.textContent).toBe("Convert wheel, loading");
  });

  it("draws the pending ring: 8 faint placeholders and … in the center", () => {
    const { root, view } = harness();
    view.open(pending);
    const svg = svgOf(root);
    expect(svg.getAttribute("data-kind")).toBe("pending");
    expect(byClass(svg, "wheel-placeholder")).toHaveLength(8);
    expect(oneByClass(svg, "wheel-pending").getAttribute("aria-hidden")).toBe("true");
    expect(byClass(svg, "wheel-segment")).toHaveLength(0);
    expect(texts(oneByClass(svg, "wheel-center"))).toEqual(["…"]);
  });

  it("draws entries with a path, aria-label, <title> and label per segment", () => {
    const { root, view } = harness();
    const five = entriesPayload(5, { seq: 4, titles: ["JPG", "PNG", "WEBP", "TIFF", "BMP"] });
    view.open(five);
    const svg = svgOf(root);
    const paths = byClass(svg, "wheel-segment");
    expect(paths.map((p) => p.getAttribute("aria-label"))).toEqual(["JPG", "PNG", "WEBP", "TIFF", "BMP"]);
    expect(paths.map((p) => p.children[0]?.textContent)).toEqual(["JPG", "PNG", "WEBP", "TIFF", "BMP"]);
    expect(paths.every((p) => p.getAttribute("fill-rule") === "evenodd")).toBe(true);
    expect(paths.every((p) => (p.getAttribute("d") ?? "").startsWith("M "))).toBe(true);
    const labels = byClass(svg, "wheel-label");
    expect(labels.map((l) => l.textContent)).toEqual(["JPG", "PNG", "WEBP", "TIFF", "BMP"]);
    const first = byTag(labels[0] as FakeElement, "tspan")[0];
    expect(first?.getAttribute("x")).toBe("0");
    expect(first?.getAttribute("y")).toBe("-84");
    expect(byClass(svg, "wheel-center")).toHaveLength(0);
    expect(svg.classList.contains("dense")).toBe(false);
  });

  it("uses the dense class when the payload says so", () => {
    const { root, view } = harness();
    view.open(entriesPayload(8));
    expect(svgOf(root).classList.contains("dense")).toBe(true);
  });

  it("draws the empty-state message, no segments and no center label", () => {
    const { root, view } = harness();
    view.open(empty);
    const svg = svgOf(root);
    expect(byClass(svg, "wheel-segment")).toHaveLength(0);
    expect(byClass(svg, "wheel-center")).toHaveLength(0);
    const lines = texts(oneByClass(svg, "wheel-message"));
    expect(lines.length).toBeLessThanOrEqual(3);
    expect(lines.join(" ")).toBe(
      "No tools fit here. Switch to conversions with Shift or file actions with Alt.",
    );
  });

  it("animates a new wheel in, unless the user prefers reduced motion", () => {
    const moving = harness();
    moving.view.open(pending);
    expect(svgOf(moving.root).classList.contains("wheel-enter")).toBe(true);

    const still = harness({ prefersReducedMotion: () => true });
    still.view.open(pending);
    expect(svgOf(still.root).classList.contains("wheel-enter")).toBe(false);
  });

  it("redraws a swap or the entries arriving in place, without a new animation", () => {
    const { root, view } = harness({ prefersReducedMotion: () => true });
    view.open(pending);
    const first = svgOf(root);
    view.open({ ...entriesPayload(4), seq: 1 });
    expect(svgOf(root)).toBe(first);
    expect(first.classList.contains("wheel-enter")).toBe(false);
    expect(byClass(first, "wheel-segment")).toHaveLength(4);
    expect(byClass(first, "wheel-placeholder")).toHaveLength(0);
  });

  it("replaces the SVG for a new seq", () => {
    const { root, view } = harness();
    view.open(pending);
    const first = svgOf(root);
    view.open(entries);
    expect(svgOf(root)).not.toBe(first);
    expect(svgOf(root).getAttribute("data-seq")).toBe("2");
  });
});

describe("wheel_painted", () => {
  it("is invoked with the seq on the next frame after an open", async () => {
    const { view, frames, invoke } = harness();
    view.open(pending);
    expect(invoke).not.toHaveBeenCalled();
    expect(frames).toHaveLength(1);
    frames.shift()?.();
    expect(invoke).toHaveBeenCalledWith(WHEEL_PAINTED, { seq: 1 });
    await Promise.resolve();
  });

  it("is not invoked for highlights or closes", () => {
    const { view, frames } = harness();
    view.open(entriesPayload(3, { seq: 6 }));
    frames.length = 0;
    view.highlight({ seq: 6, index: 1 });
    view.close({ seq: 6, reason: "drop" });
    expect(frames).toHaveLength(0);
  });

  it("is skipped when the wheel closed before the frame", () => {
    const { view, frames, invoke } = harness();
    view.open(pending);
    view.close({ seq: 1, reason: "escape" });
    frames.shift()?.();
    expect(invoke).not.toHaveBeenCalled();
  });

  it("swallows a failing or throwing invoke", async () => {
    const rejecting = harness({ invoke: () => Promise.reject(new Error("no ipc")) });
    rejecting.view.open(pending);
    expect(() => rejecting.frames.shift()?.()).not.toThrow();
    await Promise.resolve();

    const throwing = harness({
      invoke: () => {
        throw new Error("no ipc");
      },
    });
    throwing.view.open(pending);
    expect(() => throwing.frames.shift()?.()).not.toThrow();
  });
});

describe("highlight", () => {
  it("marks the segment and its label and shows the title in the center", () => {
    const { root, view } = harness();
    view.open(entriesPayload(4, { seq: 5, titles: ["JPG", "PNG", "WEBP", "COPY PATH"] }));
    view.highlight({ seq: 5, index: 3 });
    const svg = svgOf(root);
    const flags = byClass(svg, "wheel-segment").map((p) => p.classList.contains("is-highlighted"));
    expect(flags).toEqual([false, false, false, true]);
    expect(byClass(svg, "wheel-label").map((l) => l.classList.contains("is-highlighted"))).toEqual(
      flags,
    );
    expect(texts(oneByClass(svg, "wheel-center"))).toEqual(["COPY PATH"]);
  });

  it("hides the center label on null", () => {
    const { root, view } = harness();
    view.open(entriesPayload(4, { seq: 5 }));
    view.highlight({ seq: 5, index: 1 });
    view.highlight({ seq: 5, index: null });
    expect(byClass(svgOf(root), "wheel-center")).toHaveLength(0);
    expect(byClass(svgOf(root), "is-highlighted")).toHaveLength(0);
  });

  it("ignores another seq", () => {
    const { root, view } = harness();
    view.open(entriesPayload(4, { seq: 5 }));
    view.highlight({ seq: 4, index: 1 });
    view.highlight({ seq: 6, index: 2 });
    expect(byClass(svgOf(root), "is-highlighted")).toHaveLength(0);
  });

  it("drives the tick: play on a new segment, cancel on null", () => {
    const { view, sounds } = harness();
    view.open(entriesPayload(4, { seq: 5, sounds: true }));
    view.highlight({ seq: 5, index: 1 });
    view.highlight({ seq: 5, index: 1 });
    view.highlight({ seq: 5, index: 2 });
    view.highlight({ seq: 5, index: null });
    expect(sounds).toEqual(["opened:true", "cancel", "play", "play", "cancel"]);
  });
});

describe("close", () => {
  it("removes every child at once for its seq", () => {
    const { root, view } = harness();
    view.open(entriesPayload(4, { seq: 5 }));
    view.highlight({ seq: 5, index: 0 });
    view.close({ seq: 5, reason: "drop" });
    expect(root.children).toEqual([]);
  });

  it("ignores a stale close and clears on a newer one", () => {
    const { root, view } = harness();
    view.open(entriesPayload(4, { seq: 5 }));
    view.close({ seq: 4, reason: "cancel" });
    expect(root.children).toHaveLength(1);
    view.close({ seq: 6, reason: "noFiles" });
    expect(root.children).toEqual([]);
  });

  it("then a new open draws a fresh wheel", () => {
    const { root, view } = harness();
    view.open(entriesPayload(4, { seq: 5 }));
    view.close({ seq: 5, reason: "dragEnded" });
    view.open(pending);
    expect(svgOf(root).getAttribute("data-kind")).toBe("pending");
    expect(svgOf(root).classList.contains("wheel-enter")).toBe(true);
  });
});

describe("a Shortcuts wheel", () => {
  it("draws the golden payload: names, an emoji, a glyph, a group's mark and the group's name", () => {
    const { root, view } = harness();
    view.open(shortcuts);
    const svg = svgOf(root);
    expect(svg.getAttribute("data-kind")).toBe("shortcuts");
    expect(svg.getAttribute("data-seq")).toBe("4");
    expect(svg.getAttribute("aria-label")).toBe("Shortcuts wheel, Edit, 3 choices");
    expect(svg.classList.contains("dense")).toBe(false);

    const paths = byClass(svg, "wheel-segment");
    expect(paths.map((p) => p.getAttribute("aria-label"))).toEqual(["Save", "Tidy up, group", "Select all"]);
    expect(paths.map((p) => p.getAttribute("data-index"))).toEqual(["0", "1", "2"]);
    expect(paths.every((p) => (p.getAttribute("d") ?? "").startsWith("M "))).toBe(true);

    expect(oneByClass(svg, "wheel-labels").getAttribute("aria-hidden")).toBe("true");
    expect(byClass(svg, "wheel-label").map((l) => l.textContent)).toEqual(["Save", "Tidy up", "Select all"]);
    expect(byClass(svg, "wheel-icon")).toHaveLength(2);

    // The emoji is text, centered on its square over the label.
    const emoji = oneByClass(svg, "wheel-emoji");
    expect(emoji.localName).toBe("text");
    expect(emoji.textContent).toBe("💾");
    expect(emoji.getAttribute("x")).toBe("0");
    expect(emoji.getAttribute("y")).toBe("-91.5");
    expect(emoji.getAttribute("font-size")).toBe("17");
    const save = byTag(byClass(svg, "wheel-label")[0] as FakeElement, "tspan")[0];
    expect(save?.getAttribute("x")).toBe("0");
    expect(save?.getAttribute("y")).toBe("-73");

    // The glyph is its paths, moved and scaled onto its square.
    const glyph = oneByClass(svg, "wheel-glyph");
    expect(glyph.localName).toBe("g");
    expect(glyph.getAttribute("transform")).toBe("translate(62 24.5) scale(0.833333)");
    expect(glyph.children.map((p) => p.localName)).toEqual(GLYPHS.stack.map(() => "path"));
    expect(glyph.children.map((p) => p.getAttribute("d"))).toEqual([...GLYPHS.stack]);

    const mark = oneByClass(svg, "wheel-group-mark");
    expect(mark.localName).toBe("path");
    expect(mark.getAttribute("d")).toMatch(/^M \S+ \S+ A 123 123 0 0 1 \S+ \S+$/);

    expect(texts(oneByClass(svg, "wheel-center"))).toEqual(["Edit"]);
    expect(byClass(svg, "wheel-message")).toHaveLength(0);
    expect(byClass(svg, "wheel-placeholder")).toHaveLength(0);
  });

  it("carries no <title> at all, so no tooltip pops over a wheel that stays open", () => {
    const { root, view } = harness();
    view.open(shortcuts);
    expect(byTag(svgOf(root), "title")).toEqual([]);
    expect(svgOf(root).getAttribute("role")).toBe("img");
    // A highlight and a group's redraw are drawn the same way.
    view.highlight({ seq: 4, index: 0 });
    view.open(shortcutsPayload(8, { seq: 4, groups: [3] }));
    expect(byTag(svgOf(root), "title")).toEqual([]);
    view.open(noShortcuts);
    expect(byTag(svgOf(root), "title")).toEqual([]);
    expect(svgOf(root).getAttribute("aria-label")).not.toBe("");

    // The drag wheels keep theirs: on the wheel and on every segment.
    view.open(entriesPayload(4, { seq: 9 }));
    expect(byTag(svgOf(root), "title")).toHaveLength(5);
  });

  it("paints and ticks on a highlight, and marks the icon and the group's mark with the segment", () => {
    const { root, view, sounds } = harness();
    view.open(shortcuts);
    view.highlight({ seq: 4, index: 1 });
    const svg = svgOf(root);
    const flags = byClass(svg, "wheel-segment").map((p) => p.classList.contains("is-highlighted"));
    expect(flags).toEqual([false, true, false]);
    expect(byClass(svg, "wheel-label").map((l) => l.classList.contains("is-highlighted"))).toEqual(flags);
    expect(oneByClass(svg, "wheel-glyph").classList.contains("is-highlighted")).toBe(true);
    expect(oneByClass(svg, "wheel-emoji").classList.contains("is-highlighted")).toBe(false);
    expect(oneByClass(svg, "wheel-group-mark").classList.contains("is-highlighted")).toBe(true);
    expect(texts(oneByClass(svg, "wheel-center"))).toEqual(["Tidy up"]);

    view.highlight({ seq: 4, index: 0 });
    expect(oneByClass(svgOf(root), "wheel-emoji").classList.contains("is-highlighted")).toBe(true);
    expect(oneByClass(svgOf(root), "wheel-glyph").classList.contains("is-highlighted")).toBe(false);
    expect(texts(oneByClass(svgOf(root), "wheel-center"))).toEqual(["Save"]);

    view.highlight({ seq: 4, index: null });
    expect(byClass(svgOf(root), "is-highlighted")).toHaveLength(0);
    expect(texts(oneByClass(svgOf(root), "wheel-center"))).toEqual(["Edit"]);
    expect(sounds).toEqual(["opened:true", "cancel", "play", "play", "cancel"]);
  });

  it("ignores a highlight past the last shortcut", () => {
    const { root, view, sounds } = harness();
    view.open(shortcuts);
    view.highlight({ seq: 4, index: 3 });
    expect(byClass(svgOf(root), "is-highlighted")).toHaveLength(0);
    expect(sounds).toEqual(["opened:true", "cancel"]);
  });

  it("shows icons alone in the dense style, with the full name spoken and in the center on hover", () => {
    const { root, view } = harness();
    const icons: WheelIcon[] = Array.from({ length: 8 }, (_, index) =>
      index % 2 === 0 ? { kind: "glyph", value: "clock" } : { kind: "emoji", value: "🍇" },
    );
    const titles = Array.from({ length: 8 }, (_, index) => `Open the report ${index}`);
    view.open(shortcutsPayload(8, { seq: 6, icons, titles, groups: [3] }));
    const svg = svgOf(root);
    expect(svg.classList.contains("dense")).toBe(true);
    expect(byClass(svg, "wheel-label")).toHaveLength(0);
    expect(byClass(svg, "wheel-glyph")).toHaveLength(4);
    expect(byClass(svg, "wheel-emoji")).toHaveLength(4);
    const spoken = titles.map((title, index) => (index === 3 ? `${title}, group` : title));
    const paths = byClass(svg, "wheel-segment");
    expect(paths.map((p) => p.getAttribute("aria-label"))).toEqual(spoken);
    expect(byTag(svg, "title")).toEqual([]);
    expect(byClass(svg, "wheel-center")).toHaveLength(0);
    view.highlight({ seq: 6, index: 3 });
    expect(texts(oneByClass(svgOf(root), "wheel-center"))).toEqual(["Open the", "report 3"]);
  });

  it("draws no icon for a glyph it doesn't know, and keeps the label", () => {
    const { root, view } = harness();
    const unknown: WheelIcon = { kind: "glyph", value: "rocket" };
    view.open(shortcutsPayload(8, { icons: Array<WheelIcon>(8).fill(unknown) }));
    const svg = svgOf(root);
    expect(byClass(svg, "wheel-icon")).toHaveLength(0);
    expect(byClass(svg, "wheel-label")).toHaveLength(8);
    expect(byClass(svg, "wheel-label")[0]?.textContent).toBe("Item 0");
  });

  it("puts what the user typed in as text, whatever it looks like", () => {
    const { root, view } = harness();
    const markup = "<b>&amp;</b>";
    view.open(shortcutsPayload(3, { titles: [markup], icons: [{ kind: "emoji", value: "<" }], trail: [markup] }));
    const svg = svgOf(root);
    expect(byClass(svg, "wheel-segment")[0]?.getAttribute("aria-label")).toBe(markup);
    expect(oneByClass(svg, "wheel-emoji").textContent).toBe("<");
    expect(oneByClass(svg, "wheel-emoji").children).toEqual([]);
    expect(texts(oneByClass(svg, "wheel-center")).join("")).toBe(markup);
    expect(byTag(svg, "b")).toHaveLength(0);
  });

  it("keeps every drawn icon and label line inside its segment, for 1 to 12 shortcuts", () => {
    const g = shortcuts.geometry;
    for (let n = 1; n <= 12; n += 1) {
      const { root, view } = harness();
      const icons: WheelIcon[] = Array.from({ length: n }, (_, index) =>
        index % 2 === 0 ? { kind: "emoji", value: "🍇" } : { kind: "glyph", value: "lock" },
      );
      view.open(shortcutsPayload(n, { icons, titles: Array<string>(n).fill("Mission Control") }));
      const svg = svgOf(root);
      const drawn = byClass(svg, "wheel-icon");
      expect(drawn).toHaveLength(n);
      drawn.forEach((icon, index) => {
        const { x, y, size } = iconSquare(icon);
        const half = size / 2;
        for (const [dx, dy] of [[-half, -half], [half, -half], [-half, half], [half, half]] as const) {
          expect(coreHitTest(x + dx, y + dy, n, g), `n=${n} icon ${index}`).toBe(index);
        }
      });
      const labels = byClass(svg, "wheel-label");
      expect(labels).toHaveLength(n >= g.denseAt ? 0 : n);
      labels.forEach((label, index) => {
        const spans = byTag(label, "tspan");
        expect(spans).toHaveLength(1);
        const x = numberOf(spans[0], "x");
        const y = numberOf(spans[0], "y");
        for (const dy of [-TEXT.labelLineHeight / 2, TEXT.labelLineHeight / 2]) {
          expect(coreHitTest(x, y + dy, n, g), `n=${n} label ${index}`).toBe(index);
        }
      });
    }
  });

  it("draws a group's mark on the backdrop's rim, outside the ring", () => {
    const g = shortcuts.geometry;
    for (let n = 1; n <= 12; n += 1) {
      const { root, view } = harness();
      view.open(shortcutsPayload(n, { groups: Array.from({ length: n }, (_, index) => index) }));
      const marks = byClass(svgOf(root), "wheel-group-mark");
      expect(marks).toHaveLength(n);
      for (const mark of marks) {
        const numbers = (mark.getAttribute("d") ?? "").split(" ").map(Number);
        const [, x0, y0, , r, , , , , x1, y1] = numbers;
        expect(r).toBeGreaterThan(g.outerR);
        expect(r).toBeLessThan(g.backdropR);
        expect(Math.hypot(x0 ?? 0, y0 ?? 0)).toBeCloseTo(r ?? 0, 4);
        expect(Math.hypot(x1 ?? 0, y1 ?? 0)).toBeCloseTo(r ?? 0, 4);
      }
    }
  });

  it("shows the empty message for a menu with no shortcuts, through the message's own element", () => {
    const { root, view } = harness();
    view.open(noShortcuts);
    const svg = svgOf(root);
    expect(svg.getAttribute("data-kind")).toBe("shortcuts");
    expect(svg.getAttribute("aria-label")).toBe("Shortcuts wheel, no choices");
    expect(byClass(svg, "wheel-segment")).toHaveLength(0);
    expect(byClass(svg, "wheel-center")).toHaveLength(0);
    const lines = texts(oneByClass(svg, "wheel-message"));
    expect(lines.length).toBeLessThanOrEqual(3);
    expect(lines.join(" ")).toBe("No shortcuts here yet. Add some in Settings.");
    view.highlight({ seq: 5, index: 0 });
    expect(byClass(svgOf(root), "is-highlighted")).toHaveLength(0);
  });

  it("redraws in place when a group opens, without a new animation", () => {
    const { root, view } = harness();
    view.open(shortcutsPayload(4, { seq: 9, groups: [1] }));
    const first = svgOf(root);
    expect(first.classList.contains("wheel-enter")).toBe(true);
    view.highlight({ seq: 9, index: 1 });
    view.open(shortcutsPayload(2, { seq: 9, trail: ["Item 1"] }));
    expect(svgOf(root)).toBe(first);
    expect(byClass(first, "wheel-segment")).toHaveLength(2);
    expect(byClass(first, "is-highlighted")).toHaveLength(0);
    expect(byClass(first, "wheel-group-mark")).toHaveLength(0);
    expect(texts(oneByClass(first, "wheel-center"))).toEqual(["Item 1"]);
    // Back out of the group: the same element again.
    view.open(shortcutsPayload(4, { seq: 9, groups: [1] }));
    expect(svgOf(root)).toBe(first);
    expect(byClass(first, "wheel-center")).toHaveLength(0);
  });

  it("clears on a close with the reason picked", () => {
    const { root, view, sounds } = harness();
    view.open(shortcuts);
    view.highlight({ seq: 4, index: 0 });
    view.close({ seq: 4, reason: "picked" });
    expect(root.children).toEqual([]);
    expect(sounds[sounds.length - 1]).toBe("cancel");
  });

  it("tells Rust nothing but wheel_painted", () => {
    const { view, frames, invoke } = harness();
    view.open(shortcuts);
    view.highlight({ seq: 4, index: 1 });
    view.open(shortcutsPayload(2, { seq: 4, trail: ["Tidy up"] }));
    view.highlight({ seq: 4, index: 0 });
    while (frames.length > 0) frames.shift()?.();
    view.close({ seq: 4, reason: "picked" });
    expect(invoke.mock.calls.length).toBeGreaterThan(0);
    for (const call of invoke.mock.calls) {
      expect(call).toEqual([WHEEL_PAINTED, { seq: 4 }]);
    }
  });
});

describe("overlay.css", () => {
  const css = overlayCss.replace(/\/\*[\s\S]*?\*\//g, "");

  /** The body of the first `@media (query)` block. */
  function media(query: string): string {
    const at = css.indexOf(`@media (${query})`);
    expect(at, query).toBeGreaterThanOrEqual(0);
    const start = css.indexOf("{", at);
    let depth = 0;
    for (let index = start; index < css.length; index += 1) {
      if (css[index] === "{") depth += 1;
      if (css[index] === "}") depth -= 1;
      if (depth === 0) return css.slice(start + 1, index);
    }
    throw new Error("unbalanced braces");
  }

  const SHORTCUT_CLASSES = [".wheel-emoji", ".wheel-glyph", ".wheel-group-mark"];

  it("styles every class a shortcut's icon and mark carry", () => {
    for (const name of SHORTCUT_CLASSES) {
      expect(css, name).toContain(`${name} {`);
      expect(css, name).toContain(`${name}.is-highlighted`);
    }
  });

  it("gives them the system's colors under forced colors, highlighted or not", () => {
    const forced = media("forced-colors: active");
    for (const name of SHORTCUT_CLASSES) {
      expect(forced, name).toContain(name);
      expect(forced, name).toContain(`${name}.is-highlighted`);
    }
    expect(forced).toContain("HighlightText");
  });

  it("strokes a glyph and never fills it", () => {
    expect(css).toMatch(/\.wheel-glyph \{[^}]*fill: none;[^}]*stroke-linecap: round;[^}]*stroke-linejoin: round;/);
  });

  it("still switches the entry animation off for reduced motion", () => {
    expect(media("prefers-reduced-motion: reduce")).toMatch(/\.wheel\.wheel-enter\s*\{\s*animation: none;/);
  });

  it("lets a press fall through the labels, icons and marks to the segment", () => {
    for (const name of [".wheel-emoji", ".wheel-glyph", ".wheel-group-mark", ".wheel-label"]) {
      const rules = [...css.matchAll(/([^{}]+)\{([^{}]*)\}/g)].filter((rule) =>
        (rule[1] ?? "").split(",").some((selector) => selector.trim() === name),
      );
      expect(rules.some((rule) => (rule[2] ?? "").includes("pointer-events: none")), name).toBe(true);
    }
  });
});
