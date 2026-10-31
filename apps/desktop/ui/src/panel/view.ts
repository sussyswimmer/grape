/**
 * The tool panel window (M4, D31): one rounded glass surface in a
 * transparent window that Rust sizes from the request (352 wide, the
 * surface inset 6 px for its shadow), places beside the wheel's spot, and
 * shows and hides. The header shows the tool and the files, the body the
 * tool's one control, and the footer a hint, Cancel and Run.
 *
 * - Compress and Audio Channels: a segmented control of native radios
 *   (arrow keys and screen-reader semantics for free) with a note on the
 *   checked option.
 * - Normalize Volume: a native range input with the value in LUFS.
 * - Merge PDFs: a listbox with `aria-activedescendant` (one Tab stop) plus
 *   Move up and Move down. Rows reorder by pointer (press, move 4 px, the
 *   line marks where the row lands), by Alt+↑/↓, or by the buttons. Nothing
 *   is HTML-draggable and every `dragstart` is cancelled: WebView2 can't do
 *   HTML5 drag and drop behind Grape's drop target (D13).
 *
 * Keys arrive through the host (the document's `keydown`): Enter runs
 * unless a panel button is the target (its own click does its thing), Esc
 * cancels (or only ends a reorder gesture), and Tab stays inside the panel.
 * On Windows the panel opens without the keyboard, so the hint says so
 * until the first click; on gaining focus the main control takes it.
 *
 * The page sends the seq and item ids only, never a path. Rust validates
 * every run and hides the window; the page waits for `panel://close`.
 */

import {
  CHANNELS,
  COMPRESS_PRESETS,
  isPanelClose,
  isPanelOpen,
  PANEL_CANCEL,
  PANEL_CLOSE,
  PANEL_CURRENT,
  PANEL_OPEN,
  PANEL_RUN,
  type PanelChoice,
  type PanelClose,
  type PanelOpen,
} from "../shared/events";
import { type InvokeFn, tauriInvoke } from "../shared/ipc";
import { type ListenFn, listenAll, route } from "../shared/listen";
import {
  CANCEL_LABEL,
  CHANNEL_LABELS,
  CHANNELS_LABEL,
  CLOSE_LABEL,
  countText,
  GENERIC_ERROR,
  HINT_ACTIVE,
  HINT_INACTIVE,
  LOUDER,
  LOUDNESS_LABEL,
  LOUDNESS_NOTE,
  lufsText,
  MERGE_HELP,
  MOVE_DOWN,
  MOVE_UP,
  NOTES,
  ORDER_LABEL,
  PRESET_LABELS,
  QUALITY_LABEL,
  QUIETER,
  runLabel,
  savesAs,
} from "./copy";
import {
  announceMove,
  cancelArgs,
  chooseChannels,
  choosePreset,
  DRAG_THRESHOLD,
  edgeScroll,
  initialState,
  mergedName,
  moveItem,
  moveSelected,
  type PanelState,
  ROW_HEIGHT,
  ROWS_SHOWN,
  runArgs,
  scrollFor,
  select,
  selectBy,
  selectEdge,
  setLufs,
  targetIndex,
} from "./model";

const SVG_NS = "http://www.w3.org/2000/svg";

/** Grape's own glyphs on a 24 × 24 grid, drawn as round strokes in the current color. */
const STROKES = {
  close: ["M7 7l10 10", "M17 7 7 17"],
  up: ["M6 15l6-6 6 6"],
  down: ["M6 9l6 6 6-6"],
} as const;

/** The grip on a merge row: two columns of three dots. */
const GRIP_DOTS: readonly (readonly [number, number])[] = [
  [9, 6],
  [15, 6],
  [9, 12],
  [15, 12],
  [9, 18],
  [15, 18],
];

/** The parts of a key event the panel reads (a `KeyboardEvent`, or a stand-in in tests). */
export interface PanelKey {
  readonly key: string;
  readonly altKey: boolean;
  readonly shiftKey: boolean;
  readonly repeat: boolean;
  readonly isComposing: boolean;
  readonly target: unknown;
  preventDefault(): void;
}

/** Where keys and window focus come from. */
export interface PanelHost {
  onKey(listener: (event: PanelKey) => void): void;
  onFocusChange(listener: (focused: boolean) => void): void;
  hasFocus(): boolean;
}

