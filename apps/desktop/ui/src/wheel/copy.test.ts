import { describe, expect, it } from "vitest";
import type { KeyNames, UiSegment } from "../shared/events";
import { EMPTY_COPY, emptyMessage, PENDING_LABEL, wheelLabel, withKeyNames } from "./copy";

const WINDOWS: KeyNames = { conversions: "Shift", tools: "Ctrl", actions: "Alt" };

function segment(index: number): UiSegment {
  return {
    index,
    center: 0,
    start: 0,
    end: 0,
    labelX: 0,
    labelY: 0,
    title: "PNG",
    entry: { kind: "convert", key: "png" },
  };
}

describe("emptyMessage", () => {
  it("names the actions key when nothing converts", () => {
    expect(emptyMessage("noConversions", WINDOWS)).toBe(
      "Nothing to convert here. Switch to file actions with Alt.",
    );
  });

  it("names the conversions and actions keys when no tool fits", () => {
    expect(emptyMessage("noTools", WINDOWS)).toBe(
      "No tools fit here. Switch to conversions with Shift or file actions with Alt.",
    );
  });

  it("uses whatever the user mapped, e.g. Caps", () => {
    const keys: KeyNames = { conversions: "Caps", tools: "Right Ctrl", actions: "Right Alt" };
    expect(emptyMessage("noConversions", keys)).toBe(
      "Nothing to convert here. Switch to file actions with Right Alt.",
    );
    expect(emptyMessage("noTools", keys)).toBe(
      "No tools fit here. Switch to conversions with Caps or file actions with Right Alt.",
    );
  });

  it("uses the macOS names when Rust sends them", () => {
    const mac: KeyNames = { conversions: "Shift", tools: "Command", actions: "Control" };
    expect(emptyMessage("noConversions", mac)).toContain("Switch to file actions with Control.");
  });

  it("never hard-codes a key name in the templates", () => {
    for (const template of Object.values(EMPTY_COPY)) {
      expect(template).not.toMatch(/Shift|Ctrl|Alt|Command|Control|Option/);
      expect(template).toMatch(/\{actions\}/);
    }
  });
});

describe("withKeyNames", () => {
  it("replaces every placeholder, including repeats and {tools}", () => {
    expect(withKeyNames("{tools}/{tools}/{conversions}/{actions}", WINDOWS)).toBe(
      "Ctrl/Ctrl/Shift/Alt",
    );
  });

  it("treats key names as plain text", () => {
    const odd: KeyNames = { conversions: "$&", tools: "$1", actions: "{conversions}" };
    expect(withKeyNames("{conversions} {tools} {actions}", odd)).toBe("$& $1 {conversions}");
  });

  it("leaves unknown braces alone", () => {
    expect(withKeyNames("{shift} {actions", WINDOWS)).toBe("{shift} {actions");
  });
});

describe("wheelLabel", () => {
  it("names the wheel and how many choices it has", () => {
    expect(wheelLabel("conversions", { kind: "pending" })).toBe("Convert wheel, loading");
    expect(wheelLabel("tools", { kind: "empty", state: "noTools" })).toBe("Tools wheel, no choices");
    expect(
      wheelLabel("actions", { kind: "entries", dense: false, segments: [segment(0)] }),
    ).toBe("File actions wheel, 1 choice");
    expect(
      wheelLabel("conversions", {
        kind: "entries",
        dense: false,
        segments: [segment(0), segment(1), segment(2)],
      }),
    ).toBe("Convert wheel, 3 choices");
  });
});

describe("PENDING_LABEL", () => {
  it("is a single ellipsis character", () => {
    expect(PENDING_LABEL).toBe("…");
    expect(PENDING_LABEL).toHaveLength(1);
  });
});
