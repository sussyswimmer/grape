import { describe, expect, it } from "vitest";
import {
  FLOAT_CLEAR,
  FLOAT_DRAG_ENDED,
  FLOAT_DRAG_OUT,
  FLOAT_GET_VIEW,
  FLOAT_REMOVE,
  FLOAT_REVEAL,
  FLOAT_VIEW,
  FLOAT_WINDOW,
  isFloatDragEnded,
  isFloatView,
  isPanelClose,
  isPanelOpen,
  isToastHidePayload,
  isToastShowPayload,
  isWheelClosePayload,
  isWheelHighlightPayload,
  isWheelOpenPayload,
  PANEL_CANCEL,
  PANEL_CLOSE,
  PANEL_CURRENT,
  PANEL_OPEN,
  PANEL_RUN,
  PANEL_WINDOW,
  TOAST_CLICKED,
  TOAST_HIDE,
  TOAST_SHOW,
  WHEEL_CLOSE,
  WHEEL_HIGHLIGHT,
  WHEEL_OPEN,
  WHEEL_PAINTED,
} from "./events";
import { clone, get, type Json, mutate, objectPaths, paths, wrongType } from "../test-support/json";

// The golden payloads from docs/design/m1-drag-wheel.md §C, plus the float
// shelf's (M5) and the tool panel's (M4). The Rust side `include_str!`s the
// same files, so both suites test one contract. The Settings views share
// the folder (src/settings/model.test.ts covers them), and so do the
// panel's command args, UI → Rust, which aren't events
// (src/panel/model.test.ts covers them).
const raw = import.meta.glob<string>(
  [
    "./fixtures/*.json",
    "!./fixtures/settings-view-*.json",
    "!./fixtures/panel-run-*.json",
    "!./fixtures/panel-cancel.json",
  ],
  {
    query: "?raw",
    import: "default",
    eager: true,
  },
);

function fixture(name: string): string {
  const text = raw[`./fixtures/${name}`];
  if (text === undefined) {
    throw new Error(`missing fixture ${name}`);
  }
  return text;
}

function parsed(name: string): unknown {
  return JSON.parse(fixture(name));
}

type Guard = (value: unknown) => boolean;

const GUARDS: Readonly<Record<string, Guard>> = {
  isWheelOpenPayload,
  isWheelHighlightPayload,
  isWheelClosePayload,
  isToastShowPayload,
  isToastHidePayload,
  isFloatView,
  isFloatDragEnded,
  isPanelOpen,
  isPanelClose,
};

/** Each fixture and the one guard that must accept it. */
const CASES: readonly (readonly [string, string])[] = [
  ["wheel-open-pending.json", "isWheelOpenPayload"],
  ["wheel-open-entries.json", "isWheelOpenPayload"],
  ["wheel-open-empty.json", "isWheelOpenPayload"],
  ["wheel-highlight.json", "isWheelHighlightPayload"],
  ["wheel-highlight-null.json", "isWheelHighlightPayload"],
  ["wheel-close.json", "isWheelClosePayload"],
  ["toast-show-working.json", "isToastShowPayload"],
  ["toast-show-done.json", "isToastShowPayload"],
  ["toast-hide.json", "isToastHidePayload"],
  ["float-view.json", "isFloatView"],
  ["float-view-empty.json", "isFloatView"],
  ["float-drag-ended.json", "isFloatDragEnded"],
  ["panel-open-compress.json", "isPanelOpen"],
  ["panel-open-channels.json", "isPanelOpen"],
  ["panel-open-normalize.json", "isPanelOpen"],
  ["panel-open-merge.json", "isPanelOpen"],
  ["panel-close.json", "isPanelClose"],
];

function guard(name: string): Guard {
  const found = GUARDS[name];
  if (found === undefined) {
    throw new Error(`no guard ${name}`);
  }
  return found;
}

