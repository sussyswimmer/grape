import { describe, expect, it } from "vitest";
import type { PanelOpen } from "../shared/events";
import { fixtureJson, panelOpenFixture } from "../test-support/fixtures";
import {
  announceMove,
  cancelArgs,
  chooseChannels,
  choosePreset,
  edgeScroll,
  initialState,
  maxScroll,
  mergedName,
  moveItem,
  moveSelected,
  type PanelState,
  ROW_HEIGHT,
  ROWS_SHOWN,
  runArgs,
  scrollFor,
  select,
  selectBy,
  selectEdge,
  setLufs,
  targetIndex,
} from "./model";

const compress = panelOpenFixture("panel-open-compress.json");
const channels = panelOpenFixture("panel-open-channels.json");
const normalize = panelOpenFixture("panel-open-normalize.json");
const merge = panelOpenFixture("panel-open-merge.json");

// The command args (UI → Rust) sit next to the event fixtures; Rust's tests
// deserialize the same files.
const commandFixtures = import.meta.glob<string>(
  ["../shared/fixtures/panel-run-*.json", "../shared/fixtures/panel-cancel.json"],
  { query: "?raw", import: "default", eager: true },
);

function busy(s: PanelState, phase: "running" | "closing" = "running"): PanelState {
  return { ...s, phase };
}

/** A merge request with `n` files, ids 1..n. */
function mergeOf(n: number): PanelOpen {
  const items = Array.from({ length: n }, (_, index) => ({ id: index + 1, name: `f${index + 1}.pdf` }));
  return {
    seq: 9,
    title: "Merge PDFs",
    items,
    panel: { tool: "mergePDF", mergedNames: items.map((item) => `${item.name} Merged.pdf`) },
  };
}

describe("initialState", () => {
  it("starts from Rust's request: drop order, Rust's choice, the first row", () => {
    expect(initialState(compress)).toEqual({
      request: compress,
      order: [1, 2],
      choice: { tool: "compress", preset: "balanced" },
      selected: 1,
      phase: "idle",
      error: null,
    });
    expect(initialState(channels).choice).toEqual({ tool: "audioChannels", channels: "mono" });
    expect(initialState(channels).order).toEqual([1]);
    expect(initialState(normalize).choice).toEqual({ tool: "normalizeAudio", targetLufs: -16 });
    expect(initialState(merge).choice).toEqual({ tool: "mergePDF" });
    expect(initialState(merge).order).toEqual([1, 2, 3]);
    expect(initialState(merge).selected).toBe(1);
  });
});

describe("runArgs", () => {
  it("matches each panel-run fixture after the matching change", () => {
    const cases: [string, PanelState][] = [
      ["panel-run-compress.json", choosePreset(initialState(compress), "small")],
      ["panel-run-channels.json", chooseChannels(initialState(channels), "stereo")],
      ["panel-run-normalize.json", setLufs(initialState(normalize), -14)],
      ["panel-run-merge.json", moveSelected(select(initialState(merge), 2), -1)],
    ];
    for (const [name, state] of cases) {
      expect(runArgs(state), name).toEqual(fixtureJson(name));
    }
  });

  it("copies the order and the choice, so later changes can't reach a sent run", () => {
    const state = initialState(merge);
    const args = runArgs(state);
    args.order.push(9);
    expect(state.order).toEqual([1, 2, 3]);
    expect(args.choice).not.toBe(state.choice);
  });

  it("cancelArgs matches panel-cancel.json", () => {
    expect(cancelArgs(initialState(merge))).toEqual(fixtureJson("panel-cancel.json"));
  });
});

describe("the command fixtures", () => {
  it("are the four runs and the cancel", () => {
    expect(Object.keys(commandFixtures).sort()).toEqual(
      [
        "panel-cancel.json",
        "panel-run-channels.json",
        "panel-run-compress.json",
        "panel-run-merge.json",
        "panel-run-normalize.json",
      ].map((name) => `../shared/fixtures/${name}`),
    );
  });

  it.each(Object.entries(commandFixtures))("%s is one line of JSON with a trailing newline", (_name, text) => {
    expect(text.startsWith("{")).toBe(true);
    expect(text.endsWith("}\n")).toBe(true);
    expect(text.indexOf("\n")).toBe(text.length - 1);
    expect(text).not.toContain("\r");
    expect(text.charCodeAt(0)).not.toBe(0xfeff);
  });
});

