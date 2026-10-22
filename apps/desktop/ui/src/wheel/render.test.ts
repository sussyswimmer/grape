import { describe, expect, it, vi } from "vitest";
import { WHEEL_PAINTED } from "../shared/events";
import {
  byClass,
  byTag,
  type FakeElement,
  fakeRoot,
  oneByClass,
} from "../test-support/fake-dom";
import { openFixture } from "../test-support/fixtures";
import { entriesPayload } from "../test-support/payloads";
import { mountWheel, type WheelView, type WheelViewOptions } from "./render";
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
      "No tools fit this selection. Tap Shift to convert or Alt for file actions.",
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
