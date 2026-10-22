import { describe, expect, it } from "vitest";
import { isWheelOpenPayload, type WheelOpenPayload } from "../shared/events";
import { openFixture } from "../test-support/fixtures";
import { entriesPayload } from "../test-support/payloads";
import { emptyMessage } from "./copy";
import { segmentPath } from "./geometry";
import {
  applyClose,
  applyHighlight,
  applyOpen,
  buildScene,
  CLOSED,
  labelLines,
  messageLines,
  PENDING_SEGMENTS,
  pendingPaths,
  stackLines,
  TEXT,
  validHighlight,
  type WheelScene,
  type WheelState,
  wrapLines,
} from "./scene";

const pending = openFixture("wheel-open-pending.json"); // seq 1
const entries = openFixture("wheel-open-entries.json"); // seq 2
const empty = openFixture("wheel-open-empty.json"); // seq 3

function opened(payload: WheelOpenPayload, highlight: number | null = null): WheelState {
  return { shown: payload, highlight };
}

function scene(state: WheelState): WheelScene {
  const built = buildScene(state);
  if (built === null) throw new Error("no scene");
  return built;
}

describe("applyOpen", () => {
  it("draws a fresh wheel for a new seq", () => {
    const step = applyOpen(CLOSED, pending);
    expect(step.paint).toBe("fresh");
    expect(step.state).toEqual({ shown: pending, highlight: null });
    expect(applyOpen(opened(pending), entries).paint).toBe("fresh");
  });

  it("redraws in place for the same seq (key swap or entries arriving) and resets the highlight", () => {
    const five = entriesPayload(5, { seq: 7 });
    const swapped = { ...entriesPayload(3, { seq: 7 }), mode: "tools" as const };
    const step = applyOpen(opened(five, 2), swapped);
    expect(step.paint).toBe("update");
    expect(step.state).toEqual({ shown: swapped, highlight: null });
    expect(step.sound).toBe("cancel");
  });

  it("shows every open, even one with a lower seq (Rust's counter may restart)", () => {
    expect(applyOpen(opened(empty), pending).paint).toBe("fresh");
  });
});

describe("applyHighlight", () => {
  const five = entriesPayload(5, { seq: 9, sounds: true });

  it("highlights a new segment and plays the tick when sounds are on", () => {
    const step = applyHighlight(opened(five), { seq: 9, index: 2 });
    expect(step).toEqual({ state: opened(five, 2), paint: "update", sound: "play" });
  });

  it("stays silent when sounds are off", () => {
    const quiet = entriesPayload(5, { seq: 9, sounds: false });
    expect(applyHighlight(opened(quiet), { seq: 9, index: 2 }).sound).toBe("none");
  });

  it("does nothing when the index doesn't change", () => {
    const state = opened(five, 2);
    expect(applyHighlight(state, { seq: 9, index: 2 })).toEqual({
      state,
      paint: "none",
      sound: "none",
    });
    expect(applyHighlight(opened(five), { seq: 9, index: null }).paint).toBe("none");
  });

  it("clears the highlight and cancels the tick on null", () => {
    const step = applyHighlight(opened(five, 2), { seq: 9, index: null });
    expect(step).toEqual({ state: opened(five, null), paint: "update", sound: "cancel" });
  });

  it("ticks again when moving back onto a segment after a gap", () => {
    const off = applyHighlight(opened(five, 2), { seq: 9, index: null }).state;
    expect(applyHighlight(off, { seq: 9, index: 2 }).sound).toBe("play");
  });

  it("ignores highlights for another seq or a closed wheel", () => {
    const state = opened(five, 1);
    expect(applyHighlight(state, { seq: 8, index: 3 }).paint).toBe("none");
    expect(applyHighlight(state, { seq: 10, index: 3 }).state).toBe(state);
    expect(applyHighlight(CLOSED, { seq: 9, index: 3 })).toEqual({
      state: CLOSED,
      paint: "none",
      sound: "none",
    });
  });

  it("treats an index outside the content as no highlight", () => {
    expect(applyHighlight(opened(five), { seq: 9, index: 5 }).paint).toBe("none");
    expect(applyHighlight(opened(five, 1), { seq: 9, index: 99 }).state.highlight).toBeNull();
    expect(applyHighlight(opened(pending), { seq: 1, index: 0 }).paint).toBe("none");
    expect(applyHighlight(opened(empty), { seq: 3, index: 0 }).paint).toBe("none");
  });
});

