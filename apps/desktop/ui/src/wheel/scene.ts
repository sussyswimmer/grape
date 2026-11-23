/**
 * What the wheel shows, decided without a DOM (docs/design/m1-drag-wheel.md
 * §D.2). `render.ts` turns a `WheelScene` into SVG; everything here is pure
 * and unit-tested in Node.
 *
 * The UI never decides behavior: Rust owns the state machine and the hit
 * test. This module only tracks which payload is on screen, applies the
 * seq guard, validates the highlight against the content, and lays out text.
 */

import type {
  ShortcutSegment,
  WheelClosePayload,
  WheelContent,
  WheelHighlightPayload,
  WheelIcon,
  WheelOpenPayload,
} from "../shared/events";
import { emptyMessage, PENDING_LABEL, segmentName, SHORTCUTS_EMPTY, wheelLabel } from "./copy";
import { groupMarkPath, labelAnchor, segmentPath } from "./geometry";
import { GLYPH_GRID, glyphPaths } from "./glyphs";

// ---------------------------------------------------------------------------
// State and the seq guard
// ---------------------------------------------------------------------------

export interface WheelState {
  /** The payload on screen, or `null` when the wheel is closed. */
  readonly shown: WheelOpenPayload | null;
  /** The highlighted segment, always a valid index into `shown`'s segments. */
  readonly highlight: number | null;
}

export const CLOSED: WheelState = { shown: null, highlight: null };

/**
 * What the DOM must do: nothing, draw a new wheel (a new seq, so it may
 * animate in), redraw the current one in place (key swap, entries arrived,
 * highlight; never animated), or clear everything at once.
 */
export type Paint = "none" | "fresh" | "update" | "clear";

/** What the tick must do. */
export type Sound = "none" | "play" | "cancel";

export interface Step {
  readonly state: WheelState;
  readonly paint: Paint;
  readonly sound: Sound;
}

function unchanged(state: WheelState): Step {
  return { state, paint: "none", sound: "none" };
}

/** `index` if it names a drawn segment of `content` (entries or shortcuts), else `null`. */
export function validHighlight(content: WheelContent, index: number | null): number | null {
  if (index === null || (content.kind !== "entries" && content.kind !== "shortcuts")) {
    return null;
  }
  return Number.isSafeInteger(index) && index >= 0 && index < content.segments.length
    ? index
    : null;
}

/**
 * `wheel://open`. Every open is shown, whatever its seq: a new seq is a new
 * wheel, the same seq is a swap or the entries arriving. Either way Rust
 * has reset the highlight, so it starts at `null`.
 */
export function applyOpen(state: WheelState, payload: WheelOpenPayload): Step {
  const fresh = state.shown === null || state.shown.seq !== payload.seq;
  return {
    state: { shown: payload, highlight: null },
    paint: fresh ? "fresh" : "update",
    sound: "cancel",
  };
}

/**
 * `wheel://highlight`. Ignored unless it is for the wheel on screen. A move
 * to a new segment plays the tick (when sounds are on); leaving every
 * segment cancels a tick that just started.
 */
export function applyHighlight(state: WheelState, payload: WheelHighlightPayload): Step {
  const shown = state.shown;
  if (shown === null || payload.seq !== shown.seq) {
    return unchanged(state);
  }
  const index = validHighlight(shown.content, payload.index);
  if (index === state.highlight) {
    return unchanged(state);
  }
  let sound: Sound = "cancel";
  if (index !== null) {
    sound = shown.sounds ? "play" : "none";
  }
  return { state: { shown, highlight: index }, paint: "update", sound };
}

/**
 * `wheel://close`. Clears the wheel on screen. A close for an older seq is
 * stale and ignored. A close for a newer seq also clears: it means an open
 * was lost, so whatever is drawn is out of date and must not flash on the
 * next show.
 */
