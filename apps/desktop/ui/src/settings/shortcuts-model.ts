/**
 * The Shortcuts section's contract (docs/design/m10-shortcuts-wheel.md §8).
 * Rust owns the menus, shortcuts and steps: each `shortcuts_*` command
 * answers with the whole `ShortcutsView`, and the page draws exactly that.
 * The types mirror `apps/desktop/src-tauri/src/shortcuts/model.rs`, and
 * both sides test against `src/shared/fixtures/shortcuts-view-*.json` and
 * `shortcuts-step-*.json`.
 *
 * A view arrives as `unknown` and goes through `isShortcutsView` before the
 * page touches it. Everything here is pure: no DOM.
 */

import { type Fields, hasExactKeys, isOneOf } from "../shared/guards";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** A key and the modifiers held with it. `key` is an id from the view's `keys`. */
export interface ChordWire { key: string; shift: boolean; ctrl: boolean; alt: boolean; meta: boolean }
export type ModifierId = "shift" | "ctrl" | "alt" | "meta";
export interface HotkeyView { chord: ChordWire; label: string }
export type Binding = "off" | "bound" | "failed";
export type Access = "granted" | "denied" | "notNeeded";
export type OpensAt = "cursor" | "center";
export interface Limits {
  items: number;
  steps: number;
  name: number;
  icon: number;
  text: number;
  script: number;
  url: number;
  menus: number;
  delayMinMs: number;
  delayMaxMs: number;
}
export interface KeyView { id: string; label: string }
export type ModifierNames = Record<ModifierId, string>;
export type Icon = { kind: "none" } | { kind: "emoji"; value: string } | { kind: "glyph"; value: string };
export type Scope = { kind: "everywhere" } | { kind: "app"; name: string };
export type ClipboardAction = "copy" | "paste" | "clear";
export type Placement =
  | "leftHalf"
  | "rightHalf"
  | "topHalf"
  | "bottomHalf"
  | "fill"
  | "center"
  | "topLeft"
  | "topRight"
  | "bottomLeft"
  | "bottomRight";
export type SystemControl =
  | "sleepDisplay"
  | "lockScreen"
  | "screensaver"
  | "missionControl"
  | "toggleMute"
  | "volumeUp"
  | "volumeDown"
  | "toggleDarkMode"
  | "screenshotArea";
export type StepKind =
  | "keys"
  | "typeText"
  | "clipboard"
  | "openApp"
  | "openFile"
  | "openUrl"
  | "appleShortcut"
  | "appleScript"
  | "shell"
  | "keyboardMaestro"
  | "window"
  | "system"
  | "notify"
  | "speak"
  | "delay";
/** A step's kind and values. An app, file or folder shows by name only. */
export type StepBody =
  | { kind: "keys"; chord: ChordWire | null; label: string | null }
  | { kind: "typeText"; text: string }
  | { kind: "clipboard"; action: ClipboardAction }
  | { kind: "openApp"; name: string | null }
  | { kind: "openFile"; name: string | null; folder: boolean }
  | { kind: "openUrl"; url: string }
  | { kind: "appleShortcut"; name: string }
  | { kind: "appleScript"; source: string }
  | { kind: "shell"; source: string }
  | { kind: "keyboardMaestro"; name: string }
  | { kind: "window"; placement: Placement }
  | { kind: "system"; control: SystemControl }
  | { kind: "notify"; text: string }
  | { kind: "speak"; text: string }
  | { kind: "delay"; ms: number };
export type StepView = { id: number } & StepBody;
/** A shortcut: a name, an icon and its steps. */
export interface StepsItem { id: number; name: string; icon: Icon; kind: "steps"; steps: StepView[] }
/** A group: a second ring of shortcuts. It never holds another group. */
export interface GroupItem { id: number; name: string; icon: Icon; kind: "group"; items: StepsItem[] }
export type ItemView = StepsItem | GroupItem;
export interface MenuView { id: number; scope: Scope; withEverywhere: boolean; items: ItemView[] }
export interface ShortcutsView {
  /** False where the wheel isn't built yet: the page then shows one line. */
  available: boolean;
  enabled: boolean;
  /** `null` when no key combination is set. */
  hotkey: HotkeyView | null;
  binding: Binding;
  access: Access;
  opensAt: OpensAt;
  /** A sentence about the change that made this view, e.g. "Everywhere didn't fit in this menu." */
  note: string | null;
  limits: Limits;
  /** Every key a chord may use, in menu order. */
  keys: KeyView[];
  /** What this OS calls the four modifiers. */
  modifiers: ModifierNames;
  /** Every glyph id an icon may use, in menu order. */
  glyphs: string[];
  menus: MenuView[];
}

/** What `shortcuts_add_item` makes. */
export type NewItemKind = "steps" | "group";

/**
 * A step's new values, as `shortcuts_set_step` takes them. There is none
 * for an app, file or folder: Rust picks those (`shortcuts_pick_target`).
 */
export type StepEdit =
  | { kind: "keys"; chord: ChordWire | null }
  | { kind: "typeText"; text: string }
  | { kind: "clipboard"; action: ClipboardAction }
  | { kind: "openUrl"; url: string }
  | { kind: "appleShortcut"; name: string }
  | { kind: "appleScript"; source: string }
  | { kind: "shell"; source: string }
  | { kind: "keyboardMaestro"; name: string }
  | { kind: "window"; placement: Placement }
  | { kind: "system"; control: SystemControl }
  | { kind: "notify"; text: string }
  | { kind: "speak"; text: string }
  | { kind: "delay"; ms: number };

