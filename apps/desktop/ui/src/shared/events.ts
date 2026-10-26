/**
 * The event contract between Rust and the UI (docs/design/m1-drag-wheel.md
 * §C). Rust emits the `wheel://…` events to the `overlay` window and the
 * `toast://…` events to the `toast` window; the UI answers with two app
 * commands. The float shelf (M5) adds the `float://…` events, emitted to
 * the `float` window, and five `float_*` commands that name items by id.
 * The tool panel (M4, D31) adds `panel://open` and `panel://close`,
 * emitted to the `panel` window, and three `panel_*` commands that answer
 * by the panel's seq and item ids, never paths. The types mirror
 * `apps/desktop/src-tauri/src/events.rs`, and both sides test against the
 * same golden JSON in `./fixtures/` (the `panel-run-*` and `panel-cancel`
 * files are command args, UI → Rust, not events).
 *
 * Payloads arrive as `unknown`. Every one goes through its guard before the
 * UI touches it, so a contract drift fails loudly in one place instead of
 * drawing a half-broken wheel.
 */

import { type Fields, hasExactKeys, isOneOf } from "./guards";

// ---------------------------------------------------------------------------
// Types (§C, verbatim)
// ---------------------------------------------------------------------------

export type WheelMode = "conversions" | "tools" | "actions";
export interface Point { x: number; y: number }
export interface Geometry { innerR: number; outerR: number; backdropR: number; gap: number; corner: number; labelR: number; denseAt: number; screenMargin: number }
export type WheelEntry = { kind: "convert" | "tool" | "action"; key: string };
export interface UiSegment { index: number; center: number; start: number; end: number; labelX: number; labelY: number; title: string; entry: WheelEntry }
export type EmptyState = "noConversions" | "noTools";
export type WheelContent =
  | { kind: "pending" }
  | { kind: "entries"; dense: boolean; segments: UiSegment[] }
  | { kind: "empty"; state: EmptyState };
export interface KeyNames { conversions: string; tools: string; actions: string }
export interface WheelOpenPayload { seq: number; mode: WheelMode; anchor: Point; scale: number; geometry: Geometry; keys: KeyNames; sounds: boolean; content: WheelContent }
export interface WheelHighlightPayload { seq: number; index: number | null }
export type CloseReason = "drop" | "cancel" | "escape" | "noFiles" | "dragEnded" | "watchdog" | "paused";
export interface WheelClosePayload { seq: number; reason: CloseReason }
export type ToastTone = "working" | "done" | "error" | "info";
export interface ToastShowPayload { id: number; tone: ToastTone; title: string; detail: string | null; progress: number | null; revealable: boolean }
export interface ToastHidePayload { id: number }

// The float shelf (M5). Rust keeps the paths; the page gets ids and names.
export type FloatKind = "file" | "folder";
/** One shelf item. `id` is a safe integer ≥ 1 that Rust never reuses; `name` is a display name, never a path. */
export interface FloatItem { id: number; name: string; kind: FloatKind; missing: boolean }
/** What the shelf lists, oldest first. */
export interface FloatView { items: FloatItem[] }
/** `nothing`: there was nothing to drag, or the button was already up. */
export type FloatOutcome = "copied" | "linked" | "cancelled" | "nothing" | "failed";
export interface FloatDragEnded { outcome: FloatOutcome }

// The tool panel (M4, D31). Rust keeps the paths; the page gets a seq, item
// ids (1..=n in drop order) and display names, and answers with ids only.
/** grape-core's `ToolKey` serde keys for the four v1 panel tools. */
export type PanelTool = "compress" | "audioChannels" | "normalizeAudio" | "mergePDF";
/** grape-core's `CompressPreset` serde names. */
export type CompressPreset = "small" | "balanced" | "high";
/** grape-core's `Channels` serde names. */
export type Channels = "mono" | "stereo";
/** One dropped item: `id` is 1..=n in drop order; `name` is a display name, never a path. */
export interface PanelItem { id: number; name: string }
/**
 * What the panel offers, per tool. Normalize's numbers are whole LUFS on the
 * `stepLufs` grid. `mergedNames[i]` is the merged PDF's name if `items[i]`
 * goes first.
 */