export function applyClose(state: WheelState, payload: WheelClosePayload): Step {
  const shown = state.shown;
  if (shown === null || payload.seq < shown.seq) {
    return unchanged(state);
  }
  return { state: CLOSED, paint: "clear", sound: "cancel" };
}

// ---------------------------------------------------------------------------
// Text layout
// ---------------------------------------------------------------------------

const ELLIPSIS = "\u2026";

/**
 * Greedy word wrap: at most `maxChars` per line where words allow (a word
 * is never split), at most `maxLines` lines. Overflow ends the last line
 * with "…".
 */
export function wrapLines(text: string, maxChars: number, maxLines: number): string[] {
  const words = text.trim().split(/\s+/).filter((word) => word.length > 0);
  const lines: string[] = [];
  let current = "";
  for (const word of words) {
    if (current === "") {
      current = word;
    } else if (current.length + 1 + word.length <= maxChars) {
      current = `${current} ${word}`;
    } else {
      lines.push(current);
      current = word;
    }
  }
  if (current !== "") {
    lines.push(current);
  }
  if (maxLines < 1) {
    return [];
  }
  if (lines.length <= maxLines) {
    return lines;
  }
  const kept = lines.slice(0, maxLines);
  const last = kept[maxLines - 1] ?? "";
  const room = Math.max(0, maxChars - 1);
  kept[maxLines - 1] = `${last.length <= room ? last : last.slice(0, room)}${ELLIPSIS}`;
  return kept;
}

/**
 * A unit that draws about twice as wide as a Latin letter: CJK, kana,
 * hangul, fullwidth forms and emoji.
 */
const WIDE =
  /[\p{scx=Han}\p{scx=Hiragana}\p{scx=Katakana}\p{scx=Hangul}\p{Emoji_Presentation}\uff01-\uff60\uffe0-\uffe6\ufe0f\u20e3]/u;

/**
 * User-typed text as the units a line may break between: whole characters
 * as a reader sees them (an emoji with its joiners, a letter with its
 * accents), or code points where the webview can't tell. Never half of a
 * surrogate pair.
 */
export function textUnits(text: string): string[] {
  if (typeof Intl.Segmenter === "function") {
    const segmenter = new Intl.Segmenter(undefined, { granularity: "grapheme" });
    return Array.from(segmenter.segment(text), (part) => part.segment);
  }
  return Array.from(text);
}

/** How wide one unit draws, in Latin-letter widths. */
function unitCells(unit: string): number {
  return WIDE.test(unit) ? 2 : 1;
}

/** How wide `text` draws, in Latin-letter widths: a wide unit counts 2. */
export function textCells(text: string): number {
  return textUnits(text).reduce((sum, unit) => sum + unitCells(unit), 0);
}

/**
 * Word wrap for text the user typed, measured with `textCells`: at most
 * `maxCells` per line and `maxLines` lines. A word wider than a line
 * breaks between units, and overflow ends the last line with "…".
 */
export function wrapCells(text: string, maxCells: number, maxLines: number): string[] {
  const words = text.trim().split(/\s+/).filter((word) => word.length > 0);
  const lines: string[] = [];
  let current = "";
  let used = 0;
  const endLine = (): void => {
    if (current !== "") {
      lines.push(current);
    }
    current = "";
    used = 0;
  };
  for (const word of words) {
    const space = current === "" ? 0 : 1;
    const units = textUnits(word);
    const width = units.reduce((sum, unit) => sum + unitCells(unit), 0);
    if (used + space + width <= maxCells) {
      current = space === 0 ? word : `${current} ${word}`;
      used += space + width;
      continue;
    }
    endLine();
    for (const unit of units) {
      const cells = unitCells(unit);
      if (used > 0 && used + cells > maxCells) {
        endLine();
      }
      current += unit;
      used += cells;
    }
  }
  endLine();
  if (maxLines < 1) {
    return [];
  }
  if (lines.length <= maxLines) {
    return lines;
  }
  // The last line takes as much of what is left as fits, then "…". Every
  // line is a piece of the words joined by single spaces, in order.
  const kept = lines.slice(0, maxLines - 1);
  let rest = words.join(" ");
  for (const line of kept) {
    rest = rest.slice(line.length).trimStart();
  }
  let last = "";
  let room = maxCells - 1;
  for (const unit of textUnits(rest)) {
    room -= unitCells(unit);
    if (room < 0) {
      break;
    }
    last += unit;
  }
  kept.push(`${last.trimEnd()}${ELLIPSIS}`);
  return kept;
}