describe("event names", () => {
  it("match the contract", () => {
    expect(WHEEL_OPEN).toBe("wheel://open");
    expect(WHEEL_HIGHLIGHT).toBe("wheel://highlight");
    expect(WHEEL_CLOSE).toBe("wheel://close");
    expect(TOAST_SHOW).toBe("toast://show");
    expect(TOAST_HIDE).toBe("toast://hide");
    expect(WHEEL_PAINTED).toBe("wheel_painted");
    expect(TOAST_CLICKED).toBe("toast_clicked");
    expect(FLOAT_VIEW).toBe("float://view");
    expect(FLOAT_DRAG_ENDED).toBe("float://drag-ended");
    expect(FLOAT_GET_VIEW).toBe("float_view");
    expect(FLOAT_REMOVE).toBe("float_remove");
    expect(FLOAT_CLEAR).toBe("float_clear");
    expect(FLOAT_REVEAL).toBe("float_reveal");
    expect(FLOAT_DRAG_OUT).toBe("float_drag_out");
    expect(FLOAT_WINDOW).toBe("float");
    expect(PANEL_OPEN).toBe("panel://open");
    expect(PANEL_CLOSE).toBe("panel://close");
    expect(PANEL_CURRENT).toBe("panel_current");
    expect(PANEL_RUN).toBe("panel_run");
    expect(PANEL_CANCEL).toBe("panel_cancel");
    expect(PANEL_WINDOW).toBe("panel");
  });
});

describe("golden fixtures", () => {
  it("are exactly the nine payloads from §C, the float shelf's three and the panel's five", () => {
    expect(Object.keys(raw).sort()).toEqual(CASES.map(([name]) => `./fixtures/${name}`).sort());
  });

  it.each(CASES)("%s is one line of JSON with a trailing newline", (name) => {
    const text = fixture(name);
    expect(text.endsWith("}\n")).toBe(true);
    expect(text.indexOf("\n")).toBe(text.length - 1);
    expect(text).not.toContain("\r");
    expect(text.startsWith("{")).toBe(true);
  });

  it.each(CASES)("%s passes %s and no other guard", (name, accepting) => {
    const value = parsed(name);
    for (const [guardName, check] of Object.entries(GUARDS)) {
      expect(check(value), `${guardName}(${name})`).toBe(guardName === accepting);
    }
  });

  it("keeps the toast title's ellipsis as one character", () => {
    const value = parsed("toast-show-working.json");
    expect(isToastShowPayload(value)).toBe(true);
    if (isToastShowPayload(value)) {
      expect(value.title).toBe("Making photo.png…");
    }
  });
});

describe("malformed variants fail", () => {
  describe.each(CASES)("%s", (name, accepting) => {
    const check = guard(accepting);
    const value = clone(parsed(name));

    it.each(paths(value).map((path) => [path.join(".")] as const))(
      "without %s",
      (dotted) => {
        const path = paths(value).find((candidate) => candidate.join(".") === dotted);
        if (path === undefined) throw new Error(dotted);
        const last = path[path.length - 1];
        // Removing an array element is a different variant (handled below).
        if (typeof last === "number") return;
        expect(check(mutate(value, path, undefined))).toBe(false);
      },
    );

    it.each(paths(value).map((path) => [path.join(".")] as const))(
      "with a wrong type at %s",
      (dotted) => {
        const path = paths(value).find((candidate) => candidate.join(".") === dotted);
        if (path === undefined) throw new Error(dotted);
        expect(check(mutate(value, path, wrongType(get(value, path))))).toBe(false);
      },
    );

    it("with an extra key on any object", () => {
      for (const path of objectPaths(value)) {
        const copy = clone(value);
        const target = get(copy, path) as { [key: string]: Json };
        target["extra"] = 1;
        expect(check(copy), `extra key at "${path.join(".")}"`).toBe(false);
      }
    });

    it("with NaN or ±Infinity at any number", () => {
      for (const path of paths(value)) {
        if (typeof get(value, path) !== "number") continue;
        for (const bad of [Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY]) {
          expect(check(mutate(value, path, bad)), `${String(bad)} at ${path.join(".")}`).toBe(
            false,
          );
        }
      }
    });

    it("when it is not an object", () => {
      for (const bad of [null, undefined, 4, "x", true, [], [value]]) {
        expect(check(bad)).toBe(false);
      }
    });
  });
});

