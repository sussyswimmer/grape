// Draws the wheel into the overlay's SVG from a wheel://open payload. The
// overlay fills the whole monitor, so the wheel is translated to the
// anchor Rust picked. Nothing here decides behavior: highlight and close
// come from Rust too.

import {
  modeCaption,
  segmentCaption,
  segmentPath,
  type WheelMode,
  type WheelOpen,
} from "./geometry";

const SVG_NS = "http://www.w3.org/2000/svg";

/** The drawn wheel, kept so highlight updates don't query the DOM. */
export interface DrawnWheel {
  mode: WheelMode;
  segments: SVGPathElement[];
  caption: SVGTextElement;
}

/** Replaces whatever `root` held with the wheel described by `open`. */
export function drawWheel(root: SVGGElement, open: WheelOpen): DrawnWheel {
  root.replaceChildren();
  root.setAttribute("transform", `translate(${open.anchor.x} ${open.anchor.y})`);

  const backdrop = document.createElementNS(SVG_NS, "circle");
  backdrop.setAttribute("class", "wheel-backdrop");
  backdrop.setAttribute("r", String(open.geometry.backdropR));
  root.append(backdrop);

  const segments = open.segments.map((segment) => {
    const path = document.createElementNS(SVG_NS, "path");
    path.setAttribute("class", "wheel-segment");
    path.setAttribute("d", segmentPath(segment, open.geometry));
    path.setAttribute("data-index", String(segment.index));
    path.setAttribute("aria-label", segmentCaption(segment.index));
    root.append(path);

    const label = document.createElementNS(SVG_NS, "text");
    label.setAttribute("class", "wheel-label");
    label.setAttribute("x", String(segment.labelX));
    label.setAttribute("y", String(segment.labelY));
    label.textContent = String(segment.index + 1);
    root.append(label);
    return path;
  });

  const caption = document.createElementNS(SVG_NS, "text");
  caption.setAttribute("class", "wheel-caption");
  caption.textContent = modeCaption(open.mode);
  root.append(caption);

  root.removeAttribute("hidden");
  return { mode: open.mode, segments, caption };
}

/** Highlights segment `index`, or none. */
export function highlight(wheel: DrawnWheel, index: number | null): void {
  wheel.segments.forEach((path, i) => {
    path.classList.toggle("is-highlighted", i === index);
  });
  wheel.caption.textContent =
    index === null ? modeCaption(wheel.mode) : segmentCaption(index);
}

/** Removes the wheel. */
export function clearWheel(root: SVGGElement): void {
  root.replaceChildren();
  root.setAttribute("hidden", "");
}
