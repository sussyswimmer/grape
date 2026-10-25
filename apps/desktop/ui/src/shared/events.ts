/**
 * The event contract between Rust and the UI (docs/design/m1-drag-wheel.md
 * §C). Rust emits the `wheel://…` events to the `overlay` window and the
 * `toast://…` events to the `toast` window; the UI answers with two app
 * commands. The float shelf (M5) adds the `float://…` events, emitted to
 * the `float` window, and five `float_*` commands that name items by id.
 * The types mirror `apps/desktop/src-tauri/src/events.rs`, and both sides
 * test against the same golden JSON in `./fixtures/`.
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

/** The Tauri window labels these events are emitted to. */
export const OVERLAY_WINDOW = "overlay";
export const TOAST_WINDOW = "toast";
export const FLOAT_WINDOW = "float";

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

/** A shelf item id: Rust hands them out from 1 and never reuses one. */
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
