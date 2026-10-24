/**
 * The Settings contract (M6). Rust owns the settings: it stores them,
 * applies the key-swap rule and opens the folder pickers. Each `settings_*`
 * command answers with the whole `SettingsView`, and the page draws exactly
 * that. The types mirror `apps/desktop/src-tauri/src/settings.rs`, and both
 * sides test against `src/shared/fixtures/settings-view-*.json`.
 *
 * A view arrives as `unknown` and goes through `isSettingsView` before the
 * page touches it. The helpers below are pure: no DOM.
 */

import { type Fields, hasExactKeys, isOneOf } from "../shared/guards";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export type Platform = "windows" | "macos";
export type KeyId = "shift" | "ctrl" | "alt" | "meta";
export type Wheel = "conversions" | "tools" | "actions";
export type KeyMap = Record<Wheel, KeyId>;
export interface KeyChoice { key: KeyId; name: string }
export type OutputView = { kind: "nextToSource" } | { kind: "folder"; path: string };
export interface FallbackView { path: string; isDefault: boolean }
export interface SettingsView {
  platform: Platform;
  keys: KeyMap;
  keyChoices: KeyChoice[];
  output: OutputView;
  fallbackDir: FallbackView;
  sounds: boolean;
  /** `null` where there are no haptics (Windows); the page hides the row. */
  haptics: boolean | null;
  /** `null` when Grape couldn't read the OS setting. */
  launchAtLogin: boolean | null;
  /** Set only right after a key swap, e.g. "Ctrl now opens Conversions, and Shift opens Tools." */
  note: string | null;
  /** The app the Terminal action opens, and the ones this OS offers, in page order. */
  terminal: TerminalView;
  /** True until the user dismisses the welcome card with "Got it". */
  welcome: boolean;
}
/** An app the Terminal action can open. Each OS offers two. */
export type TerminalId = "windowsTerminal" | "powershell" | "terminal" | "iterm2";
export interface TerminalChoice { id: TerminalId; name: string }
export interface TerminalView { choice: TerminalId; choices: TerminalChoice[] }
export type FolderTarget = "output" | "fallback";
export type ToggleName = "sounds" | "haptics" | "launchAtLogin";

// ---------------------------------------------------------------------------
// Commands. Each answers with a `SettingsView`, or rejects with a short
// sentence for the page. The page never sends a path: Rust opens the pickers.
// ---------------------------------------------------------------------------

/** No args. */
export const SETTINGS_GET = "settings_get";
/** Args `{ wheel, key }`. Taking another wheel's key swaps the two. */
export const SETTINGS_SET_KEY = "settings_set_key";
/** No args. */
export const SETTINGS_SET_OUTPUT_NEXT_TO_SOURCE = "settings_set_output_next_to_source";
/** Args `{ target }`. Opens the folder picker; cancelling it changes nothing. */
export const SETTINGS_PICK_FOLDER = "settings_pick_folder";
/** No args. */
export const SETTINGS_USE_DEFAULT_FALLBACK = "settings_use_default_fallback";
/** Args `{ name, on }`. */
export const SETTINGS_SET_TOGGLE = "settings_set_toggle";
/** Args `{ terminal }`. An app this OS doesn't offer is refused. */
export const SETTINGS_SET_TERMINAL = "settings_set_terminal";
/** No args. Hides the welcome card for good: Settings no longer opens at launch. */
export const SETTINGS_DISMISS_WELCOME = "settings_dismiss_welcome";

// ---------------------------------------------------------------------------
// Enum values, kept next to the types so a new variant is a compile error
// here until the guard knows it.
// ---------------------------------------------------------------------------

const PLATFORMS: readonly Platform[] = ["windows", "macos"];
const KEY_IDS: readonly KeyId[] = ["shift", "ctrl", "alt", "meta"];
const TERMINAL_IDS: readonly TerminalId[] = ["windowsTerminal", "powershell", "terminal", "iterm2"];

/** The wheels, in the order the page lists them. */
export const WHEELS: readonly Wheel[] = ["conversions", "tools", "actions"];

// ---------------------------------------------------------------------------
// Guard
// ---------------------------------------------------------------------------

/** A choice's name, as the page shows it. Never empty. */
function isName(value: unknown): value is string {
  return typeof value === "string" && value.length > 0;
}

/** At least one choice, each `id` at most once. Any order. */
function isChoiceList<C>(
  value: unknown,
  isChoice: (item: unknown) => item is C,
  id: (choice: C) => string,
): value is C[] {
  if (!Array.isArray(value) || value.length === 0 || !value.every(isChoice)) {
    return false;
  }
  const ids = new Set(value.map(id));
  return ids.size === value.length;
}

