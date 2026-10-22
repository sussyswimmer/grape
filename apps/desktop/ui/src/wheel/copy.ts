/**
 * The wheel's words (docs/design/m1-drag-wheel.md §C "Copy"). Grape's own
 * copy, never the reference app's. Key names always come from the payload,
 * so a remapped key ("Caps", "Right Alt") shows up as the user set it.
 * Pure: no DOM.
 */

import type { EmptyState, KeyNames, WheelContent, WheelMode } from "../shared/events";

/** Empty-state messages, with `{conversions}`, `{tools}` and `{actions}` placeholders. */
export const EMPTY_COPY: Readonly<Record<EmptyState, string>> = {
  noConversions: "Nothing to convert here. Tap {actions} for file actions.",
  noTools: "No tools fit this selection. Tap {conversions} to convert or {actions} for file actions.",
};

/** The center label while Rust is still waiting for the dragged paths. */
export const PENDING_LABEL = "…";

const PLACEHOLDER = /\{(conversions|tools|actions)\}/g;

/**
 * Replaces `{conversions}`, `{tools}` and `{actions}` with the user's key
 * names in one pass, so a key name is never itself re-substituted or read
 * as a `$&`-style replacement pattern.
 */
export function withKeyNames(template: string, keys: KeyNames): string {
  return template.replace(PLACEHOLDER, (_match, name: string) => {
    switch (name) {
      case "conversions":
        return keys.conversions;
      case "tools":
        return keys.tools;
      default:
        return keys.actions;
    }
  });
}

/** The empty-state message for `state`, naming the user's current keys. */
export function emptyMessage(state: EmptyState, keys: KeyNames): string {
  return withKeyNames(EMPTY_COPY[state], keys);
}

/** A short spoken name for each wheel. */
const MODE_NAMES: Readonly<Record<WheelMode, string>> = {
  conversions: "Convert",
  tools: "Tools",
  actions: "File actions",
};

/** The whole wheel's accessible name, e.g. "Convert wheel, 5 choices". */
export function wheelLabel(mode: WheelMode, content: WheelContent): string {
  const name = `${MODE_NAMES[mode]} wheel`;
  switch (content.kind) {
    case "pending":
      return `${name}, loading`;
    case "empty":
      return `${name}, no choices`;
    case "entries": {
      const n = content.segments.length;
      return `${name}, ${n} ${n === 1 ? "choice" : "choices"}`;
    }
  }
}
