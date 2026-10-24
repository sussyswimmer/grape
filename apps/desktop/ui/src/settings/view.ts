/**
 * The Settings page (M6): keys, where files go, the fallback folder, and
 * the on/off switches. There's no Save button. Each change is one
 * `settings_*` command, and Rust answers with the whole new view, which the
 * page draws. The page decides nothing: Rust swaps keys, opens the folder
 * pickers and stores every change.
 *
 * Commands run one at a time, in the order the user made the changes, and
 * the page redraws once the last one has answered, so a slow reply never
 * paints over a newer one. A failed command shows its message in the
 * status line, and the page asks Rust for the settings again (a failed
 * save still changed them, and launch at login may have ended up in
 * another state) and draws that, which puts the control the user changed
 * back. Only if that fails too does it redraw from the last good view.
 * The page builds its controls once and only updates them, so keyboard
 * focus survives every redraw.
 *
 * A key picked with the mouse is sent at once. A key reached by stepping
 * with the keyboard is sent only when the user settles on it (see
 * `isStepKey`), and the select keeps showing it until Rust has answered.
 */

import { type InvokeFn, tauriInvoke } from "../shared/ipc";
import {
  choicesSignature,
  errorText,
  type FolderTarget,
  GENERIC_ERROR,
  isKeyId,
  isSettingsView,
  type KeyChoice,
  type KeyId,
  leadText,
  outputFolder,
  type Platform,
  SETTINGS_GET,
  SETTINGS_PICK_FOLDER,
  SETTINGS_SET_KEY,
  SETTINGS_SET_OUTPUT_NEXT_TO_SOURCE,
  SETTINGS_SET_TOGGLE,
  SETTINGS_USE_DEFAULT_FALLBACK,
  type SettingsView,
  TOGGLE_NAMES,
  type ToggleName,
  toggleRows,
  type Wheel,
  WHEEL_LABELS,
  WHEELS,
} from "./model";

/** Keys that step a closed select from one option to the next. */
const STEP_KEYS: ReadonlySet<string> = new Set([
  "ArrowUp",
  "ArrowDown",
  "ArrowLeft",
  "ArrowRight",
  "Home",
  "End",
  "PageUp",
  "PageDown",
]);

/**
 * `true` for a key that moves a closed select's value: on Windows an arrow,
 * Home, End or a page key, and on both OSes a typed character
 * (type-ahead). Chromium fires `change` for every such step. Sending each
 * one would let the swap rule move a third wheel's key on the way, so
 * after a step a select sends only the key the user settles on: when it
 * loses focus, on Enter, or when the page goes away. No timer: a user
 * reading each option aloud with a screen reader takes as long as they
 * like.
 *
 * Keys that open the list instead aren't steps: Space, F4 and
 * Alt+Up/Down, and on macOS the arrows (WebKit opens the menu on them). A
 * pick from the open list fires one `change` and is sent at once.
 */
export function isStepKey(
  event: Pick<KeyboardEvent, "key" | "ctrlKey" | "metaKey" | "altKey">,
  platform: Platform,
): boolean {
  if (event.ctrlKey || event.metaKey || event.altKey || event.key === " ") {
    return false;
  }
  if (platform === "windows" && STEP_KEYS.has(event.key)) {
    return true;
  }
  return event.key.length === 1;
}

/** Seams for tests. Production code passes nothing. */
export interface SettingsPageOptions {
  readonly invoke?: InvokeFn;
}

/** The mounted page. */
export interface SettingsPage {
  /**
   * Resolves once every command sent so far has answered and the page
   * shows the result. Never rejects. A key reached with the keyboard that
   * the user hasn't settled on yet hasn't been sent.
   */
  idle(): Promise<void>;
}

/** The parts of one switch row the page updates. */
interface ToggleParts {
  readonly row: HTMLElement;
  readonly label: HTMLElement;
  readonly detail: HTMLElement;
  readonly input: HTMLInputElement;
}

/** One key select's picks that Rust hasn't answered yet. */
interface KeyEdit {
  /** The key the select keeps showing: the latest pick, sent or not. `null` shows the view's key. */
  held: KeyId | null;
  /** The latest pick hasn't been sent: the user is stepping with the keyboard. */
  waiting: boolean;
  /** The user last moved this select with a step key, not the mouse. */
  stepping: boolean;
  /** Picks sent that haven't answered yet. */
  unanswered: number;
}

/**
 * Builds the page into `root` (the page's `#settings-root`), asks Rust for
 * the current settings, and wires every control to its command.
 */
