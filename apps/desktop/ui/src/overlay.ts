import "./styles/tokens.css";
import "./styles/overlay.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { mustGetElementById } from "./shared/dom";
import type { WheelHighlight, WheelOpen } from "./wheel/geometry";
import { clearWheel, drawWheel, highlight, type DrawnWheel } from "./wheel/render";

// The overlay window starts hidden. Rust frames it on the monitor under the
// cursor, sends wheel://open, then shows it. The UI never decides behavior:
// it draws what arrives, and tells Rust once a wheel is on screen so the
// key-to-wheel latency can be logged.

const root = mustGetElementById(document, "wheel");
if (!(root instanceof SVGGElement)) {
  throw new Error('Grape UI: #wheel must be an SVG <g> element');
}
const wheelRoot: SVGGElement = root;
let drawn: DrawnWheel | null = null;

/** Runs `then` after the next frame has been painted. */
function afterPaint(then: () => void): void {
  requestAnimationFrame(() => requestAnimationFrame(then));
}

void listen<WheelOpen>("wheel://open", (event) => {
  const open = event.payload;
  drawn = drawWheel(wheelRoot, open);
  afterPaint(() => {
    invoke("wheel_painted", { seq: open.seq }).catch((err: unknown) => {
      console.warn("Grape: wheel_painted failed", err);
    });
  });
});

void listen<WheelHighlight>("wheel://highlight", (event) => {
  if (drawn !== null) {
    highlight(drawn, event.payload.index);
  }
});

void listen("wheel://close", () => {
  drawn = null;
  clearWheel(wheelRoot);
});