describe("semantic checks", () => {
  const entries = clone(parsed("wheel-open-entries.json"));
  const pending = clone(parsed("wheel-open-pending.json"));

  it("rejects ids and seqs that are not non-negative integers", () => {
    for (const bad of [-1, 1.5, 2 ** 53]) {
      expect(isWheelOpenPayload(mutate(entries, ["seq"], bad))).toBe(false);
      expect(isWheelHighlightPayload({ seq: bad, index: 0 })).toBe(false);
      expect(isWheelHighlightPayload({ seq: 1, index: bad })).toBe(false);
      expect(isWheelClosePayload({ seq: bad, reason: "drop" })).toBe(false);
      expect(isToastHidePayload({ id: bad })).toBe(false);
      expect(isFloatView({ items: [{ id: bad, name: "a", kind: "file", missing: false }] })).toBe(
        false,
      );
    }
    expect(isWheelHighlightPayload({ seq: 0, index: 11 })).toBe(true);
  });

  it("rejects unknown enum values", () => {
    expect(isWheelOpenPayload(mutate(entries, ["mode"], "convert"))).toBe(false);
    expect(isWheelOpenPayload(mutate(entries, ["mode"], "Conversions"))).toBe(false);
    expect(
      isWheelOpenPayload(mutate(entries, ["content", "segments", 0, "entry", "kind"], "file")),
    ).toBe(false);
    expect(isWheelOpenPayload(mutate(pending, ["content", "kind"], "loading"))).toBe(false);
    expect(
      isWheelOpenPayload(
        mutate(pending, ["content"], { kind: "empty", state: "noActions" }),
      ),
    ).toBe(false);
    expect(isWheelClosePayload({ seq: 1, reason: "dropped" })).toBe(false);
    expect(isWheelClosePayload({ seq: 1, reason: "NoFiles" })).toBe(false);
    for (const reason of ["drop", "cancel", "escape", "noFiles", "dragEnded", "watchdog", "paused"]) {
      expect(isWheelClosePayload({ seq: 1, reason })).toBe(true);
    }
    const done = clone(parsed("toast-show-done.json"));
    expect(isToastShowPayload(mutate(done, ["tone"], "success"))).toBe(false);
    for (const tone of ["working", "done", "error", "info"]) {
      expect(isToastShowPayload(mutate(done, ["tone"], tone))).toBe(true);
    }
    const shelf = clone(parsed("float-view.json"));
    expect(isFloatView(mutate(shelf, ["items", 0, "kind"], "File"))).toBe(false);
    expect(isFloatView(mutate(shelf, ["items", 0, "kind"], "link"))).toBe(false);
    expect(isFloatDragEnded({ outcome: "moved" })).toBe(false);
    expect(isFloatDragEnded({ outcome: "Copied" })).toBe(false);
    for (const outcome of ["copied", "linked", "cancelled", "nothing", "failed"]) {
      expect(isFloatDragEnded({ outcome })).toBe(true);
    }
  });

  it("rejects a shelf item id of 0 (Rust hands them out from 1)", () => {
    const shelf = clone(parsed("float-view.json"));
    expect(isFloatView(mutate(shelf, ["items", 0, "id"], 0))).toBe(false);
    expect(isFloatView(mutate(shelf, ["items", 0, "id"], 2 ** 53 - 1))).toBe(true);
  });

  it("rejects a shelf that lists one id twice (rows are keyed by id)", () => {
    const shelf = clone(parsed("float-view.json"));
    expect(isFloatView(mutate(shelf, ["items", 1, "id"], 1))).toBe(false);
  });

  it("accepts a shelf item with any name, and items in any id order", () => {
    const shelf = clone(parsed("float-view.json"));
    expect(isFloatView(mutate(shelf, ["items", 0, "name"], ""))).toBe(true);
    const items = get(shelf, ["items"]);
    expect(isFloatView({ items: [...(items as Json[])].reverse() })).toBe(true);
  });

  it("rejects grape-core's own WheelContent shape (kind + value)", () => {
    expect(
      isWheelOpenPayload(
        mutate(pending, ["content"], {
          kind: "entries",
          value: [{ kind: "convert", key: "png" }],
        }),
      ),
    ).toBe(false);
    expect(
      isWheelOpenPayload(mutate(pending, ["content"], { kind: "empty", value: "noTools" })),
    ).toBe(false);
  });

  it("rejects content fields that belong to another kind", () => {
    expect(
      isWheelOpenPayload(mutate(pending, ["content"], { kind: "pending", dense: false })),
    ).toBe(false);
    expect(
      isWheelOpenPayload(
        mutate(pending, ["content"], { kind: "empty", state: "noTools", segments: [] }),
      ),
    ).toBe(false);
  });

  it("rejects entries with no segments or with indexes out of order", () => {
    expect(isWheelOpenPayload(mutate(entries, ["content", "segments"], []))).toBe(false);
    expect(isWheelOpenPayload(mutate(entries, ["content", "segments", 0, "index"], 1))).toBe(
      false,
    );
    const segment = get(entries, ["content", "segments", 0]);
    const two = mutate(entries, ["content", "segments"], [segment, { ...(segment as object), index: 1 }]);
    expect(isWheelOpenPayload(two)).toBe(true);
    const swapped = mutate(entries, ["content", "segments"], [{ ...(segment as object), index: 1 }, segment]);
    expect(isWheelOpenPayload(swapped)).toBe(false);
  });

  it("rejects an empty entry key", () => {
    expect(
      isWheelOpenPayload(mutate(entries, ["content", "segments", 0, "entry", "key"], "")),
    ).toBe(false);
  });

  it("rejects geometry that would break the segment math", () => {
    const bad: [string, number][] = [
      ["innerR", 130], // inside-out ring
      ["outerR", 48], // zero-width ring
      ["backdropR", 100], // backdrop smaller than the ring
      ["gap", -1],
      ["gap", 100], // gap wider than the dead zone
      ["corner", -1],
      ["labelR", -1],
      ["screenMargin", -1],
      ["denseAt", 6.5],
    ];
    for (const [key, number] of bad) {
      expect(isWheelOpenPayload(mutate(entries, ["geometry", key], number)), key).toBe(false);
    }
  });

  it("rejects a non-positive scale", () => {
    expect(isWheelOpenPayload(mutate(entries, ["scale"], 0))).toBe(false);
    expect(isWheelOpenPayload(mutate(entries, ["scale"], -1.5))).toBe(false);
    expect(isWheelOpenPayload(mutate(entries, ["scale"], 2.25))).toBe(true);
  });

  it("accepts any key names, including empty or custom ones", () => {
    expect(isWheelOpenPayload(mutate(entries, ["keys", "actions"], "Caps"))).toBe(true);
    expect(isWheelOpenPayload(mutate(entries, ["keys", "tools"], "Right Ctrl"))).toBe(true);
  });

  it("accepts any finite toast progress (the view clamps it to 0..=1)", () => {
    const working = clone(parsed("toast-show-working.json"));
    for (const ok of [0, 0.25, 1, 1.0000001, -0.01]) {
      expect(isToastShowPayload(mutate(working, ["progress"], ok))).toBe(true);
    }
  });

  it("accepts a toast detail as text or null, never missing", () => {
    const working = clone(parsed("toast-show-working.json"));
    expect(isToastShowPayload(mutate(working, ["detail"], "photo.png"))).toBe(true);
    expect(isToastShowPayload(mutate(working, ["detail"], undefined))).toBe(false);
  });
});