// ---------------------------------------------------------------------------
// Commands. Each answers with a `ShortcutsView`, or rejects with a short
// sentence for the page. The page never sends a path, and no command runs a
// shortcut.
// ---------------------------------------------------------------------------

/** No args. */
export const SHORTCUTS_GET = "shortcuts_get";
/** Args `{ on }`. Refused while there is no key combination. */
export const SHORTCUTS_SET_ENABLED = "shortcuts_set_enabled";
/** Args `{ chord }`. `null` also turns the wheel off. */
export const SHORTCUTS_SET_HOTKEY = "shortcuts_set_hotkey";
/** Args `{ at }`. */
export const SHORTCUTS_SET_OPENS_AT = "shortcuts_set_opens_at";
/** No args. Shows the system's own prompt. */
export const SHORTCUTS_REQUEST_ACCESS = "shortcuts_request_access";
/** No args. Opens the app picker; cancelling it changes nothing. */
export const SHORTCUTS_ADD_MENU = "shortcuts_add_menu";
/** Args `{ menu }`. */
export const SHORTCUTS_REMOVE_MENU = "shortcuts_remove_menu";
/** Args `{ menu, on }`. */
export const SHORTCUTS_SET_MENU_EVERYWHERE = "shortcuts_set_menu_everywhere";
/** Args `{ menu, group, kind }`. Rust names the new shortcut or group. */
export const SHORTCUTS_ADD_ITEM = "shortcuts_add_item";
/** Args `{ item, name, icon }`. */
export const SHORTCUTS_SET_ITEM = "shortcuts_set_item";
/** Args `{ item }`. */
export const SHORTCUTS_REMOVE_ITEM = "shortcuts_remove_item";
/** Args `{ item, to }`. `to` is a 0-based place in the item's own ring or group. */
export const SHORTCUTS_MOVE_ITEM = "shortcuts_move_item";
/** Args `{ item, kind }`. */
export const SHORTCUTS_ADD_STEP = "shortcuts_add_step";
/** Args `{ step, value }`. `value` is a `StepEdit` of the step's own kind. */
export const SHORTCUTS_SET_STEP = "shortcuts_set_step";
/** Args `{ step }`. */
export const SHORTCUTS_REMOVE_STEP = "shortcuts_remove_step";
/** Args `{ step, to }`. */
export const SHORTCUTS_MOVE_STEP = "shortcuts_move_step";
/** Args `{ step }`. Opens the picker for an app, or for a file or folder. */
export const SHORTCUTS_PICK_TARGET = "shortcuts_pick_target";

/** A command's args, as `invoke` takes them. */
export type Args = Record<string, unknown>;

/** A chord with its fields in the order Rust writes them. */
export function chordWire(key: string, mods: Readonly<Record<ModifierId, boolean>>): ChordWire {
  return { key, shift: mods.shift, ctrl: mods.ctrl, alt: mods.alt, meta: mods.meta };
}

export function setEnabledArgs(on: boolean): Args {
  return { on };
}

export function setHotkeyArgs(chord: ChordWire | null): Args {
  return { chord };
}

export function setOpensAtArgs(at: OpensAt): Args {
  return { at };
}

export function removeMenuArgs(menu: number): Args {
  return { menu };
}

export function setMenuEverywhereArgs(menu: number, on: boolean): Args {
  return { menu, on };
}

/** `group` is the group the new shortcut goes into, or `null` for the menu's own ring. */
export function addItemArgs(menu: number, group: number | null, kind: NewItemKind): Args {
  return { menu, group, kind };
}

export function setItemArgs(item: number, name: string, icon: Icon): Args {
  return { item, name, icon };
}

export function removeItemArgs(item: number): Args {
  return { item };
}

export function moveItemArgs(item: number, to: number): Args {
  return { item, to };
}

export function addStepArgs(item: number, kind: StepKind): Args {
  return { item, kind };
}

export function setStepArgs(step: number, value: StepEdit): Args {
  return { step, value };
}

export function removeStepArgs(step: number): Args {
  return { step };
}

export function moveStepArgs(step: number, to: number): Args {
  return { step, to };
}

export function pickTargetArgs(step: number): Args {
  return { step };
}

/**
 * One builder per `StepEdit` shape. Each writes `kind` first and then its
 * value, which is the order of the `shortcuts-step-*.json` fixtures.
 */
export const stepEdit = {
  keys: (chord: ChordWire | null): StepEdit => ({ kind: "keys", chord }),
  typeText: (text: string): StepEdit => ({ kind: "typeText", text }),
  clipboard: (action: ClipboardAction): StepEdit => ({ kind: "clipboard", action }),
  openUrl: (url: string): StepEdit => ({ kind: "openUrl", url }),
  appleShortcut: (name: string): StepEdit => ({ kind: "appleShortcut", name }),
  appleScript: (source: string): StepEdit => ({ kind: "appleScript", source }),
  shell: (source: string): StepEdit => ({ kind: "shell", source }),
  keyboardMaestro: (name: string): StepEdit => ({ kind: "keyboardMaestro", name }),
  window: (placement: Placement): StepEdit => ({ kind: "window", placement }),
  system: (control: SystemControl): StepEdit => ({ kind: "system", control }),
  notify: (text: string): StepEdit => ({ kind: "notify", text }),
  speak: (text: string): StepEdit => ({ kind: "speak", text }),
  delay: (ms: number): StepEdit => ({ kind: "delay", ms }),
} as const;