describe("choices", () => {
  it("picks a preset or a channel layout only on its own tool", () => {
    const s = initialState(compress);
    expect(choosePreset(s, "high").choice).toEqual({ tool: "compress", preset: "high" });
    expect(choosePreset(s, "balanced")).toBe(s);
    const c = initialState(channels);
    expect(chooseChannels(c, "stereo").choice).toEqual({ tool: "audioChannels", channels: "stereo" });
    expect(choosePreset(c, "small")).toBe(c);
    expect(chooseChannels(s, "stereo")).toBe(s);
    expect(setLufs(s, -14)).toBe(s);
  });

  it("takes no change while running or closing", () => {
    const s = initialState(compress);
    expect(choosePreset(busy(s), "small").choice).toEqual(s.choice);
    expect(chooseChannels(busy(initialState(channels), "closing"), "stereo").choice).toEqual({
      tool: "audioChannels",
      channels: "mono",
    });
    expect(setLufs(busy(initialState(normalize)), -14).choice).toEqual({ tool: "normalizeAudio", targetLufs: -16 });
    const m = busy(initialState(merge));
    expect(moveSelected(m, 1)).toBe(m);
    expect(moveItem(m, 0, 2)).toBe(m);
    expect(select(m, 2)).toBe(m);
  });

  it("clears a rejected run's message on a change, not on a selection", () => {
    const failed: PanelState = { ...initialState(merge), error: "Grape couldn't use that choice." };
    expect(select(failed, 2).error).toBe("Grape couldn't use that choice.");
    expect(moveSelected(failed, 1).error).toBeNull();
    const compressFailed: PanelState = { ...initialState(compress), error: "x" };
    expect(choosePreset(compressFailed, "high").error).toBeNull();
  });
});

describe("setLufs", () => {
  const s = initialState(normalize);
  const target = (state: PanelState): number | null =>
    state.choice.tool === "normalizeAudio" ? state.choice.targetLufs : null;

  it("snaps to the step and clamps into min..max", () => {
    expect(target(setLufs(s, -14))).toBe(-14);
    expect(target(setLufs(s, -30))).toBe(-24);
    expect(target(setLufs(s, -9.6))).toBe(-10);
    expect(target(setLufs(s, 0))).toBe(-10);
    expect(target(setLufs(s, -14.4))).toBe(-14);
    expect(target(setLufs(s, -24))).toBe(-24);
    expect(target(setLufs(s, -10))).toBe(-10);
  });

  it("snaps to a coarser grid from the min", () => {
    const two = initialState({
      ...normalize,
      panel: { tool: "normalizeAudio", targetLufs: -16, minLufs: -24, maxLufs: -10, stepLufs: 2 },
    });
    expect(target(setLufs(two, -15.2))).toBe(-16);
    expect(target(setLufs(two, -14.8))).toBe(-14);
  });

  it("ignores a value that isn't a number, and returns the same state when nothing changes", () => {
    expect(setLufs(s, Number.NaN)).toBe(s);
    expect(setLufs(s, Number.POSITIVE_INFINITY)).toBe(s);
    expect(setLufs(s, -16.2)).toBe(s);
  });
});

describe("selection", () => {
  const s = initialState(merge);

  it("selects by id, by step without wrapping, and the ends", () => {
    expect(select(s, 3).selected).toBe(3);
    expect(select(s, 4)).toBe(s);
    expect(selectBy(s, 1).selected).toBe(2);
    expect(selectBy(s, -1)).toBe(s);
    expect(selectBy(select(s, 3), 1).selected).toBe(3);
    expect(selectEdge(s, "last").selected).toBe(3);
    expect(selectEdge(select(s, 3), "first").selected).toBe(1);
  });
});

