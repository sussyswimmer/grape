/**
 * The tool panel's state, as pure functions (D31). Rust sends one request
 * (`PanelOpen`): a seq, the items by id and display name, and what the
 * panel offers. The page keeps the order, the choice, the selected row and
 * a phase, and answers with the seq and ids only (`runArgs`, `cancelArgs`).
 *
 * Phases: `idle` takes changes; `running` (Run was sent) and `closing`
 * (Cancel was sent) take none until `panel://close` or a newer
 * `panel://open` arrives, or a rejected run puts the panel back to `idle`.
 * Every change function returns the same state object when nothing
 * changes, so the view can skip work.
 */

import type {
  Channels,
  CompressPreset,
  PanelChoice,
  PanelOpen,
  PanelRunArgs,
  PanelSpec,
} from "../shared/events";
import { movedText } from "./copy";

/** One merge row, in logical px (Rust's `ROW`). */
export const ROW_HEIGHT = 36;
/** Rows the merge list shows before it scrolls (Rust's `ROWS_SHOWN`). */
export const ROWS_SHOWN = 6;
/** How far a press moves, in px, before it becomes a reorder. */
export const DRAG_THRESHOLD = 4;
/** During a reorder, the list scrolls while the pointer is this close to its top or bottom. */
export const EDGE_ZONE = 12;

export type PanelPhase = "idle" | "running" | "closing";

export interface PanelState {
  readonly request: PanelOpen;
  /** The item ids in the order they'll run (Merge: the page order). */
  readonly order: readonly number[];
  readonly choice: PanelChoice;
  /** The selected merge row's id (the first id for the other tools). */
  readonly selected: number;
  readonly phase: PanelPhase;
  /** A rejected run's sentence, shown in the hint until the next change or run. */
  readonly error: string | null;
}

/** The choice a panel starts from, as Rust sent it. */
function choiceFor(spec: PanelSpec): PanelChoice {
  switch (spec.tool) {
    case "compress":
      return { tool: "compress", preset: spec.preset };
    case "audioChannels":
      return { tool: "audioChannels", channels: spec.channels };
    case "normalizeAudio":
      return { tool: "normalizeAudio", targetLufs: spec.targetLufs };
    case "mergePDF":
      return { tool: "mergePDF" };
  }
}

/** The state for a fresh request: the items in drop order, Rust's choice, the first row selected. */
export function initialState(request: PanelOpen): PanelState {
  const order = request.items.map((item) => item.id);
  return {
    request,
    order,
    choice: choiceFor(request.panel),
    selected: order[0] ?? 0,
    phase: "idle",
    error: null,
  };
}

/** A change to what will run. It clears a rejected run's message, which was about the old choice. */
function changed(s: PanelState, update: Partial<Pick<PanelState, "order" | "choice">>): PanelState {
  return { ...s, ...update, error: null };
}

export function choosePreset(s: PanelState, preset: CompressPreset): PanelState {
  if (s.phase !== "idle" || s.choice.tool !== "compress" || s.choice.preset === preset) {
    return s;
  }
  return changed(s, { choice: { tool: "compress", preset } });
}

export function chooseChannels(s: PanelState, channels: Channels): PanelState {
  if (s.phase !== "idle" || s.choice.tool !== "audioChannels" || s.choice.channels === channels) {
    return s;
  }
  return changed(s, { choice: { tool: "audioChannels", channels } });
}

/** Snaps `value` to the step grid from `minLufs` and clamps it into min..max. */
export function setLufs(s: PanelState, value: number): PanelState {
  const spec = s.request.panel;
  if (s.phase !== "idle" || spec.tool !== "normalizeAudio" || s.choice.tool !== "normalizeAudio") {
    return s;
  }
  if (!Number.isFinite(value)) {
    return s;
  }
  const steps = Math.round((value - spec.minLufs) / spec.stepLufs);
  const snapped = Math.min(spec.maxLufs, Math.max(spec.minLufs, spec.minLufs + steps * spec.stepLufs));
  if (snapped === s.choice.targetLufs) {
    return s;
  }
  return changed(s, { choice: { tool: "normalizeAudio", targetLufs: snapped } });
}

/** Selects the row with this id (ids not in the list are ignored). */
export function select(s: PanelState, id: number): PanelState {
  if (s.phase !== "idle" || s.selected === id || !s.order.includes(id)) {
    return s;
  }
  return { ...s, selected: id };
}

/** Selects the row above or below, without wrapping. */
export function selectBy(s: PanelState, delta: -1 | 1): PanelState {
  const index = s.order.indexOf(s.selected);
  const next = s.order[index + delta];
  return index < 0 || next === undefined ? s : select(s, next);
}

