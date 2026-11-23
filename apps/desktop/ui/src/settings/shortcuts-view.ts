/**
 * The Shortcuts section of the Settings page (the design note
 * docs/design/m10-shortcuts-wheel.md, §8): the wheel's switch and key
 * combination, the menus, and each menu's shortcuts with their steps. It
 * mounts into its own root, next to `mountSettings`, with its own command
 * queue and status line.
 *
 * Like the rest of Settings, the page decides nothing. Each change is one
 * `shortcuts_*` command, Rust answers with the whole new view, and the page
 * draws it. Commands run one at a time, in the order the user made the
 * changes, and the page redraws once the last one has answered. A refused
 * command shows Rust's sentence in the status line, and the page reads the
 * view again.
 *
 * Text is sent when the user leaves a field (`change`), never while they
 * type. A field holding text Rust hasn't answered yet is left alone by a
 * redraw, and such text is sent when the window closes or hides: at once
 * then, with every command still waiting its turn, because a page that is
 * going away gets no answer to wait for. Text Rust refuses in a step's field
 * stays there, marked as not saved, until the user changes it.
 *
 * The wheel's key combination is a draft: nothing is sent until the user
 * presses "Set shortcut".
 *
 * Rows are keyed by Rust's ids and built once: a redraw updates them in
 * place, so focus stays where it is. Only a row Rust no longer lists is
 * taken away.
 */

import { type InvokeFn, tauriInvoke } from "../shared/ipc";
import { GLYPH_GRID, glyphPaths } from "../wheel/glyphs";
import {
  addItemArgs,
  addStepArgs,
  allIds,
  type Args,
  bindingText,
  type Choice,
  choicesSignature,
  type ChordWire,
  chordWire,
  findItem,
  findStep,
  type Icon,
  ICON_EMOJI,
  iconChoice,
  iconChoices,
  iconFromChoice,
  isOpensAt,
  isShortcutsView,
  isStepKind,
  itemSummary,
  type ItemView,
  menuChoices,
  menuName,
  menuOrEverywhere,
  menusFullText,
  type MenuView,
  MODIFIER_IDS,
  type ModifierId,
  moveItemArgs,
  moveStepArgs,
  type NewItemKind,
  OPENS_AT_CHOICES,
  pickTargetArgs,
  removeItemArgs,
  removeMenuArgs,
  removeStepArgs,
  ringFullText,
  ringTitle,
  sameChord,
  setEnabledArgs,
  setHotkeyArgs,
  setItemArgs,
  setMenuEverywhereArgs,
  setOpensAtArgs,
  setStepArgs,
  SHORTCUTS_ADD_ITEM,
  SHORTCUTS_ADD_MENU,
  SHORTCUTS_ADD_STEP,
  SHORTCUTS_COPY as COPY,
  SHORTCUTS_ERROR,
  SHORTCUTS_GET,
  SHORTCUTS_MOVE_ITEM,
  SHORTCUTS_MOVE_STEP,
  SHORTCUTS_PICK_TARGET,
  SHORTCUTS_REMOVE_ITEM,
  SHORTCUTS_REMOVE_MENU,
  SHORTCUTS_REMOVE_STEP,
  SHORTCUTS_REQUEST_ACCESS,
  SHORTCUTS_SET_ENABLED,
  SHORTCUTS_SET_HOTKEY,
  SHORTCUTS_SET_ITEM,
  SHORTCUTS_SET_MENU_EVERYWHERE,
  SHORTCUTS_SET_OPENS_AT,
  SHORTCUTS_SET_STEP,
  shortcutsErrorText,
  type ShortcutsView,
  STEP_FIELDS,
  STEP_GLYPHS,
  STEP_KIND_NAMES,
  STEP_KINDS,
  stepEdit,
  stepsFullText,
  type StepsItem,
  type StepView,
  unsavedText,
} from "./shortcuts-model";

const SVG_NS = "http://www.w3.org/2000/svg";

/** Seams for tests. Production code passes nothing. */
export interface ShortcutsPageOptions {
  readonly invoke?: InvokeFn;
}

/** The mounted section. */
export interface ShortcutsPage {
  /**
   * Resolves once every command sent so far has answered and the page
   * shows the result. Never rejects. Text the user is still typing hasn't
   * been sent.
   */
  idle(): Promise<void>;
}

/** A command and its args. */
type Message = readonly [command: string, args: Args];

/** A command in the page's queue. `reply` is `null` until it has gone to Rust. */
interface Job {
  readonly command: string;
  readonly args: Args;
  reply: Promise<unknown> | null;
}

/** What a command's `answered` gets: `null` when Rust took it, or the sentence the page shows when it didn't. */
type Problem = string | null;

/** A text control whose value goes to Rust when the user leaves it. */
interface TextField {
  readonly control: HTMLInputElement | HTMLTextAreaElement;
  /** Shows the view's value, unless the user's own text is unsent, unanswered or refused. */
  show(value: string): void;
  /** Sends the user's text, unless Rust holds it, is being sent it, or has refused it. */
  flush(): void;
}

/** Four modifier checkboxes and a key select, and for a draft the button that sends it. */
interface ChordEditor {
  readonly box: HTMLElement;
  readonly key: HTMLSelectElement;
  show(view: ShortcutsView, chord: ChordWire | null, label: string | null): void;
}

/** A row the page keeps while Rust lists its id. */
interface Row {
  readonly box: HTMLElement;
  /** Forgets the row's text fields and the rows inside it. */
  drop(): void;
}

interface StepRow extends Row {
  /** Where focus goes when the step is new. */
  readonly first: HTMLElement;
  readonly up: HTMLButtonElement;
  readonly down: HTMLButtonElement;
  show(step: StepView, index: number, item: StepsItem, view: ShortcutsView): void;
}

interface ItemRow extends Row {
  readonly name: HTMLInputElement;
  readonly up: HTMLButtonElement;
  readonly down: HTMLButtonElement;
  show(item: ItemView, index: number, count: number, view: ShortcutsView): void;
}

/**
 * Builds the section into `root` (the page's `#shortcuts-root`), asks Rust
 * for the shortcuts, and wires every control to its command.
 */