function isKeyChoice(value: unknown): value is KeyChoice {
  return hasExactKeys(value, ["key", "name"]) && isOneOf(value["key"], KEY_IDS) && isName(value["name"]);
}

function isTerminalChoice(value: unknown): value is TerminalChoice {
  return hasExactKeys(value, ["id", "name"]) && isOneOf(value["id"], TERMINAL_IDS) && isName(value["name"]);
}

/** The choices, and a current app that's one of them, so the select can show it. */
function isTerminalView(value: unknown): value is TerminalView {
  if (!hasExactKeys(value, ["choice", "choices"])) {
    return false;
  }
  const choice = value["choice"];
  const choices = value["choices"];
  return (
    isOneOf(choice, TERMINAL_IDS) &&
    isChoiceList(choices, isTerminalChoice, (item) => item.id) &&
    choices.some((item) => item.id === choice)
  );
}

function isKeyMap(value: unknown): value is KeyMap {
  return (
    hasExactKeys(value, WHEELS) &&
    isOneOf(value["conversions"], KEY_IDS) &&
    isOneOf(value["tools"], KEY_IDS) &&
    isOneOf(value["actions"], KEY_IDS)
  );
}

/** A folder Rust picked. It's never empty; the page doesn't judge it further. */
function isPath(value: unknown): value is string {
  return typeof value === "string" && value.length > 0;
}

function isOutputView(value: unknown): value is OutputView {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  switch ((value as Fields)["kind"]) {
    case "nextToSource":
      return hasExactKeys(value, ["kind"]);
    case "folder":
      return hasExactKeys(value, ["kind", "path"]) && isPath(value["path"]);
    default:
      return false;
  }
}

function isFallbackView(value: unknown): value is FallbackView {
  return (
    hasExactKeys(value, ["path", "isDefault"]) &&
    isPath(value["path"]) &&
    typeof value["isDefault"] === "boolean"
  );
}

function isBoolOrNull(value: unknown): value is boolean | null {
  return value === null || typeof value === "boolean";
}

/**
 * Guards a `SettingsView`. Besides the types, the three wheels must have
 * three different keys (Rust's swap rule keeps them apart), and each must
 * be one of `keyChoices`, so every select can show its key. The terminal
 * app must be one of its own choices for the same reason.
 */
export function isSettingsView(value: unknown): value is SettingsView {
  if (
    !hasExactKeys(value, [
      "platform",
      "keys",
      "keyChoices",
      "output",
      "fallbackDir",
      "sounds",
      "haptics",
      "launchAtLogin",
      "note",
      "terminal",
      "welcome",
    ])
  ) {
    return false;
  }
  const keys = value["keys"];
  const choices = value["keyChoices"];
  if (!isKeyMap(keys) || !isChoiceList(choices, isKeyChoice, (choice) => choice.key)) {
    return false;
  }
  const used = WHEELS.map((wheel) => keys[wheel]);
  const offered = new Set(choices.map((choice) => choice.key));
  return (
    isOneOf(value["platform"], PLATFORMS) &&
    new Set(used).size === used.length &&
    used.every((key) => offered.has(key)) &&
    isOutputView(value["output"]) &&
    isFallbackView(value["fallbackDir"]) &&
    typeof value["sounds"] === "boolean" &&
    isBoolOrNull(value["haptics"]) &&
    isBoolOrNull(value["launchAtLogin"]) &&
    (value["note"] === null || typeof value["note"] === "string") &&
    isTerminalView(value["terminal"]) &&
    typeof value["welcome"] === "boolean"
  );
}

/** True for a key id, e.g. a `<select>`'s value before it's sent to Rust. */
export function isKeyId(value: unknown): value is KeyId {
  return isOneOf(value, KEY_IDS);
}

/** True for a terminal app id, e.g. the Terminal `<select>`'s value before it's sent to Rust. */
export function isTerminalId(value: unknown): value is TerminalId {
  return isOneOf(value, TERMINAL_IDS);
}

// ---------------------------------------------------------------------------
// Copy (Grape's own words) and the pure helpers the page draws with
// ---------------------------------------------------------------------------

/** Each wheel's label next to its key select. */
export const WHEEL_LABELS: Readonly<Record<Wheel, string>> = {
  conversions: "Conversions",
  tools: "Tools",
  actions: "File actions",
};

/** Shown when a command fails without a sentence of its own, or answers with something unreadable. */
export const GENERIC_ERROR = "Something went wrong. Close Settings and open it again.";

/** The line under the header: where Grape lives, and that there's no Save button. */
export function leadText(platform: Platform): string {
  const place = platform === "macos" ? "menu bar" : "notification area";
  return `Grape runs in the ${place}. Changes apply right away.`;
}