export type PanelSpec =
  | { tool: "compress"; preset: CompressPreset }
  | { tool: "audioChannels"; channels: Channels }
  | { tool: "normalizeAudio"; targetLufs: number; minLufs: number; maxLufs: number; stepLufs: number }
  | { tool: "mergePDF"; mergedNames: string[] };
/** `panel://open`, and `panel_current`'s answer: show this request, replacing whatever was shown. */
export interface PanelOpen { seq: number; title: string; items: PanelItem[]; panel: PanelSpec }
/** `panel://close`: panel `seq` closed (run, cancelled or paused). */
export interface PanelClose { seq: number }
/** What the page chose, in `panel_run`. */
export type PanelChoice =
  | { tool: "compress"; preset: CompressPreset }
  | { tool: "audioChannels"; channels: Channels }
  | { tool: "normalizeAudio"; targetLufs: number }
  | { tool: "mergePDF" };
/** `panel_run`'s args. A type alias, not an interface, so it passes as `invoke`'s args record. */
export type PanelRunArgs = { seq: number; order: number[]; choice: PanelChoice };

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

/** Rust → overlay: render or re-render the wheel (open, key swap, entries arrived). */
export const WHEEL_OPEN = "wheel://open";
/** Rust → overlay: the highlighted segment changed. */
export const WHEEL_HIGHLIGHT = "wheel://highlight";
/** Rust → overlay: clear the wheel now. */
export const WHEEL_CLOSE = "wheel://close";
/** Rust → toast: show this toast, replacing whatever is shown. */
export const TOAST_SHOW = "toast://show";
/** Rust → toast: hide this toast if it is still the one shown. */
export const TOAST_HIDE = "toast://hide";

/** UI → Rust command, args `{ seq }`: the wheel for `seq` reached the screen. */
export const WHEEL_PAINTED = "wheel_painted";
/** UI → Rust command, args `{ id }`: the user clicked a revealable toast. */
export const TOAST_CLICKED = "toast_clicked";

/** Rust → float: what the shelf lists now. Sent after every change and before the window shows. */
export const FLOAT_VIEW = "float://view";
/** Rust → float: a drag out of the shelf is over, so the page takes gestures again. */
export const FLOAT_DRAG_ENDED = "float://drag-ended";

/** UI → Rust command, no args: answers with the current `FloatView`. The page asks once, on load. */
export const FLOAT_GET_VIEW = "float_view";
/** UI → Rust command, args `{ id }`: take this item off the shelf (the file is never touched). */
export const FLOAT_REMOVE = "float_remove";
/** UI → Rust command, no args: empty the shelf and hide it. */
export const FLOAT_CLEAR = "float_clear";
/** UI → Rust command, args `{ id }`: show this item in the file manager. */
export const FLOAT_REVEAL = "float_reveal";
/**
 * UI → Rust command, args `{ ids }`: start a drag of these items, while the
 * button is still down. Returns at once; `float://drag-ended` follows when
 * the drag is over.
 */
export const FLOAT_DRAG_OUT = "float_drag_out";

/** Rust → panel: show this request (new, or replacing the open one). Sent before the window shows. */
export const PANEL_OPEN = "panel://open";
/** Rust → panel: panel `seq` closed, after the window hid. The page ignores another seq. */
export const PANEL_CLOSE = "panel://close";

/** UI → Rust command, no args: answers with the open `PanelOpen`, or `null`. The page asks once, on load. */
export const PANEL_CURRENT = "panel_current";
/**
 * UI → Rust command, args `PanelRunArgs`: run panel `seq` with this order
 * and choice. Resolves `null` (a stale seq or a second run is ignored), or
 * rejects with a sentence and the panel stays open.
 */
