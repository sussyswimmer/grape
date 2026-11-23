import { afterEach, describe, expect, it, vi } from "vitest";
import { isWheelOpenPayload, type WheelIcon, type WheelOpenPayload } from "../shared/events";
import { openFixture } from "../test-support/fixtures";
import { coreHitTest, entriesPayload, shortcutsPayload } from "../test-support/payloads";
import { emptyMessage, SHORTCUTS_EMPTY } from "./copy";
import { groupMarkPath, segmentPath } from "./geometry";
import { GLYPHS } from "./glyphs";
import {
  applyClose,
  applyHighlight,
  applyOpen,
  buildScene,
  CLOSED,
  emojiFontSize,
  glyphTransform,
  iconArt,
  labelLines,
  messageLines,
  PENDING_SEGMENTS,
  pendingPaths,
  shortcutLabelLines,
  shortcutLayout,
  stackLines,
  TEXT,
  textCells,
  textUnits,
  validHighlight,
  type WheelScene,
  type WheelState,
  wrapCells,
  wrapLines,
} from "./scene";

const pending = openFixture("wheel-open-pending.json"); // seq 1
const entries = openFixture("wheel-open-entries.json"); // seq 2
const empty = openFixture("wheel-open-empty.json"); // seq 3
const shortcuts = openFixture("wheel-open-shortcuts.json"); // seq 4, sounds on, inside "Edit"
const noShortcuts = openFixture("wheel-open-shortcuts-empty.json"); // seq 5