// ---------------------------------------------------------------------------
// Enum values, kept next to the types so a new variant is a compile error
// here until the guard knows it.
// ---------------------------------------------------------------------------

const BINDINGS: readonly Binding[] = ["off", "bound", "failed"];
const ACCESSES: readonly Access[] = ["granted", "denied", "notNeeded"];
const OPENS_AT: readonly OpensAt[] = ["cursor", "center"];
const CLIPBOARD_ACTIONS: readonly ClipboardAction[] = ["copy", "paste", "clear"];
const PLACEMENTS: readonly Placement[] = [
  "leftHalf",
  "rightHalf",
  "topHalf",
  "bottomHalf",
  "fill",
  "center",
  "topLeft",
  "topRight",
  "bottomLeft",
  "bottomRight",
];
const SYSTEM_CONTROLS: readonly SystemControl[] = [
  "sleepDisplay",
  "lockScreen",
  "screensaver",
  "missionControl",
  "toggleMute",
  "volumeUp",
  "volumeDown",
  "toggleDarkMode",
  "screenshotArea",
];

/** The four modifiers, in the order the page lists them. */
export const MODIFIER_IDS: readonly ModifierId[] = ["shift", "ctrl", "alt", "meta"];

/** Every step kind, in the order "Add a step" offers them (Rust's `StepKindId::ALL`). */
export const STEP_KINDS: readonly StepKind[] = [
  "keys",
  "typeText",
  "clipboard",
  "openApp",
  "openFile",
  "openUrl",
  "appleShortcut",
  "appleScript",
  "shell",
  "keyboardMaestro",
  "window",
  "system",
  "notify",
  "speak",
  "delay",
];

/** True for a step kind, e.g. the "Add a step" select's value before it's sent to Rust. */
export function isStepKind(value: unknown): value is StepKind {
  return isOneOf(value, STEP_KINDS);
}

/** True for a place the wheel can open, e.g. a select's value before it's sent to Rust. */
export function isOpensAt(value: unknown): value is OpensAt {
  return isOneOf(value, OPENS_AT);
}

// ---------------------------------------------------------------------------
// Guard
// ---------------------------------------------------------------------------

/** What the guard carries down the tree: the ids chords and glyph icons may use, and the ids seen so far. */
interface Seen {
  readonly keys: ReadonlySet<string>;
  readonly glyphs: ReadonlySet<string>;
  readonly ids: Set<number>;
}

/** A name Rust chose, as the page shows it. Never empty. */
function isName(value: unknown): value is string {
  return typeof value === "string" && value.length > 0;
}

/** Something the user typed or picked. Its type is all the page checks. */
function isText(value: unknown): value is string {
  return typeof value === "string";
}

function isCount(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

/** One of Rust's ids, not seen before in this view: the page builds element ids from them. */
function isFreshId(value: unknown, seen: Seen): value is number {
  if (!isCount(value) || value === 0 || seen.ids.has(value)) {
    return false;
  }
  seen.ids.add(value);
  return true;
}

/** Strings, each at most once: the options of one select. */
function isIdList(value: unknown): value is string[] {
  return Array.isArray(value) && value.every(isName) && new Set(value).size === value.length;
}

function isKeyView(value: unknown): value is KeyView {
  return hasExactKeys(value, ["id", "label"]) && isName(value["id"]) && isName(value["label"]);
}

function isKeyList(value: unknown): value is KeyView[] {
  return Array.isArray(value) && value.every(isKeyView) && isIdList(value.map((key) => key.id));
}

function isChord(value: unknown, seen: Seen): value is ChordWire {
  if (!hasExactKeys(value, ["key", "shift", "ctrl", "alt", "meta"])) {
    return false;
  }
  const key = value["key"];
  return (
    typeof key === "string" &&
    seen.keys.has(key) &&
    MODIFIER_IDS.every((modifier) => typeof value[modifier] === "boolean")
  );
}

function isHotkey(value: unknown, seen: Seen): value is HotkeyView {
  return hasExactKeys(value, ["chord", "label"]) && isChord(value["chord"], seen) && isText(value["label"]);
}

function isLimits(value: unknown): value is Limits {
  const names = [
    "items",
    "steps",
    "name",
    "icon",
    "text",
    "script",
    "url",
    "menus",
    "delayMinMs",
    "delayMaxMs",
  ] as const;
  if (!hasExactKeys(value, names)) {
    return false;
  }
  const min = value["delayMinMs"];
  const max = value["delayMaxMs"];
  const positive = names.every((name) => {
    const limit = value[name];
    return isCount(limit) && limit > 0;
  });
  return positive && isCount(min) && isCount(max) && min <= max;
}

function isModifierNames(value: unknown): value is ModifierNames {
  return hasExactKeys(value, MODIFIER_IDS) && MODIFIER_IDS.every((modifier) => isName(value[modifier]));
}

function isIcon(value: unknown, seen: Seen): value is Icon {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  switch ((value as Fields)["kind"]) {
    case "none":
      return hasExactKeys(value, ["kind"]);
    case "emoji":
      return hasExactKeys(value, ["kind", "value"]) && isText(value["value"]);
    case "glyph": {
      if (!hasExactKeys(value, ["kind", "value"])) {
        return false;
      }
      const id = value["value"];
      return typeof id === "string" && seen.glyphs.has(id);
    }
    default:
      return false;
  }
}

function isScope(value: unknown): value is Scope {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  switch ((value as Fields)["kind"]) {
    case "everywhere":
      return hasExactKeys(value, ["kind"]);
    case "app":
      return hasExactKeys(value, ["kind", "name"]) && isText(value["name"]);
    default:
      return false;
  }
}

function isTextOrNull(value: unknown): value is string | null {
  return value === null || typeof value === "string";
}

function isStep(value: unknown, seen: Seen): value is StepView {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return false;
  }
  if (!isFreshId((value as Fields)["id"], seen)) {
    return false;
  }
  switch ((value as Fields)["kind"]) {
    case "keys": {
      if (!hasExactKeys(value, ["id", "kind", "chord", "label"])) {
        return false;
      }
      // A chord and its label come together, or not at all.
      return value["chord"] === null
        ? value["label"] === null
        : isChord(value["chord"], seen) && isText(value["label"]);
    }
    case "typeText":
    case "notify":
    case "speak":
      return hasExactKeys(value, ["id", "kind", "text"]) && isText(value["text"]);
    case "clipboard":
      return hasExactKeys(value, ["id", "kind", "action"]) && isOneOf(value["action"], CLIPBOARD_ACTIONS);
    case "openApp":
      return hasExactKeys(value, ["id", "kind", "name"]) && isTextOrNull(value["name"]);
    case "openFile":
      return (
        hasExactKeys(value, ["id", "kind", "name", "folder"]) &&
        isTextOrNull(value["name"]) &&
        typeof value["folder"] === "boolean"
      );
    case "openUrl":
      return hasExactKeys(value, ["id", "kind", "url"]) && isText(value["url"]);
    case "appleShortcut":
    case "keyboardMaestro":
      return hasExactKeys(value, ["id", "kind", "name"]) && isText(value["name"]);
    case "appleScript":
    case "shell":
      return hasExactKeys(value, ["id", "kind", "source"]) && isText(value["source"]);
    case "window":
      return hasExactKeys(value, ["id", "kind", "placement"]) && isOneOf(value["placement"], PLACEMENTS);
    case "system":
      return hasExactKeys(value, ["id", "kind", "control"]) && isOneOf(value["control"], SYSTEM_CONTROLS);
    case "delay":
      return hasExactKeys(value, ["id", "kind", "ms"]) && isCount(value["ms"]);
    default:
      return false;
  }
}