describe("moving", () => {
  const s = initialState(merge);

  it("moves the selected row by one, and the selection follows", () => {
    const down = moveSelected(s, 1);
    expect(down.order).toEqual([2, 1, 3]);
    expect(down.selected).toBe(1);
    expect(moveSelected(down, 1).order).toEqual([2, 3, 1]);
  });

  it("is a no-op at the ends", () => {
    expect(moveSelected(s, -1)).toBe(s);
    const last = select(s, 3);
    expect(moveSelected(last, 1)).toBe(last);
  });

  it("moveItem moves by index, clamps both ends, and is a no-op when they're equal", () => {
    expect(moveItem(s, 2, 0).order).toEqual([3, 1, 2]);
    expect(moveItem(s, 0, 2).order).toEqual([2, 3, 1]);
    expect(moveItem(s, 0, 99).order).toEqual([2, 3, 1]);
    expect(moveItem(s, 99, -5).order).toEqual([3, 1, 2]);
    expect(moveItem(s, 1, 1)).toBe(s);
    expect(moveItem(s, 5, 2)).toBe(s);
    expect(moveItem(s, Number.NaN, 1)).toBe(s);
  });
});

describe("the reorder math", () => {
  it("targetIndex picks the row under the pointer, from the press's row middle", () => {
    expect(targetIndex(2, 0, 0, 3)).toBe(2);
    expect(targetIndex(2, -17, 0, 3)).toBe(2);
    expect(targetIndex(2, -19, 0, 3)).toBe(1);
    expect(targetIndex(2, -72, 0, 3)).toBe(0);
    expect(targetIndex(0, 36, 0, 3)).toBe(1);
    expect(targetIndex(0, 18, 0, 3)).toBe(1);
    expect(targetIndex(0, 17, 0, 3)).toBe(0);
  });

  it("targetIndex clamps into the list", () => {
    expect(targetIndex(1, -500, 0, 3)).toBe(0);
    expect(targetIndex(1, 500, 0, 3)).toBe(2);
    expect(targetIndex(0, 0, 0, 0)).toBe(0);
  });

  it("targetIndex counts how far the list scrolled since the press", () => {
    expect(targetIndex(0, 0, ROW_HEIGHT * 3, 10)).toBe(3);
    expect(targetIndex(5, 0, -ROW_HEIGHT * 2, 10)).toBe(3);
  });

  it("scrollFor keeps a row inside a six-row window", () => {
    expect(scrollFor(0, 0)).toBe(0);
    expect(scrollFor(5, 0)).toBe(0);
    expect(scrollFor(6, 0)).toBe(ROW_HEIGHT);
    expect(scrollFor(9, 0)).toBe(4 * ROW_HEIGHT);
    expect(scrollFor(2, 4 * ROW_HEIGHT)).toBe(2 * ROW_HEIGHT);
    expect(scrollFor(5, 2 * ROW_HEIGHT)).toBe(2 * ROW_HEIGHT);
    expect(ROWS_SHOWN).toBe(6);
  });

  it("edgeScroll scrolls a quarter row near an edge, and never past the list", () => {
    const count = 10;
    expect(maxScroll(count)).toBe(4 * ROW_HEIGHT);
    expect(maxScroll(3)).toBe(0);
    expect(edgeScroll(100, 36, count)).toBe(36);
    expect(edgeScroll(5, 36, count)).toBe(27);
    expect(edgeScroll(-40, 36, count)).toBe(27);
    expect(edgeScroll(5, 4, count)).toBe(0);
    expect(edgeScroll(6 * ROW_HEIGHT - 5, 36, count)).toBe(45);
    expect(edgeScroll(6 * ROW_HEIGHT - 5, maxScroll(count), count)).toBe(maxScroll(count));
    // A short list never scrolls.
    expect(edgeScroll(3 * ROW_HEIGHT - 2, 0, 3)).toBe(0);
  });
});

describe("merge text", () => {
  it("mergedName follows the first id, and is null for the other tools", () => {
    const s = initialState(merge);
    expect(mergedName(s)).toBe("report Merged.pdf");
    expect(mergedName(moveSelected(select(s, 2), -1))).toBe("cover Merged.pdf");
    expect(mergedName(moveItem(s, 2, 0))).toBe("appendix Merged.pdf");
    expect(mergedName(initialState(compress))).toBeNull();
  });

  it("announceMove reads the new place", () => {
    const moved = moveSelected(select(initialState(merge), 2), -1);
    expect(announceMove(moved, 2)).toBe("cover.pdf moved to 1 of 3.");
    expect(announceMove(moved, 1)).toBe("report.pdf moved to 2 of 3.");
  });

  it("handles a long list", () => {
    const s = initialState(mergeOf(50));
    expect(selectEdge(s, "last").selected).toBe(50);
    expect(mergedName(moveItem(s, 49, 0))).toBe("f50.pdf Merged.pdf");
  });
});