describe("panel semantic checks", () => {
  const compress = clone(parsed("panel-open-compress.json"));
  const channels = clone(parsed("panel-open-channels.json"));
  const normalize = clone(parsed("panel-open-normalize.json"));
  const merge = clone(parsed("panel-open-merge.json"));

  it("rejects seq 0 and item id 0 (Rust counts both from 1)", () => {
    expect(isPanelOpen(mutate(compress, ["seq"], 0))).toBe(false);
    expect(isPanelOpen(mutate(compress, ["items", 0, "id"], 0))).toBe(false);
    expect(isPanelClose({ seq: 0 })).toBe(false);
    for (const bad of [-1, 1.5, 2 ** 53]) {
      expect(isPanelOpen(mutate(compress, ["seq"], bad))).toBe(false);
      expect(isPanelOpen(mutate(compress, ["items", 1, "id"], bad))).toBe(false);
      expect(isPanelClose({ seq: bad })).toBe(false);
    }
    expect(isPanelOpen(mutate(compress, ["seq"], 2 ** 53 - 1))).toBe(true);
    expect(isPanelClose({ seq: 2 ** 53 - 1 })).toBe(true);
  });

  it("rejects an id listed twice (the page answers by id)", () => {
    expect(isPanelOpen(mutate(compress, ["items", 1, "id"], 1))).toBe(false);
    expect(isPanelOpen(mutate(merge, ["items", 2, "id"], 2))).toBe(false);
  });

  it("accepts ids in any order (they're Rust's, the page only echoes them)", () => {
    expect(isPanelOpen(mutate(compress, ["items"], [{ id: 2, name: "b" }, { id: 7, name: "a" }]))).toBe(
      true,
    );
  });

  it("rejects a request with no items, or an empty title", () => {
    expect(isPanelOpen(mutate(compress, ["items"], []))).toBe(false);
    expect(isPanelOpen(mutate(compress, ["title"], ""))).toBe(false);
  });

  it("rejects an unknown tool, preset or channel layout", () => {
    expect(isPanelOpen(mutate(compress, ["panel", "tool"], "removeMetadata"))).toBe(false);
    expect(isPanelOpen(mutate(compress, ["panel"], { tool: "removeMetadata" }))).toBe(false);
    expect(isPanelOpen(mutate(compress, ["panel", "tool"], "Compress"))).toBe(false);
    expect(isPanelOpen(mutate(compress, ["panel", "preset"], "medium"))).toBe(false);
    expect(isPanelOpen(mutate(channels, ["panel", "channels"], "surround"))).toBe(false);
    for (const preset of ["small", "balanced", "high"]) {
      expect(isPanelOpen(mutate(compress, ["panel", "preset"], preset))).toBe(true);
    }
    for (const layout of ["mono", "stereo"]) {
      expect(isPanelOpen(mutate(channels, ["panel", "channels"], layout))).toBe(true);
    }
  });

  it("rejects a spec with another tool's fields", () => {
    expect(isPanelOpen(mutate(compress, ["panel"], { tool: "compress", channels: "mono" }))).toBe(false);
    expect(isPanelOpen(mutate(channels, ["panel"], { tool: "audioChannels", preset: "small" }))).toBe(
      false,
    );
    expect(isPanelOpen(mutate(merge, ["panel"], { tool: "mergePDF" }))).toBe(false);
  });

  it("rejects a merge of one file, and merged names that don't match the items", () => {
    const one = mutate(merge, ["items"], [{ id: 1, name: "report.pdf" }]);
    expect(isPanelOpen(mutate(one, ["panel", "mergedNames"], ["report Merged.pdf"]))).toBe(false);
    expect(isPanelOpen(mutate(merge, ["panel", "mergedNames"], ["report Merged.pdf", "cover Merged.pdf"]))).toBe(
      false,
    );
    expect(
      isPanelOpen(
        mutate(merge, ["panel", "mergedNames"], ["a.pdf", "b.pdf", "c.pdf", "d.pdf"]),
      ),
    ).toBe(false);
  });

  it("accepts any merged name", () => {
    expect(isPanelOpen(mutate(merge, ["panel", "mergedNames", 0], ""))).toBe(true);
    expect(isPanelOpen(mutate(merge, ["panel", "mergedNames", 1], "report Merged 2.pdf"))).toBe(true);
  });

  it("rejects a loudness that the slider couldn't show", () => {
    const bad: [string, number][] = [
      ["targetLufs", -25], // below min
      ["targetLufs", -9], // above max
      ["stepLufs", 0],
      ["stepLufs", -1],
      ["minLufs", -10], // min = max
      ["minLufs", -5], // min > max
      ["targetLufs", -16.5],
    ];
    for (const [key, number] of bad) {
      expect(isPanelOpen(mutate(normalize, ["panel", key], number)), `${key} ${number}`).toBe(false);
    }
  });

  it("rejects a target off the step grid", () => {
    const stepTwo = mutate(normalize, ["panel", "stepLufs"], 2);
    expect(isPanelOpen(mutate(stepTwo, ["panel", "targetLufs"], -15))).toBe(false);
    expect(isPanelOpen(mutate(stepTwo, ["panel", "targetLufs"], -16))).toBe(true);
    // max − min (14) must be a whole number of steps, too.
    expect(isPanelOpen(mutate(normalize, ["panel", "stepLufs"], 4))).toBe(false);
  });

  it("accepts the min and the max as targets", () => {
    expect(isPanelOpen(mutate(normalize, ["panel", "targetLufs"], -24))).toBe(true);
    expect(isPanelOpen(mutate(normalize, ["panel", "targetLufs"], -10))).toBe(true);
  });

  it("accepts any item name, including one that looks like a path", () => {
    expect(isPanelOpen(mutate(channels, ["items", 0, "name"], ""))).toBe(true);
    expect(isPanelOpen(mutate(channels, ["items", 0, "name"], "a/b.wav"))).toBe(true);
  });
});
