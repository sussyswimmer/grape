/**
 * The toast's state, decided without a DOM (docs/design/m1-drag-wheel.md
 * §D.4). Rust composes every string and decides when to show and hide; the
 * UI only tracks which toast is on screen.
 */

import type { ToastHidePayload, ToastShowPayload } from "../shared/events";

/** The toast on screen, or `null`. */
export type ToastState = ToastShowPayload | null;

/** `toast://show` replaces whatever is shown (a newer batch, or an update of the same id). */
export function applyShow(_state: ToastState, payload: ToastShowPayload): ToastState {
  return payload;
}

/** `toast://hide` hides the toast only if it is still the one shown. */
export function applyHide(state: ToastState, payload: ToastHidePayload): ToastState {
  return state !== null && state.id === payload.id ? null : state;
}

/** The id a click should report, or `null` when the shown toast can't reveal anything. */
export function clickTarget(state: ToastState): number | null {
  return state !== null && state.revealable ? state.id : null;
}

/** The progress bar's width in percent (one decimal), or `null` for no bar. */
export function progressPercent(progress: number | null): number | null {
  if (progress === null || !Number.isFinite(progress)) {
    return null;
  }
  const clamped = Math.min(1, Math.max(0, progress));
  return Math.round(clamped * 1000) / 10;
}
