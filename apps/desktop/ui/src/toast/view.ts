/**
 * The toast window (docs/design/m1-drag-wheel.md §D.4): a rounded glass
 * pill in a 320×64 tool window that Rust sizes, places, shows and hides.
 * It shows a tone dot, the title, an optional detail and a thin progress
 * bar. Clicking anywhere asks Rust to reveal the output, but only when the
 * toast says it can; the pointer cursor appears only then. Nothing else.
 */

import {
  isToastHidePayload,
  isToastShowPayload,
  TOAST_CLICKED,
  TOAST_HIDE,
  TOAST_SHOW,
  type ToastHidePayload,
  type ToastShowPayload,
} from "../shared/events";
import { callQuietly, type InvokeFn, tauriInvoke } from "../shared/ipc";
import { type ListenFn, listenAll, route } from "../shared/listen";
import { applyHide, applyShow, clickTarget, progressPercent, type ToastState } from "./model";

/** One method per `toast://…` event. Payloads must already be guarded. */
export interface ToastView {
  show(payload: ToastShowPayload): void;
  hide(payload: ToastHidePayload): void;
}

/** Seams for tests. Production code passes nothing. */
export interface ToastViewOptions {
  readonly invoke?: InvokeFn;
}

/**
 * Mounts the toast into `root` (the page's `#toast-root`, which covers the
 * window) and returns the view the event wiring drives.
 */
export function mountToast(root: HTMLElement, options: ToastViewOptions = {}): ToastView {
  const doc = root.ownerDocument;
  const callRust = options.invoke ?? tauriInvoke;
  let state: ToastState = null;

  // A live region that exists before any text arrives, so screen readers
  // announce each toast.
  root.setAttribute("role", "status");
  root.setAttribute("aria-live", "polite");
  root.replaceChildren();

  root.addEventListener("click", () => {
    const id = clickTarget(state);
    if (id !== null) {
      callQuietly(callRust, TOAST_CLICKED, { id });
    }
  });

  function element(tag: string, className: string, text?: string): HTMLElement {
    const node = doc.createElement(tag);
    node.classList.add(className);
    if (text !== undefined) {
      node.textContent = text;
    }
    return node;
  }

  function draw(): void {
    root.classList.toggle("is-revealable", clickTarget(state) !== null);
    if (state === null) {
      root.replaceChildren();
      return;
    }
    const pill = element("div", "toast");
    pill.setAttribute("data-tone", state.tone);
    pill.setAttribute("data-id", String(state.id));

    const dot = element("span", "toast-dot");
    dot.setAttribute("aria-hidden", "true");

    const text = element("div", "toast-text");
    text.append(element("div", "toast-title", state.title));
    if (state.detail !== null) {
      text.append(element("div", "toast-detail", state.detail));
    }
    pill.append(dot, text);

    const percent = progressPercent(state.progress);
    if (percent !== null) {
      const track = element("div", "toast-progress");
      track.setAttribute("role", "progressbar");
      track.setAttribute("aria-valuemin", "0");
      track.setAttribute("aria-valuemax", "100");
      track.setAttribute("aria-valuenow", String(Math.round(percent)));
      const bar = element("div", "toast-progress-bar");
      bar.style.setProperty("width", `${percent}%`);
      track.append(bar);
      pill.append(track);
    }
    root.replaceChildren(pill);
  }

  return {
    show(payload) {
      state = applyShow(state, payload);
      draw();
    },
    hide(payload) {
      const next = applyHide(state, payload);
      if (next !== state) {
        state = next;
        draw();
      }
    },
  };
}

/**
 * Listens for `toast://show` and `toast://hide`, guards each payload and
 * forwards it to `view`. Resolves to a function that stops listening and
 * never rejects, so `void wireToast(view)` is safe.
 */
export function wireToast(view: ToastView, listenFn?: ListenFn): Promise<() => void> {
  return listenAll(
    [
      route(TOAST_SHOW, isToastShowPayload, (payload) => view.show(payload)),
      route(TOAST_HIDE, isToastHidePayload, (payload) => view.hide(payload)),
    ],
    "Grape toast: could not listen for toast events",
    listenFn,
  );
}
