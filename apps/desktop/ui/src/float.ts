import "./styles/tokens.css";
import "./styles/float.css";
import { mustGetElementById } from "./shared/dom";
import { mountFloat, wireFloat } from "./float/view";

// Rust builds the shelf window hidden on the first Alt-drop on FLOAT, then
// sizes, places, shows and hides it, and emits float://view after every
// change. The page asks for the current view once on load and only ever
// sends ids back, never paths.
const shelf = mountFloat(mustGetElementById(document, "float-root"));
void wireFloat(shelf);