export function selectEdge(s: PanelState, edge: "first" | "last"): PanelState {
  const next = edge === "first" ? s.order[0] : s.order[s.order.length - 1];
  return next === undefined ? s : select(s, next);
}

/** Moves the selected row up or down by one; a no-op at the ends. The selection follows it. */
export function moveSelected(s: PanelState, delta: -1 | 1): PanelState {
  const from = s.order.indexOf(s.selected);
  const to = from + delta;
  if (from < 0 || to < 0 || to >= s.order.length) {
    return s;
  }
  return moveItem(s, from, to);
}

function clampIndex(index: number, count: number): number {
  return Math.min(count - 1, Math.max(0, Math.trunc(index)));
}

/** Moves the row at index `from` to index `to` (both clamped into the list); a no-op when they're equal. */
export function moveItem(s: PanelState, from: number, to: number): PanelState {
  const count = s.order.length;
  if (s.phase !== "idle" || count === 0 || !Number.isFinite(from) || !Number.isFinite(to)) {
    return s;
  }
  const a = clampIndex(from, count);
  const b = clampIndex(to, count);
  if (a === b) {
    return s;
  }
  const order = [...s.order];
  const [moved] = order.splice(a, 1);
  if (moved === undefined) {
    return s;
  }
  order.splice(b, 0, moved);
  return changed(s, { order });
}

/**
 * Where a reorder that started on row `start` lands: the row under the
 * pointer, taking the press as the row's middle. `dy` is how far the
 * pointer moved; `scrollDelta` how far the list scrolled since the press.
 */
export function targetIndex(start: number, dy: number, scrollDelta: number, count: number): number {
  if (count <= 0) {
    return 0;
  }
  const y = start * ROW_HEIGHT + ROW_HEIGHT / 2 + dy + scrollDelta;
  return Math.min(count - 1, Math.max(0, Math.floor(y / ROW_HEIGHT)));
}

/** The scroll offset that keeps row `index` inside the list's `ROWS_SHOWN`-row window. */
export function scrollFor(index: number, scrollTop: number): number {
  const top = index * ROW_HEIGHT;
  const shown = ROWS_SHOWN * ROW_HEIGHT;
  if (top < scrollTop) {
    return Math.max(0, top);
  }
  if (top + ROW_HEIGHT > scrollTop + shown) {
    return top + ROW_HEIGHT - shown;
  }
  return scrollTop;
}

/** How far a list of `count` rows can scroll. */
export function maxScroll(count: number): number {
  return Math.max(0, (count - ROWS_SHOWN) * ROW_HEIGHT);
}

/**
 * During a reorder: the list's next scroll offset. `pointerY` is the
 * pointer's height inside the list's visible part. Within `EDGE_ZONE` of
 * the top or bottom it scrolls `ROW_HEIGHT / 4` that way, clamped.
 */
export function edgeScroll(pointerY: number, scrollTop: number, count: number): number {
  const shown = Math.min(count, ROWS_SHOWN) * ROW_HEIGHT;
  let next = scrollTop;
  if (pointerY < EDGE_ZONE) {
    next = scrollTop - ROW_HEIGHT / 4;
  } else if (pointerY > shown - EDGE_ZONE) {
    next = scrollTop + ROW_HEIGHT / 4;
  }
  return Math.min(maxScroll(count), Math.max(0, next));
}

/** `panel_run`'s args: the seq, the ids in order and the choice. */
export function runArgs(s: PanelState): PanelRunArgs {
  return { seq: s.request.seq, order: [...s.order], choice: { ...s.choice } };
}

/** `panel_cancel`'s args. */
export function cancelArgs(s: PanelState): { seq: number } {
  return { seq: s.request.seq };
}

/** The merged PDF's name for the current first file, or `null` for the other tools. */
export function mergedName(s: PanelState): string | null {
  const spec = s.request.panel;
  if (spec.tool !== "mergePDF") {
    return null;
  }
  const first = s.order[0];
  const index = s.request.items.findIndex((item) => item.id === first);
  return spec.mergedNames[index] ?? null;
}

/** The display name of item `id`, or "" when there's none. */
export function itemName(s: PanelState, id: number): string {
  return s.request.items.find((item) => item.id === id)?.name ?? "";
}

/** The live region's line after item `id` moved: "cover.pdf moved to 1 of 3." */
export function announceMove(s: PanelState, id: number): string {
  return movedText(itemName(s, id), s.order.indexOf(id) + 1, s.order.length);
}
