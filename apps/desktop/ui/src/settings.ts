import "./styles/tokens.css";
import "./styles/settings.css";
import { mustGetElementById } from "./shared/dom";
import { mountSettings } from "./settings/view";

// Rust builds the Settings window from the tray. The page asks Rust for the
// current settings, and each change goes straight back as a command: Rust
// stores it, applies it at once and answers with the new view.
mountSettings(mustGetElementById(document, "settings-root"));
