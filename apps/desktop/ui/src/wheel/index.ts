/**
 * The wheel overlay's public surface. The overlay entry (`src/overlay.ts`)
 * needs only:
 *
 * ```ts
 * import { mountWheel, wireOverlay } from "./wheel";
 * import { mustGetElementById } from "./shared/dom";
 *
 * const view = mountWheel(mustGetElementById(document, "wheel-root"));
 * void wireOverlay(view);
 * ```
 *
 * Importing this module also loads the overlay styles.
 */

import "../styles/tokens.css";
import "../styles/overlay.css";
import {
  isWheelClosePayload,
  isWheelHighlightPayload,
  isWheelOpenPayload,
  WHEEL_CLOSE,
  WHEEL_HIGHLIGHT,
  WHEEL_OPEN,
} from "../shared/events";
import { type ListenFn, listenAll, route } from "../shared/listen";
import type { WheelView } from "./render";

export { mountWheel } from "./render";
export type { InvokeFn, SoundSink, WheelView, WheelViewOptions } from "./render";
export type { ListenFn } from "../shared/listen";

/**
 * Listens for `wheel://open`, `wheel://highlight` and `wheel://close`,
 * guards each payload and forwards it to `view`. Resolves to a function
 * that stops listening. Never rejects (see `listenAll`), so
 * `void wireOverlay(view)` is safe.
 */
export function wireOverlay(view: WheelView, listenFn?: ListenFn): Promise<() => void> {
  return listenAll(
    [
      route(WHEEL_OPEN, isWheelOpenPayload, (payload) => view.open(payload)),
      route(WHEEL_HIGHLIGHT, isWheelHighlightPayload, (payload) => view.highlight(payload)),
      route(WHEEL_CLOSE, isWheelClosePayload, (payload) => view.close(payload)),
    ],
    "Grape overlay: could not listen for wheel events",
    listenFn,
  );
}