function isList<T>(value: unknown, isItem: (item: unknown) => item is T): value is T[] {
  return Array.isArray(value) && value.every(isItem);
}

/** A shortcut or a group. `inGroup` refuses a group: there are two levels only. */
function isItem(value: unknown, seen: Seen, inGroup: boolean): value is ItemView {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return false;
  }
  const kind = (value as Fields)["kind"];
  if (kind === "steps") {
    if (!hasExactKeys(value, ["id", "name", "icon", "kind", "steps"])) {
      return false;
    }
  } else if (kind === "group" && !inGroup) {
    if (!hasExactKeys(value, ["id", "name", "icon", "kind", "items"])) {
      return false;
    }
  } else {
    return false;
  }
  if (!isFreshId(value["id"], seen) || !isText(value["name"]) || !isIcon(value["icon"], seen)) {
    return false;
  }
  return kind === "steps"
    ? isList(value["steps"], (step): step is StepView => isStep(step, seen))
    : isList(value["items"], (child): child is ItemView => isItem(child, seen, true));
}

function isMenu(value: unknown, seen: Seen): value is MenuView {
  return (
    hasExactKeys(value, ["id", "scope", "withEverywhere", "items"]) &&
    isFreshId(value["id"], seen) &&
    isScope(value["scope"]) &&
    typeof value["withEverywhere"] === "boolean" &&
    isList(value["items"], (item): item is ItemView => isItem(item, seen, false))
  );
}

/**
 * Guards a `ShortcutsView`. Besides the types: every id is used once (the
 * page builds element ids from them), a chord's key is one of `keys` and a
 * glyph icon one of `glyphs` (so every select can show its value), a group
 * holds no group, and one menu is Everywhere, never two (where the wheel
 * isn't available there may be none: the page draws no menu then). Text the
 * user typed or picked is checked for its type only, never its length: Rust
 * keeps the limits.
 */
export function isShortcutsView(value: unknown): value is ShortcutsView {
  if (
    !hasExactKeys(value, [
      "available",
      "enabled",
      "hotkey",
      "binding",
      "access",
      "opensAt",
      "note",
      "limits",
      "keys",
      "modifiers",
      "glyphs",
      "menus",
    ])
  ) {
    return false;
  }
  const keys = value["keys"];
  const glyphs = value["glyphs"];
  if (!isKeyList(keys) || !isIdList(glyphs)) {
    return false;
  }
  const seen: Seen = {
    keys: new Set(keys.map((key) => key.id)),
    glyphs: new Set(glyphs),
    ids: new Set(),
  };
  const available = value["available"];
  const hotkey = value["hotkey"];
  const menus = value["menus"];
  if (typeof available !== "boolean" || !isList(menus, (menu): menu is MenuView => isMenu(menu, seen))) {
    return false;
  }
  const everywhere = menus.filter((menu) => menu.scope.kind === "everywhere").length;
  return (
    (everywhere === 1 || (everywhere === 0 && !available)) &&
    typeof value["enabled"] === "boolean" &&
    (hotkey === null || isHotkey(hotkey, seen)) &&
    isOneOf(value["binding"], BINDINGS) &&
    isOneOf(value["access"], ACCESSES) &&
    isOneOf(value["opensAt"], OPENS_AT) &&
    isTextOrNull(value["note"]) &&
    isLimits(value["limits"]) &&
    isModifierNames(value["modifiers"])
  );
}