/** The real host: the document's `keydown`, and the window's `focus` and `blur`. */
export function browserHost(win: Window, doc: Document): PanelHost {
  return {
    onKey(listener) {
      doc.addEventListener("keydown", (event) => listener(event));
    },
    onFocusChange(listener) {
      win.addEventListener("focus", () => listener(true));
      win.addEventListener("blur", () => listener(false));
    },
    hasFocus() {
      return doc.hasFocus();
    },
  };
}

/** For a document without a window: no keys, never focused. */
const NO_HOST: PanelHost = {
  onKey: () => undefined,
  onFocusChange: () => undefined,
  hasFocus: () => false,
};

/** One method per `panel://…` event. Payloads must already be guarded. */
export interface PanelView {
  open(payload: PanelOpen): void;
  close(payload: PanelClose): void;
}

/** Seams for tests. Production code passes the browser host only. */
export interface PanelOptions {
  readonly invoke?: InvokeFn;
  readonly host?: PanelHost;
}

/** A pointer reorder on the merge list: pressed, then lifted once it moved `DRAG_THRESHOLD` px. */
type Gesture =
  | { readonly kind: "idle" }
  | {
      readonly kind: "pressed" | "lifted";
      readonly pointerId: number;
      /** The pressed row's id and its index at the press. */
      readonly id: number;
      readonly start: number;
      readonly x: number;
      readonly y: number;
      /** The list's scroll offset at the press. */
      readonly scroll: number;
      /** Where the row lands if the button comes up now. */
      readonly target: number;
    };

const IDLE: Gesture = { kind: "idle" };

interface Segment {
  readonly label: HTMLElement;
  readonly input: HTMLInputElement;
  readonly value: string;
}

interface RowParts {
  readonly row: HTMLElement;
  readonly position: HTMLElement;
}

type BodyParts =
  | { readonly kind: "segments"; readonly segments: readonly Segment[]; readonly note: HTMLElement }
  | { readonly kind: "loudness"; readonly slider: HTMLInputElement; readonly value: HTMLElement }
  | {
      readonly kind: "merge";
      readonly list: HTMLElement;
      readonly rows: ReadonlyMap<number, RowParts>;
      readonly line: HTMLElement;
      readonly output: HTMLElement;
      readonly up: HTMLButtonElement;
      readonly down: HTMLButtonElement;
      readonly announce: HTMLElement;
    };

interface Parts {
  readonly close: HTMLButtonElement;
  readonly hint: HTMLElement;
  readonly cancel: HTMLButtonElement;
  readonly run: HTMLButtonElement;
  readonly body: BodyParts;
}

/** The value a segmented control shows for this choice, or `null` for the other tools. */
function segmentValue(choice: PanelChoice): string | null {
  switch (choice.tool) {
    case "compress":
      return choice.preset;
    case "audioChannels":
      return choice.channels;
    default:
      return null;
  }
}

function segmentNote(choice: PanelChoice): string {
  switch (choice.tool) {
    case "compress":
      return NOTES.compress[choice.preset];
    case "audioChannels":
      return NOTES.audioChannels[choice.channels];
    default:
      return "";
  }
}

/**
 * Mounts the panel into `root` (the page's `#panel-root`, which covers the
 * window) and returns the view the event wiring drives. The root stays
 * empty and hidden until a request arrives.
 */