/** Half of a surrogate pair standing alone. */
const LONE_SURROGATE = /[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/;

const FAMILY = "👨\u200d👩\u200d👧\u200d👦";

afterEach(() => {
  vi.unstubAllGlobals();
});

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

  it("redraws in place when a group opens or closes (same seq), with no highlight", () => {
    const ring = shortcutsPayload(4, { seq: 7, groups: [1] });
    const inside = shortcutsPayload(2, { seq: 7, trail: ["Item 1"] });
    const into = applyOpen(opened(ring, 1), inside);
    expect(into).toEqual({ state: opened(inside), paint: "update", sound: "cancel" });
    expect(applyOpen(into.state, ring).paint).toBe("update");
    expect(applyOpen(CLOSED, shortcuts).paint).toBe("fresh");
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

  it("paints and ticks on a Shortcuts wheel", () => {
    const step = applyHighlight(opened(shortcuts), { seq: 4, index: 1 });
    expect(step).toEqual({ state: opened(shortcuts, 1), paint: "update", sound: "play" });
    const next = applyHighlight(step.state, { seq: 4, index: 2 });
    expect(next).toEqual({ state: opened(shortcuts, 2), paint: "update", sound: "play" });
    const off = applyHighlight(next.state, { seq: 4, index: null });
    expect(off).toEqual({ state: opened(shortcuts), paint: "update", sound: "cancel" });
    const quiet = shortcutsPayload(3, { seq: 4, sounds: false });
    expect(applyHighlight(opened(quiet), { seq: 4, index: 0 }).sound).toBe("none");
  });

  it("highlights nothing past a Shortcuts wheel's segments, or on an empty menu", () => {
    expect(applyHighlight(opened(shortcuts), { seq: 4, index: 3 }).paint).toBe("none");
    expect(applyHighlight(opened(noShortcuts), { seq: 5, index: 0 }).paint).toBe("none");
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

  it("accepts the segments of a Shortcuts wheel", () => {
    expect(validHighlight(shortcuts.content, 0)).toBe(0);
    expect(validHighlight(shortcuts.content, 2)).toBe(2);
    expect(validHighlight(shortcuts.content, 3)).toBeNull();
    expect(validHighlight(shortcuts.content, -1)).toBeNull();
    expect(validHighlight(shortcuts.content, 0.5)).toBeNull();
    expect(validHighlight(shortcuts.content, null)).toBeNull();
    expect(validHighlight(noShortcuts.content, 0)).toBeNull();
    expect(validHighlight({ kind: "empty", state: "noTools" }, 0)).toBeNull();
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

describe("textUnits and textCells", () => {
  it("keeps a character whole, with its joiners, accents and selectors", () => {
    expect(textUnits("a💾b")).toEqual(["a", "💾", "b"]);
    expect(textUnits(`${FAMILY}x`)).toEqual([FAMILY, "x"]);
    expect(textUnits("e\u0301")).toEqual(["e\u0301"]);
    expect(textUnits("1\ufe0f\u20e3")).toEqual(["1\ufe0f\u20e3"]);
    expect(textUnits("")).toEqual([]);
  });

  it("falls back to code points where the webview has no segmenter", () => {
    vi.stubGlobal("Intl", {});
    expect(textUnits("a💾b")).toEqual(["a", "💾", "b"]);
    expect(textUnits("😀😀").every((unit) => !LONE_SURROGATE.test(unit))).toBe(true);
    expect(textUnits(FAMILY)).toHaveLength(7);
  });

  it("counts a wide unit as two letters", () => {
    expect(textCells("Save")).toBe(4);
    expect(textCells("Select all")).toBe(10);
    expect(textCells("保存")).toBe(4);
    expect(textCells("スクリーン")).toBe(10);
    expect(textCells("한글")).toBe(4);
    expect(textCells("、。「」")).toBe(8);
    expect(textCells("ＡＢ")).toBe(4);
    expect(textCells("💾")).toBe(2);
    expect(textCells(FAMILY)).toBe(2);
    expect(textCells("🇯🇵")).toBe(2);
    expect(textCells("1\ufe0f\u20e3")).toBe(2);
    expect(textCells("e\u0301")).toBe(1);
    expect(textCells("")).toBe(0);
  });
});

describe("wrapCells", () => {
  it("packs words like wrapLines while they fit", () => {
    expect(wrapCells("Tidy up", 10, 2)).toEqual(["Tidy up"]);
    expect(wrapCells("Mission Control", 10, 2)).toEqual(["Mission", "Control"]);
    expect(wrapCells("  a   b  ", 10, 2)).toEqual(["a b"]);
    expect(wrapCells("", 10, 2)).toEqual([]);
    expect(wrapCells("   ", 10, 2)).toEqual([]);
    expect(wrapCells("a b", 5, 0)).toEqual([]);
  });

  it("breaks a word wider than the line, and lets the next word follow it", () => {
    expect(wrapCells("ABCDEFGHIJKL", 10, 2)).toEqual(["ABCDEFGHIJ", "KL"]);
    expect(wrapCells("ABCDEFGHIJKL mn", 10, 2)).toEqual(["ABCDEFGHIJ", "KL mn"]);
    expect(wrapCells("ab ABCDEFGHIJKL", 10, 3)).toEqual(["ab", "ABCDEFGHIJ", "KL"]);
  });

  it("fills the last line with what fits and ends it with an ellipsis", () => {
    expect(wrapCells("Lock screen", 10, 1)).toEqual(["Lock scre…"]);
    expect(wrapCells("one two three four", 7, 2)).toEqual(["one two", "three…"]);
    expect(wrapCells("aaaa bbbb cccc", 4, 2)).toEqual(["aaaa", "bbb…"]);
    expect(wrapCells("a b", 1, 1)).toEqual(["…"]);
  });

  it("puts a unit wider than the line on a line of its own", () => {
    expect(wrapCells("保存", 1, 2)).toEqual(["保", "存"]);
    expect(wrapCells("保存する", 1, 2)).toEqual(["保", "…"]);
  });
});

describe("shortcutLabelLines", () => {
  const UNBROKEN = "ABCDEFGHIJKLMNOPQRSTUVWX";
  const KANA = "スクリーンショットを撮る";

  it("leaves short names as they are", () => {
    expect(shortcutLabelLines("Save", false)).toEqual(["Save"]);
    expect(shortcutLabelLines("Select all", false)).toEqual(["Select all"]);
    expect(shortcutLabelLines("Select all", true)).toEqual(["Select", "all"]);
    expect(shortcutLabelLines("Mission Control", false)).toEqual(["Mission", "Control"]);
    expect(shortcutLabelLines("  Tidy   up ", false)).toEqual(["Tidy up"]);
    expect(shortcutLabelLines("", false)).toEqual([]);
  });

  it("breaks an unbroken name of 24 units and cuts it with an ellipsis", () => {
    expect(UNBROKEN).toHaveLength(24);
    expect(shortcutLabelLines(UNBROKEN, false)).toEqual(["ABCDEFGHIJ", "KLMNOPQRS…"]);
    expect(shortcutLabelLines(UNBROKEN, true)).toEqual(["ABCDEFGH", "IJKLMNO…"]);
    expect(shortcutLabelLines(UNBROKEN, false, 1)).toEqual(["ABCDEFGHI…"]);
  });

  it("wraps a CJK name by its width, with no spaces to break at", () => {
    expect(shortcutLabelLines("保存", false)).toEqual(["保存"]);
    expect(shortcutLabelLines(KANA, false)).toEqual(["スクリーン", "ショット…"]);
    expect(shortcutLabelLines(KANA, true)).toEqual(["スクリー", "ンショ…"]);
    expect(shortcutLabelLines(KANA, false, 1)).toEqual(["スクリー…"]);
    expect(shortcutLabelLines("写真を開く", false)).toEqual(["写真を開く"]);
  });

  it("wraps an emoji name without tearing an emoji", () => {
    expect(shortcutLabelLines("🎉🎉🎉🎉🎉🎉", false)).toEqual(["🎉🎉🎉🎉🎉", "🎉"]);
    const faces = "😀".repeat(12);
    expect(faces).toHaveLength(24);
    expect(shortcutLabelLines(faces, false)).toEqual(["😀😀😀😀😀", "😀😀😀😀…"]);
    expect(shortcutLabelLines(faces, true)).toEqual(["😀😀😀😀", "😀😀😀…"]);
    expect(shortcutLabelLines(`${FAMILY} Family`, false)).toEqual([`${FAMILY} Family`]);
    expect(shortcutLabelLines(FAMILY.repeat(6), false)).toEqual([FAMILY.repeat(5), FAMILY]);
    expect(shortcutLabelLines("Party 🎉", false)).toEqual(["Party 🎉"]);
  });

  const SAMPLES = [
    UNBROKEN,
    KANA,
    "😀".repeat(12),
    FAMILY.repeat(3),
    "Mission Control",
    "a 😀b保c 存😀 dddddddddddd",
    "e\u0301".repeat(12),
    "🇯🇵🇫🇷🇩🇪🇮🇹🇪🇸🇬🇧",
  ];

  it("never splits a surrogate pair, and never runs past the line", () => {
    for (const title of SAMPLES) {
      for (const dense of [false, true]) {
        for (const maxLines of [1, 2]) {
          const lines = shortcutLabelLines(title, dense, maxLines);
          expect(lines.length, title).toBeLessThanOrEqual(maxLines);
          for (const line of lines) {
            expect(LONE_SURROGATE.test(line), `${title} → ${line}`).toBe(false);
            expect(textCells(line), `${title} → ${line}`).toBeLessThanOrEqual(
              dense ? TEXT.denseLabelChars : TEXT.labelChars,
            );
          }
        }
      }
    }
  });

  it("keeps surrogate pairs whole without a segmenter, too", () => {
    vi.stubGlobal("Intl", {});
    for (const title of SAMPLES) {
      for (const line of shortcutLabelLines(title, true)) {
        expect(LONE_SURROGATE.test(line), `${title} → ${line}`).toBe(false);
      }
    }
    expect(shortcutLabelLines("😀".repeat(12), false)).toEqual(["😀😀😀😀😀", "😀😀😀😀…"]);
  });
});

describe("iconArt", () => {
  it("draws nothing for no icon, an unknown glyph or an empty emoji", () => {
    expect(iconArt({ kind: "none" })).toBeNull();
    expect(iconArt({ kind: "glyph", value: "rocket" })).toBeNull();
    expect(iconArt({ kind: "glyph", value: "constructor" })).toBeNull();
    expect(iconArt({ kind: "glyph", value: "" })).toBeNull();
    expect(iconArt({ kind: "emoji", value: "" })).toBeNull();
    expect(iconArt({ kind: "emoji", value: "  " })).toBeNull();
  });

  it("draws a known glyph from its paths", () => {
    expect(iconArt({ kind: "glyph", value: "stack" })).toEqual({ kind: "glyph", paths: GLYPHS.stack });
    expect(iconArt({ kind: "glyph", value: "key" })).toEqual({ kind: "glyph", paths: GLYPHS.key });
  });

  it("draws one emoji, or two letters, of what was typed", () => {
    expect(iconArt({ kind: "emoji", value: "💾" })).toEqual({ kind: "emoji", text: "💾" });
    expect(iconArt({ kind: "emoji", value: FAMILY })).toEqual({ kind: "emoji", text: FAMILY });
    expect(iconArt({ kind: "emoji", value: "🇯🇵" })).toEqual({ kind: "emoji", text: "🇯🇵" });
    expect(iconArt({ kind: "emoji", value: "1\ufe0f\u20e3" })).toEqual({
      kind: "emoji",
      text: "1\ufe0f\u20e3",
    });
    expect(iconArt({ kind: "emoji", value: "👍👍" })).toEqual({ kind: "emoji", text: "👍" });
    expect(iconArt({ kind: "emoji", value: "OK" })).toEqual({ kind: "emoji", text: "OK" });
    expect(iconArt({ kind: "emoji", value: "abcdefghijklmnop" })).toEqual({ kind: "emoji", text: "ab" });
    expect(iconArt({ kind: "emoji", value: "a💾" })).toEqual({ kind: "emoji", text: "a" });
  });
});

describe("shortcutLayout", () => {
  const g = shortcuts.geometry;
  const EMOJI: WheelIcon = { kind: "emoji", value: "💾" };
  const GLYPH: WheelIcon = { kind: "glyph", value: "gear" };

  function segmentsOf(payload: WheelOpenPayload) {
    if (payload.content.kind !== "shortcuts") throw new Error("not shortcuts");
    return payload.content.segments;
  }

  it("puts a label without an icon exactly at Rust's label point", () => {
    for (let n = 1; n <= 12; n += 1) {
      const payload = shortcutsPayload(n, { titles: Array(n).fill("Mission Control") });
      for (const seg of segmentsOf(payload)) {
        const layout = shortcutLayout(seg, n >= g.denseAt);
        expect(layout.icon).toBeNull();
        expect(layout.label.x).toBe(seg.labelX);
        expect(layout.label.lines.map((line) => line.text)).toEqual(["Mission", "Control"]);
        const ys = layout.label.lines.map((line) => line.y);
        expect(((ys[0] ?? 0) + (ys[1] ?? 0)) / 2).toBeCloseTo(seg.labelY, 9);
        expect((ys[1] ?? 0) - (ys[0] ?? 0)).toBeCloseTo(
          n >= g.denseAt ? TEXT.denseLabelLineHeight : TEXT.labelLineHeight,
          9,
        );
      }
    }
  });

  it("stacks an icon over one line of label, centered together on the label point", () => {
    const [seg] = segmentsOf(shortcutsPayload(4, { titles: ["Mission Control"], icons: [EMOJI] }));
    if (seg === undefined) throw new Error("no segment");
    const layout = shortcutLayout(seg, false);
    expect(layout.label.lines.map((line) => line.text)).toEqual(["Mission C…"]);
    const icon = layout.icon;
    const line = layout.label.lines[0];
    if (icon === null || line === undefined) throw new Error("no icon or label");
    expect(icon.size).toBe(TEXT.iconSize);
    expect(icon.x).toBe(seg.labelX);
    expect(layout.label.x).toBe(seg.labelX);
    const top = icon.y - icon.size / 2;
    const bottom = line.y + TEXT.labelLineHeight / 2;
    expect((top + bottom) / 2).toBeCloseTo(seg.labelY, 9);
    expect(line.y - TEXT.labelLineHeight / 2 - (icon.y + icon.size / 2)).toBeCloseTo(TEXT.iconGap, 9);
  });

  it("shows the icon alone in the dense style, on the label point", () => {
    const [seg] = segmentsOf(shortcutsPayload(8, { icons: [GLYPH] }));
    if (seg === undefined) throw new Error("no segment");
    const layout = shortcutLayout(seg, true);
    expect(layout.label.lines).toEqual([]);
    expect(layout.icon).toEqual({
      x: seg.labelX,
      y: seg.labelY,
      size: TEXT.denseIconSize,
      art: { kind: "glyph", paths: GLYPHS.gear },
    });
  });

  it("centers an icon whose shortcut has no name to show", () => {
    const [seg] = segmentsOf(shortcutsPayload(4, { titles: [""], icons: [EMOJI] }));
    if (seg === undefined) throw new Error("no segment");
    const layout = shortcutLayout(seg, false);
    expect(layout.label.lines).toEqual([]);
    expect(layout.icon?.y).toBe(seg.labelY);
  });

  it("falls back to the label when the icon can't be drawn, also in the dense style", () => {
    const unknown: WheelIcon = { kind: "glyph", value: "rocket" };
    const [seg] = segmentsOf(shortcutsPayload(8, { titles: ["Select all"], icons: [unknown] }));
    if (seg === undefined) throw new Error("no segment");
    const layout = shortcutLayout(seg, true);
    expect(layout.icon).toBeNull();
    expect(layout.label.lines.map((line) => line.text)).toEqual(["Select", "all"]);
  });

  it("keeps the icon's square and the label's line inside the segment for 1 to 12 segments", () => {
    for (let n = 1; n <= 12; n += 1) {
      const dense = n >= g.denseAt;
      const payload = shortcutsPayload(n, {
        titles: Array(n).fill("Mission Control"),
        icons: Array.from({ length: n }, (_, index) => (index % 2 === 0 ? EMOJI : GLYPH)),
      });
      for (const seg of segmentsOf(payload)) {
        const layout = shortcutLayout(seg, dense);
        const icon = layout.icon;
        if (icon === null) throw new Error("no icon");
        const half = icon.size / 2;
        const where = `n=${n} segment ${seg.index}`;
        for (const [dx, dy] of [[-half, -half], [half, -half], [-half, half], [half, half], [0, 0]] as const) {
          expect(coreHitTest(icon.x + dx, icon.y + dy, n, g), `${where} icon`).toBe(seg.index);
        }
        expect(layout.label.lines.length, where).toBe(dense ? 0 : 1);
        const lineHeight = dense ? TEXT.denseLabelLineHeight : TEXT.labelLineHeight;
        for (const line of layout.label.lines) {
          expect(line.y - lineHeight / 2, where).toBeGreaterThanOrEqual(icon.y + half);
          for (const dy of [-lineHeight / 2, 0, lineHeight / 2]) {
            expect(coreHitTest(layout.label.x, line.y + dy, n, g), `${where} label`).toBe(seg.index);
          }
        }
      }
    }
  });

  it("keeps a two-line label without an icon inside the segment, too", () => {
    for (let n = 1; n <= 12; n += 1) {
      const dense = n >= g.denseAt;
      const lineHeight = dense ? TEXT.denseLabelLineHeight : TEXT.labelLineHeight;
      const payload = shortcutsPayload(n, { titles: Array(n).fill("Mission Control") });
      for (const seg of segmentsOf(payload)) {
        for (const line of shortcutLayout(seg, dense).label.lines) {
          for (const dy of [-lineHeight / 2, lineHeight / 2]) {
            expect(coreHitTest(seg.labelX, line.y + dy, n, g), `n=${n} segment ${seg.index}`).toBe(
              seg.index,
            );
          }
        }
      }
    }
  });

  it("scales a glyph's grid onto its square, and an emoji's font to it", () => {
    const icon = { x: 72, y: 34.5, size: 20, art: { kind: "glyph", paths: GLYPHS.stack } } as const;
    const { x, y, scale } = glyphTransform(icon);
    expect(x).toBe(62);
    expect(y).toBe(24.5);
    expect(scale).toBeCloseTo(20 / 24, 12);
    expect(emojiFontSize({ x: 0, y: 0, size: 20, art: { kind: "emoji", text: "💾" } })).toBe(17);
    expect(emojiFontSize({ x: 0, y: 0, size: 18, art: { kind: "emoji", text: "💾" } })).toBeLessThan(18);
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

  it("draws the golden Shortcuts wheel: icons, a group's mark and the open group's name", () => {
    const s = scene(opened(shortcuts));
    const g = shortcuts.geometry;
    if (shortcuts.content.kind !== "shortcuts") throw new Error("not shortcuts");
    const source = shortcuts.content.segments;
    expect(s.kind).toBe("shortcuts");
    expect(s.label).toBe("Shortcuts wheel, Edit, 3 choices");
    expect(s.dense).toBe(false);
    expect(s.placeholders).toEqual([]);
    expect(s.message).toBeNull();
    expect(s.segments.map((seg) => seg.title)).toEqual(["Save", "Tidy up", "Select all"]);
    expect(s.segments.map((seg) => seg.name)).toEqual(["Save", "Tidy up, group", "Select all"]);
    expect(s.segments.map((seg) => seg.d)).toEqual(source.map((seg) => segmentPath(seg, 3, g)));
    expect(s.segments.every((seg) => seg.d.startsWith("M ") && !seg.d.includes("NaN"))).toBe(true);
    expect(s.segments.map((seg) => seg.highlighted)).toEqual([false, false, false]);
    expect(s.segments.map((seg) => seg.icon?.art)).toEqual([
      { kind: "emoji", text: "💾" },
      { kind: "glyph", paths: GLYPHS.stack },
      undefined,
    ]);
    expect(s.segments.map((seg) => seg.label.lines.map((l) => l.text))).toEqual([
      ["Save"],
      ["Tidy up"],
      ["Select all"],
    ]);
    const [, group] = source;
    if (group === undefined) throw new Error("no group");
    expect(s.segments.map((seg) => seg.mark)).toEqual([null, groupMarkPath(group, 3, g), null]);
    // The plain segment's label sits on Rust's label point.
    expect(s.segments[2]?.label).toEqual({ x: -72, lines: [{ text: "Select all", y: 42 }] });
    // Nothing hovered: the center names the open group.
    expect(s.center?.lines.map((l) => l.text)).toEqual(["Edit"]);
    expect(s.center?.x).toBe(0);
  });

  it("shows the hovered shortcut's name in the center, over the group's", () => {
    const s = scene(opened(shortcuts, 2));
    expect(s.segments.map((seg) => seg.highlighted)).toEqual([false, false, true]);
    expect(s.center?.lines.map((l) => l.text)).toEqual(["Select all"]);
    expect(scene(opened(shortcuts, 1)).center?.lines.map((l) => l.text)).toEqual(["Tidy up"]);
  });

  it("names the innermost open group, and nothing at the root", () => {
    const deep = shortcutsPayload(3, { trail: ["Work", "Edit more"] });
    expect(scene(opened(deep)).center?.lines.map((l) => l.text)).toEqual(["Edit more"]);
    expect(scene(opened(deep)).label).toBe("Shortcuts wheel, Edit more, 3 choices");
    const root = shortcutsPayload(3);
    expect(scene(opened(root)).center).toBeNull();
    expect(scene(opened(root)).label).toBe("Shortcuts wheel, 3 choices");
    expect(scene(opened(shortcutsPayload(3, { trail: [""] }))).center).toBeNull();
  });

  it("wraps a long name in the center, whatever it is made of", () => {
    const titles = ["ABCDEFGHIJKLMNOPQRSTUVWX", "スクリーンショットを撮る", "😀".repeat(12)];
    const payload = shortcutsPayload(3, { titles });
    expect(scene(opened(payload, 0)).center?.lines.map((l) => l.text)).toEqual([
      "ABCDEFGHIJKL",
      "MNOPQRSTUVWX",
    ]);
    expect(scene(opened(payload, 1)).center?.lines.map((l) => l.text)).toEqual([
      "スクリーンシ",
      "ョットを撮る",
    ]);
    expect(scene(opened(payload, 2)).center?.lines.map((l) => l.text)).toEqual([
      "😀😀😀😀😀😀",
      "😀😀😀😀😀😀",
    ]);
  });

  it("still lays out names far longer than Rust allows", () => {
    const titles = ["n".repeat(500), "語".repeat(500), "😀".repeat(250)];
    const icons: WheelIcon[] = [{ kind: "none" }, { kind: "emoji", value: "x".repeat(500) }];
    const payload = shortcutsPayload(3, { titles, icons, trail: ["t".repeat(500)] });
    for (const highlight of [null, 0, 1, 2]) {
      const s = scene(opened(payload, highlight));
      for (const seg of s.segments) {
        expect(seg.label.lines.length).toBeLessThanOrEqual(TEXT.labelLines);
        for (const line of seg.label.lines) {
          expect(textCells(line.text)).toBeLessThanOrEqual(TEXT.labelChars);
        }
        expect(seg.title).toHaveLength(500);
      }
      expect(s.segments[1]?.icon?.art).toEqual({ kind: "emoji", text: "xx" });
      expect(s.center?.lines).toHaveLength(TEXT.centerLines);
      for (const line of s.center?.lines ?? []) {
        expect(textCells(line.text)).toBeLessThanOrEqual(TEXT.centerChars);
      }
    }
  });

  it("shows the empty message for a menu or a group with no shortcuts", () => {
    const s = scene(opened(noShortcuts));
    expect(s.kind).toBe("shortcuts");
    expect(s.segments).toEqual([]);
    expect(s.placeholders).toEqual([]);
    expect(s.center).toBeNull();
    const lines = s.message?.lines.map((l) => l.text) ?? [];
    expect(lines.length).toBeGreaterThan(0);
    expect(lines.length).toBeLessThanOrEqual(TEXT.messageLines);
    expect(lines.join(" ")).toBe("No shortcuts here yet. Add some in Settings.");
    expect(lines.join(" ")).toBe(SHORTCUTS_EMPTY);
    expect(s.label).toBe("Shortcuts wheel, no choices");
    // An empty group: the message alone, with no name under it.
    const group = scene(opened(shortcutsPayload(0, { trail: ["Edit"] })));
    expect(group.center).toBeNull();
    expect(group.message?.lines.map((l) => l.text).join(" ")).toBe(SHORTCUTS_EMPTY);
    expect(group.label).toBe("Shortcuts wheel, Edit, no choices");
  });

  it("shows icons alone in the dense style, and the name in the center on hover", () => {
    const icons: WheelIcon[] = Array.from({ length: 8 }, (_, index) =>
      index % 2 === 0 ? { kind: "emoji", value: "💾" } : { kind: "glyph", value: "clock" },
    );
    const payload = shortcutsPayload(8, { icons, titles: Array(8).fill("Mission Control") });
    if (payload.content.kind !== "shortcuts") throw new Error("not shortcuts");
    const source = payload.content.segments;
    const s = scene(opened(payload));
    expect(s.dense).toBe(true);
    s.segments.forEach((seg, index) => {
      expect(seg.label.lines).toEqual([]);
      expect(seg.icon?.x).toBe(source[index]?.labelX);
      expect(seg.icon?.y).toBe(source[index]?.labelY);
      expect(seg.icon?.size).toBe(TEXT.denseIconSize);
      expect(seg.title).toBe("Mission Control");
      expect(seg.name).toBe("Mission Control");
    });
    expect(s.center).toBeNull();
    expect(scene(opened(payload, 5)).center?.lines.map((l) => l.text)).toEqual(["Mission", "Control"]);
  });

  it("uses the dense label style for a shortcut without an icon", () => {
    const dense = scene(opened(shortcutsPayload(7, { titles: Array(7).fill("Select all") })));
    expect(dense.dense).toBe(true);
    expect(dense.segments[0]?.icon).toBeNull();
    expect(dense.segments[0]?.label.lines.map((l) => l.text)).toEqual(["Select", "all"]);
    const ys = dense.segments[0]?.label.lines.map((l) => l.y) ?? [];
    expect((ys[1] ?? 0) - (ys[0] ?? 0)).toBe(TEXT.denseLabelLineHeight);
    expect(scene(opened(shortcutsPayload(6))).dense).toBe(false);
  });

  it("marks every group, and only groups", () => {
    for (let n = 1; n <= 12; n += 1) {
      const payload = shortcutsPayload(n, { groups: [0, n - 1] });
      const s = scene(opened(payload));
      s.segments.forEach((seg, index) => {
        const group = index === 0 || index === n - 1;
        expect(seg.mark === null, `n=${n} segment ${index}`).toBe(!group);
        expect(seg.name).toBe(group ? `Item ${index}, group` : `Item ${index}`);
        if (seg.mark !== null) {
          expect(seg.mark.startsWith("M ")).toBe(true);
          expect(seg.mark).not.toContain("NaN");
        }
      });
    }
  });

  it("gives the other wheels' segments no icon, no mark and their title as the name", () => {
    const s = scene(opened(entriesPayload(5)));
    for (const seg of s.segments) {
      expect(seg.icon).toBeNull();
      expect(seg.mark).toBeNull();
      expect(seg.name).toBe(seg.title);
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
