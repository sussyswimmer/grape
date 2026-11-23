/**
 * The wheel's words (docs/design/m1-drag-wheel.md §C "Copy"). Grape's own
 * copy, never the reference app's: no sentence here may repeat the strings
 * INTERACTION_SPEC §5 records as observed. Key names always come from the
 * payload, so a remapped key ("Caps", "Right Alt") shows up as the user set
 * it. Pure: no DOM.
 */

import type { EmptyState, KeyNames, WheelContent, WheelMode } from "../shared/events";

/**
 * Empty-state messages, with `{conversions}`, `{tools}` and `{actions}`
 * placeholders. Each must fit messageLines' 3 lines with long key names
 * such as "Right Shift" (scene.test.ts checks this).
 */
export const EMPTY_COPY: Readonly<Record<EmptyState, string>> = {
  noConversions: "Nothing to convert here. Switch to file actions with {actions}.",
  noTools:
    "No tools fit here. Switch to conversions with {conversions} or file actions with {actions}.",
};

/** The message of a Shortcuts menu or group with nothing in it. It names no key. */
export const SHORTCUTS_EMPTY = "No shortcuts here yet. Add some in Settings.";

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
  shortcuts: "Shortcuts",
};

/** A segment's spoken name: its title, and for a group the word "group". */
export function segmentName(title: string, group: boolean): string {
  return group ? `${title}, group` : title;
}

function choices(n: number): string {
  if (n === 0) {
    return "no choices";
  }
  return `${n} ${n === 1 ? "choice" : "choices"}`;
}

/**
 * The whole wheel's accessible name, e.g. "Convert wheel, 5 choices". A
 * Shortcuts wheel inside a group names the group too: "Shortcuts wheel,
 * Edit, 3 choices".
 */
export function wheelLabel(mode: WheelMode, content: WheelContent): string {
  const name = `${MODE_NAMES[mode]} wheel`;
  switch (content.kind) {
    case "pending":
      return `${name}, loading`;
    case "empty":
      return `${name}, no choices`;
    case "entries":
      return `${name}, ${choices(content.segments.length)}`;
    case "shortcuts": {
      const group = content.trail[content.trail.length - 1];
      const where = group === undefined ? name : `${name}, ${group}`;
      return `${where}, ${choices(content.segments.length)}`;
    }
  }
}
