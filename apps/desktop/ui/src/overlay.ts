import "./styles/tokens.css";
import "./styles/overlay.css";
import { mustGetElementById } from "./shared/dom";

// The overlay window starts hidden. Rust shows it, positions it and (from
// M3) sends wheel://open with the segments to draw. The UI never decides
// behavior, so for now this script only checks the page is intact.
const wheel = mustGetElementById(document, "wheel");
wheel.setAttribute("data-state", "placeholder");
