/**
 * Draws the wheel as SVG inside the overlay (docs/design/m1-drag-wheel.md
 * §D.2). The decisions live in `scene.ts`; this file only turns a scene
 * into DOM nodes, so it stays thin.
 *
 * - `svg#wheel` is absolutely positioned at `anchor − backdropR` in the
 *   overlay's CSS px (the overlay window covers one monitor, and Rust sends
 *   the anchor relative to it), with a viewBox centered on the anchor.
 * - A new seq draws a fresh SVG that animates in (unless the user prefers
 *   reduced motion). A key swap, the entries arriving or a highlight redraws
 *   the same SVG in place, without animation.
 * - A close removes every child at once, so the next show never flashes a
 *   stale wheel.
 * - After drawing an open, the next animation frame reports `wheel_painted`
 *   to Rust (the M1 latency log). Errors there are swallowed.
 * - Highlight changes drive the synthesized tick (`tick.ts`).
 */

import {
  WHEEL_PAINTED,
  type WheelClosePayload,
  type WheelHighlightPayload,
  type WheelOpenPayload,
} from "../shared/events";
import { callQuietly, type InvokeFn, tauriInvoke } from "../shared/ipc";
import { formatNumber } from "./geometry";
import {
  applyClose,
  applyHighlight,
  applyOpen,
  buildScene,
  CLOSED,
  type Sound,
  type Step,
  type TextBlock,
  type WheelScene,
  type WheelState,
} from "./scene";
import { createTick, type SoundSink } from "./tick";

const SVG_NS = "http://www.w3.org/2000/svg";

/** The wheel overlay: one method per `wheel://…` event. Payloads must already be guarded. */
export interface WheelView {
  open(payload: WheelOpenPayload): void;
  highlight(payload: WheelHighlightPayload): void;
  close(payload: WheelClosePayload): void;
}

/** Seams for tests. Production code passes nothing. */
export interface WheelViewOptions {
  readonly invoke?: InvokeFn;
  readonly requestFrame?: (callback: () => void) => void;
  readonly prefersReducedMotion?: () => boolean;
  readonly sound?: SoundSink;
}

/**
 * Mounts the wheel into `root` (the overlay's `#wheel-root`, which covers
 * the window) and returns the view the event wiring drives. `root` is
 * emptied first.
 */
export function mountWheel(root: HTMLElement, options: WheelViewOptions = {}): WheelView {
  const doc = root.ownerDocument;
  const win = doc.defaultView;
  const callRust: InvokeFn = options.invoke ?? tauriInvoke;
  const requestFrame =
    options.requestFrame ??
    ((callback: () => void): void => {
      if (win === null) {
        callback();
      } else {
        win.requestAnimationFrame(() => callback());
      }
    });
  const prefersReducedMotion =
    options.prefersReducedMotion ??
    ((): boolean =>
      win !== null &&
      typeof win.matchMedia === "function" &&
      win.matchMedia("(prefers-reduced-motion: reduce)").matches);
  const sound = options.sound ?? createTick();

  let state: WheelState = CLOSED;
  let svg: SVGSVGElement | null = null;
  root.replaceChildren();

  function clear(): void {
    root.replaceChildren();
    svg = null;
  }

  function playSound(effect: Sound): void {
    if (effect === "play") {
      sound.play();
    } else if (effect === "cancel") {
      sound.cancel();
    }
  }

  function reportPainted(seq: number): void {
    requestFrame(() => {
      // Closed or replaced before it reached the screen: nothing to report.
      if (state.shown?.seq === seq) {
        callQuietly(callRust, WHEEL_PAINTED, { seq });
      }
    });
  }

  function apply(step: Step): WheelScene | null {
    state = step.state;
    playSound(step.sound);
    if (step.paint === "none") {
      return null;
    }
    const scene = buildScene(state);
    if (step.paint === "clear" || scene === null) {
      clear();
      return null;
    }
    if (step.paint === "fresh" || svg === null) {
      const fresh = doc.createElementNS(SVG_NS, "svg");
      drawScene(fresh, scene);
      if (!prefersReducedMotion()) {
        fresh.classList.add("wheel-enter");
      }
      root.replaceChildren(fresh);
      svg = fresh;
    } else {
      drawScene(svg, scene);
    }
    return scene;
  }

  return {
    open(payload) {
      sound.opened(payload.sounds);
      const scene = apply(applyOpen(state, payload));
      if (scene !== null) {
        reportPainted(scene.seq);
      }
    },
    highlight(payload) {
      apply(applyHighlight(state, payload));
    },
    close(payload) {
      apply(applyClose(state, payload));
    },
  };
}

