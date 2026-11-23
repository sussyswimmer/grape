import { mustGetElementById } from "./shared/dom";
import { mountWheel, quietPage, wireOverlay } from "./wheel";

// The overlay window starts hidden. Rust frames it on the monitor under the
// cursor, sends wheel://open, then shows it. The UI never decides behavior:
// it draws what arrives, and tells Rust once a wheel is on screen
// (wheel_painted) so the key-to-wheel latency can be logged. A click on a
// Shortcuts wheel is Rust's to read too: the page only cancels what the
// webview would do with it.

quietPage(document);
const view = mountWheel(mustGetElementById(document, "wheel-root"));
void wireOverlay(view);