export const PANEL_RUN = "panel_run";
/** UI → Rust command, args `{ seq }`: close panel `seq`, and nothing runs. */
export const PANEL_CANCEL = "panel_cancel";

/** The Tauri window labels these events are emitted to. */
export const OVERLAY_WINDOW = "overlay";
export const TOAST_WINDOW = "toast";
export const FLOAT_WINDOW = "float";
export const PANEL_WINDOW = "panel";

// ---------------------------------------------------------------------------
// Enum values, kept next to the types so a new variant is a compile error
// here until the guard knows it.
// ---------------------------------------------------------------------------

const WHEEL_MODES: readonly WheelMode[] = ["conversions", "tools", "actions"];
const ENTRY_KINDS: readonly WheelEntry["kind"][] = ["convert", "tool", "action"];
const EMPTY_STATES: readonly EmptyState[] = ["noConversions", "noTools"];
const CLOSE_REASONS: readonly CloseReason[] = [
  "drop",
  "cancel",
  "escape",
  "noFiles",
  "dragEnded",
  "watchdog",
  "paused",
];
const TOAST_TONES: readonly ToastTone[] = ["working", "done", "error", "info"];
const FLOAT_KINDS: readonly FloatKind[] = ["file", "folder"];
const FLOAT_OUTCOMES: readonly FloatOutcome[] = ["copied", "linked", "cancelled", "nothing", "failed"];
export const COMPRESS_PRESETS: readonly CompressPreset[] = ["small", "balanced", "high"];
export const CHANNELS: readonly Channels[] = ["mono", "stereo"];

// ---------------------------------------------------------------------------
// Guard helpers
// ---------------------------------------------------------------------------

function isFiniteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