// ---------------------------------------------------------------------------
// Copy (Grape's own words)
// ---------------------------------------------------------------------------

/** Shown when a command fails without a sentence of its own, or answers with something unreadable. */
export const SHORTCUTS_ERROR = "Something went wrong with your shortcuts. Close Settings and open it again.";

/** The text to show for a rejected command: Rust's own sentence, or `SHORTCUTS_ERROR`. */
export function shortcutsErrorText(reason: unknown): string {
  return typeof reason === "string" && reason.trim().length > 0 ? reason : SHORTCUTS_ERROR;
}

/** Every fixed sentence and label of the section. */
export const SHORTCUTS_COPY = {
  title: "Shortcuts",
  lead:
    "Press a key combination in any app and a wheel of your own shortcuts opens. " +
    "A shortcut can press keys, type text, open things or run a script.",
  /** The whole section where the wheel isn't built yet. */
  unavailable: "The Shortcuts wheel is on the Mac first. It isn't on this computer yet.",
  plainText: "Shortcuts are saved as plain text on this Mac. Don't put passwords in them.",
  enabled: "Shortcuts wheel",
  enabledDetail: "Opens from any app with the key combination below.",
  hotkey: "Key combination",
  key: "Key",
  noKey: "None",
  notSet: "Not set",
  opensAt: "The wheel opens",
  accessDenied:
    "Grape needs Accessibility access to press keys, type and move windows. Turn it on in System Settings.",
  /** macOS ties the grant to one build, so an update can leave a dead entry in the list. */
  accessRepair: "If Grape is already in that list, remove it with − and add it again.",
  accessGranted: "Grape has Accessibility access.",
  askAccess: "Ask for access",
  askAccessLabel: "Ask for access to press keys",
  menus: "Menus",
  menusHint: "Everywhere shows in every app. An app with a menu of its own shows that one instead.",
  menu: "Menu",
  everywhere: "Everywhere",
  addMenu: "Add an app…",
  addMenuLabel: "Add an app and give it a menu",
  removeMenu: "Remove menu",
  withEverywhere: "Also show Everywhere",
  withEverywhereDetail: "Adds your Everywhere shortcuts to this menu as one more group.",
  name: "Name",
  namePlaceholder: "Name",
  icon: "Icon",
  noIcon: "No icon",
  emojiChoice: "Emoji…",
  emoji: "Emoji",
  emojiPlaceholder: "One emoji",
  steps: "Steps",
  addStep: "Add a step",
  addStepPrompt: "Choose a kind…",
  addShortcut: "Add shortcut",
  addGroup: "Add group",
  moveUp: "Move up",
  moveDown: "Move down",
  remove: "Remove",
  choose: "Choose…",
  noSteps: "No steps yet",
  emptyGroup: "Empty group",
  emptyRing: "No shortcuts here yet.",
} as const;

/** A select's option. */
export interface Choice<T extends string = string> {
  readonly value: T;
  readonly label: string;
}

/** Where the wheel can open, in page order. */
export const OPENS_AT_CHOICES: readonly Choice<OpensAt>[] = [
  { value: "cursor", label: "At the pointer" },
  { value: "center", label: "In the middle of the screen" },
];

const CLIPBOARD_CHOICES: readonly Choice<ClipboardAction>[] = [
  { value: "copy", label: "Copy" },
  { value: "paste", label: "Paste" },
  { value: "clear", label: "Clear the clipboard" },
];

const PLACEMENT_CHOICES: readonly Choice<Placement>[] = [
  { value: "leftHalf", label: "Left half" },
  { value: "rightHalf", label: "Right half" },
  { value: "topHalf", label: "Top half" },
  { value: "bottomHalf", label: "Bottom half" },
  { value: "fill", label: "Fill the screen" },
  { value: "center", label: "Middle of the screen" },
  { value: "topLeft", label: "Top left quarter" },
  { value: "topRight", label: "Top right quarter" },
  { value: "bottomLeft", label: "Bottom left quarter" },
  { value: "bottomRight", label: "Bottom right quarter" },
];

const SYSTEM_CHOICES: readonly Choice<SystemControl>[] = [
  { value: "sleepDisplay", label: "Sleep the display" },
  { value: "lockScreen", label: "Lock the screen" },
  { value: "screensaver", label: "Start the screen saver" },
  { value: "missionControl", label: "Mission Control" },
  { value: "toggleMute", label: "Mute or unmute" },
  { value: "volumeUp", label: "Volume up" },
  { value: "volumeDown", label: "Volume down" },
  { value: "toggleDarkMode", label: "Switch between light and dark" },
  { value: "screenshotArea", label: "Screenshot an area" },
];

/** What "Add a step" and a step's row call each kind. */
export const STEP_KIND_NAMES: Readonly<Record<StepKind, string>> = {
  keys: "Press keys",
  typeText: "Type text",
  clipboard: "Clipboard",
  openApp: "Open an app",
  openFile: "Open a file or folder",
  openUrl: "Open a link",
  appleShortcut: "Run an Apple Shortcut",
  appleScript: "Run AppleScript",
  shell: "Run a shell script",
  keyboardMaestro: "Run a Keyboard Maestro macro",
  window: "Move the window",
  system: "System",
  notify: "Show a notice",
  speak: "Speak",
  delay: "Wait",
};

