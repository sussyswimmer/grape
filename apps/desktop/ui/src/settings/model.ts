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
}
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

// ---------------------------------------------------------------------------
// Enum values, kept next to the types so a new variant is a compile error
// here until the guard knows it.
// ---------------------------------------------------------------------------

const PLATFORMS: readonly Platform[] = ["windows", "macos"];
const KEY_IDS: readonly KeyId[] = ["shift", "ctrl", "alt", "meta"];

/** The wheels, in the order the page lists them. */
export const WHEELS: readonly Wheel[] = ["conversions", "tools", "actions"];

// ---------------------------------------------------------------------------
// Guard
// ---------------------------------------------------------------------------

function isKeyChoice(value: unknown): value is KeyChoice {
  return (
    hasExactKeys(value, ["key", "name"]) &&
    isOneOf(value["key"], KEY_IDS) &&
    typeof value["name"] === "string" &&
    value["name"].length > 0
  );
}

/** At least one choice, each key at most once. Any order. */
function isKeyChoices(value: unknown): value is KeyChoice[] {
  if (!Array.isArray(value) || value.length === 0 || !value.every(isKeyChoice)) {
    return false;
  }
  const keys = new Set(value.map((choice: KeyChoice) => choice.key));
  return keys.size === value.length;
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
 * be one of `keyChoices`, so every select can show its key.
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
    ])
  ) {
    return false;
  }
  const keys = value["keys"];
  const choices = value["keyChoices"];
  if (!isKeyMap(keys) || !isKeyChoices(choices)) {
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
    (value["note"] === null || typeof value["note"] === "string")
  );
}

/** True for a key id, e.g. a `<select>`'s value before it's sent to Rust. */
export function isKeyId(value: unknown): value is KeyId {
  return isOneOf(value, KEY_IDS);
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