export function mountShortcuts(root: HTMLElement, options: ShortcutsPageOptions = {}): ShortcutsPage {
  const doc = root.ownerDocument;
  const callRust = options.invoke ?? tauriInvoke;

  /** The last view that passed the guard, or `null` before the first. */
  let shown: ShortcutsView | null = null;
  let error: string | null = null;
  let pending = 0;
  let picking = false;
  let queue: Promise<void> = Promise.resolve();
  /** Queued commands that haven't gone to Rust yet, oldest first. */
  const waiting: Job[] = [];
  /** The menu whose shortcuts show. `null` until a view picks Everywhere. */
  let menuId: number | null = null;
  /** Where focus should go after the next redraw: a new row, or a moved one. */
  let wish: (() => HTMLElement | null) | null = null;
  /** Every text field in the page, so closing the window can send them all. */
  const fields = new Set<TextField>();
  const itemRows = new Map<number, ItemRow>();
  const stepRows = new Map<number, StepRow>();

  // --- Small builders --------------------------------------------------------

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

  /** A button. Its accessible name is set here or with each view; it always contains `text`. */
  function button(text: string, label = text): HTMLButtonElement {
    const node = element("button", "button", text);
    node.setAttribute("type", "button");
    node.setAttribute("aria-label", label);
    return node;
  }

  function labelFor(id: string, text: string, className = "field-label"): HTMLLabelElement {
    const node = element("label", className, text);
    node.setAttribute("for", id);
    return node;
  }

  function option(value: string, name: string): HTMLOptionElement {
    const node = element("option", "field-option", name);
    node.setAttribute("value", value);
    return node;
  }

  /** A select with this id. */
  function select(id: string, choices: readonly Choice[] = []): HTMLSelectElement {
    const node = element("select", "field-select");
    node.setAttribute("id", id);
    node.append(...choices.map((choice) => option(choice.value, choice.label)));
    return node;
  }

  /** A polite live region: it exists before any text arrives, so each message is announced. */
  function liveLine(className: string): HTMLElement {
    const line = element("p", className);
    line.setAttribute("role", "status");
    line.setAttribute("aria-live", "polite");
    return line;
  }

  /** Writes a live region's text only when it changes, so a redraw doesn't read it out again. */
  function say(line: HTMLElement, text: string): void {
    if (line.textContent !== text) {
      line.textContent = text;
    }
  }

  function setHidden(node: HTMLElement, hidden: boolean): void {
    if (hidden) {
      node.setAttribute("hidden", "");
    } else {
      node.removeAttribute("hidden");
    }
  }

  /** Like `setHidden`, but first moves focus to `next` if it is inside `node`, so it never drops to the page. */
  function hideKeepingFocus(node: HTMLElement, hidden: boolean, next: HTMLElement): void {
    if (hidden && node.contains(doc.activeElement)) {
      next.focus();
    }
    setHidden(node, hidden);
  }

  /**
   * Marks a button whose press hasn't answered yet. It does nothing until
   * then, but it isn't `disabled`: it keeps the focus and its place in the
   * Tab order.
   */
  function setWaiting(node: HTMLButtonElement, on: boolean): void {
    if (on) {
      node.setAttribute("aria-disabled", "true");
    } else {
      node.removeAttribute("aria-disabled");
    }
  }

  function isWaiting(node: HTMLButtonElement): boolean {
    return node.getAttribute("aria-disabled") === "true";
  }

  /**
   * The second click of a double click. The first one's row may be gone
   * already, with its neighbour's button under the pointer in its place.
   * (A press from the keyboard counts no clicks.)
   */
  function isRepeat(event: Event): boolean {
    return (event as MouseEvent).detail > 1;
  }

  /** A row's Remove button: one press, one command, and nothing more until Rust has answered it. */
  function wireRemove(node: HTMLButtonElement, message: () => Message | null): void {
    node.addEventListener("click", (event: Event) => {
      const next = isRepeat(event) || isWaiting(node) ? null : message();
      if (next === null) {
        return;
      }
      setWaiting(node, true);
      void send(next[0], next[1], () => setWaiting(node, false));
    });
  }

  /**
   * Holds or frees the Move buttons of one list. While a move there hasn't
   * answered, a second one would count its place from the old order. (A
   * button that is off, at an end of the list, needs no mark.)
   */
  function holdMoves(rows: readonly (Pick<ItemRow, "up" | "down"> | undefined)[], on: boolean): void {
    for (const row of rows) {
      for (const node of row === undefined ? [] : [row.up, row.down]) {
        setWaiting(node, on && !node.disabled);
      }
    }
  }

  /** True while a move in this row's list hasn't answered. */
  function movesHeld(row: Pick<ItemRow, "up" | "down">): boolean {
    return isWaiting(row.up) || isWaiting(row.down);
  }

  /** One of Grape's glyphs, stroked by `settings.css`, or `null` for an id the page can't draw. */
  function glyph(id: string): SVGElement | null {
    const paths = glyphPaths(id);
    if (paths === null) {
      return null;
    }
    const svg = doc.createElementNS(SVG_NS, "svg");
    svg.classList.add("sc-glyph");
    svg.setAttribute("viewBox", `0 0 ${GLYPH_GRID} ${GLYPH_GRID}`);
    svg.setAttribute("aria-hidden", "true");
    for (const d of paths) {
      const path = doc.createElementNS(SVG_NS, "path");
      path.setAttribute("d", d);
      svg.append(path);
    }
    return svg;
  }

  function drawIcon(holder: HTMLElement, icon: Icon): void {
    if (icon.kind === "glyph") {
      const drawing = glyph(icon.value);
      holder.replaceChildren(...(drawing === null ? [] : [drawing]));
    } else {
      // Text the user typed: never markup.
      holder.textContent = icon.kind === "emoji" ? icon.value : "";
    }
  }

  /** A row with a switch: its label and detail on the left. */
  function switchRow(
    id: string,
    text: string,
    detail: string,
    changed: (on: boolean) => void,
  ): { readonly row: HTMLElement; readonly input: HTMLInputElement } {
    const row = element("div", "toggle");
    const words = element("div", "toggle-text");
    const about = element("span", "toggle-detail", detail);
    about.setAttribute("id", `${id}-detail`);
    words.append(labelFor(id, text, "toggle-label"), about);
    const input = element("input", "toggle-switch");
    input.setAttribute("type", "checkbox");
    input.setAttribute("role", "switch");
    input.setAttribute("id", id);
    input.setAttribute("aria-describedby", `${id}-detail`);
    input.addEventListener("change", () => changed(input.checked));
    row.append(words, input);
    return { row, input };
  }

  // --- Text fields -----------------------------------------------------------

  /**
   * Tracks one text control. `message` turns its text into a command, or
   * `null` for text Rust couldn't read, which puts Rust's value back.
   *
   * With `unsaved` (a line under the control), text Rust refuses stays in
   * the control, marked, with Rust's sentence in that line, and isn't sent
   * again until the user changes it. Without one, Rust's value goes back in.
   */
  function textField(
    control: HTMLInputElement | HTMLTextAreaElement,
    message: (value: string) => Message | null,
    unsaved: HTMLElement | null = null,
  ): TextField {
    // What Rust holds, by the last view. Not always what the control shows.
    let held = control.value;
    // The text of the newest command on its way, while one is.
    let sent: string | null = null;
    let unanswered = 0;
    // The newest answered command, until the next redraw.
    let settled: { readonly text: string; readonly problem: Problem } | null = null;
    // Text Rust refused, while the control still shows it.
    let refused: string | null = null;
    let marked = false;

    /** Marks the control's text as not saved, with Rust's sentence, or takes the mark away. */
    function mark(problem: Problem): void {
      if (unsaved === null || (problem === null && !marked)) {
        return;
      }
      marked = problem !== null;
      if (problem === null) {
        control.removeAttribute("aria-invalid");
      } else {
        control.setAttribute("aria-invalid", "true");
      }
      unsaved.textContent = problem === null ? "" : unsavedText(problem);
    }

    function flush(): void {
      const value = control.value;
      if (value === refused) {
        return;
      }
      // What Rust holds, or will once the command on its way has landed.
      const known = sent ?? held;
      const next = value === known ? null : message(value);
      if (next === null) {
        if (value !== known) {
          control.value = known;
        }
        if (sent === null) {
          // The control shows what Rust holds again.
          refused = null;
          mark(null);
        }
        return;
      }
      refused = null;
      sent = value;
      unanswered += 1;
      void send(next[0], next[1], (problem) => {
        unanswered -= 1;
        if (unanswered === 0) {
          settled = { text: value, problem };
          sent = null;
        }
      });
    }

    function show(value: string): void {
      if (unanswered > 0) {
        return;
      }
      const answer = settled;
      settled = null;
      const current = control.value;
      // The field's own command has answered, and the user hasn't typed since.
      const own = answer !== null && current === answer.text;
      if (own && answer.problem !== null && unsaved !== null && value === held && current !== value) {
        // Refused, and Rust holds what it held (a failed save would have changed it): the text stays.
        refused = current;
        mark(answer.problem);
        return;
      }
      if (!own && current !== held && current !== value) {
        // Typed and not sent yet: the text stays, and only what Rust holds moves on.
        held = value;
        if (answer !== null && answer.problem === null) {
          mark(null);
        }
        return;
      }
      if (current !== value) {
        control.value = value;
      }
      // Read back: a control may keep less than it was given (a one-line field drops line breaks).
      held = control.value;
      refused = null;
      mark(null);
    }

    control.addEventListener("change", flush);
    if (control.localName === "input") {
      // Enter settles a one-line field, wherever the webview fires `change`.
      control.addEventListener("keydown", (event: Event) => {
        if ((event as KeyboardEvent).key === "Enter") {
          flush();
        }
      });
    }
    const field: TextField = { control, show, flush };
    fields.add(field);
    return field;
  }

  /** Queues every text the user typed and hasn't left yet. */
  function flushAll(): void {
    for (const field of [...fields]) {
      field.flush();
    }
  }

  /**
   * Closing or hiding Settings must not lose text the user typed. It goes to
   * Rust now, after every command still waiting its turn and in the order
   * they were made: the page may be gone before an answer comes. The answers
   * are read one at a time all the same, if the page lives to see them.
   */
  function flushNow(): void {
    flushAll();
    for (const job of [...waiting]) {
      void begin(job);
    }
  }

  // --- Chords ----------------------------------------------------------------

  /**
   * Builds a chord editor whose controls get ids starting with `prefix`.
   *
   * Without `apply`, `pick` gets each chord the user makes, or `null` when
   * they choose no key. Modifiers ticked while no key is chosen wait, unsent,
   * for the key.
   *
   * With `apply` (a button's text and name), the controls are a draft:
   * nothing is sent until the user presses that button, and then `pick`
   * gets the chord once, or `null` with no key chosen. The button is on only
   * while the draft differs from the chord Rust holds. A draft Rust refuses
   * stays as it is.
   */
  function chordEditor(
    prefix: string,
    title: string,
    pick: (chord: ChordWire | null) => Promise<void>,
    apply: { readonly text: string; readonly label: string } | null = null,
  ): ChordEditor {
    const box = element("div", "sc-chord");
    box.setAttribute("role", "group");
    box.setAttribute("aria-label", title);
    const boxes = element("div", "sc-chord-mods");
    const names = new Map<ModifierId, HTMLElement>();
    const checks = new Map<ModifierId, HTMLInputElement>();
    for (const modifier of MODIFIER_IDS) {
      const id = `${prefix}-${modifier}`;
      const check = element("input", "choice-input");
      check.setAttribute("type", "checkbox");
      check.setAttribute("id", id);
      check.addEventListener("change", () => changed(false));
      const name = labelFor(id, modifier, "sc-check-label");
      const pair = element("span", "sc-check");
      pair.append(check, name);
      boxes.append(pair);
      names.set(modifier, name);
      checks.set(modifier, check);
    }
    const keyId = `${prefix}-key`;
    const key = select(keyId);
    key.addEventListener("change", () => changed(true));
    const keyPair = element("span", "sc-chord-key");
    keyPair.append(labelFor(keyId, COPY.key, "sc-check-label"), key);
    const set = apply === null ? null : button(apply.text, apply.label);
    const current = element("span", "sc-chord-label");
    box.append(boxes, keyPair, ...(set === null ? [] : [set]), current);

    // The chord Rust holds, by the last view.
    let held: ChordWire | null = null;
    // Without a button: the user ticked modifiers with no key chosen, and a redraw leaves them alone.
    let draft = false;
    let drawnKeys: string | null = null;
    // The button's command is on its way.
    let setting = false;

    const ticked = (modifier: ModifierId): boolean => checks.get(modifier)?.checked ?? false;

    /** The chord the controls spell, or `null` with no key chosen. */
    function chosen(): ChordWire | null {
      if (key.value === "") {
        return null;
      }
      return chordWire(key.value, {
        shift: ticked("shift"),
        ctrl: ticked("ctrl"),
        alt: ticked("alt"),
        meta: ticked("meta"),
      });
    }

    /** True while the controls show exactly `chord`: its key and modifiers, or no key and no tick. */
    function shows(chord: ChordWire | null): boolean {
      return (
        key.value === (chord?.key ?? "") &&
        MODIFIER_IDS.every((modifier) => ticked(modifier) === (chord?.[modifier] ?? false))
      );
    }

    /** Switches the button on while there is a draft to send. Focus never stays on a button that is off. */
    function drawButton(): void {
      if (set === null) {
        return;
      }
      const off = sameChord(chosen(), held);
      if (off && doc.activeElement === set) {
        key.focus();
      }
      set.disabled = off;
    }

    function changed(fromKey: boolean): void {
      const view = shown;
      if (view === null) {
        return;
      }
      const offered = key.value === "" || view.keys.some((choice) => choice.id === key.value);
      if (set !== null) {
        if (!offered) {
          key.value = held?.key ?? "";
        }
        drawButton();
        return;
      }
      if (key.value === "") {
        draft = true;
        if (fromKey) {
          void pick(null);
        }
        return;
      }
      if (!offered) {
        redraw();
        return;
      }
      draft = false;
      void pick(chosen());
    }

    set?.addEventListener("click", () => {
      const chord = chosen();
      if (setting || shown === null || sameChord(chord, held)) {
        return;
      }
      setting = true;
      void pick(chord).then(() => {
        setting = false;
      });
    });

    function show(view: ShortcutsView, chord: ChordWire | null, label: string | null): void {
      const signature = choicesSignature(view.keys.map((choice) => [choice.id, choice.label]));
      if (signature !== drawnKeys) {
        const picked = key.value;
        const keys = view.keys.map((choice) => option(choice.id, choice.label));
        key.replaceChildren(option("", COPY.noKey), ...keys);
        // New options must not undo a draft's key.
        key.value = picked;
        drawnKeys = signature;
      }
      for (const modifier of MODIFIER_IDS) {
        const name = names.get(modifier);
        if (name !== undefined) {
          name.textContent = view.modifiers[modifier];
        }
      }
      // A draft the user is making stays: only what Rust holds moves on.
      const follows = set === null ? chord !== null || !draft : shows(held);
      held = chord;
      if (follows) {
        draft = false;
        for (const modifier of MODIFIER_IDS) {
          const check = checks.get(modifier);
          if (check !== undefined) {
            check.checked = chord?.[modifier] ?? false;
          }
        }
        key.value = chord?.key ?? "";
      }
      current.textContent = label ?? COPY.notSet;
      drawButton();
    }

    return { box, key, show };
  }

  // --- Keyed rows ------------------------------------------------------------

  /**
   * Makes `list` hold one row per entry, in order. A row whose key is still
   * wanted is kept as it is; one that isn't is dropped, after focus inside
   * it has moved to `next`. When the order changes, the row holding focus
   * stays put and the others move around it.
   */
  function arrange<T, R extends Row>(
    list: HTMLElement,
    rows: Map<string, R>,
    entries: readonly T[],
    keyOf: (entry: T) => string,
    build: (entry: T) => R,
    next: HTMLElement,
  ): R[] {
    const wanted = new Set(entries.map(keyOf));
    for (const [key, row] of [...rows]) {
      if (wanted.has(key)) {
        continue;
      }
      if (row.box.contains(doc.activeElement)) {
        next.focus();
      }
      list.removeChild(row.box);
      row.drop();
      rows.delete(key);
    }
    const kept = entries.map((entry) => {
      const key = keyOf(entry);
      const old = rows.get(key);
      if (old !== undefined) {
        return old;
      }
      const made = build(entry);
      rows.set(key, made);
      return made;
    });
    const boxes = kept.map((row) => row.box);
    const inOrder = list.children.length === boxes.length && boxes.every((box, at) => list.children[at] === box);
    if (inOrder) {
      return kept;
    }
    const pinned = boxes.findIndex((box) => box.contains(doc.activeElement));
    const still = boxes[pinned];
    if (still === undefined) {
      list.append(...boxes);
      return kept;
    }
    for (const box of boxes.slice(0, pinned)) {
      list.insertBefore(box, still);
    }
    list.append(...boxes.slice(pinned + 1));
    return kept;
  }

  /** The key a row is kept under: a step or item whose kind changed is another row. */
  function rowKey(entry: { readonly id: number; readonly kind: string }): string {
    return `${entry.id}:${entry.kind}`;
  }

  /** After the next redraw, focus the control `find` returns for the id that wasn't in `before`. */
  function focusNew(before: ReadonlySet<number>, find: (id: number) => HTMLElement | undefined): void {
    wish = () => {
      const view = shown;
      if (view === null) {
        return null;
      }
      for (const id of allIds(view)) {
        const control = before.has(id) ? undefined : find(id);
        if (control !== undefined) {
          return control;
        }
      }
      return null;
    };
  }

  /** After the next redraw, focus the button that moved the row, or the other one when that one is off. */
  function focusMoved(find: () => Pick<ItemRow, "up" | "down"> | undefined, by: number): void {
    wish = () => {
      const row = find();
      if (row === undefined) {
        return null;
      }
      const [first, second] = by < 0 ? [row.up, row.down] : [row.down, row.up];
      if (!first.disabled) {
        return first;
      }
      return second.disabled ? null : second;
    };
  }

  // --- Steps -----------------------------------------------------------------

  /** The line under a step's field that says its text wasn't saved, and why. Empty until then. */
  function unsavedLine(id: string): HTMLElement {
    const line = element("p", "sc-unsaved");
    line.setAttribute("id", id);
    return line;
  }

  function buildStep(first: StepView): StepRow {
    const id = first.id;
    const kind = first.kind;
    const prefix = `sc-step-${id}`;
    const field = STEP_FIELDS[kind];
    const kindName = STEP_KIND_NAMES[kind];
    const tracked: TextField[] = [];

    const box = element("div", "sc-step");
    box.setAttribute("role", "group");
    const head = element("div", "sc-step-head");
    const title = element("span", "sc-step-title");
    const up = button(COPY.moveUp);
    const down = button(COPY.moveDown);
    const remove = button(COPY.remove);
    up.addEventListener("click", () => move(-1));
    down.addEventListener("click", () => move(1));
    wireRemove(remove, () => [SHORTCUTS_REMOVE_STEP, removeStepArgs(id)]);
    const drawing = glyph(STEP_GLYPHS[kind]);
    head.append(...(drawing === null ? [] : [drawing]), title, up, down, remove);
    const body = element("div", "sc-step-body");
    box.append(head, body);

    function move(by: number): void {
      const place = shown === null ? null : findStep(shown, id);
      if (place === null || movesHeld({ up, down })) {
        return;
      }
      const list = place.item.steps.map((step) => stepRows.get(step.id));
      holdMoves(list, true);
      void send(SHORTCUTS_MOVE_STEP, moveStepArgs(id, place.index + by), () => {
        holdMoves(list, false);
        focusMoved(() => stepRows.get(id), by);
      });
    }

    let focusFirst: HTMLElement;
    let showField: (step: StepView, view: ShortcutsView) => void;
    switch (field.control) {
      case "chord": {
        const editor = chordEditor(prefix, field.label, (chord) =>
          send(SHORTCUTS_SET_STEP, setStepArgs(id, stepEdit.keys(chord))),
        );
        body.append(editor.box);
        focusFirst = editor.key;
        showField = (step, view) => {
          if (step.kind === "keys") {
            editor.show(view, step.chord, step.label);
          }
        };
        break;
      }
      case "text": {
        const controlId = `${prefix}-${field.key}`;
        const control = field.lines > 1 ? element("textarea", "sc-text") : element("input", "sc-input");
        if (field.lines > 1) {
          control.setAttribute("rows", String(field.lines));
        } else {
          control.setAttribute("type", "text");
        }
        control.setAttribute("id", controlId);
        control.setAttribute("placeholder", field.placeholder);
        control.setAttribute("autocomplete", "off");
        if (!field.prose) {
          control.classList.add("sc-code");
          control.setAttribute("spellcheck", "false");
          control.setAttribute("autocapitalize", "off");
          control.setAttribute("autocorrect", "off");
        }
        body.append(labelFor(controlId, field.label), control);
        const described: string[] = [];
        if (field.hint !== null) {
          const hint = element("p", "sc-hint", field.hint);
          hint.setAttribute("id", `${controlId}-hint`);
          described.push(`${controlId}-hint`);
          body.append(hint);
        }
        const unsaved = unsavedLine(`${controlId}-unsaved`);
        described.push(`${controlId}-unsaved`);
        control.setAttribute("aria-describedby", described.join(" "));
        body.append(unsaved);
        const tracker = textField(
          control,
          (value) => [SHORTCUTS_SET_STEP, setStepArgs(id, field.edit(value))],
          unsaved,
        );
        tracked.push(tracker);
        focusFirst = control;
        showField = (step, view) => {
          control.setAttribute("maxlength", String(view.limits[field.limit]));
          tracker.show(field.read(step));
        };
        break;
      }
      case "choice": {
        const controlId = `${prefix}-${field.key}`;
        const control = select(controlId, field.choices);
        control.addEventListener("change", () => {
          const edit = field.edit(control.value);
          if (edit === null) {
            redraw();
            return;
          }
          void send(SHORTCUTS_SET_STEP, setStepArgs(id, edit));
        });
        body.append(labelFor(controlId, field.label), control);
        focusFirst = control;
        showField = (step) => {
          control.value = field.read(step);
        };
        break;
      }
      case "number": {
        const controlId = `${prefix}-${field.key}`;
        const control = element("input", "sc-input");
        control.classList.add("sc-number");
        control.setAttribute("type", "number");
        control.setAttribute("inputmode", "numeric");
        control.setAttribute("id", controlId);
        const unsaved = unsavedLine(`${controlId}-unsaved`);
        control.setAttribute("aria-describedby", `${controlId}-unsaved`);
        body.append(labelFor(controlId, field.label), control, unsaved);
        const tracker = textField(
          control,
          (value) => {
            const edit = field.edit(value);
            return edit === null ? null : [SHORTCUTS_SET_STEP, setStepArgs(id, edit)];
          },
          unsaved,
        );
        tracked.push(tracker);
        focusFirst = control;
        showField = (step, view) => {
          control.setAttribute("min", String(view.limits.delayMinMs));
          control.setAttribute("max", String(view.limits.delayMaxMs));
          tracker.show(field.read(step));
        };
        break;
      }
      case "pick": {
        const picked = element("span", "sc-picked");
        const choose = button(COPY.choose, field.label);
        choose.addEventListener("click", () => pickWith(SHORTCUTS_PICK_TARGET, pickTargetArgs(id)));
        body.append(picked, choose);
        focusFirst = choose;
        showField = (step) => {
          const name = field.read(step);
          picked.textContent = name ?? field.unset;
          picked.classList.toggle("is-unset", name === null);
        };
        break;
      }
    }

    const row: StepRow = {
      box,
      first: focusFirst,
      up,
      down,
      show(step, index, item, view) {
        const place = `step ${index + 1} of ${item.name}`;
        title.textContent = `${index + 1}. ${kindName}`;
        box.setAttribute("aria-label", `Step ${index + 1}: ${kindName}`);
        up.setAttribute("aria-label", `${COPY.moveUp}: ${place}`);
        down.setAttribute("aria-label", `${COPY.moveDown}: ${place}`);
        remove.setAttribute("aria-label", `${COPY.remove}: ${place}`);
        up.disabled = index === 0;
        down.disabled = index === item.steps.length - 1;
        showField(step, view);
      },
      drop() {
        for (const field of tracked) {
          fields.delete(field);
        }
        if (stepRows.get(id) === row) {
          stepRows.delete(id);
        }
      },
    };
    stepRows.set(id, row);
    return row;
  }

  // --- Shortcuts and groups --------------------------------------------------

  function addItem(group: number | null, kind: NewItemKind): void {
    const view = shown;
    const menu = view === null ? null : menuOrEverywhere(view, menuId);
    if (view === null || menu === null) {
      return;
    }
    const before = allIds(view);
    void send(SHORTCUTS_ADD_ITEM, addItemArgs(menu.id, group, kind), () => {
      focusNew(before, (id) => itemRows.get(id)?.name);
    });
  }

  function buildItem(first: ItemView): ItemRow {
    const id = first.id;
    const prefix = `sc-item-${id}`;
    const nameId = `${prefix}-name`;
    const iconId = `${prefix}-icon`;
    const emojiId = `${prefix}-emoji`;
    const fullId = `${prefix}-full`;
    const isGroup = first.kind === "group";

    const box = element("div", "sc-item");
    box.setAttribute("role", "group");
    if (isGroup) {
      box.classList.add("is-group");
    }
    const head = element("div", "sc-item-head");
    const picture = element("span", "sc-item-icon");
    picture.setAttribute("aria-hidden", "true");
    const nameInput = element("input", "sc-input");
    nameInput.classList.add("sc-name");
    nameInput.setAttribute("type", "text");
    nameInput.setAttribute("id", nameId);
    nameInput.setAttribute("placeholder", COPY.namePlaceholder);
    nameInput.setAttribute("autocomplete", "off");
    const up = button(COPY.moveUp);
    const down = button(COPY.moveDown);
    const remove = button(COPY.remove);
    up.addEventListener("click", () => move(-1));
    down.addEventListener("click", () => move(1));
    wireRemove(remove, () => [SHORTCUTS_REMOVE_ITEM, removeItemArgs(id)]);
    head.append(picture, labelFor(nameId, COPY.name, "visually-hidden"), nameInput, up, down, remove);
    const summary = element("p", "sc-item-summary");

    const editor = element("div", "sc-item-editor");
    const iconSelect = select(iconId);
    const emojiInput = element("input", "sc-input");
    emojiInput.classList.add("sc-emoji");
    emojiInput.setAttribute("type", "text");
    emojiInput.setAttribute("id", emojiId);
    emojiInput.setAttribute("placeholder", COPY.emojiPlaceholder);
    emojiInput.setAttribute("autocomplete", "off");
    const emojiPair = element("span", "sc-emoji-pair");
    emojiPair.append(labelFor(emojiId, COPY.emoji, "visually-hidden"), emojiInput);
    const iconLine = element("div", "sc-line");
    iconLine.append(labelFor(iconId, COPY.icon), iconSelect, emojiPair);
    editor.append(iconLine);
    box.append(head, summary, editor);

    // The user chose "Emoji…" and hasn't typed one yet: nothing is sent until they do.
    // It stays chosen, however long they take, until Rust holds an emoji or they choose another icon.
    let wantsEmoji = false;
    let drawnGlyphs: string | null = null;
    let drawnIcon: string | null = null;

    function move(by: number): void {
      const place = shown === null ? null : findItem(shown, id);
      if (place === null || movesHeld({ up, down })) {
        return;
      }
      const list = (place.group?.items ?? place.menu.items).map((item) => itemRows.get(item.id));
      holdMoves(list, true);
      void send(SHORTCUTS_MOVE_ITEM, moveItemArgs(id, place.index + by), () => {
        holdMoves(list, false);
        focusMoved(() => itemRows.get(id), by);
      });
    }

    /** The icon a rename sends along: what the icon controls show, or the view's while they show none. */
    function iconNow(): Icon {
      const view = shown;
      const chosen = view === null ? null : iconFromChoice(iconSelect.value, emojiInput.value, view.glyphs);
      const place = view === null ? null : findItem(view, id);
      return chosen ?? place?.item.icon ?? { kind: "none" };
    }

    // Both halves go in one command, so each sends the other's newest value.
    const nameField = textField(nameInput, (value) => [SHORTCUTS_SET_ITEM, setItemArgs(id, value, iconNow())]);
    // Leaving the field empty sends nothing and cancels nothing: a blur also comes
    // when the window stops being the key one, as it does for the system's emoji picker.
    const emojiField = textField(emojiInput, (value) =>
      value.length > 0 ? [SHORTCUTS_SET_ITEM, setItemArgs(id, nameInput.value, { kind: "emoji", value })] : null,
    );
    iconSelect.addEventListener("change", () => {
      const view = shown;
      if (view === null) {
        return;
      }
      if (iconSelect.value === ICON_EMOJI && emojiInput.value === "") {
        wantsEmoji = true;
        setHidden(emojiPair, false);
        emojiInput.focus();
        return;
      }
      wantsEmoji = false;
      const icon = iconFromChoice(iconSelect.value, emojiInput.value, view.glyphs);
      if (icon === null) {
        redraw();
        return;
      }
      void send(SHORTCUTS_SET_ITEM, setItemArgs(id, nameInput.value, icon));
    });

    // What's inside: a shortcut's steps, or a group's shortcuts, and the control that adds one.
    const inside = element("div", isGroup ? "sc-children" : "sc-steps");
    const full = element("p", "sc-limit");
    full.setAttribute("id", fullId);
    const steps = new Map<string, StepRow>();
    const children = new Map<string, ItemRow>();
    const addLine = element("div", "sc-line");
    let adder: HTMLButtonElement | HTMLSelectElement;
    if (isGroup) {
      const add = button(COPY.addShortcut);
      add.addEventListener("click", () => addItem(id, "steps"));
      addLine.append(add);
      adder = add;
    } else {
      const addId = `${prefix}-add-step`;
      const add = select(addId, [
        { value: "", label: COPY.addStepPrompt },
        ...STEP_KINDS.map((kind) => ({ value: kind, label: STEP_KIND_NAMES[kind] })),
      ]);
      add.addEventListener("change", () => {
        const kind = add.value;
        add.value = "";
        const view = shown;
        if (view === null || !isStepKind(kind)) {
          return;
        }
        const before = allIds(view);
        void send(SHORTCUTS_ADD_STEP, addStepArgs(id, kind), () => {
          focusNew(before, (step) => stepRows.get(step)?.first);
        });
      });
      addLine.append(labelFor(addId, COPY.addStep), add);
      adder = add;
    }
    adder.setAttribute("aria-describedby", fullId);
    editor.append(inside, addLine, full);

    const row: ItemRow = {
      box,
      name: nameInput,
      up,
      down,
      show(item, index, count, view) {
        box.setAttribute("aria-label", `${item.kind === "group" ? "Group" : "Shortcut"}: ${item.name}`);
        up.setAttribute("aria-label", `${COPY.moveUp}: ${item.name}`);
        down.setAttribute("aria-label", `${COPY.moveDown}: ${item.name}`);
        remove.setAttribute("aria-label", `${COPY.remove}: ${item.name}`);
        up.disabled = index === 0;
        down.disabled = index === count - 1;
        nameInput.setAttribute("maxlength", String(view.limits.name));
        nameField.show(item.name);
        summary.textContent = itemSummary(item);

        const iconSignature = JSON.stringify(item.icon);
        if (iconSignature !== drawnIcon) {
          drawIcon(picture, item.icon);
          drawnIcon = iconSignature;
        }
        const choices = iconChoices(view.glyphs);
        const glyphSignature = choicesSignature(choices.map((choice) => [choice.value, choice.label]));
        if (glyphSignature !== drawnGlyphs) {
          iconSelect.replaceChildren(...choices.map((choice) => option(choice.value, choice.label)));
          drawnGlyphs = glyphSignature;
        }
        if (item.icon.kind === "emoji") {
          wantsEmoji = false;
        }
        iconSelect.value = wantsEmoji ? ICON_EMOJI : iconChoice(item.icon);
        emojiInput.setAttribute("maxlength", String(view.limits.icon));
        emojiField.show(item.icon.kind === "emoji" ? item.icon.value : "");
        hideKeepingFocus(emojiPair, iconSelect.value !== ICON_EMOJI, iconSelect);

        if (item.kind === "group") {
          const isFull = item.items.length >= view.limits.items;
          adder.disabled = isFull;
          adder.setAttribute("aria-label", `${COPY.addShortcut} to ${item.name}`);
          full.textContent = isFull ? ringFullText(view.limits.items) : "";
          const rows = arrange(inside, children, item.items, rowKey, buildItem, isFull ? nameInput : adder);
          rows.forEach((child, at) => {
            const entry = item.items[at];
            if (entry !== undefined) {
              child.show(entry, at, item.items.length, view);
            }
          });
        } else {
          const isFull = item.steps.length >= view.limits.steps;
          adder.disabled = isFull;
          full.textContent = isFull ? stepsFullText(view.limits.steps) : "";
          const rows = arrange(inside, steps, item.steps, rowKey, buildStep, isFull ? nameInput : adder);
          rows.forEach((step, at) => {
            const entry = item.steps[at];
            if (entry !== undefined) {
              step.show(entry, at, item, view);
            }
          });
        }
      },
      drop() {
        fields.delete(nameField);
        fields.delete(emojiField);
        for (const inner of [...steps.values(), ...children.values()]) {
          inner.drop();
        }
        if (itemRows.get(id) === row) {
          itemRows.delete(id);
        }
      },
    };
    itemRows.set(id, row);
    return row;
  }

  // --- The section's fixed parts ---------------------------------------------

  const section = element("section", "settings-section");
  section.classList.add("shortcuts");
  section.setAttribute("aria-labelledby", "shortcuts-heading");
  const heading = element("h2", "section-title", COPY.title);
  heading.setAttribute("id", "shortcuts-heading");
  const lead = element("p", "section-hint");
  /** Everything but the heading and the lead. Only in the page while the wheel is available. */
  const body = element("div", "sc-body");
  let bodyShown = false;
  section.append(heading, lead);

  const enabled = switchRow("sc-enabled", COPY.enabled, COPY.enabledDetail, (on) => {
    void send(SHORTCUTS_SET_ENABLED, setEnabledArgs(on));
  });

  const hotkeyBox = element("div", "sc-block");
  const hotkeyTitle = element("h3", "sc-title", COPY.hotkey);
  const hotkey = chordEditor(
    "sc-hotkey",
    COPY.hotkey,
    (chord) => send(SHORTCUTS_SET_HOTKEY, setHotkeyArgs(chord)),
    { text: COPY.setHotkey, label: COPY.setHotkeyLabel },
  );
  const binding = liveLine("sc-binding");
  hotkeyBox.append(hotkeyTitle, hotkey.box, binding);

  const accessBox = element("div", "sc-access");
  const accessText = liveLine("sc-access-text");
  const accessRepair = element("p", "sc-hint", COPY.accessRepair);
  const askAccess = button(COPY.askAccess, COPY.askAccessLabel);
  askAccess.addEventListener("click", () => {
    void send(SHORTCUTS_REQUEST_ACCESS, {});
  });
  accessBox.append(accessText, accessRepair, askAccess);

  const opensAt = select("sc-opens-at", OPENS_AT_CHOICES);
  opensAt.addEventListener("change", () => {
    const at = opensAt.value;
    if (!isOpensAt(at)) {
      redraw();
      return;
    }
    void send(SHORTCUTS_SET_OPENS_AT, setOpensAtArgs(at));
  });
  const opensAtRow = element("div", "field");
  opensAtRow.append(labelFor("sc-opens-at", COPY.opensAt), opensAt);

  const plainText = element("p", "sc-warning", COPY.plainText);

  // --- Menus -----------------------------------------------------------------

  const menusBox = element("div", "sc-block");
  const menuSelect = select("sc-menu");
  let drawnMenus: string | null = null;
  menuSelect.addEventListener("change", () => {
    const id = Number(menuSelect.value);
    if (shown === null || !shown.menus.some((menu) => menu.id === id)) {
      redraw();
      return;
    }
    // The other menu's rows go away: send what the user typed in them first.
    flushAll();
    menuId = id;
    redraw();
  });
  const menuRow = element("div", "field");
  menuRow.append(labelFor("sc-menu", COPY.menu), menuSelect);
  const addMenu = button(COPY.addMenu, COPY.addMenuLabel);
  const menusFull = element("p", "sc-limit");
  menusFull.setAttribute("id", "sc-menus-full");
  addMenu.setAttribute("aria-describedby", "sc-menus-full");
  addMenu.addEventListener("click", () => {
    const view = shown;
    if (view === null) {
      return;
    }
    const before = new Set(view.menus.map((menu) => menu.id));
    pickWith(SHORTCUTS_ADD_MENU, {}, () => {
      // Show the app's new menu, if the picker made one.
      const added = shown?.menus.find((menu) => !before.has(menu.id));
      if (added !== undefined) {
        menuId = added.id;
      }
    });
  });
  const removeMenu = button(COPY.removeMenu);
  wireRemove(removeMenu, () => {
    const menu = shown === null ? null : menuOrEverywhere(shown, menuId);
    const isApp = menu !== null && menu.scope.kind !== "everywhere";
    return isApp ? [SHORTCUTS_REMOVE_MENU, removeMenuArgs(menu.id)] : null;
  });
  const menuButtons = element("div", "sc-buttons");
  menuButtons.append(addMenu, removeMenu);
  const withEverywhere = switchRow("sc-menu-everywhere", COPY.withEverywhere, COPY.withEverywhereDetail, (on) => {
    const menu = shown === null ? null : menuOrEverywhere(shown, menuId);
    if (menu === null || menu.scope.kind === "everywhere") {
      redraw();
      return;
    }
    void send(SHORTCUTS_SET_MENU_EVERYWHERE, setMenuEverywhereArgs(menu.id, on));
  });
  const note = liveLine("settings-note");
  menusBox.append(
    element("h3", "sc-title", COPY.menus),
    element("p", "sc-hint", COPY.menusHint),
    menuRow,
    menuButtons,
    menusFull,
    withEverywhere.row,
    note,
  );

  // --- The chosen menu's shortcuts ---------------------------------------------

  const ring = element("div", "sc-block");
  const ringHeading = element("h3", "sc-title");
  const ringList = element("div", "sc-items");
  const ringRows = new Map<string, ItemRow>();
  const ringEmpty = element("p", "sc-hint", COPY.emptyRing);
  const addShortcut = button(COPY.addShortcut);
  const addGroup = button(COPY.addGroup);
  const ringFull = element("p", "sc-limit");
  ringFull.setAttribute("id", "sc-ring-full");
  addShortcut.setAttribute("aria-describedby", "sc-ring-full");
  addGroup.setAttribute("aria-describedby", "sc-ring-full");
  addShortcut.addEventListener("click", () => addItem(null, "steps"));
  addGroup.addEventListener("click", () => addItem(null, "group"));
  const ringButtons = element("div", "sc-buttons");
  ringButtons.append(addShortcut, addGroup);
  ring.append(ringHeading, ringEmpty, ringList, ringButtons, ringFull);

  body.append(enabled.row, hotkeyBox, accessBox, opensAtRow, plainText, menusBox, ring);

  // The status line comes after the section, as the rest of Settings has it.
  const status = liveLine("settings-status");
  status.classList.add("sc-status");
  root.replaceChildren(section, status);

  // Closing Settings sends unsent text, and coming back to it reads the
  // view again: that is how the page learns access was granted in System
  // Settings. (The test DOM has no window unless a test gives it one.)
  const win = doc.defaultView;
  if (win !== null) {
    win.addEventListener("pagehide", flushNow);
    win.addEventListener("focus", reread);
    doc.addEventListener("visibilitychange", () => {
      if (doc.visibilityState === "hidden") {
        flushNow();
      } else {
        reread();
      }
    });
  }

  // --- Commands --------------------------------------------------------------

  /**
   * Queues one command behind the others; resolves once it has answered.
   * `answered` runs then, before the page redraws, and is told how it went.
   * A `quiet` command is a re-read nobody asked for: it leaves the status
   * line alone.
   */
  function send(command: string, args: Args, answered?: (problem: Problem) => void, quiet = false): Promise<void> {
    if (!quiet) {
      error = null;
    }
    pending += 1;
    root.setAttribute("aria-busy", "true");
    const job: Job = { command, args, reply: null };
    waiting.push(job);
    const done = queue.then(() => run(job, quiet)).then((problem) => answered?.(problem));
    queue = done.then(() => {
      pending -= 1;
      if (pending === 0) {
        root.removeAttribute("aria-busy");
        draw();
      }
    });
    return done;
  }

  /**
   * One picker at a time, for "Add an app…" and every "Choose…": asking
   * again while one is open sends nothing.
   */
  function pickWith(command: string, args: Args, answered?: () => void): void {
    if (picking) {
      return;
    }
    picking = true;
    void send(command, args, answered).then(() => {
      picking = false;
    });
  }

  /** Logs that a reply didn't match the contract. Never the reply: it holds what the user typed. */
  function warnShape(command: string): void {
    console.warn(`Grape shortcuts: ${command} answered with a view that doesn't match the contract`);
  }

  /**
   * The text to show for a rejected `command`. A reason that isn't Rust's
   * sentence is logged by its kind only, never its text.
   */
  function rejectionText(command: string, reason: unknown): string {
    if (typeof reason !== "string") {
      const kind = reason instanceof Error ? reason.name : typeof reason;
      console.error(`Grape shortcuts: ${command} failed (${kind})`);
    }
    return shortcutsErrorText(reason);
  }

  /**
   * Sends `job` to Rust, unless it has gone already (`flushNow`), and
   * answers with Rust's reply.
   */
  function begin(job: Job): Promise<unknown> {
    if (job.reply !== null) {
      return job.reply;
    }
    const at = waiting.indexOf(job);
    if (at >= 0) {
      waiting.splice(at, 1);
    }
    let reply: Promise<unknown>;
    try {
      reply = Promise.resolve(callRust(job.command, job.args));
    } catch (reason: unknown) {
      reply = Promise.reject(reason);
    }
    // `run` reads how it went when the job's turn comes. Until then a refusal isn't an unhandled one.
    reply.catch(() => undefined);
    job.reply = reply;
    return reply;
  }

  /**
   * Calls Rust and keeps a good reply. Never rejects: it answers `null`, or
   * the sentence for a command Rust refused or answered unreadably.
   */
  async function run(job: Job, quiet: boolean): Promise<Problem> {
    const command = job.command;
    try {
      const reply = await begin(job);
      if (!isShortcutsView(reply)) {
        warnShape(command);
        if (!quiet) {
          error = SHORTCUTS_ERROR;
        }
        return SHORTCUTS_ERROR;
      } else if (!quiet) {
        shown = reply;
      } else if (shown === null) {
        // The first view, after a load that failed.
        shown = reply;
        error = null;
      } else {
        // A note belongs to the change that made it: a re-read keeps the one on show.
        shown = { ...reply, note: reply.note ?? shown.note };
      }
      return null;
    } catch (reason: unknown) {
      const text = rejectionText(command, reason);
      if (quiet) {
        return text;
      }
      error = text;
      if (command !== SHORTCUTS_GET) {
        await refresh();
      }
      return text;
    }
  }

  /**
   * After a command fails, reads the shortcuts again: Rust may have changed
   * them anyway (a failed save keeps the change). The failed command's
   * message stays, and so does the last good view if this fails too.
   */
  async function refresh(): Promise<void> {
    try {
      const reply = await callRust(SHORTCUTS_GET, {});
      if (isShortcutsView(reply)) {
        shown = { ...reply, note: null };
      } else {
        warnShape(SHORTCUTS_GET);
      }
    } catch (reason: unknown) {
      rejectionText(SHORTCUTS_GET, reason);
    }
  }

  /** Reads the view again when the window comes back, unless a command is about to bring one. */
  function reread(): void {
    if (pending > 0 || (shown !== null && !shown.available)) {
      return;
    }
    void send(SHORTCUTS_GET, {}, undefined, true);
  }

  // --- Drawing ---------------------------------------------------------------

  /** Draws now, unless a command is on its way: its answer redraws, from a newer view. */
  function redraw(): void {
    if (pending === 0) {
      draw();
    }
  }

  function drawMenus(view: ShortcutsView, menu: MenuView): void {
    const choices = menuChoices(view);
    const signature = choicesSignature(choices.map((choice) => [String(choice.id), menuName(choice.scope)]));
    if (signature !== drawnMenus) {
      menuSelect.replaceChildren(...choices.map((choice) => option(String(choice.id), menuName(choice.scope))));
      drawnMenus = signature;
    }
    menuSelect.value = String(menu.id);

    const isFull = view.menus.length >= view.limits.menus;
    addMenu.disabled = isFull;
    menusFull.textContent = isFull ? menusFullText(view.limits.menus) : "";
    const isApp = menu.scope.kind === "app";
    removeMenu.setAttribute("aria-label", `${COPY.removeMenu}: ${menuName(menu.scope)}`);
    hideKeepingFocus(removeMenu, !isApp, menuSelect);
    withEverywhere.input.checked = isApp && menu.withEverywhere;
    hideKeepingFocus(withEverywhere.row, !isApp, menuSelect);
    say(note, view.note ?? "");
  }

  function drawRing(view: ShortcutsView, menu: MenuView): void {
    ringHeading.textContent = ringTitle(menu.scope);
    const isFull = menu.items.length >= view.limits.items;
    addShortcut.disabled = isFull;
    addGroup.disabled = isFull;
    ringFull.textContent = isFull ? ringFullText(view.limits.items) : "";
    setHidden(ringEmpty, menu.items.length > 0);
    const rows = arrange(ringList, ringRows, menu.items, rowKey, buildItem, isFull ? menuSelect : addShortcut);
    rows.forEach((row, at) => {
      const item = menu.items[at];
      if (item !== undefined) {
        row.show(item, at, menu.items.length, view);
      }
    });
  }

  function draw(): void {
    say(status, error ?? "");
    const view = shown;
    if (view === null) {
      return;
    }
    if (bodyShown !== view.available) {
      bodyShown = view.available;
      section.replaceChildren(heading, lead, ...(bodyShown ? [body] : []));
    }
    lead.textContent = view.available ? COPY.lead : COPY.unavailable;
    const menu = view.available ? menuOrEverywhere(view, menuId) : null;
    if (menu === null) {
      return;
    }
    menuId = menu.id;

    enabled.input.checked = view.enabled;
    hotkey.show(view, view.hotkey?.chord ?? null, view.hotkey?.label ?? null);
    say(binding, bindingText(view));

    const denied = view.access === "denied";
    say(accessText, denied ? COPY.accessDenied : COPY.accessGranted);
    setHidden(accessRepair, !denied);
    hideKeepingFocus(askAccess, !denied, enabled.input);
    setHidden(accessBox, view.access === "notNeeded");

    opensAt.value = view.opensAt;
    drawMenus(view, menu);
    drawRing(view, menu);

    const target = wish?.() ?? null;
    wish = null;
    target?.focus();
  }

  void send(SHORTCUTS_GET, {});

  return {
    idle: () => queue,
  };
}