export function mountSettings(root: HTMLElement, options: SettingsPageOptions = {}): SettingsPage {
  const doc = root.ownerDocument;
  const callRust = options.invoke ?? tauriInvoke;

  /** The last view that passed the guard, or `null` before the first. */
  let shown: SettingsView | null = null;
  let error: string | null = null;
  let pending = 0;
  let picking = false;
  let queue: Promise<void> = Promise.resolve();
  let drawnChoices: string | null = null;

  function element<K extends keyof HTMLElementTagNameMap>(
    tag: K,
    className: string,
    text?: string,
  ): HTMLElementTagNameMap[K] {
    const node = doc.createElement(tag);
    node.classList.add(className);
    if (text !== undefined) {
      node.textContent = text;
    }
    return node;
  }

  function button(text: string, label: string): HTMLButtonElement {
    const node = element("button", "button", text);
    node.setAttribute("type", "button");
    node.setAttribute("aria-label", label);
    return node;
  }

  function section(id: string, title: string, hint?: string): HTMLElement {
    const box = element("section", "settings-section");
    box.setAttribute("aria-labelledby", `${id}-heading`);
    const heading = element("h2", "section-title", title);
    heading.setAttribute("id", `${id}-heading`);
    box.append(heading);
    if (hint !== undefined) {
      box.append(element("p", "section-hint", hint));
    }
    return box;
  }

  /** A polite live region: it exists before any text arrives, so each message is announced. */
  function liveLine(className: string): HTMLElement {
    const line = element("p", className);
    line.setAttribute("role", "status");
    line.setAttribute("aria-live", "polite");
    return line;
  }

  // --- Header line and status ----------------------------------------------

  const lead = element("p", "lead");
  const status = liveLine("settings-status");
  const sections = element("div", "settings-sections");

  // --- Keys ------------------------------------------------------------------

  const keys = section(
    "keys",
    "Keys",
    "While you drag a file, tap a key to open its wheel. Each wheel has its own key: pick one that's taken and the two swap.",
  );

  function keyEdit(): KeyEdit {
    return { held: null, waiting: false, stepping: false, unanswered: 0 };
  }

  const keyEdits: Readonly<Record<Wheel, KeyEdit>> = {
    conversions: keyEdit(),
    tools: keyEdit(),
    actions: keyEdit(),
  };

  /** Sends `wheel`'s latest pick, if one is waiting. */
  function settleKey(wheel: Wheel): void {
    const edit = keyEdits[wheel];
    const key = edit.held;
    if (!edit.waiting || key === null) {
      return;
    }
    edit.waiting = false;
    edit.unanswered += 1;
    void send(SETTINGS_SET_KEY, { wheel, key }, () => {
      edit.unanswered -= 1;
      // A newer pick, waiting or sent, keeps showing until it has answered.
      if (edit.unanswered === 0 && !edit.waiting) {
        edit.held = null;
      }
    });
  }

  /** Sends every waiting pick: the page is going away. */
  function settleAllKeys(): void {
    for (const wheel of WHEELS) {
      settleKey(wheel);
    }
  }

  /** Appends a labelled key select for `wheel` to the Keys section. */
  function keyField(wheel: Wheel): HTMLSelectElement {
    const id = `key-${wheel}`;
    const field = element("div", "field");
    const label = element("label", "field-label", WHEEL_LABELS[wheel]);
    label.setAttribute("for", id);
    const select = element("select", "field-select");
    select.setAttribute("id", id);
    const edit = keyEdits[wheel];
    select.addEventListener("change", () => {
      const key = select.value;
      if (!isKeyId(key)) {
        draw();
        return;
      }
      edit.held = key;
      edit.waiting = true;
      // A pick from the open list (mouse or keyboard) goes at once; a step
      // on the closed select waits until the user settles.
      if (!edit.stepping) {
        settleKey(wheel);
      }
    });
    select.addEventListener("mousedown", () => {
      edit.stepping = false;
    });
    select.addEventListener("blur", () => {
      edit.stepping = false;
      settleKey(wheel);
    });
    select.addEventListener("keydown", (event: KeyboardEvent) => {
      if (event.key === "Enter") {
        edit.stepping = false;
        settleKey(wheel);
      } else {
        // Any other key (one that opens the list, Tab, …) ends the steps.
        edit.stepping = isStepKey(event, shown?.platform ?? "windows");
      }
    });
    field.append(label, select);
    keys.append(field);
    return select;
  }

  const selects: Readonly<Record<Wheel, HTMLSelectElement>> = {
    conversions: keyField("conversions"),
    tools: keyField("tools"),
    actions: keyField("actions"),
  };

  // Closing Settings must not lose a key the user stepped to and hasn't
  // left yet. (The test DOM has no window.)
  const win = doc.defaultView;
  if (win !== null) {
    win.addEventListener("pagehide", settleAllKeys);
    win.document.addEventListener("visibilitychange", () => {
      if (win.document.visibilityState === "hidden") {
        settleAllKeys();
      }
    });
  }
  const note = liveLine("settings-note");
  keys.append(note);

  // --- Where files go --------------------------------------------------------

  const output = section("output", "Where files go", "Grape never changes the original.");
  const choices = element("div", "choices");
  choices.setAttribute("role", "radiogroup");
  choices.setAttribute("aria-labelledby", "output-heading");

  function radio(id: string, text: string): { readonly box: HTMLElement; readonly input: HTMLInputElement } {
    const box = element("div", "choice");
    const input = element("input", "choice-input");
    input.setAttribute("type", "radio");
    input.setAttribute("name", "output");
    input.setAttribute("id", id);
    const label = element("label", "choice-label", text);
    label.setAttribute("for", id);
    box.append(input, label);
    return { box, input };
  }

  const nextToSource = radio("output-next", "Next to the original");
  const inFolder = radio("output-folder", "In a folder I choose");
  nextToSource.input.addEventListener("change", () => {
    if (nextToSource.input.checked) {
      void send(SETTINGS_SET_OUTPUT_NEXT_TO_SOURCE, {});
    }
  });
  // The view only has a folder once one is picked, so choosing this radio
  // always opens the picker. Cancelling it puts the radio back.
  inFolder.input.addEventListener("change", () => {
    if (inFolder.input.checked) {
      pickFolder("output");
    }
  });

  const outputRow = element("div", "folder-row");
  const outputPath = element("span", "folder-path");
  outputPath.setAttribute("id", "output-path");
  const chooseOutput = button("Choose…", "Choose the folder for new files");
  chooseOutput.addEventListener("click", () => pickFolder("output"));
  outputRow.append(outputPath, chooseOutput);
  inFolder.box.append(outputRow);
  choices.append(nextToSource.box, inFolder.box);
  output.append(choices);

  // --- Fallback folder -------------------------------------------------------

  const fallback = section(
    "fallback",
    "Fallback folder",
    "Grape saves here when it can't write where a file should go.",
  );
  const fallbackRow = element("div", "folder-row");
  const fallbackPath = element("span", "folder-path");
  const defaultBadge = element("span", "folder-badge", "(default)");
  const fallbackButtons = element("div", "folder-buttons");
  const chooseFallback = button("Choose…", "Choose the fallback folder");
  chooseFallback.addEventListener("click", () => pickFolder("fallback"));
  const useDefault = button("Use the default", "Use the default fallback folder");
  useDefault.addEventListener("click", () => {
    void send(SETTINGS_USE_DEFAULT_FALLBACK, {});
  });
  fallbackButtons.append(chooseFallback, useDefault);
  fallbackRow.append(fallbackPath, defaultBadge);
  fallback.append(fallbackRow, fallbackButtons);

  // --- Switches --------------------------------------------------------------

  const general = section("general", "General");

  /** Appends a switch row for `name` to the General section. Its text comes with each view. */
  function toggleRow(name: ToggleName): ToggleParts {
    const id = `toggle-${name}`;
    const row = element("div", "toggle");
    const text = element("div", "toggle-text");
    const label = element("label", "toggle-label");
    label.setAttribute("for", id);
    const detail = element("span", "toggle-detail");
    detail.setAttribute("id", `${id}-detail`);
    text.append(label, detail);
    const input = element("input", "toggle-switch");
    input.setAttribute("type", "checkbox");
    input.setAttribute("role", "switch");
    input.setAttribute("id", id);
    input.setAttribute("aria-describedby", `${id}-detail`);
    input.addEventListener("change", () => {
      if (input.getAttribute("aria-disabled") === "true") {
        // Its state couldn't be read, so it can't be changed: put it back.
        input.checked = !input.checked;
        return;
      }
      void send(SETTINGS_SET_TOGGLE, { name, on: input.checked });
    });
    row.append(text, input);
    general.append(row);
    return { row, label, detail, input };
  }

  const toggles: Readonly<Record<ToggleName, ToggleParts>> = {
    sounds: toggleRow("sounds"),
    haptics: toggleRow("haptics"),
    launchAtLogin: toggleRow("launchAtLogin"),
  };

  sections.append(keys, output, fallback, general);
  setHidden(lead, true);
  setHidden(sections, true);
  // The status line comes after the sections and sticks to the window's
  // bottom edge, so an error stays in view wherever the page is scrolled.
  root.replaceChildren(lead, sections, status);

  // --- Commands --------------------------------------------------------------

  /**
   * Queues one command behind the others; resolves once it has answered.
   * `answered` runs then, before the page redraws.
   */
  function send(command: string, args: Record<string, unknown>, answered?: () => void): Promise<void> {
    error = null;
    pending += 1;
    root.setAttribute("aria-busy", "true");
    const done = queue.then(() => run(command, args)).then(() => answered?.());
    queue = done.then(() => {
      pending -= 1;
      if (pending === 0) {
        root.removeAttribute("aria-busy");
        draw();
      }
    });
    return done;
  }

  /** The text to show for a rejected `command`. Logs a reason that isn't Rust's sentence. */
  function rejectionText(command: string, reason: unknown): string {
    if (typeof reason !== "string") {
      console.error(`Grape settings: ${command} failed`, reason);
    }
    return errorText(reason);
  }

  /** Calls Rust and keeps a good reply. Never rejects. */
  async function run(command: string, args: Record<string, unknown>): Promise<void> {
    try {
      const reply = await callRust(command, args);
      if (isSettingsView(reply)) {
        shown = reply;
      } else {
        // The reply itself is never logged: it holds folder paths.
        console.warn(`Grape settings: ${command} answered with a view that doesn't match the contract`);
        error = GENERIC_ERROR;
      }
    } catch (reason: unknown) {
      error = rejectionText(command, reason);
      if (command !== SETTINGS_GET) {
        await refresh();
      }
    }
  }

  /**
   * After a command fails, reads the settings again: Rust may have changed
   * them anyway (a failed save keeps the change), or launch at login may be
   * in another state than the page shows. The failed command's message
   * stays, and so does the last good view if this fails too.
   */
  async function refresh(): Promise<void> {
    try {
      const reply = await callRust(SETTINGS_GET, {});
      if (isSettingsView(reply)) {
        // A swap note belongs to the change that made it, never to a re-read.
        shown = { ...reply, note: null };
      } else {
        console.warn(`Grape settings: ${SETTINGS_GET} answered with a view that doesn't match the contract`);
      }
    } catch (reason: unknown) {
      rejectionText(SETTINGS_GET, reason);
    }
  }

  /**
   * One picker at a time: asking again while one is open sends nothing,
   * and only redraws, which puts a radio the user just chose back.
   */
  function pickFolder(target: FolderTarget): void {
    if (picking) {
      draw();
      return;
    }
    picking = true;
    void send(SETTINGS_PICK_FOLDER, { target }).then(() => {
      picking = false;
    });
  }

  // --- Drawing ---------------------------------------------------------------

  function setHidden(node: HTMLElement, hidden: boolean): void {
    if (hidden) {
      node.setAttribute("hidden", "");
    } else {
      node.removeAttribute("hidden");
    }
  }

  /** Like `setHidden`, but first moves focus to `next` if `node` has it, so focus never drops to the page. */
  function hideKeepingFocus(node: HTMLElement, hidden: boolean, next: HTMLElement): void {
    if (hidden && doc.activeElement === node) {
      next.focus();
    }
    setHidden(node, hidden);
  }

  function option(choice: KeyChoice): HTMLOptionElement {
    const node = element("option", "field-option", choice.name);
    node.setAttribute("value", choice.key);
    return node;
  }

  function draw(): void {
    status.textContent = error ?? "";
    const view = shown;
    if (view === null) {
      return;
    }
    setHidden(lead, false);
    setHidden(sections, false);
    lead.textContent = leadText(view.platform);

    const signature = choicesSignature(view.keyChoices);
    for (const wheel of WHEELS) {
      const select = selects[wheel];
      if (signature !== drawnChoices) {
        select.replaceChildren(...view.keyChoices.map(option));
      }
      select.value = keyEdits[wheel].held ?? view.keys[wheel];
    }
    drawnChoices = signature;
    note.textContent = view.note ?? "";

    const folder = outputFolder(view);
    nextToSource.input.checked = folder === null;
    inFolder.input.checked = folder !== null;
    outputPath.textContent = folder ?? "";
    // Only the radios can hide this row, so focus is never inside it then.
    setHidden(outputRow, folder === null);

    fallbackPath.textContent = view.fallbackDir.path;
    setHidden(defaultBadge, !view.fallbackDir.isDefault);
    hideKeepingFocus(useDefault, view.fallbackDir.isDefault, chooseFallback);

    const rows = new Map(toggleRows(view).map((row) => [row.name, row]));
    for (const name of TOGGLE_NAMES) {
      const parts = toggles[name];
      const row = rows.get(name);
      setHidden(parts.row, row === undefined);
      if (row === undefined) {
        continue;
      }
      parts.label.textContent = row.label;
      parts.detail.textContent = row.detail;
      parts.input.checked = row.on;
      // `aria-disabled`, not `disabled`: the switch keeps focus across
      // redraws and stays in the Tab order, so its detail ("Couldn't
      // check") is still read out. Its change handler puts it back.
      if (row.enabled) {
        parts.input.removeAttribute("aria-disabled");
      } else {
        parts.input.setAttribute("aria-disabled", "true");
      }
    }
  }

  void send(SETTINGS_GET, {});

  return {
    idle: () => queue,
  };
}