/** A Rust `u64`/`usize` that survived JSON: a non-negative safe integer. */
function isCount(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function isPoint(value: unknown): value is Point {
  return (
    hasExactKeys(value, ["x", "y"]) && isFiniteNumber(value["x"]) && isFiniteNumber(value["y"])
  );
}

/**
 * The geometry constants from grape-core. Besides the types, the radii must
 * nest (inner < outer ≤ backdrop) and the gap must fit inside the dead zone,
 * so the segment math downstream can never produce NaN.
 */
function isGeometry(value: unknown): value is Geometry {
  if (
    !hasExactKeys(value, [
      "innerR",
      "outerR",
      "backdropR",
      "gap",
      "corner",
      "labelR",
      "denseAt",
      "screenMargin",
    ])
  ) {
    return false;
  }
  const innerR = value["innerR"];
  const outerR = value["outerR"];
  const backdropR = value["backdropR"];
  const gap = value["gap"];
  const corner = value["corner"];
  const labelR = value["labelR"];
  const screenMargin = value["screenMargin"];
  return (
    isFiniteNumber(innerR) &&
    isFiniteNumber(outerR) &&
    isFiniteNumber(backdropR) &&
    isFiniteNumber(gap) &&
    isFiniteNumber(corner) &&
    isFiniteNumber(labelR) &&
    isFiniteNumber(screenMargin) &&
    isCount(value["denseAt"]) &&
    gap >= 0 &&
    innerR > gap / 2 &&
    outerR > innerR &&
    backdropR >= outerR &&
    corner >= 0 &&
    labelR >= 0 &&
    screenMargin >= 0
  );
}

function isKeyNames(value: unknown): value is KeyNames {
  return (
    hasExactKeys(value, ["conversions", "tools", "actions"]) &&
    typeof value["conversions"] === "string" &&
    typeof value["tools"] === "string" &&
    typeof value["actions"] === "string"
  );
}

function isWheelEntry(value: unknown): value is WheelEntry {
  return (
    hasExactKeys(value, ["kind", "key"]) &&
    isOneOf(value["kind"], ENTRY_KINDS) &&
    typeof value["key"] === "string" &&
    value["key"].length > 0
  );
}

function isUiSegment(value: unknown, position: number): value is UiSegment {
  return (
    hasExactKeys(value, [
      "index",
      "center",
      "start",
      "end",
      "labelX",
      "labelY",
      "title",
      "entry",
    ]) &&
    // Rust zips `wheel_layout(n)` with the entries, so index == position.
    value["index"] === position &&
    isFiniteNumber(value["center"]) &&
    isFiniteNumber(value["start"]) &&
    isFiniteNumber(value["end"]) &&
    isFiniteNumber(value["labelX"]) &&
    isFiniteNumber(value["labelY"]) &&
    typeof value["title"] === "string" &&
    isWheelEntry(value["entry"])
  );
}

function isWheelContent(value: unknown): value is WheelContent {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const kind: unknown = (value as Fields)["kind"];
  switch (kind) {
    case "pending":
      return hasExactKeys(value, ["kind"]);
    case "entries": {
      if (!hasExactKeys(value, ["kind", "dense", "segments"])) {
        return false;
      }
      const segments = value["segments"];
      // grape-core sends `empty` instead of zero entries.
      return (
        typeof value["dense"] === "boolean" &&
        Array.isArray(segments) &&
        segments.length > 0 &&
        segments.every((segment: unknown, position: number) => isUiSegment(segment, position))
      );
    }
    case "empty":
      return hasExactKeys(value, ["kind", "state"]) && isOneOf(value["state"], EMPTY_STATES);
    default:
      return false;
  }
}

// ---------------------------------------------------------------------------
// Payload guards
// ---------------------------------------------------------------------------

/** Guards a `wheel://open` payload. */
export function isWheelOpenPayload(value: unknown): value is WheelOpenPayload {
  return (
    hasExactKeys(value, [
      "seq",
      "mode",
      "anchor",
      "scale",
      "geometry",
      "keys",
      "sounds",
      "content",
    ]) &&
    isCount(value["seq"]) &&
    isOneOf(value["mode"], WHEEL_MODES) &&
    isPoint(value["anchor"]) &&
    isFiniteNumber(value["scale"]) &&
    value["scale"] > 0 &&
    isGeometry(value["geometry"]) &&
    isKeyNames(value["keys"]) &&
    typeof value["sounds"] === "boolean" &&
    isWheelContent(value["content"])
  );
}

/** Guards a `wheel://highlight` payload. */
export function isWheelHighlightPayload(value: unknown): value is WheelHighlightPayload {
  return (
    hasExactKeys(value, ["seq", "index"]) &&
    isCount(value["seq"]) &&
    (value["index"] === null || isCount(value["index"]))
  );
}

/** Guards a `wheel://close` payload. */
export function isWheelClosePayload(value: unknown): value is WheelClosePayload {
  return (
    hasExactKeys(value, ["seq", "reason"]) &&
    isCount(value["seq"]) &&
    isOneOf(value["reason"], CLOSE_REASONS)
  );
}

/**
 * Guards a `toast://show` payload. `progress` is meant as a fraction in
 * 0..=1; any finite number passes and the view clamps it, so a value a
 * rounding error past 1 never drops a whole toast.
 */
export function isToastShowPayload(value: unknown): value is ToastShowPayload {
  if (!hasExactKeys(value, ["id", "tone", "title", "detail", "progress", "revealable"])) {
    return false;
  }
  const progress = value["progress"];
  return (
    isCount(value["id"]) &&
    isOneOf(value["tone"], TOAST_TONES) &&
    typeof value["title"] === "string" &&
    (value["detail"] === null || typeof value["detail"] === "string") &&
    (progress === null || isFiniteNumber(progress)) &&
    typeof value["revealable"] === "boolean"
  );
}

/** Guards a `toast://hide` payload. */
export function isToastHidePayload(value: unknown): value is ToastHidePayload {
  return hasExactKeys(value, ["id"]) && isCount(value["id"]);
}

/** A shelf item id or a panel seq or item id: Rust counts them from 1. */
function isItemId(value: unknown): value is number {
  return isCount(value) && value >= 1;
}

function isFloatItem(value: unknown): value is FloatItem {
  return (
    hasExactKeys(value, ["id", "name", "kind", "missing"]) &&
    isItemId(value["id"]) &&
    typeof value["name"] === "string" &&
    isOneOf(value["kind"], FLOAT_KINDS) &&
    typeof value["missing"] === "boolean"
  );
}

/**
 * Guards a `float://view` payload (and the `float_view` answer). Besides
 * the types, no id may appear twice: the page keys its rows by id.
 */
export function isFloatView(value: unknown): value is FloatView {
  if (!hasExactKeys(value, ["items"])) {
    return false;
  }
  const items = value["items"];
  if (!Array.isArray(items) || !items.every((item: unknown) => isFloatItem(item))) {
    return false;
  }
  const ids = new Set(items.map((item: FloatItem) => item.id));
  return ids.size === items.length;
}

/** Guards a `float://drag-ended` payload. */
export function isFloatDragEnded(value: unknown): value is FloatDragEnded {
  return hasExactKeys(value, ["outcome"]) && isOneOf(value["outcome"], FLOAT_OUTCOMES);
}

/** A whole number that survived JSON exactly (Rust's `i8`/`u8` fit). */
function isWhole(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value);
}