describe("applyClose", () => {
  it("clears the wheel for its seq", () => {
    expect(applyClose(opened(entries, 0), { seq: 2, reason: "drop" })).toEqual({
      state: CLOSED,
      paint: "clear",
      sound: "cancel",
    });
  });

  it("ignores a stale close for an earlier seq", () => {
    const state = opened(entries);
    expect(applyClose(state, { seq: 1, reason: "cancel" })).toEqual({
      state,
      paint: "none",
      sound: "none",
    });
  });

  it("clears on a close for a newer seq, whose open was lost", () => {
    expect(applyClose(opened(entries), { seq: 5, reason: "noFiles" }).paint).toBe("clear");
  });

  it("does nothing when already closed", () => {
    expect(applyClose(CLOSED, { seq: 1, reason: "escape" }).paint).toBe("none");
  });
});

describe("validHighlight", () => {
  it("accepts only indexes of drawn segments", () => {
    const content = entriesPayload(3).content;
    expect(validHighlight(content, 0)).toBe(0);
    expect(validHighlight(content, 2)).toBe(2);
    expect(validHighlight(content, 3)).toBeNull();
    expect(validHighlight(content, -1)).toBeNull();
    expect(validHighlight(content, 1.5)).toBeNull();
    expect(validHighlight(content, null)).toBeNull();
    expect(validHighlight({ kind: "pending" }, 0)).toBeNull();
  });
});

describe("wrapLines", () => {
  it("packs words greedily without splitting them", () => {
    expect(wrapLines("MERGE PDF", 10, 2)).toEqual(["MERGE PDF"]);
    expect(wrapLines("CREATE PDF", 8, 2)).toEqual(["CREATE", "PDF"]);
    expect(wrapLines("NORMALIZE", 8, 2)).toEqual(["NORMALIZE"]);
    expect(wrapLines("  a   b  ", 10, 2)).toEqual(["a b"]);
    expect(wrapLines("", 10, 2)).toEqual([]);
  });

  it("ends the last line with an ellipsis when it runs out of lines", () => {
    expect(wrapLines("one two three four", 7, 2)).toEqual(["one two", "three…"]);
    expect(wrapLines("aaaa bbbb cccc", 4, 2)).toEqual(["aaaa", "bbb…"]);
    expect(wrapLines("a b", 1, 1)).toEqual(["…"]);
    expect(wrapLines("a b", 5, 0)).toEqual([]);
  });
});

describe("labelLines", () => {
  it("uses one line for short titles and wraps longer ones", () => {
    expect(labelLines("PNG", false)).toEqual(["PNG"]);
    expect(labelLines("COPY PATH", false)).toEqual(["COPY PATH"]);
    expect(labelLines("COPY PATH", true)).toEqual(["COPY", "PATH"]);
    expect(labelLines("CREATE PDF", true)).toEqual(["CREATE", "PDF"]);
  });

  it("never uses more than two lines", () => {
    expect(labelLines("A VERY LONG TOOL NAME INDEED", false).length).toBeLessThanOrEqual(2);
  });
});

describe("messageLines", () => {
  it("fits both empty states in at most 3 lines with the default keys", () => {
    for (const state of ["noConversions", "noTools"] as const) {
      const text = emptyMessage(state, entries.keys);
      const lines = messageLines(text);
      expect(lines.length).toBeLessThanOrEqual(TEXT.messageLines);
      expect(lines.join(" ")).toBe(text);
    }
  });

  it("widens before cutting, and cuts with an ellipsis only as a last resort", () => {
    const keys = { conversions: "Right Shift", tools: "Right Ctrl", actions: "Right Alt" };
    const text = emptyMessage("noTools", keys);
    const lines = messageLines(text);
    expect(lines.length).toBeLessThanOrEqual(3);
    expect(lines.join(" ")).toBe(text);
    const huge = { conversions: "x".repeat(40), tools: "t", actions: "y".repeat(40) };
    const cut = messageLines(emptyMessage("noTools", huge));
    expect(cut).toHaveLength(3);
    expect(cut[2]?.endsWith("…")).toBe(true);
  });
});

describe("stackLines", () => {
  it("centers the block vertically on y", () => {
    expect(stackLines(["a"], 5, 10, 12)).toEqual({ x: 5, lines: [{ text: "a", y: 10 }] });
    expect(stackLines(["a", "b"], 0, 0, 12).lines.map((l) => l.y)).toEqual([-6, 6]);
    expect(stackLines(["a", "b", "c"], 0, -84, 10).lines.map((l) => l.y)).toEqual([-94, -84, -74]);
  });
});

