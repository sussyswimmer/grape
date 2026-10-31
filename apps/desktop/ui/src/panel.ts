import "./styles/tokens.css";
import "./styles/panel.css";
import { mustGetElementById } from "./shared/dom";
import { browserHost, mountPanel, wirePanel } from "./panel/view";

// Rust declares the panel window hidden, then sizes, places, shows and
// hides it, and sends panel://open before each show. The page asks for the
// open request once on load and answers by seq and item ids, never paths.
const view = mountPanel(mustGetElementById(document, "panel-root"), {
  host: browserHost(window, document),
});
void wirePanel(view);