export function mountPanel(root: HTMLElement, options: PanelOptions = {}): PanelView {
  const doc = root.ownerDocument;
  const callRust = options.invoke ?? tauriInvoke;
  const view = doc.defaultView;
  const host = options.host ?? (view === null ? NO_HOST : browserHost(view, doc));

  let state: PanelState | null = null;
  let parts: Parts | null = null;
  let gesture: Gesture = IDLE;
  let active = host.hasFocus();

  // --- Elements ----------------------------------------------------------------

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

  function svg(): SVGSVGElement {
    const node = doc.createElementNS(SVG_NS, "svg");
    node.setAttribute("viewBox", "0 0 24 24");
    node.setAttribute("aria-hidden", "true");
    node.setAttribute("focusable", "false");
    return node;
  }

  function strokes(name: keyof typeof STROKES): SVGSVGElement {
    const node = svg();
    for (const d of STROKES[name]) {
      const path = doc.createElementNS(SVG_NS, "path");
      path.setAttribute("d", d);
      node.append(path);
    }
    return node;
  }

  function grip(): SVGSVGElement {
    const node = svg();
    for (const [cx, cy] of GRIP_DOTS) {
      const dot = doc.createElementNS(SVG_NS, "circle");
      dot.setAttribute("cx", String(cx));
      dot.setAttribute("cy", String(cy));
      dot.setAttribute("r", "1.6");
      node.append(dot);
    }
    return node;
  }

  function button(id: string, className: string, text?: string): HTMLButtonElement {
    const node = element("button", className, text);
    node.setAttribute("type", "button");
    node.setAttribute("id", id);
    return node;
  }

  function iconButton(id: string, label: string, glyph: keyof typeof STROKES): HTMLButtonElement {
    const node = button(id, "panel-icon-button");
    node.setAttribute("aria-label", label);
    node.setAttribute("title", label);
    node.append(strokes(glyph));
    return node;
  }

  // --- State -------------------------------------------------------------------

  /** Takes `next` and redraws, also when nothing changed: a refused change puts a native control back. */
  function apply(next: PanelState): void {
    state = next;
    update();
  }

  function run(): void {
    if (state === null || state.phase !== "idle") {
      return;
    }
    const seq = state.request.seq;
    apply({ ...state, phase: "running", error: null });
    let call: Promise<unknown>;
    try {
      call = callRust(PANEL_RUN, runArgs(state));
    } catch (error: unknown) {
      call = Promise.reject(error);
    }
    // On success Rust hides the window and sends panel://close; until then
    // the panel stays `running`, so a second Enter sends nothing.
    call.then(
      () => undefined,
      (reason: unknown) => {
        if (state === null || state.request.seq !== seq || state.phase !== "running") {
          return;
        }
        const error = typeof reason === "string" && reason.length > 0 ? reason : GENERIC_ERROR;
        apply({ ...state, phase: "idle", error });
        if (active) {
          focusMainIfLost();
        }
      },
    );
  }

  function cancel(): void {
    if (state === null || state.phase !== "idle") {
      return;
    }
    const seq = state.request.seq;
    apply({ ...state, phase: "closing" });
    let call: Promise<unknown>;
    try {
      call = callRust(PANEL_CANCEL, cancelArgs(state));
    } catch (error: unknown) {
      call = Promise.reject(error);
    }
    // On success Rust hides the window and sends panel://close. It refuses
    // while a mouse button is down (that Esc belongs to a drag), and the
    // panel then stays open and takes input again.
    call.then(
      () => undefined,
      () => {
        if (state === null || state.request.seq !== seq || state.phase !== "closing") {
          return;
        }
        apply({ ...state, phase: "idle" });
        if (active) {
          focusMainIfLost();
        }
      },
    );
  }

  // --- Focus -------------------------------------------------------------------

  /** The checked radio, the slider or the list. */
  function mainControl(body: BodyParts, current: PanelState): HTMLElement {
    switch (body.kind) {
      case "segments": {
        const value = segmentValue(current.choice);
        const checked = body.segments.find((segment) => segment.value === value) ?? body.segments[0];
        return checked?.input ?? body.note;
      }
      case "loudness":
        return body.slider;
      case "merge":
        return body.list;
    }
  }

  function focusMainIfLost(): void {
    if (state === null || parts === null) {
      return;
    }
    const focused = doc.activeElement;
    if (focused === null || focused === doc.body || focused === root) {
      mainControl(parts.body, state).focus();
    }
  }

  /** Tab order: × → the main control → Move up → Move down → Cancel → Run, skipping disabled buttons. */
  function cycleFocus(backwards: boolean): void {
    if (state === null || parts === null) {
      return;
    }
    const main = mainControl(parts.body, state);
    const candidates: HTMLElement[] = [parts.close, main];
    if (parts.body.kind === "merge") {
      candidates.push(parts.body.up, parts.body.down);
    }
    candidates.push(parts.cancel, parts.run);
    const order = candidates.filter((node) => !isDisabledButton(node));
    if (order.length === 0) {
      return;
    }
    const focused = doc.activeElement;
    const body = parts.body;
    // Any radio of the group stands in the main control's place.
    const atMain = body.kind === "segments" && body.segments.some((segment) => segment.input === focused);
    const index = order.findIndex((node) => node === focused || (atMain && node === main));
    const step = backwards ? -1 : 1;
    const next = index < 0 ? (backwards ? order.length - 1 : 0) : (index + step + order.length) % order.length;
    order[next]?.focus();
  }

  function isDisabledButton(node: HTMLElement): boolean {
    return parts !== null && isOwnButton(node) && (node as HTMLButtonElement).disabled;
  }

  function isOwnButton(target: unknown): boolean {
    if (parts === null) {
      return false;
    }
    const own: unknown[] = [parts.close, parts.cancel, parts.run];
    if (parts.body.kind === "merge") {
      own.push(parts.body.up, parts.body.down);
    }
    return own.includes(target);
  }

  // --- Merge list ----------------------------------------------------------------

  function setGesture(next: Gesture): void {
    gesture = next;
    root.setAttribute("data-gesture", next.kind);
  }

  function releaseCapture(pointerId: number): void {
    if (parts === null || parts.body.kind !== "merge") {
      return;
    }
    const list = parts.body.list;
    try {
      if (list.hasPointerCapture(pointerId)) {
        list.releasePointerCapture(pointerId);
      }
    } catch {
      // Already released.
    }
  }

  /** Ends a reorder gesture without moving anything. */
  function abortGesture(): void {
    if (gesture.kind === "idle") {
      return;
    }
    const { pointerId } = gesture;
    setGesture(IDLE);
    releaseCapture(pointerId);
    update();
  }

  /** Keeps the selected row inside the list's visible rows, from the row math. */
  function revealSelected(): void {
    if (state === null || parts === null || parts.body.kind !== "merge") {
      return;
    }
    const list = parts.body.list;
    const next = scrollFor(state.order.indexOf(state.selected), list.scrollTop);
    if (next !== list.scrollTop) {
      list.scrollTop = next;
    }
  }

  function announce(id: number): void {
    if (state === null || parts === null || parts.body.kind !== "merge") {
      return;
    }
    parts.body.announce.textContent = announceMove(state, id);
  }

  /** Moves the selected file by one (Alt+↑/↓, Move up, Move down). */
  function moveBy(delta: -1 | 1): void {
    if (state === null || gesture.kind !== "idle") {
      return;
    }
    const next = moveSelected(state, delta);
    if (next === state) {
      return;
    }
    apply(next);
    announce(next.selected);
    revealSelected();
  }

  function press(id: number, event: PointerEvent): void {
    if (state === null || parts === null || parts.body.kind !== "merge") {
      return;
    }
    if (state.phase !== "idle" || event.button !== 0 || !event.isPrimary) {
      return;
    }
    abortGesture();
    const list = parts.body.list;
    apply(select(state, id));
    list.focus();
    const start = state.order.indexOf(id);
    setGesture({
      kind: "pressed",
      pointerId: event.pointerId,
      id,
      start,
      x: event.clientX,
      y: event.clientY,
      scroll: list.scrollTop,
      target: start,
    });
  }

  function pointerMove(event: PointerEvent): void {
    if (gesture.kind === "idle" || event.pointerId !== gesture.pointerId) {
      return;
    }
    if (state === null || parts === null || parts.body.kind !== "merge") {
      return;
    }
    // The button came up where the page couldn't see it.
    if ((event.buttons & 1) === 0) {
      abortGesture();
      return;
    }
    const list = parts.body.list;
    const dy = event.clientY - gesture.y;
    let lifted = gesture;
    if (gesture.kind === "pressed") {
      if (Math.hypot(event.clientX - gesture.x, dy) < DRAG_THRESHOLD) {
        return;
      }
      try {
        list.setPointerCapture(gesture.pointerId);
      } catch {
        // The pointer is already gone.
        abortGesture();
        return;
      }
      lifted = { ...gesture, kind: "lifted" };
    }
    const count = state.order.length;
    const scrollTop = list.scrollTop;
    // The pointer's height in the list's visible part, taking the press as the row's middle.
    const pointerY = lifted.start * ROW_HEIGHT + ROW_HEIGHT / 2 - lifted.scroll + dy;
    const scrolled = edgeScroll(pointerY, scrollTop, count);
    if (scrolled !== scrollTop) {
      list.scrollTop = scrolled;
    }
    setGesture({ ...lifted, target: targetIndex(lifted.start, dy, scrolled - lifted.scroll, count) });
    update();
  }

  function pointerUp(event: PointerEvent): void {
    if (gesture.kind === "idle" || event.pointerId !== gesture.pointerId) {
      return;
    }
    const ended = gesture;
    setGesture(IDLE);
    releaseCapture(ended.pointerId);
    if (ended.kind === "lifted" && state !== null && state.order[ended.start] === ended.id) {
      const next = moveItem(state, ended.start, ended.target);
      if (next !== state) {
        apply(next);
        announce(ended.id);
        return;
      }
    }
    update();
  }

  function listKey(event: PanelKey): void {
    if (state === null || gesture.kind !== "idle") {
      return;
    }
    switch (event.key) {
      case "ArrowUp":
      case "ArrowDown":
        event.preventDefault();
        apply(selectBy(state, event.key === "ArrowUp" ? -1 : 1));
        revealSelected();
        return;
      case "Home":
      case "End":
        event.preventDefault();
        apply(selectEdge(state, event.key === "Home" ? "first" : "last"));
        revealSelected();
        return;
      default:
        return;
    }
  }

  // --- Building ------------------------------------------------------------------

  function segmentsBody(
    body: HTMLElement,
    labelText: string,
    options: readonly { readonly value: string; readonly text: string; choose(): PanelState | null }[],
  ): BodyParts {
    const label = element("p", "panel-label", labelText);
    label.setAttribute("id", "panel-choice-label");
    const group = element("div", "panel-segments");
    group.setAttribute("id", "panel-choice");
    group.setAttribute("role", "radiogroup");
    group.setAttribute("aria-labelledby", "panel-choice-label");
    group.setAttribute("aria-describedby", "panel-note");
    const segments = options.map((option): Segment => {
      const box = element("label", "panel-segment");
      const input = element("input", "panel-segment-input");
      input.setAttribute("type", "radio");
      input.setAttribute("name", "panel-choice");
      input.setAttribute("value", option.value);
      input.addEventListener("change", () => {
        const next = input.checked ? option.choose() : null;
        if (next !== null) {
          apply(next);
        } else {
          update();
        }
      });
      box.append(input, element("span", "panel-segment-text", option.text));
      group.append(box);
      return { label: box, input, value: option.value };
    });
    const note = element("p", "panel-note");
    note.setAttribute("id", "panel-note");
    note.setAttribute("aria-live", "polite");
    body.append(label, group, note);
    return { kind: "segments", segments, note };
  }

  function loudnessBody(body: HTMLElement, request: PanelOpen): BodyParts {
    const spec = request.panel;
    const row = element("div", "panel-label-row");
    const label = element("label", "panel-label", LOUDNESS_LABEL);
    label.setAttribute("id", "panel-lufs-label");
    label.setAttribute("for", "panel-lufs");
    const value = element("output", "panel-lufs-value");
    value.setAttribute("id", "panel-lufs-value");
    value.setAttribute("for", "panel-lufs");
    // An <output> is a polite live region; the slider's aria-valuetext
    // already says the value, so a screen reader would read every step twice.
    value.setAttribute("aria-hidden", "true");
    row.append(label, value);

    const slider = element("input", "panel-range");
    slider.setAttribute("id", "panel-lufs");
    slider.setAttribute("type", "range");
    slider.setAttribute("aria-describedby", "panel-note");
    if (spec.tool === "normalizeAudio") {
      // The range first, so the browser never clamps the value into 0..100.
      slider.setAttribute("min", String(spec.minLufs));
      slider.setAttribute("max", String(spec.maxLufs));
      slider.setAttribute("step", String(spec.stepLufs));
      slider.setAttribute("value", String(spec.targetLufs));
    }
    const onSlide = (): void => {
      if (state !== null) {
        apply(setLufs(state, Number(slider.value)));
      }
    };
    slider.addEventListener("input", onSlide);
    slider.addEventListener("change", onSlide);

    const scale = element("div", "panel-scale");
    scale.append(element("span", "panel-scale-end", QUIETER), element("span", "panel-scale-end", LOUDER));
    const note = element("p", "panel-note", LOUDNESS_NOTE);
    note.setAttribute("id", "panel-note");
    body.append(row, slider, scale, note);
    return { kind: "loudness", slider, value };
  }

  function mergeBody(body: HTMLElement, request: PanelOpen): BodyParts {
    const label = element("p", "panel-label", ORDER_LABEL);
    label.setAttribute("id", "panel-order-label");
    const help = element("p", "visually-hidden", MERGE_HELP);
    help.setAttribute("id", "panel-order-help");

    // The list and its drop line share a frame, so the line is placed from the row math alone.
    const frame = element("div", "panel-list-frame");
    const list = element("ul", "panel-list");
    list.setAttribute("id", "panel-order");
    list.setAttribute("role", "listbox");
    list.setAttribute("aria-labelledby", "panel-order-label");
    list.setAttribute("aria-describedby", "panel-order-help");
    list.setAttribute("draggable", "false");
    list.tabIndex = 0;
    const rows = new Map<number, RowParts>();
    for (const item of request.items) {
      const row = element("li", "panel-row");
      row.setAttribute("id", `panel-item-${item.id}`);
      row.setAttribute("role", "option");
      row.setAttribute("draggable", "false");
      row.setAttribute("title", item.name);
      row.setAttribute("aria-setsize", String(request.items.length));
      const gripBox = element("span", "panel-grip");
      gripBox.setAttribute("aria-hidden", "true");
      gripBox.append(grip());
      const position = element("span", "panel-position");
      position.setAttribute("aria-hidden", "true");
      row.append(gripBox, position, element("span", "panel-name", item.name));
      row.addEventListener("pointerdown", (event) => press(item.id, event));
      rows.set(item.id, { row, position });
    }
    list.addEventListener("lostpointercapture", (event) => {
      if (gesture.kind !== "idle" && event.pointerId === gesture.pointerId) {
        abortGesture();
      }
    });
    list.addEventListener("scroll", () => {
      if (gesture.kind === "lifted") {
        update();
      }
    });
    const line = element("div", "panel-drop-line");
    line.setAttribute("aria-hidden", "true");
    line.hidden = true;
    frame.append(list, line);

    const tools = element("div", "panel-list-tools");
    const output = element("p", "panel-output");
    output.setAttribute("id", "panel-output");
    const up = iconButton("panel-move-up", MOVE_UP, "up");
    up.setAttribute("aria-keyshortcuts", "Alt+ArrowUp");
    up.addEventListener("click", () => moveBy(-1));
    const down = iconButton("panel-move-down", MOVE_DOWN, "down");
    down.setAttribute("aria-keyshortcuts", "Alt+ArrowDown");
    down.addEventListener("click", () => moveBy(1));
    tools.append(output, up, down);

    const announcer = element("div", "visually-hidden");
    announcer.setAttribute("id", "panel-announce");
    announcer.setAttribute("role", "status");

    body.append(label, help, frame, tools, announcer);
    return { kind: "merge", list, rows, line, output, up, down, announce: announcer };
  }

  function build(request: PanelOpen): Parts {
    const surface = element("div", "panel-surface");
    surface.setAttribute("role", "dialog");
    surface.setAttribute("aria-labelledby", "panel-title");
    surface.setAttribute("aria-describedby", "panel-count panel-hint");

    const header = element("header", "panel-header");
    const title = element("h1", "panel-title", request.title);
    title.setAttribute("id", "panel-title");
    const count = element("p", "panel-count", countText(request));
    count.setAttribute("id", "panel-count");
    const [only] = request.items;
    if (request.items.length === 1 && only !== undefined) {
      count.setAttribute("title", only.name);
    }
    const close = iconButton("panel-close", CLOSE_LABEL, "close");
    close.addEventListener("click", cancel);
    header.append(title, count, close);

    const content = element("div", "panel-body");
    let body: BodyParts;
    switch (request.panel.tool) {
      case "compress":
        body = segmentsBody(
          content,
          QUALITY_LABEL,
          COMPRESS_PRESETS.map((preset) => ({
            value: preset,
            text: PRESET_LABELS[preset],
            choose: () => (state === null ? null : choosePreset(state, preset)),
          })),
        );
        break;
      case "audioChannels":
        body = segmentsBody(
          content,
          CHANNELS_LABEL,
          CHANNELS.map((channels) => ({
            value: channels,
            text: CHANNEL_LABELS[channels],
            choose: () => (state === null ? null : chooseChannels(state, channels)),
          })),
        );
        break;
      case "normalizeAudio":
        body = loudnessBody(content, request);
        break;
      case "mergePDF":
        body = mergeBody(content, request);
        break;
    }

    const footer = element("footer", "panel-footer");
    const hint = element("p", "panel-hint");
    hint.setAttribute("id", "panel-hint");
    const cancelButton = button("panel-cancel", "panel-button", CANCEL_LABEL);
    cancelButton.addEventListener("click", cancel);
    const runButton = button("panel-run", "panel-button", runLabel(request.panel.tool));
    runButton.classList.add("is-primary");
    runButton.addEventListener("click", run);
    footer.append(hint, cancelButton, runButton);

    surface.append(header, content, footer);
    root.replaceChildren(surface);
    return { close, hint, cancel: cancelButton, run: runButton, body };
  }

  // --- Drawing -------------------------------------------------------------------

  function update(): void {
    root.setAttribute("data-active", String(active));
    if (state === null || parts === null) {
      return;
    }
    const current = state;
    const busy = current.phase !== "idle";
    root.setAttribute("data-state", current.phase);
    parts.close.disabled = busy;
    parts.cancel.disabled = busy;
    parts.run.disabled = busy;
    parts.hint.textContent = current.error ?? (active ? HINT_ACTIVE : HINT_INACTIVE);
    parts.hint.classList.toggle("is-error", current.error !== null);

    const body = parts.body;
    switch (body.kind) {
      case "segments": {
        const value = segmentValue(current.choice);
        for (const segment of body.segments) {
          const checked = segment.value === value;
          segment.input.checked = checked;
          segment.label.classList.toggle("is-checked", checked);
        }
        body.note.textContent = segmentNote(current.choice);
        break;
      }
      case "loudness": {
        if (current.choice.tool === "normalizeAudio") {
          const target = current.choice.targetLufs;
          const text = lufsText(target);
          if (body.slider.value !== String(target)) {
            body.slider.value = String(target);
          }
          body.slider.setAttribute("aria-valuetext", text);
          body.value.textContent = text;
        }
        break;
      }
      case "merge":
        drawMerge(body, current, busy);
        break;
    }
  }

  function drawMerge(body: Extract<BodyParts, { kind: "merge" }>, current: PanelState, busy: boolean): void {
    const { list, rows } = body;
    const drawn = current.order.flatMap((id) => {
      const row = rows.get(id);
      return row === undefined ? [] : [row];
    });
    const inPlace = drawn.every((row, index) => list.children[index] === row.row);
    if (!inPlace || list.children.length !== drawn.length) {
      list.replaceChildren(...drawn.map((row) => row.row));
    }
    const lifted = gesture.kind === "lifted" ? gesture : null;
    current.order.forEach((id, index) => {
      const row = rows.get(id);
      if (row === undefined) {
        return;
      }
      const selected = id === current.selected;
      row.position.textContent = String(index + 1);
      row.row.setAttribute("aria-posinset", String(index + 1));
      row.row.setAttribute("aria-selected", String(selected));
      row.row.classList.toggle("is-selected", selected);
      row.row.classList.toggle("is-lifted", lifted !== null && lifted.id === id);
    });
    list.setAttribute("aria-activedescendant", `panel-item-${current.selected}`);

    const name = mergedName(current) ?? "";
    body.output.textContent = savesAs(name);
    body.output.setAttribute("title", name);

    const index = current.order.indexOf(current.selected);
    body.up.disabled = busy || index <= 0;
    body.down.disabled = busy || index < 0 || index >= current.order.length - 1;
    // A Move button that just reached its end hands the keyboard to the list.
    const focused = doc.activeElement;
    if (!busy && ((focused === body.up && body.up.disabled) || (focused === body.down && body.down.disabled))) {
      list.focus();
    }

    if (lifted === null || lifted.target === lifted.start) {
      body.line.hidden = true;
      return;
    }
    // Above the row it lands on when moving up, below it when moving down.
    const edge = lifted.target < lifted.start ? lifted.target : lifted.target + 1;
    const shown = Math.min(current.order.length, ROWS_SHOWN) * ROW_HEIGHT;
    const top = Math.min(shown - 2, Math.max(0, edge * ROW_HEIGHT - list.scrollTop - 1));
    body.line.style.setProperty("top", `${top}px`);
    body.line.hidden = false;
  }

  // --- Events on the root (it lives as long as the page) ---------------------------

  root.replaceChildren();
  root.hidden = true;
  setGesture(IDLE);
  update();

  // Nothing here is HTML-draggable, and a stray text or image drag is
  // cancelled before WebView2 can start one. No context menu either.
  root.addEventListener("dragstart", (event) => event.preventDefault());
  root.addEventListener("contextmenu", (event) => event.preventDefault());
  root.addEventListener("pointermove", pointerMove);
  root.addEventListener("pointerup", pointerUp);
  root.addEventListener("pointercancel", (event) => {
    if (gesture.kind !== "idle" && event.pointerId === gesture.pointerId) {
      abortGesture();
    }
  });

  host.onKey((event) => {
    if (state === null || parts === null || event.isComposing) {
      return;
    }
    switch (event.key) {
      case "Escape":
        if (gesture.kind !== "idle") {
          event.preventDefault();
          abortGesture();
        } else if (!event.repeat) {
          event.preventDefault();
          cancel();
        }
        return;
      case "Enter":
        // A panel button's own click (Enter on Cancel cancels).
        if (isOwnButton(event.target)) {
          return;
        }
        event.preventDefault();
        if (!event.repeat && gesture.kind === "idle") {
          run();
        }
        return;
      case "Tab":
        event.preventDefault();
        cycleFocus(event.shiftKey);
        return;
      default:
        break;
    }
    if (parts.body.kind !== "merge") {
      return;
    }
    if (event.altKey && (event.key === "ArrowUp" || event.key === "ArrowDown")) {
      event.preventDefault();
      moveBy(event.key === "ArrowUp" ? -1 : 1);
    } else if (!event.altKey && event.target === parts.body.list) {
      listKey(event);
    }
  });

  host.onFocusChange((focused) => {
    active = focused;
    update();
    if (focused) {
      focusMainIfLost();
    }
  });

  return {
    open(payload) {
      if (gesture.kind !== "idle") {
        const { pointerId } = gesture;
        releaseCapture(pointerId);
        setGesture(IDLE);
      }
      state = initialState(payload);
      parts = build(payload);
      root.setAttribute("data-tool", payload.panel.tool);
      root.hidden = false;
      active = host.hasFocus();
      update();
      if (active) {
        mainControl(parts.body, state).focus();
      }
    },
    close(payload) {
      if (state === null || payload.seq !== state.request.seq) {
        return;
      }
      if (gesture.kind !== "idle") {
        releaseCapture(gesture.pointerId);
        setGesture(IDLE);
      }
      state = null;
      parts = null;
      root.replaceChildren();
      root.hidden = true;
      root.removeAttribute("data-tool");
      root.removeAttribute("data-state");
    },
  };
}

