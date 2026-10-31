/**
 * The float shelf's state, decided without a DOM. Rust owns the shelf: it
 * keeps the paths, adds and removes items, sizes, places, shows and hides
 * the window, and runs every drag out of it. The page lists what
 * `float://view` says and turns a press that moves into one
 * `float_drag_out` naming ids, never paths.
 *
 * A drag goes idle → pressed (the primary button went down on a row or on
 * "Drag all") → dragging (it moved `DRAG_THRESHOLD` px with the button
 * still held, so the page asks Rust for the drag once) → idle again when
 * `float://drag-ended` arrives. A press ends without a drag when the
 * button comes up, which the page may only learn from a pointermove with
 * no button held. While dragging, the page ignores every pointer gesture:
 * after an OS drag WebView2, and WKWebView as far as is known, send no
 * mouseup, so only Rust's event (or the `DRAG_FALLBACK_MS` fallback) ends
 * it.
 */

import type { FloatItem, FloatView } from "../shared/events";

/** How far (CSS px) a pressed pointer must move before the press becomes a drag. */
export const DRAG_THRESHOLD = 4;

/** How long the page waits for `float://drag-ended` before it takes gestures again. */
export const DRAG_FALLBACK_MS = 10_000;

/** What a press starts from: one row, or the "Drag all" handle. */
export type DragSource = { readonly kind: "item"; readonly id: number } | { readonly kind: "all" };

/** Where the pointer gesture stands. */
export type Gesture =
  | { readonly kind: "idle" }
  | {
      readonly kind: "pressed";
      readonly source: DragSource;
      readonly pointerId: number;
      readonly x: number;
      readonly y: number;
    }
  | { readonly kind: "dragging" };

export const IDLE: Gesture = { kind: "idle" };

/** The `buttons` bit of the primary (left) mouse button. */
const PRIMARY_BUTTON = 1;

/** An item the shelf can drag: anything that isn't missing. */
function draggable(item: FloatItem): boolean {
  return !item.missing;
}

/**
 * The ids a drag from `source` carries, in shelf order: the row's item if
 * it's still listed and not missing, or for "Drag all" every item that
 * isn't missing. Empty when there's nothing to drag.
 */
export function dragIds(view: FloatView, source: DragSource): number[] {
  const items =
    source.kind === "all" ? view.items : view.items.filter((item) => item.id === source.id);
  return items.filter(draggable).map((item) => item.id);
}

/**
 * The primary button went down on `source` at (`x`, `y`). Starts a press
 * only from idle and only when there's something to drag; otherwise the
 * gesture is unchanged (a missing row, a press while a drag runs).
 */
export function press(
  gesture: Gesture,
  view: FloatView,
  source: DragSource,
  pointerId: number,
  x: number,
  y: number,
): Gesture {
  if (gesture.kind !== "idle" || dragIds(view, source).length === 0) {
    return gesture;
  }
  return { kind: "pressed", source, pointerId, x, y };
}

/** What a pointermove did: the next gesture, and the ids to drag when it just became a drag. */
export interface Step {
  readonly gesture: Gesture;
  readonly drag: readonly number[] | null;
}

/**
 * The pointer moved to (`x`, `y`) with `buttons` held. A press ends when
 * the primary button is no longer held, and becomes a drag once it has
 * moved `DRAG_THRESHOLD` px. The ids are worked out now, from the view
 * as it is, so an item removed since the press isn't sent; if nothing is
 * left to drag the press just ends.
 */
export function move(
  gesture: Gesture,
  view: FloatView,
  pointerId: number,
  x: number,
  y: number,
  buttons: number,
): Step {
  if (gesture.kind !== "pressed" || gesture.pointerId !== pointerId) {
    return { gesture, drag: null };
  }
  if ((buttons & PRIMARY_BUTTON) === 0) {
    return { gesture: IDLE, drag: null };
  }
  if (Math.hypot(x - gesture.x, y - gesture.y) < DRAG_THRESHOLD) {
    return { gesture, drag: null };
  }
  const ids = dragIds(view, gesture.source);
  return ids.length === 0
    ? { gesture: IDLE, drag: null }
    : { gesture: { kind: "dragging" }, drag: ids };
}

/**
 * pointerup or pointercancel: a press on that pointer ends. A drag keeps
 * going; only `dragOver` ends it.
 */
export function release(gesture: Gesture, pointerId: number): Gesture {
  return gesture.kind === "pressed" && gesture.pointerId === pointerId ? IDLE : gesture;
}

/** `float://drag-ended`, or the fallback timer: a drag is over. A press in progress is left alone. */
export function dragOver(gesture: Gesture): Gesture {
  return gesture.kind === "dragging" ? IDLE : gesture;
}

/**
 * Where keyboard focus goes when the focused row `focused` leaves the
 * list: the next row that's still there, else the previous one, else
 * `null` (the close button). `before` and `after` are the ids in list
 * order. A row that's still listed keeps the focus; one `before` never
 * had goes to the first row.
 */
export function focusAfterRemoval(
  before: readonly number[],
  after: readonly number[],
  focused: number,
): number | null {
  const kept = new Set(after);
  if (kept.has(focused)) {
    return focused;
  }
  const at = before.indexOf(focused);
  if (at < 0) {
    return after[0] ?? null;
  }
  const next = before.slice(at + 1).find((id) => kept.has(id));
  if (next !== undefined) {
    return next;
  }
  const previous = before
    .slice(0, at)
    .reverse()
    .find((id) => kept.has(id));
  return previous ?? null;
}

// ---------------------------------------------------------------------------
// Copy (Grape's own words)
// ---------------------------------------------------------------------------

export const COPY = {
  close: "Close the shelf",
  list: "Items on the shelf",
  missing: "Missing",
  nothingToDrag: "Nothing to drag",
  unnamed: "Unnamed item",
} as const;

/** The header's title, with the count once there's something on the shelf: "Shelf · 3". */
export function titleText(count: number): string {
  return count > 0 ? `Shelf · ${count}` : "Shelf";
}

/** The name a row shows. Rust always sends one; an empty name still reads as something. */
export function displayName(name: string): string {
  return name.length > 0 ? name : COPY.unnamed;
}

/** The row's × button label. */
export function removeLabel(name: string): string {
  return `Remove ${displayName(name)}`;
}

/** The "Drag all" handle's text for `draggableCount` items that can go. */
export function dragAllText(draggableCount: number): string {
  return draggableCount > 0 ? `Drag all ${draggableCount}` : COPY.nothingToDrag;
}

/** The "Drag all" row is there from two items, as Rust's window height counts it. */
export function hasFooter(count: number): boolean {
  return count >= 2;
}