/** The glyph that goes with each step kind, drawn next to the step's name. */
export const STEP_GLYPHS: Readonly<Record<StepKind, string>> = {
  keys: "key",
  typeText: "text",
  clipboard: "clipboard",
  openApp: "app",
  openFile: "file",
  openUrl: "link",
  appleShortcut: "bolt",
  appleScript: "script",
  shell: "terminal",
  keyboardMaestro: "bolt",
  window: "window",
  system: "gear",
  notify: "bell",
  speak: "speaker",
  delay: "clock",
};

/** The binding state line under the key combination. */
export function bindingText(view: ShortcutsView): string {
  if (view.hotkey === null) {
    return "Choose a key to turn the wheel on.";
  }
  switch (view.binding) {
    case "bound":
      return `${view.hotkey.label} opens the wheel.`;
    case "failed":
      return "macOS wouldn't give Grape that shortcut. Try another.";
    case "off":
      return view.enabled ? "The wheel isn't listening right now." : "The wheel is off.";
  }
}

/** A menu's name: "Everywhere", or its app's. */
export function menuName(scope: Scope): string {
  return scope.kind === "everywhere" ? SHORTCUTS_COPY.everywhere : scope.name;
}

/** The heading over a menu's shortcuts. */
export function ringTitle(scope: Scope): string {
  return scope.kind === "everywhere" ? "Shortcuts everywhere" : `Shortcuts in ${scope.name}`;
}

/** Why "Add shortcut" and "Add group" are off: the ring or group is full. */
export function ringFullText(limit: number): string {
  return `That's ${limit} shortcuts, the most one ring holds.`;
}

/** Why "Add a step" is off. */
export function stepsFullText(limit: number): string {
  return `That's ${limit} steps, the most one shortcut holds.`;
}

/** Why "Add an app…" is off. */
export function menusFullText(limit: number): string {
  return `That's ${limit} menus, the most Grape keeps.`;
}

// ---------------------------------------------------------------------------
// Icons
// ---------------------------------------------------------------------------

/** The icon select's value for no icon. */
export const ICON_NONE = "none";
/** The icon select's value for text the user types. */
export const ICON_EMOJI = "emoji";
const GLYPH_PREFIX = "glyph:";

const GLYPH_NAMES: Readonly<Record<string, string>> = {
  key: "Key",
  text: "Letter T",
  clipboard: "Clipboard",
  app: "App",
  file: "File",
  folder: "Folder",
  link: "Link",
  bolt: "Bolt",
  script: "Script",
  terminal: "Terminal",
  window: "Window",
  gear: "Gear",
  bell: "Bell",
  speaker: "Speaker",
  clock: "Clock",
  stack: "Stack",
  star: "Star",
  heart: "Heart",
  home: "House",
  search: "Magnifier",
  mail: "Envelope",
  music: "Music note",
  camera: "Camera",
  lock: "Padlock",
};

/** A glyph's name in the icon select. An id this build has no name for shows capitalized. */
export function glyphName(id: string): string {
  if (Object.hasOwn(GLYPH_NAMES, id)) {
    return GLYPH_NAMES[id] ?? id;
  }
  return id.slice(0, 1).toUpperCase() + id.slice(1);
}

/** The icon select's choices: no icon, typed text, then each glyph Rust offers. */
export function iconChoices(glyphs: readonly string[]): Choice[] {
  return [
    { value: ICON_NONE, label: SHORTCUTS_COPY.noIcon },
    { value: ICON_EMOJI, label: SHORTCUTS_COPY.emojiChoice },
    ...glyphs.map((id) => ({ value: `${GLYPH_PREFIX}${id}`, label: glyphName(id) })),
  ];
}

/** The icon select's value for `icon`. */
export function iconChoice(icon: Icon): string {
  switch (icon.kind) {
    case "none":
      return ICON_NONE;
    case "emoji":
      return ICON_EMOJI;
    case "glyph":
      return `${GLYPH_PREFIX}${icon.value}`;
  }
}

/**
 * The icon a choice stands for. `null` when it stands for none yet: Emoji
 * with no text typed, or a value the select doesn't offer.
 */
export function iconFromChoice(choice: string, emoji: string, glyphs: readonly string[]): Icon | null {
  if (choice === ICON_NONE) {
    return { kind: "none" };
  }
  if (choice === ICON_EMOJI) {
    return emoji.length > 0 ? { kind: "emoji", value: emoji } : null;
  }
  const id = choice.startsWith(GLYPH_PREFIX) ? choice.slice(GLYPH_PREFIX.length) : null;
  return id !== null && glyphs.includes(id) ? { kind: "glyph", value: id } : null;
}

// ---------------------------------------------------------------------------
// Steps: which kind has which field
// ---------------------------------------------------------------------------

/** Which of the view's limits a text field's `maxlength` comes from. */
export type TextLimit = "text" | "script" | "url";

