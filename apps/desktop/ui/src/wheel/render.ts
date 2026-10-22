// Draws the wheel into the overlay's SVG from a wheel://open payload. The
// overlay fills the whole monitor, so the wheel is translated to the
// anchor Rust picked. Nothing here decides behavior: highlight and close
// come from Rust too. This is the interim renderer; the full one (rounded
// corners, tick, reduced motion) replaces it in M3.

import { emptyMessage, modeCaption, segmentPath, type WheelOpen } from "./geometry";

const SVG_NS = "http://www.w3.org/2000/svg";

/** The drawn wheel, kept so highlight updates don't query the DOM. */
export interface DrawnWheel {
  seq: number;
  idle: string;
  titles: string[];
  segments: SVGPathElement[];
  caption: SVGTextElement;
}

/** Replaces whatever `root` held with the wheel described by `open`. */
export function drawWheel(root: SVGGElement, open: WheelOpen): DrawnWheel {
  root.replaceChildren();
  root.setAttribute("transform", `translate(${open.anchor.x} ${open.anchor.y})`);
  const { geometry, content } = open;

  const backdrop = document.createElementNS(SVG_NS, "circle");
  backdrop.setAttribute("class", "wheel-backdrop");
  backdrop.setAttribute("r", String(geometry.backdropR));
  root.append(backdrop);

  let idle = modeCaption(open.mode);
  const titles: string[] = [];
  const segments: SVGPathElement[] = [];
  if (content.kind === "pending") {
    // Files not known yet: a neutral ring until the drop target's Enter.
    const ring = document.createElementNS(SVG_NS, "circle");
    ring.setAttribute("class", "wheel-pending");
    ring.setAttribute("r", String((geometry.innerR + geometry.outerR) / 2));
    ring.setAttribute("stroke-width", String(geometry.outerR - geometry.innerR));
    root.append(ring);
    idle = "…";
  } else if (content.kind === "empty") {
    idle = emptyMessage(content.state, open.keys);
  } else {
    for (const segment of content.segments) {
      const path = document.createElementNS(SVG_NS, "path");
      path.setAttribute("class", "wheel-segment");
      path.setAttribute("d", segmentPath(segment, geometry));
      path.setAttribute("aria-label", segment.title);
      root.append(path);

      const label = document.createElementNS(SVG_NS, "text");
      label.setAttribute("class", content.dense ? "wheel-label is-dense" : "wheel-label");
      label.setAttribute("x", String(segment.labelX));
      label.setAttribute("y", String(segment.labelY));
      label.textContent = segment.title;
      root.append(label);
      segments.push(path);
      titles.push(segment.title);
    }
  }

  const caption = document.createElementNS(SVG_NS, "text");
  caption.setAttribute("class", content.kind === "empty" ? "wheel-caption is-empty" : "wheel-caption");
  if (content.kind === "empty") {
    // The message sits under the wheel, where there's room for it.
    caption.setAttribute("y", String(geometry.backdropR + 20));
  }
  caption.textContent = idle;
  root.append(caption);

  root.removeAttribute("hidden");
  return { seq: open.seq, idle, titles, segments, caption };
}

/** Highlights segment `index`, or none. */
export function highlight(wheel: DrawnWheel, index: number | null): void {
  wheel.segments.forEach((path, i) => {
    path.classList.toggle("is-highlighted", i === index);
  });
  wheel.caption.textContent =
    index === null ? wheel.idle : (wheel.titles[index] ?? wheel.idle);
}

/** Removes the wheel. */
export function clearWheel(root: SVGGElement): void {
  root.replaceChildren();
  root.setAttribute("hidden", "");
}