/** Test seam for `wirePanel`. */
export interface WireOptions {
  readonly listen?: ListenFn;
  readonly invoke?: InvokeFn;
}

/**
 * Listens for `panel://open` and `panel://close`, guards each payload and
 * forwards it to `view`, then asks Rust once for the open request
 * (`panel_current`): after a reload, or for a request sent before the page
 * listened. The answer is shown only if it's newer than every seq an event
 * already brought, so it can't bring back a panel that closed while it was
 * on its way. `null`, a rejection or a malformed answer does nothing.
 * Resolves to a function that stops listening and never rejects, so
 * `void wirePanel(view)` is safe.
 */
export async function wirePanel(view: PanelView, options: WireOptions = {}): Promise<() => void> {
  const invoke = options.invoke ?? tauriInvoke;
  let newest = 0;
  const stop = await listenAll(
    [
      route(PANEL_OPEN, isPanelOpen, (payload) => {
        newest = Math.max(newest, payload.seq);
        view.open(payload);
      }),
      route(PANEL_CLOSE, isPanelClose, (payload) => {
        newest = Math.max(newest, payload.seq);
        view.close(payload);
      }),
    ],
    "Grape panel: could not listen for panel events",
    options.listen,
  );
  let answer: unknown;
  try {
    answer = await invoke(PANEL_CURRENT, {});
  } catch (error: unknown) {
    console.warn("Grape panel: could not ask Rust for the open panel", error);
    return stop;
  }
  if (answer === null) {
    return stop;
  }
  if (!isPanelOpen(answer)) {
    console.warn(`Grape panel: ignored a ${PANEL_CURRENT} answer that doesn't match the contract`);
  } else if (answer.seq > newest) {
    view.open(answer);
  }
  return stop;
}
