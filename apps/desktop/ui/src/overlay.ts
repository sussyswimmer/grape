import { mustGetElementById } from "./shared/dom";
import { mountWheel, wireOverlay } from "./wheel";

// The overlay window starts hidden. Rust frames it on the monitor under the
// cursor, sends wheel://open, then shows it. The UI never decides behavior:
// it draws what arrives, and tells Rust once a wheel is on screen
// (wheel_painted) so the key-to-wheel latency can be logged.

const view = mountWheel(mustGetElementById(document, "wheel-root"));
void wireOverlay(view);
