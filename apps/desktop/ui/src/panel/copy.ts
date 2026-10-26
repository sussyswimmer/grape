/**
 * Every word the tool panel shows (docs/INTERACTION_SPEC.md §11, D31), in
 * Grape's own words. The view takes its text from here only.
 */

import type { Channels, CompressPreset, PanelOpen, PanelTool } from "../shared/events";

/** The × in the header: its `aria-label` and `title`. */
export const CLOSE_LABEL = "Close";
export const CANCEL_LABEL = "Cancel";

/** The footer hint while the panel has the keyboard. */
export const HINT_ACTIVE = "Enter runs · Esc cancels";
/** The footer hint while another window has the keyboard (the panel opens that way on Windows). */
export const HINT_INACTIVE = "Click here to use the keyboard.";
/** Shown in the hint when a run is rejected without a sentence of its own. */
export const GENERIC_ERROR = "Grape couldn't use that choice.";

export const QUALITY_LABEL = "Quality";
export const CHANNELS_LABEL = "Channels";
export const LOUDNESS_LABEL = "Target loudness";
export const QUIETER = "Quieter";
export const LOUDER = "Louder";
export const LOUDNESS_NOTE = "Each file ends up about this loud.";
export const ORDER_LABEL = "Merge order";
/** The merge list's description, visually hidden and read through `aria-describedby`. */
export const MERGE_HELP =
  "Drag a file to move it, or select it and use Move up and Move down. The first file names the new PDF.";
export const MOVE_UP = "Move up";
export const MOVE_DOWN = "Move down";

export const PRESET_LABELS: Readonly<Record<CompressPreset, string>> = {
  small: "Small",
  balanced: "Balanced",
  high: "High",
};

export const CHANNEL_LABELS: Readonly<Record<Channels, string>> = {
  mono: "Mono",
  stereo: "Stereo",
};

/** The note under a segmented control, for the option that's checked. */
export const NOTES: {
  readonly compress: Readonly<Record<CompressPreset, string>>;
  readonly audioChannels: Readonly<Record<Channels, string>>;
} = {
  compress: {
    small: "Smallest file. Some quality is lost.",
    balanced: "Much smaller, and hard to tell apart.",
    high: "Best quality. Saves the least space.",
  },
  audioChannels: {
    mono: "Mixes everything into one channel.",
    stereo: "Makes left and right channels.",
  },
};

const RUN_LABELS: Readonly<Record<PanelTool, string>> = {
  compress: "Compress",
  audioChannels: "Apply",
  normalizeAudio: "Normalize",
  mergePDF: "Merge",
};

/** The header's second line: "1 file · photo.jpg" for one file, else "3 files". */
export function countText(request: PanelOpen): string {
  const [only] = request.items;
  if (request.items.length === 1 && only !== undefined) {
    return `1 file · ${only.name}`;
  }
  return `${request.items.length} files`;
}

/** A loudness as the panel shows it: "−16 LUFS", with U+2212 MINUS SIGN. */
export function lufsText(value: number): string {
  return value < 0 ? `−${-value} LUFS` : `${value} LUFS`;
}

/** The Run button's label for this tool. */
export function runLabel(tool: PanelTool): string {
  return RUN_LABELS[tool];
}

/** Under the merge list: the merged PDF's name. */
export function savesAs(name: string): string {
  return `Saves as ${name}`;
}

/** The live region's line after a move: "cover.pdf moved to 1 of 3." */
export function movedText(name: string, position: number, count: number): string {
  return `${name} moved to ${position} of ${count}.`;
}
