import { describe, expect, it } from "vitest";
import { CHANNELS, COMPRESS_PRESETS, type PanelTool } from "../shared/events";
import { panelOpenFixture } from "../test-support/fixtures";
import {
  CHANNEL_LABELS,
  countText,
  HINT_ACTIVE,
  HINT_INACTIVE,
  lufsText,
  MERGE_HELP,
  movedText,
  NOTES,
  PRESET_LABELS,
  runLabel,
  savesAs,
} from "./copy";

describe("countText", () => {
  it("names the one file, and counts several", () => {
    expect(countText(panelOpenFixture("panel-open-channels.json"))).toBe("1 file · interview.wav");
    expect(countText(panelOpenFixture("panel-open-compress.json"))).toBe("2 files");
    expect(countText(panelOpenFixture("panel-open-merge.json"))).toBe("3 files");
  });
});

describe("lufsText", () => {
  it("writes the minus as U+2212, not a hyphen", () => {
    expect(lufsText(-16)).toBe("−16 LUFS");
    expect(lufsText(-16)).toBe("−16 LUFS");
    expect(lufsText(-16)).not.toContain("-");
    expect(lufsText(-24)).toBe("−24 LUFS");
    expect(lufsText(0)).toBe("0 LUFS");
  });
});

describe("runLabel", () => {
  it("names each tool's Run button", () => {
    const labels: Record<PanelTool, string> = {
      compress: "Compress",
      audioChannels: "Apply",
      normalizeAudio: "Normalize",
      mergePDF: "Merge",
    };
    for (const [tool, label] of Object.entries(labels) as [PanelTool, string][]) {
      expect(runLabel(tool)).toBe(label);
    }
  });
});

describe("the words", () => {
  it("has a label and a note for every option", () => {
    for (const preset of COMPRESS_PRESETS) {
      expect(PRESET_LABELS[preset].length).toBeGreaterThan(0);
      expect(NOTES.compress[preset].length).toBeGreaterThan(0);
    }
    for (const layout of CHANNELS) {
      expect(CHANNEL_LABELS[layout].length).toBeGreaterThan(0);
      expect(NOTES.audioChannels[layout].length).toBeGreaterThan(0);
    }
    expect(Object.keys(NOTES.compress).sort()).toEqual([...COMPRESS_PRESETS].sort());
    expect(Object.keys(NOTES.audioChannels).sort()).toEqual([...CHANNELS].sort());
  });

  it("uses the spec's sentences", () => {
    expect(NOTES.compress).toEqual({
      small: "Smallest file. Large JPEGs and videos are scaled down.",
      balanced: "Smaller, and hard to tell apart.",
      high: "Best quality. Saves the least space.",
    });
    expect(NOTES.audioChannels).toEqual({
      mono: "Mixes everything into one channel.",
      stereo: "Makes left and right channels.",
    });
    expect(HINT_ACTIVE).toBe("Enter runs · Esc cancels");
    expect(HINT_INACTIVE).toBe("Click here to use the keyboard.");
    expect(MERGE_HELP).toBe(
      "Drag a file to move it, or select it and use Move up and Move down. The first file names the new PDF.",
    );
    expect(savesAs("cover Merged.pdf")).toBe("Saves as cover Merged.pdf");
    expect(movedText("cover.pdf", 1, 3)).toBe("cover.pdf moved to 1 of 3.");
  });
});