/** The one thing a step of some kind lets the user set, and how the page draws it. */
export type StepField =
  | { readonly control: "chord"; readonly label: string }
  | {
      readonly control: "text";
      /** The field's name in the step, and the tail of the control's id. */
      readonly key: "text" | "url" | "name" | "source";
      readonly label: string;
      readonly placeholder: string;
      /** A line under the field, or `null`. */
      readonly hint: string | null;
      /** 1 for a one-line field; more for a text area of that height. */
      readonly lines: number;
      readonly limit: TextLimit;
      /** False for scripts and links: no spelling marks. */
      readonly prose: boolean;
      read(step: StepView): string;
      edit(value: string): StepEdit;
    }
  | {
      readonly control: "choice";
      readonly key: "action" | "placement" | "control";
      readonly label: string;
      readonly choices: readonly Choice[];
      read(step: StepView): string;
      /** `null` for a value the select doesn't offer. */
      edit(value: string): StepEdit | null;
    }
  | {
      readonly control: "number";
      readonly key: "ms";
      readonly label: string;
      read(step: StepView): string;
      /** `null` for text that isn't a whole number Rust could read. */
      edit(value: string): StepEdit | null;
    }
  | {
      readonly control: "pick";
      /** The button's accessible name. It contains "Choose". */
      readonly label: string;
      /** Shown until something is picked. */
      readonly unset: string;
      read(step: StepView): string | null;
    };

/** The largest whole number a delay can be sent as (Rust reads a `u32`). */
const MAX_U32 = 4_294_967_295;

/** A delay's edit from what the field holds: digits only, so Rust can always read it. */
export function delayEdit(text: string): StepEdit | null {
  const trimmed = text.trim();
  if (!/^\d{1,10}$/.test(trimmed)) {
    return null;
  }
  const ms = Number(trimmed);
  return ms <= MAX_U32 ? stepEdit.delay(ms) : null;
}

function choiceEdit<T extends string>(
  choices: readonly Choice<T>[],
  build: (value: T) => StepEdit,
): (value: string) => StepEdit | null {
  return (value) => {
    const found = choices.find((choice) => choice.value === value);
    return found === undefined ? null : build(found.value);
  };
}

/** Each kind's field. A kind has exactly one. */
export const STEP_FIELDS: Readonly<Record<StepKind, StepField>> = {
  keys: { control: "chord", label: "Keys to press" },
  typeText: {
    control: "text",
    key: "text",
    label: "Text to type",
    placeholder: "Sent on {date}",
    hint: "{date}, {time} and {clipboard} are filled in when it runs.",
    lines: 3,
    limit: "text",
    prose: true,
    read: (step) => (step.kind === "typeText" ? step.text : ""),
    edit: stepEdit.typeText,
  },
  clipboard: {
    control: "choice",
    key: "action",
    label: "What to do",
    choices: CLIPBOARD_CHOICES,
    read: (step) => (step.kind === "clipboard" ? step.action : ""),
    edit: choiceEdit(CLIPBOARD_CHOICES, stepEdit.clipboard),
  },
  openApp: {
    control: "pick",
    label: "Choose the app to open",
    unset: "No app chosen yet",
    read: (step) => (step.kind === "openApp" ? step.name : null),
  },
  openFile: {
    control: "pick",
    label: "Choose the file or folder to open",
    unset: "Nothing chosen yet",
    read: (step) => (step.kind === "openFile" ? step.name : null),
  },
  openUrl: {
    control: "text",
    key: "url",
    label: "Link",
    placeholder: "https://example.com",
    hint: null,
    lines: 1,
    limit: "url",
    prose: false,
    read: (step) => (step.kind === "openUrl" ? step.url : ""),
    edit: stepEdit.openUrl,
  },
  appleShortcut: {
    control: "text",
    key: "name",
    label: "The Shortcut's name",
    placeholder: "As the Shortcuts app shows it",
    hint: null,
    lines: 1,
    limit: "text",
    prose: true,
    read: (step) => (step.kind === "appleShortcut" ? step.name : ""),
    edit: stepEdit.appleShortcut,
  },
  appleScript: {
    control: "text",
    key: "source",
    label: "Script",
    placeholder: 'display dialog "Hello"',
    hint: null,
    lines: 4,
    limit: "script",
    prose: false,
    read: (step) => (step.kind === "appleScript" ? step.source : ""),
    edit: stepEdit.appleScript,
  },
  shell: {
    control: "text",
    key: "source",
    label: "Script",
    placeholder: "echo hello",
    hint: "Runs in zsh, in your home folder.",
    lines: 4,
    limit: "script",
    prose: false,
    read: (step) => (step.kind === "shell" ? step.source : ""),
    edit: stepEdit.shell,
  },
  keyboardMaestro: {
    control: "text",
    key: "name",
    label: "The macro's name",
    placeholder: "As Keyboard Maestro shows it",
    hint: null,
    lines: 1,
    limit: "text",
    prose: true,
    read: (step) => (step.kind === "keyboardMaestro" ? step.name : ""),
    edit: stepEdit.keyboardMaestro,
  },
  window: {
    control: "choice",
    key: "placement",
    label: "Where it goes",
    choices: PLACEMENT_CHOICES,
    read: (step) => (step.kind === "window" ? step.placement : ""),
    edit: choiceEdit(PLACEMENT_CHOICES, stepEdit.window),
  },
  system: {
    control: "choice",
    key: "control",
    label: "What to do",
    choices: SYSTEM_CHOICES,
    read: (step) => (step.kind === "system" ? step.control : ""),
    edit: choiceEdit(SYSTEM_CHOICES, stepEdit.system),
  },
  notify: {
    control: "text",
    key: "text",
    label: "What the notice says",
    placeholder: "Time for a break",
    hint: null,
    lines: 2,
    limit: "text",
    prose: true,
    read: (step) => (step.kind === "notify" ? step.text : ""),
    edit: stepEdit.notify,
  },
  speak: {
    control: "text",
    key: "text",
    label: "What to say",
    placeholder: "Time for a break",
    hint: null,
    lines: 2,
    limit: "text",
    prose: true,
    read: (step) => (step.kind === "speak" ? step.text : ""),
    edit: stepEdit.speak,
  },
  delay: {
    control: "number",
    key: "ms",
    label: "Milliseconds",
    read: (step) => (step.kind === "delay" ? String(step.ms) : ""),
    edit: delayEdit,
  },
};