describe("buildScene", () => {
  it("is null when closed", () => {
    expect(buildScene(CLOSED)).toBeNull();
  });

  it("places the SVG at anchor − backdropR with a viewBox centered on the anchor", () => {
    const s = scene(opened(pending));
    expect(s.left).toBe(400 - 126);
    expect(s.top).toBe(300 - 126);
    expect(s.size).toBe(252);
    expect(s.viewBox).toBe("-126 -126 252 252");
    expect(s.backdropR).toBe(126);
  });

  it("shows 8 placeholder segments and … while pending", () => {
    const s = scene(opened(pending));
    expect(s.kind).toBe("pending");
    expect(s.placeholders).toHaveLength(PENDING_SEGMENTS);
    expect(s.placeholders).toEqual(pendingPaths(pending));
    expect(s.placeholders.every((d) => d.startsWith("M ") && !d.includes("NaN"))).toBe(true);
    expect(s.segments).toEqual([]);
    expect(s.center?.lines.map((l) => l.text)).toEqual(["…"]);
    expect(s.message).toBeNull();
    expect(s.label).toBe("Convert wheel, loading");
  });

  it("lays the placeholders out like grape-core: item 0 at the top, clockwise", () => {
    const paths = pendingPaths(pending);
    expect(paths[0]).toBe(segmentPath({ center: Math.PI / 2 }, 8, pending.geometry));
    expect(paths[2]).toBe(segmentPath({ center: 0 }, 8, pending.geometry));
  });

  it("shows the empty-state message with the payload's key names", () => {
    const s = scene(opened(empty));
    expect(s.segments).toEqual([]);
    expect(s.placeholders).toEqual([]);
    expect(s.center).toBeNull();
    const lines = s.message?.lines.map((l) => l.text) ?? [];
    expect(lines.length).toBeGreaterThan(0);
    expect(lines.length).toBeLessThanOrEqual(3);
    expect(lines.join(" ")).toBe(
      "No tools fit here. Switch to conversions with Shift or file actions with Alt.",
    );
    expect(s.label).toBe("Tools wheel, no choices");
  });

  it("follows custom key names in the empty state", () => {
    const caps = { ...empty, keys: { conversions: "Caps", tools: "Ctrl", actions: "Right Alt" } };
    if (!isWheelOpenPayload(caps)) throw new Error("bad payload");
    const text = scene(opened(caps)).message?.lines.map((l) => l.text).join(" ");
    expect(text).toBe("No tools fit here. Switch to conversions with Caps or file actions with Right Alt.");
  });

  it("draws one path per entry with its label at (labelX, labelY)", () => {
    const s = scene(opened(entries));
    expect(s.kind).toBe("entries");
    expect(s.segments).toHaveLength(1);
    const [png] = s.segments;
    expect(png?.title).toBe("PNG");
    expect(png?.d).toBe(segmentPath({ center: 1.5 }, 1, entries.geometry));
    expect(png?.label).toEqual({ x: 0, lines: [{ text: "PNG", y: -84 }] });
    expect(png?.highlighted).toBe(false);
    expect(s.center).toBeNull();
    expect(s.label).toBe("Convert wheel, 1 choice");
  });

  it("marks the highlighted segment and shows its title in the center", () => {
    const five = entriesPayload(5, { titles: ["JPG", "PNG", "WEBP", "CREATE PDF", "BMP"] });
    const s = scene(opened(five, 3));
    expect(s.segments.map((seg) => seg.highlighted)).toEqual([false, false, false, true, false]);
    expect(s.center?.lines.map((l) => l.text)).toEqual(["CREATE PDF"]);
    expect(s.center?.x).toBe(0);
  });

  it("hides the center label when nothing is highlighted", () => {
    expect(scene(opened(entriesPayload(5), null)).center).toBeNull();
  });

  it("puts every label exactly at Rust's label point", () => {
    for (let n = 1; n <= 12; n += 1) {
      const payload = entriesPayload(n);
      if (payload.content.kind !== "entries") throw new Error("not entries");
      const s = scene(opened(payload));
      s.segments.forEach((seg, i) => {
        const source = payload.content.kind === "entries" ? payload.content.segments[i] : undefined;
        expect(seg.label.x).toBe(source?.labelX);
        const ys = seg.label.lines.map((l) => l.y);
        const middle = ys.reduce((a, b) => a + b, 0) / ys.length;
        expect(middle).toBeCloseTo(source?.labelY ?? Number.NaN, 9);
        expect(seg.d).not.toContain("NaN");
      });
    }
  });

  it("uses the dense style from the payload", () => {
    expect(scene(opened(entriesPayload(6))).dense).toBe(false);
    const dense = scene(opened(entriesPayload(7, { titles: Array(7).fill("COPY PATH") })));
    expect(dense.dense).toBe(true);
    expect(dense.segments[0]?.label.lines.map((l) => l.text)).toEqual(["COPY", "PATH"]);
    const ys = dense.segments[0]?.label.lines.map((l) => l.y) ?? [];
    expect((ys[1] ?? 0) - (ys[0] ?? 0)).toBe(TEXT.denseLabelLineHeight);
  });
});