function isPanelItem(value: unknown): value is PanelItem {
  return (
    hasExactKeys(value, ["id", "name"]) &&
    isItemId(value["id"]) &&
    typeof value["name"] === "string"
  );
}

/**
 * A panel spec with exactly its tool's keys. Normalize's target must sit
 * on the step grid inside min..max, so the slider can show it as is; Merge
 * needs two items or more and one merged name per item.
 */
function isPanelSpec(value: unknown, count: number): value is PanelSpec {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const tool: unknown = (value as Fields)["tool"];
  switch (tool) {
    case "compress":
      return hasExactKeys(value, ["tool", "preset"]) && isOneOf(value["preset"], COMPRESS_PRESETS);
    case "audioChannels":
      return hasExactKeys(value, ["tool", "channels"]) && isOneOf(value["channels"], CHANNELS);
    case "normalizeAudio": {
      if (!hasExactKeys(value, ["tool", "targetLufs", "minLufs", "maxLufs", "stepLufs"])) {
        return false;
      }
      const target = value["targetLufs"];
      const min = value["minLufs"];
      const max = value["maxLufs"];
      const step = value["stepLufs"];
      return (
        isWhole(target) &&
        isWhole(min) &&
        isWhole(max) &&
        isWhole(step) &&
        step >= 1 &&
        min < max &&
        min <= target &&
        target <= max &&
        (target - min) % step === 0 &&
        (max - min) % step === 0
      );
    }
    case "mergePDF": {
      if (!hasExactKeys(value, ["tool", "mergedNames"])) {
        return false;
      }
      const names = value["mergedNames"];
      return (
        count >= 2 &&
        Array.isArray(names) &&
        names.length === count &&
        names.every((name: unknown) => typeof name === "string")
      );
    }
    default:
      return false;
  }
}

/**
 * Guards a `panel://open` payload (and the `panel_current` answer). Item
 * ids are unique, because the page answers with them; the title is never
 * empty.
 */
export function isPanelOpen(value: unknown): value is PanelOpen {
  if (!hasExactKeys(value, ["seq", "title", "items", "panel"])) {
    return false;
  }
  const title = value["title"];
  const items = value["items"];
  if (
    !isItemId(value["seq"]) ||
    typeof title !== "string" ||
    title.length === 0 ||
    !Array.isArray(items) ||
    items.length === 0 ||
    !items.every((item: unknown) => isPanelItem(item))
  ) {
    return false;
  }
  const ids = new Set(items.map((item: PanelItem) => item.id));
  return ids.size === items.length && isPanelSpec(value["panel"], items.length);
}

/** Guards a `panel://close` payload. */
export function isPanelClose(value: unknown): value is PanelClose {
  return hasExactKeys(value, ["seq"]) && isItemId(value["seq"]);
}
