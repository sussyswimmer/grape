import "./styles/tokens.css";
import "./styles/toast.css";
import { mustGetElementById } from "./shared/dom";
import { mountToast, wireToast } from "./toast/view";

// The toast window starts hidden. Rust sizes and places it, emits
// toast://show / toast://hide, and shows or hides the window itself.
const view = mountToast(mustGetElementById(document, "toast-root"));
void wireToast(view);