/** The text to show for a rejected command: Rust's own sentence, or `GENERIC_ERROR`. */
export function errorText(reason: unknown): string {
  return typeof reason === "string" && reason.trim().length > 0 ? reason : GENERIC_ERROR;
}

/** One on/off row. */
export interface ToggleRow {
  readonly name: ToggleName;
  readonly label: string;
  /** The line under the label. */
  readonly detail: string;
  readonly on: boolean;
  /** False when the current state couldn't be read: the switch shows, but can't be used. */
  readonly enabled: boolean;
}

/** Every toggle the page can draw, in page order. */
export const TOGGLE_NAMES: readonly ToggleName[] = ["sounds", "haptics", "launchAtLogin"];

const TOGGLE_COPY: Readonly<Record<ToggleName, { label: string; detail: string }>> = {
  sounds: { label: "Sounds", detail: "A soft tick as you move between choices." },
  haptics: { label: "Haptics", detail: "A light tap on the trackpad as you move between choices." },
  launchAtLogin: { label: "Launch at login", detail: "Start Grape when you sign in." },
};

/** The detail line of a toggle whose state can't be read. */
export const UNKNOWN_STATE = "Couldn't check";

/**
 * The rows `view` shows, in page order: haptics only where it isn't
 * `null`. A `null` launch-at-login state shows as off and unusable.
 */
export function toggleRows(view: SettingsView): ToggleRow[] {
  const row = (name: ToggleName, state: boolean | null): ToggleRow => ({
    name,
    label: TOGGLE_COPY[name].label,
    detail: state === null ? UNKNOWN_STATE : TOGGLE_COPY[name].detail,
    on: state === true,
    enabled: state !== null,
  });
  const rows = [row("sounds", view.sounds)];
  if (view.haptics !== null) {
    rows.push(row("haptics", view.haptics));
  }
  rows.push(row("launchAtLogin", view.launchAtLogin));
  return rows;
}

/** The folder new files go to, or `null` when they go next to the original. */
export function outputFolder(view: SettingsView): string | null {
  return view.output.kind === "folder" ? view.output.path : null;
}

/** A stable string for `keyChoices`, so the page rebuilds a select's options only when they change. */
export function choicesSignature(choices: readonly KeyChoice[]): string {
  return JSON.stringify(choices.map((choice) => [choice.key, choice.name]));
}

/** The same for the Terminal select's choices. */
export function terminalSignature(choices: readonly TerminalChoice[]): string {
  return JSON.stringify(choices.map((choice) => [choice.id, choice.name]));
}

/** The File actions section's hint: what Terminal does, and what opens when the app can't. */
export function terminalHint(platform: Platform): string {
  const fallback =
    platform === "macos"
      ? "If iTerm2 isn't installed, Terminal opens instead."
      : "If Windows Terminal can't start, PowerShell opens instead.";
  return `Drop a file on Terminal to open its folder in the app you pick. ${fallback}`;
}

/**
 * The name each wheel's key goes by on this OS, e.g. "Ctrl" or "Command",
 * from `keyChoices`.
 */
export function keyNames(view: SettingsView): Record<Wheel, string> {
  const name = (wheel: Wheel): string => {
    const key = view.keys[wheel];
    // The guard makes sure every key in use is offered, so the id never shows.
    return view.keyChoices.find((choice) => choice.key === key)?.name ?? key;
  };
  return { conversions: name("conversions"), tools: name("tools"), actions: name("actions") };
}

// ---------------------------------------------------------------------------
// The welcome card: shown at the top of the page until the user dismisses
// it with "Got it" (`welcome` in the view).
// ---------------------------------------------------------------------------

/** Which illustration a welcome step shows. */
export type WelcomeArt = "drag" | "tap" | "drop";

/** One step of the welcome card. */
export interface WelcomeStep {
  readonly art: WelcomeArt;
  readonly title: string;
  /** The line under the title. */
  readonly detail: string;
}

/** The card's heading, the line under it, and its one button. */
export const WELCOME_COPY = {
  heading: "Welcome to Grape",
  intro: "Grape works while you drag a file, in three moves.",
  done: "Got it",
} as const;

/**
 * The three steps, in order, naming the user's current keys (from
 * `keyNames`) and the file manager on `platform`.
 */
export function welcomeSteps(platform: Platform, names: Readonly<Record<Wheel, string>>): WelcomeStep[] {
  const files = platform === "macos" ? "Finder" : "File Explorer";
  return [
    {
      art: "drag",
      title: "Drag a file",
      detail: `Start dragging any file in ${files} or on the desktop.`,
    },
    {
      art: "tap",
      title: `Tap ${names.conversions}`,
      detail: `${names.tools} for tools, ${names.actions} for file actions.`,
    },
    {
      art: "drop",
      title: "Drop it on a segment",
      detail: "Drop in the middle to cancel.",
    },
  ];
}