// ---------------------------------------------------------------------------
// Scene → SVG
// ---------------------------------------------------------------------------

function svgElement<K extends keyof SVGElementTagNameMap>(
  doc: Document,
  tag: K,
  attributes: Readonly<Record<string, string>> = {},
): SVGElementTagNameMap[K] {
  const element = doc.createElementNS(SVG_NS, tag);
  for (const [name, value] of Object.entries(attributes)) {
    element.setAttribute(name, value);
  }
  return element;
}

/** A `<text>` with one `<tspan>` per line. Text goes in via textContent only. */
function textBlock(doc: Document, block: TextBlock, className: string): SVGTextElement {
  const text = svgElement(doc, "text");
  text.classList.add(className);
  for (const line of block.lines) {
    const span = svgElement(doc, "tspan", {
      x: formatNumber(block.x),
      y: formatNumber(line.y),
    });
    span.textContent = line.text;
    text.append(span);
  }
  return text;
}

/** Redraws `svg` from `scene`, keeping the element (and any running entry animation). */
function drawScene(svg: SVGSVGElement, scene: WheelScene): void {
  const doc = svg.ownerDocument;
  svg.setAttribute("id", "wheel");
  svg.classList.add("wheel");
  svg.classList.toggle("dense", scene.dense);
  svg.setAttribute("data-kind", scene.kind);
  svg.setAttribute("data-seq", String(scene.seq));
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", scene.label);
  svg.setAttribute("viewBox", scene.viewBox);
  svg.setAttribute("width", formatNumber(scene.size));
  svg.setAttribute("height", formatNumber(scene.size));
  // Positioned inline so placement never depends on the root's id or CSS.
  svg.style.setProperty("position", "absolute");
  svg.style.setProperty("left", `${formatNumber(scene.left)}px`);
  svg.style.setProperty("top", `${formatNumber(scene.top)}px`);

  const title = svgElement(doc, "title");
  title.textContent = scene.label;
  const children: SVGElement[] = [
    title,
    svgElement(doc, "circle", {
      class: "wheel-backdrop",
      cx: "0",
      cy: "0",
      r: formatNumber(scene.backdropR),
    }),
  ];

  if (scene.placeholders.length > 0) {
    const pending = svgElement(doc, "g", { class: "wheel-pending", "aria-hidden": "true" });
    for (const d of scene.placeholders) {
      pending.append(svgElement(doc, "path", { class: "wheel-placeholder", d, "fill-rule": "evenodd" }));
    }
    children.push(pending);
  }

  if (scene.segments.length > 0) {
    const segments = svgElement(doc, "g", { class: "wheel-segments" });
    const labels = svgElement(doc, "g", { class: "wheel-labels", "aria-hidden": "true" });
    for (const segment of scene.segments) {
      const path = svgElement(doc, "path", {
        d: segment.d,
        "fill-rule": "evenodd",
        "data-index": String(segment.index),
        "aria-label": segment.title,
      });
      path.classList.add("wheel-segment");
      path.classList.toggle("is-highlighted", segment.highlighted);
      const tooltip = svgElement(doc, "title");
      tooltip.textContent = segment.title;
      path.append(tooltip);
      segments.append(path);

      const label = textBlock(doc, segment.label, "wheel-label");
      label.classList.toggle("is-highlighted", segment.highlighted);
      labels.append(label);
    }
    children.push(segments, labels);
  }

  if (scene.center !== null) {
    children.push(textBlock(doc, scene.center, "wheel-center"));
  }
  if (scene.message !== null) {
    children.push(textBlock(doc, scene.message, "wheel-message"));
  }

  svg.replaceChildren(...children);
}