/** Typography, in CSS px. Font sizes live in overlay.css; these must match it. */
export const TEXT = {
  /** Segment labels: characters per line and line height. */
  labelChars: 10,
  labelLineHeight: 13,
  /** The compact style at `dense` (Rust sets it at GEOMETRY.denseAt items). */
  denseLabelChars: 8,
  denseLabelLineHeight: 11,
  labelLines: 2,
  /** The center label inside the dead zone. */
  centerChars: 12,
  centerLineHeight: 15,
  centerLines: 2,
  /** The empty-state message on the backdrop; widened once before it is cut. */
  messageChars: 26,
  messageWideChars: 32,
  messageLineHeight: 17,
  messageLines: 3,
  /** A shortcut's icon: the side of its square, and the space under it. */
  iconSize: 20,
  denseIconSize: 18,
  iconGap: 2,
  /** An emoji's font size as a share of the icon's side. */
  emojiScale: 0.85,
  /** How much of a typed emoji is drawn, in `textCells`: one emoji, or two letters. */
  emojiCells: 2,
} as const;

/** The empty-state message as at most 3 lines. */
export function messageLines(text: string): string[] {
  const narrow = wrapLines(text, TEXT.messageChars, Number.POSITIVE_INFINITY);
  if (narrow.length <= TEXT.messageLines) {
    return narrow;
  }
  return wrapLines(text, TEXT.messageWideChars, TEXT.messageLines);
}

/** A segment label as at most 2 lines, narrower in the dense style. */
export function labelLines(title: string, dense: boolean): string[] {
  return wrapLines(title, dense ? TEXT.denseLabelChars : TEXT.labelChars, TEXT.labelLines);
}

/**
 * A shortcut's label as at most `maxLines` lines (2 unless told), narrower
 * in the dense style. The name is what the user typed, so a long word
 * breaks instead of running over, and a cut line ends with "…".
 */
export function shortcutLabelLines(
  title: string,
  dense: boolean,
  maxLines: number = TEXT.labelLines,
): string[] {
  return wrapCells(title, dense ? TEXT.denseLabelChars : TEXT.labelChars, maxLines);
}

export interface TextLine {
  readonly text: string;
  /** Baseline-centered y of this line (CSS px, wheel space). */
  readonly y: number;
}

export interface TextBlock {
  readonly x: number;
  readonly lines: readonly TextLine[];
}

/** Stacks `lines` so the block is vertically centered on `(x, y)`. */
export function stackLines(
  lines: readonly string[],
  x: number,
  y: number,
  lineHeight: number,
): TextBlock {
  const middle = (lines.length - 1) / 2;
  return {
    x,
    lines: lines.map((text, i) => ({ text, y: y + (i - middle) * lineHeight })),
  };
}

// ---------------------------------------------------------------------------
// The scene
// ---------------------------------------------------------------------------

/** Placeholder segments drawn while Rust waits for the dragged paths. */
export const PENDING_SEGMENTS = 8;

/**
 * The placeholder outlines, item 0 at the top and advancing clockwise like
 * grape-core's layout. They carry no entries, so this is the only angle the
 * UI computes itself.
 */
export function pendingPaths(payload: WheelOpenPayload): string[] {
  const n = PENDING_SEGMENTS;
  return Array.from({ length: n }, (_, i) =>
    segmentPath({ center: Math.PI / 2 - (2 * Math.PI * i) / n }, n, payload.geometry),
  );
}