// ---------------------------------------------------------------------------
// Helpers the page draws with
// ---------------------------------------------------------------------------

function choiceLabel<T extends string>(choices: readonly Choice<T>[], value: T): string {
  return choices.find((choice) => choice.value === value)?.label ?? value;
}

/** What one step does, in a few words. Typed text, scripts and links aren't repeated here. */
export function stepPhrase(step: StepView): string {
  switch (step.kind) {
    case "keys":
      return step.label === null ? STEP_KIND_NAMES.keys : `Press ${step.label}`;
    case "clipboard":
      return choiceLabel(CLIPBOARD_CHOICES, step.action);
    case "openApp":
    case "openFile":
      return step.name === null ? STEP_KIND_NAMES[step.kind] : `Open ${step.name}`;
    case "window":
      return `Window: ${choiceLabel(PLACEMENT_CHOICES, step.placement)}`;
    case "system":
      return choiceLabel(SYSTEM_CHOICES, step.control);
    case "delay":
      return `Wait ${step.ms} ms`;
    default:
      return STEP_KIND_NAMES[step.kind];
  }
}

/** How many step phrases a row's summary spells out before it counts the rest. */
const SUMMARY_STEPS = 2;

/** One line about a shortcut's steps, or a group's size, for its row. */
export function itemSummary(item: ItemView): string {
  if (item.kind === "group") {
    const count = item.items.length;
    if (count === 0) {
      return SHORTCUTS_COPY.emptyGroup;
    }
    return count === 1 ? "Group of 1 shortcut" : `Group of ${count} shortcuts`;
  }
  if (item.steps.length === 0) {
    return SHORTCUTS_COPY.noSteps;
  }
  const phrases = item.steps.slice(0, SUMMARY_STEPS).map(stepPhrase);
  const more = item.steps.length - phrases.length;
  return more > 0 ? `${phrases.join(", then ")} and ${more} more` : phrases.join(", then ");
}

/** The menus in the picker's order: Everywhere first, then the apps as Rust lists them. */
export function menuChoices(view: ShortcutsView): MenuView[] {
  return [
    ...view.menus.filter((menu) => menu.scope.kind === "everywhere"),
    ...view.menus.filter((menu) => menu.scope.kind !== "everywhere"),
  ];
}

/** The menu with this id, or Everywhere when there is none (it was removed, or none is picked yet). */
export function menuOrEverywhere(view: ShortcutsView, id: number | null): MenuView | null {
  return view.menus.find((menu) => menu.id === id) ?? menuChoices(view)[0] ?? null;
}

/** Where a shortcut or group sits: its menu, its group if it's in one, and its place there. */
export interface ItemPlace {
  readonly menu: MenuView;
  readonly group: GroupItem | null;
  readonly item: ItemView;
  readonly index: number;
  /** How many share its ring or group. */
  readonly count: number;
}

/** Finds the shortcut or group with this id. */
export function findItem(view: ShortcutsView, id: number): ItemPlace | null {
  for (const menu of view.menus) {
    for (const [index, item] of menu.items.entries()) {
      if (item.id === id) {
        return { menu, group: null, item, index, count: menu.items.length };
      }
      if (item.kind === "group") {
        const at = item.items.findIndex((child) => child.id === id);
        const child = item.items[at];
        if (child !== undefined) {
          return { menu, group: item, item: child, index: at, count: item.items.length };
        }
      }
    }
  }
  return null;
}

/** Where a step sits: its shortcut and its place in it. */
export interface StepPlace {
  readonly item: StepsItem;
  readonly step: StepView;
  readonly index: number;
  readonly count: number;
}

/** Finds the step with this id. */
export function findStep(view: ShortcutsView, id: number): StepPlace | null {
  for (const menu of view.menus) {
    for (const top of menu.items) {
      for (const item of top.kind === "group" ? top.items : [top]) {
        const index = item.steps.findIndex((step) => step.id === id);
        const step = item.steps[index];
        if (step !== undefined) {
          return { item, step, index, count: item.steps.length };
        }
      }
    }
  }
  return null;
}

/** Every menu, shortcut, group and step id in the view. */
export function allIds(view: ShortcutsView): Set<number> {
  const ids = new Set<number>();
  const addItem = (item: ItemView): void => {
    ids.add(item.id);
    if (item.kind === "group") {
      item.items.forEach(addItem);
    } else {
      for (const step of item.steps) {
        ids.add(step.id);
      }
    }
  };
  for (const menu of view.menus) {
    ids.add(menu.id);
    menu.items.forEach(addItem);
  }
  return ids;
}

/** A stable string for a list of choices, so the page rebuilds a select's options only when they change. */
export function choicesSignature(choices: readonly (readonly [string, string])[]): string {
  return JSON.stringify(choices);
}