/** What an icon draws: text for an emoji, stroked paths on the glyph grid for a glyph. */
export type IconArt =
  | { readonly kind: "emoji"; readonly text: string }
  | { readonly kind: "glyph"; readonly paths: readonly string[] };

/**
 * What `icon` draws, or `null` for no icon: the kind `none`, a glyph id
 * the page doesn't know, or an emoji with nothing in it. Only the start of
 * a typed emoji is drawn (`TEXT.emojiCells`), so it stays in its square.
 */
export function iconArt(icon: WheelIcon): IconArt | null {
  switch (icon.kind) {
    case "none":
      return null;
    case "glyph": {
      const paths = glyphPaths(icon.value);
      return paths === null ? null : { kind: "glyph", paths };
    }
    case "emoji": {
      let text = "";
      let cells = 0;
      for (const unit of textUnits(icon.value.trim())) {
        cells += unitCells(unit);
        if (text !== "" && cells > TEXT.emojiCells) {
          break;
        }
        text += unit;
      }
      return text === "" ? null : { kind: "emoji", text };
    }
  }
}

/** An icon and the square it is drawn in: its center and side (CSS px, wheel space). */
export interface SegmentIcon {
  readonly x: number;
  readonly y: number;
  readonly size: number;
  readonly art: IconArt;
}

/** A glyph's `transform`: the 24 × 24 grid moved and scaled onto the icon's square. */
export function glyphTransform(icon: SegmentIcon): { x: number; y: number; scale: number } {
  return { x: icon.x - icon.size / 2, y: icon.y - icon.size / 2, scale: icon.size / GLYPH_GRID };
}

/** An emoji's font size for its square. */
export function emojiFontSize(icon: SegmentIcon): number {
  return icon.size * TEXT.emojiScale;
}

export interface ShortcutLayout {
  readonly icon: SegmentIcon | null;
  readonly label: TextBlock;
}

/**
 * Where a shortcut's icon and label go, centered together on Rust's label
 * point. No icon: the label alone, as on the other wheels. An icon sits
 * over one line of label; in the dense style it stands alone, and the name
 * shows in the center on hover.
 */
export function shortcutLayout(
  seg: Pick<ShortcutSegment, "labelX" | "labelY" | "title" | "icon">,
  dense: boolean,
): ShortcutLayout {
  const at = labelAnchor(seg);
  const art = iconArt(seg.icon);
  const lineHeight = dense ? TEXT.denseLabelLineHeight : TEXT.labelLineHeight;
  if (art === null) {
    return {
      icon: null,
      label: stackLines(shortcutLabelLines(seg.title, dense), at.x, at.y, lineHeight),
    };
  }
  const size = dense ? TEXT.denseIconSize : TEXT.iconSize;
  const lines = dense ? [] : shortcutLabelLines(seg.title, false, 1);
  if (lines.length === 0) {
    return { icon: { x: at.x, y: at.y, size, art }, label: stackLines([], at.x, at.y, lineHeight) };
  }
  const top = at.y - (size + TEXT.iconGap + lineHeight) / 2;
  return {
    icon: { x: at.x, y: top + size / 2, size, art },
    label: stackLines(lines, at.x, top + size + TEXT.iconGap + lineHeight / 2, lineHeight),
  };
}

export interface SegmentShape {
  readonly index: number;
  readonly d: string;
  /** The full name, as sent. */
  readonly title: string;
  /** The spoken name: the title, and "group" in words for a group. */
  readonly name: string;
  readonly highlighted: boolean;
  readonly label: TextBlock;
  /** A shortcut's icon, or `null`. */
  readonly icon: SegmentIcon | null;
  /** The path of a group's mark, or `null`. */
  readonly mark: string | null;
}

export interface WheelScene {
  readonly seq: number;
  readonly kind: WheelContent["kind"];
  /** Where the SVG sits in the overlay (CSS px, top-left origin) and its size. */
  readonly left: number;
  readonly top: number;
  readonly size: number;
  /** The SVG viewBox, centered on the anchor. */
  readonly viewBox: string;
  readonly backdropR: number;
  /** The wheel's accessible name. */
  readonly label: string;
  readonly dense: boolean;
  readonly segments: readonly SegmentShape[];
  readonly placeholders: readonly string[];
  /** The center label, or `null` when it is hidden. */
  readonly center: TextBlock | null;
  /** The empty-state message, or `null`. */
  readonly message: TextBlock | null;
}

/** Everything to draw for `state`, or `null` when the wheel is closed. */
export function buildScene(state: WheelState): WheelScene | null {
  const shown = state.shown;
  if (shown === null) {
    return null;
  }
  const g = shown.geometry;
  const r = g.backdropR;
  const content = shown.content;
  const base = {
    seq: shown.seq,
    kind: content.kind,
    left: shown.anchor.x - r,
    top: shown.anchor.y - r,
    size: 2 * r,
    viewBox: `${-r} ${-r} ${2 * r} ${2 * r}`,
    backdropR: r,
    label: wheelLabel(shown.mode, content),
  };

  switch (content.kind) {
    case "pending":
      return {
        ...base,
        dense: false,
        segments: [],
        placeholders: pendingPaths(shown),
        center: stackLines([PENDING_LABEL], 0, 0, TEXT.centerLineHeight),
        message: null,
      };
    case "empty":
      return {
        ...base,
        dense: false,
        segments: [],
        placeholders: [],
        center: null,
        message: stackLines(
          messageLines(emptyMessage(content.state, shown.keys)),
          0,
          0,
          TEXT.messageLineHeight,
        ),
      };
    case "entries": {
      const n = content.segments.length;
      const lineHeight = content.dense ? TEXT.denseLabelLineHeight : TEXT.labelLineHeight;
      const highlight = validHighlight(content, state.highlight);
      const segments = content.segments.map((seg, index) => {
        const at = labelAnchor(seg);
        return {
          index,
          d: segmentPath(seg, n, g),
          title: seg.title,
          name: seg.title,
          highlighted: index === highlight,
          label: stackLines(labelLines(seg.title, content.dense), at.x, at.y, lineHeight),
          icon: null,
          mark: null,
        };
      });
      const hovered = highlight === null ? undefined : content.segments[highlight];
      return {
        ...base,
        dense: content.dense,
        segments,
        placeholders: [],
        center:
          hovered === undefined
            ? null
            : stackLines(
                wrapLines(hovered.title, TEXT.centerChars, TEXT.centerLines),
                0,
                0,
                TEXT.centerLineHeight,
              ),
        message: null,
      };
    }
    case "shortcuts": {
      const n = content.segments.length;
      if (n === 0) {
        return {
          ...base,
          dense: false,
          segments: [],
          placeholders: [],
          center: null,
          message: stackLines(messageLines(SHORTCUTS_EMPTY), 0, 0, TEXT.messageLineHeight),
        };
      }
      const highlight = validHighlight(content, state.highlight);
      const segments = content.segments.map((seg, index) => ({
        index,
        d: segmentPath(seg, n, g),
        title: seg.title,
        name: segmentName(seg.title, seg.group),
        highlighted: index === highlight,
        ...shortcutLayout(seg, content.dense),
        mark: seg.group ? groupMarkPath(seg, n, g) || null : null,
      }));
      // The hovered name, else the open group's, else nothing.
      const hovered = highlight === null ? undefined : content.segments[highlight];
      const named = hovered?.title ?? content.trail[content.trail.length - 1] ?? "";
      const lines = wrapCells(named, TEXT.centerChars, TEXT.centerLines);
      return {
        ...base,
        dense: content.dense,
        segments,
        placeholders: [],
        center: lines.length === 0 ? null : stackLines(lines, 0, 0, TEXT.centerLineHeight),
        message: null,
      };
    }
  }
}
