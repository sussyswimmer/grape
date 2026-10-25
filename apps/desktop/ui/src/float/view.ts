/**
 * The float shelf window (M5, Windows): one rounded glass panel in a
 * transparent tool window that Rust sizes from the item count (280 wide,
 * the panel inset 6 px for its shadow), places, shows and hides without
 * taking focus. The header reads "Shelf · n", moves the window (its title
 * is a `data-tauri-drag-region`) and has a × that empties the shelf. Each
 * row shows a file or folder glyph, the name, "Missing" when the item is
 * gone, and a × that takes it off the shelf without touching the file.
 * From two items a "Drag all" handle sits at the bottom.
 *
 * Pressing a row or "Drag all" and moving `DRAG_THRESHOLD` px asks Rust
 * for an OS drag of those ids (`float_drag_out`), which Rust runs as a
 * copy or a shortcut, never a move. Nothing on the page is HTML-draggable
 * and every `dragstart` is cancelled, so WebView2 never starts a drag of
 * its own. Until `float://drag-ended` arrives the page ignores pointer
 * gestures (see `model.ts`). Double-click or Enter reveals an item, unless
 * the view says it's missing (Rust checks again, as the view can be
 * stale); Delete or Backspace removes the focused one.
 *
 * Rows are keyed by id and only updated, so keyboard focus survives a
 * redraw. When the focused row leaves, focus moves to the next row, else
 * the previous one, else the close button.
 */

import {
  FLOAT_CLEAR,
  FLOAT_DRAG_ENDED,
  FLOAT_DRAG_OUT,
  FLOAT_GET_VIEW,
  FLOAT_REMOVE,
  FLOAT_REVEAL,
  FLOAT_VIEW,
  type FloatDragEnded,
  type FloatItem,
  type FloatKind,
  type FloatView,
  isFloatDragEnded,
  isFloatView,
} from "../shared/events";
import { callQuietly, type InvokeFn, tauriInvoke } from "../shared/ipc";
import { type ListenFn, listenAll, route } from "../shared/listen";
import {
  COPY,
  DRAG_FALLBACK_MS,
  type DragSource,
  displayName,
  dragAllText,
  dragIds,
  dragOver,
  focusAfterRemoval,
  type Gesture,
  hasFooter,
  IDLE,
  move,
  press,
  release,
  removeLabel,
  titleText,
} from "./model";

const SVG_NS = "http://www.w3.org/2000/svg";

/**
 * Grape's own glyphs on a 24 × 24 grid, drawn as round strokes in the
 * current color (float.css sets it).
 */
const GLYPHS = {
  file: ["M7 3h7l5 5v11a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2z", "M14 3v5h5"],
  folder: [
    "M3 7.5A2.5 2.5 0 0 1 5.5 5H9l2 2.5h7.5A2.5 2.5 0 0 1 21 10v7.5a2.5 2.5 0 0 1-2.5 2.5h-13A2.5 2.5 0 0 1 3 17.5z",
  ],
  close: ["M7 7l10 10", "M17 7 7 17"],
  /** Two stacked pages, for "Drag all". */
  stack: ["M9 8h9a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H9a2 2 0 0 1-2-2v-9a2 2 0 0 1 2-2z", "M4 15V6a2 2 0 0 1 2-2h9"],
} as const;

type GlyphName = keyof typeof GLYPHS;

/** One method per `float://…` event. Payloads must already be guarded. */
export interface FloatShelf {
  show(view: FloatView): void;
  dragEnded(payload: FloatDragEnded): void;
}

/** Seams for tests. Production code passes nothing. */
export interface FloatShelfOptions {
  readonly invoke?: InvokeFn;
}

/** The parts of one row the page updates. */
interface RowParts {
  readonly row: HTMLElement;
  readonly glyph: HTMLElement;
  readonly name: HTMLElement;
  readonly missing: HTMLElement;
  readonly remove: HTMLButtonElement;
  /** The glyph drawn, or `null` before the first update. */
  kind: FloatKind | null;
}

/**
 * Mounts the shelf into `root` (the page's `#float-root`, which covers the
 * window) and returns the view the event wiring drives.
 */
export function mountFloat(root: HTMLElement, options: FloatShelfOptions = {}): FloatShelf {
  const doc = root.ownerDocument;
  const callRust = options.invoke ?? tauriInvoke;

  let view: FloatView = { items: [] };
  let gesture: Gesture = IDLE;
  /** The element holding the pressed pointer's capture, if any. */
  let captured: { readonly holder: HTMLElement; readonly pointerId: number } | null = null;
  /** Counts drags, so a late fallback or rejection can't end a newer one. */
  let drags = 0;
  let fallback: ReturnType<typeof setTimeout> | null = null;
  const rows = new Map<number, RowParts>();
  /** The ids of the rows in the list, in list order. */
  let order: number[] = [];

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

  function glyph(name: GlyphName): SVGSVGElement {
    const svg = doc.createElementNS(SVG_NS, "svg");
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("aria-hidden", "true");
    svg.setAttribute("focusable", "false");
    for (const d of GLYPHS[name]) {
      const path = doc.createElementNS(SVG_NS, "path");
      path.setAttribute("d", d);
      svg.append(path);
    }
    return svg;
  }

  function iconButton(className: string, label: string): HTMLButtonElement {
    const node = element("button", "float-icon-button");
    node.classList.add(className);
    node.setAttribute("type", "button");
    labelButton(node, label);
    node.append(glyph("close"));
    return node;
  }

  function labelButton(node: HTMLButtonElement, label: string): void {
    node.setAttribute("aria-label", label);
    node.setAttribute("title", label);
  }

  function setHidden(node: HTMLElement, hidden: boolean): void {
    if (hidden) {
      node.setAttribute("hidden", "");
    } else {
      node.removeAttribute("hidden");
    }
  }

  // --- The panel ---------------------------------------------------------------

  const panel = element("div", "float-panel");

  const header = element("header", "float-header");
  // The title fills the header up to the close button and moves the
  // window: Tauri's drag-region script starts a native move on a press
  // anywhere inside it ("deep"). Rows are never a move region: a file drag
  // can't start during a move.
  const title = element("h1", "float-title");
  title.setAttribute("data-tauri-drag-region", "deep");
  // The script turns the second press of a double-click into "maximize",
  // which this window can't do and isn't allowed to ask for.
  title.addEventListener("mousedown", (event) => {
    if (event.detail > 1) {
      event.stopPropagation();
    }
  });
  const close = iconButton("float-close", COPY.close);
  close.addEventListener("click", () => callQuietly(callRust, FLOAT_CLEAR, {}));
  header.append(title, close);

  const list = element("div", "float-list");
  list.setAttribute("role", "list");
  list.setAttribute("aria-label", COPY.list);

  // "Drag all" is a handle for the mouse only: an OS drag can't start
  // from the keyboard, so it isn't focusable.
  const footer = element("div", "float-footer");
  const dragAll = element("div", "float-drag-all");
  dragAll.setAttribute("draggable", "false");
  const dragAllLabel = element("span", "float-drag-all-label");
  dragAll.append(glyph("stack"), dragAllLabel);
  dragAll.addEventListener("pointerdown", (event) => begin({ kind: "all" }, event, dragAll));
  footer.append(dragAll);

  panel.append(header, list, footer);
  root.replaceChildren(panel);

  // --- Gestures ----------------------------------------------------------------

  // Nothing here is HTML-draggable, and a stray text or image drag is
  // cancelled before WebView2 can start one.
  root.addEventListener("dragstart", (event) => event.preventDefault());

  root.addEventListener("pointermove", (event) => {
    const step = move(gesture, view, event.pointerId, event.clientX, event.clientY, event.buttons);
    setGesture(step.gesture);
    if (step.drag !== null) {
      startDrag(step.drag);
    }
  });
  const lift = (event: PointerEvent): void => setGesture(release(gesture, event.pointerId));
  root.addEventListener("pointerup", lift);
  root.addEventListener("pointercancel", lift);

  function setGesture(next: Gesture): void {
    if (next === gesture) {
      return;
    }
    gesture = next;
    if (next.kind !== "pressed") {
      releaseCapture();
    }
    root.setAttribute("data-gesture", next.kind);
  }

  /**
   * The primary button went down on `source`. The pointer is captured, so
   * its moves still arrive if it leaves `holder` (or the window) on the
   * way to the threshold.
   */
  function begin(source: DragSource, event: PointerEvent, holder: HTMLElement): void {
    if (event.button !== 0 || !event.isPrimary) {
      return;
    }
    const next = press(gesture, view, source, event.pointerId, event.clientX, event.clientY);
    if (next === gesture) {
      return;
    }
    setGesture(next);
    try {
      holder.setPointerCapture(event.pointerId);
      captured = { holder, pointerId: event.pointerId };
    } catch {
      // The pointer is already gone; its next move ends the press.
    }
  }

  function releaseCapture(): void {
    if (captured === null) {
      return;
    }
    const { holder, pointerId } = captured;
    captured = null;
    try {
      holder.releasePointerCapture(pointerId);
    } catch {
      // Already released.
    }
  }

  /** Asks Rust for the drag once; Rust's `float://drag-ended`, a failed call or the fallback ends it. */
  function startDrag(ids: readonly number[]): void {
    drags += 1;
    const drag = drags;
    const finish = (): void => {
      if (drag === drags) {
        endDrag();
      }
    };
    clearFallback();
    fallback = setTimeout(finish, DRAG_FALLBACK_MS);
    try {
      callRust(FLOAT_DRAG_OUT, { ids: [...ids] }).catch(finish);
    } catch {
      finish();
    }
  }

  function endDrag(): void {
    clearFallback();
    setGesture(dragOver(gesture));
  }

  function clearFallback(): void {
    if (fallback !== null) {
      clearTimeout(fallback);
      fallback = null;
    }
  }

  // --- Rows ----------------------------------------------------------------------

  function makeRow(id: number): RowParts {
    const row = element("div", "float-row");
    row.setAttribute("role", "listitem");
    row.setAttribute("tabindex", "0");
    row.setAttribute("draggable", "false");
    row.setAttribute("data-id", String(id));
    const glyphBox = element("span", "float-glyph");
    glyphBox.setAttribute("aria-hidden", "true");
    const name = element("span", "float-name");
    const missing = element("span", "float-missing", COPY.missing);
    const remove = iconButton("float-remove", "");
    row.append(glyphBox, name, missing, remove);

    row.addEventListener("pointerdown", (event) => begin({ kind: "item", id }, event, row));
    row.addEventListener("dblclick", () => reveal(id));
    row.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        if (!event.repeat) {
          reveal(id);
        }
      } else if (event.key === "Delete" || event.key === "Backspace") {
        event.preventDefault();
        callQuietly(callRust, FLOAT_REMOVE, { id });
      }
    });

    // The × is a control of its own: pressing it never starts a drag, and
    // its double-click and keys (Enter clicks it) don't reach the row.
    remove.addEventListener("pointerdown", (event) => event.stopPropagation());
    remove.addEventListener("dblclick", (event) => event.stopPropagation());
    remove.addEventListener("keydown", (event) => event.stopPropagation());
    remove.addEventListener("click", () => callQuietly(callRust, FLOAT_REMOVE, { id }));

    return { row, glyph: glyphBox, name, missing, remove, kind: null };
  }

  /** Asks Rust to show item `id` in Explorer, unless the view says it's gone. */
  function reveal(id: number): void {
    if (view.items.some((item) => item.id === id && item.missing)) {
      return;
    }
    callQuietly(callRust, FLOAT_REVEAL, { id });
  }

  function updateRow(parts: RowParts, item: FloatItem): void {
    const name = displayName(item.name);
    if (parts.name.textContent !== name) {
      parts.name.textContent = name;
    }
    parts.row.setAttribute("title", name);
    parts.row.setAttribute("data-kind", item.kind);
    parts.row.classList.toggle("is-missing", item.missing);
    setHidden(parts.missing, !item.missing);
    labelButton(parts.remove, removeLabel(item.name));
    if (parts.kind !== item.kind) {
      parts.glyph.replaceChildren(glyph(item.kind));
      parts.kind = item.kind;
    }
  }

  /** The id of the row that has focus (the row itself or its ×), or `null`. */
  function focusedId(): number | null {
    const active = doc.activeElement;
    for (const [id, parts] of rows) {
      if (active === parts.row || active === parts.remove) {
        return id;
      }
    }
    return null;
  }

  function drawChrome(): void {
    const count = view.items.length;
    title.textContent = titleText(count);
    setHidden(footer, !hasFooter(count));
    const movable = dragIds(view, { kind: "all" }).length;
    dragAllLabel.textContent = dragAllText(movable);
    dragAll.classList.toggle("is-disabled", movable === 0);
    if (movable === 0) {
      dragAll.setAttribute("aria-disabled", "true");
    } else {
      dragAll.removeAttribute("aria-disabled");
    }
  }

  root.setAttribute("data-gesture", gesture.kind);
  drawChrome();

  return {
    show(next) {
      const focused = focusedId();
      const before = order;
      const ids = next.items.map((item) => item.id);
      const listed = new Set(ids);
      view = next;

      for (const [id, parts] of rows) {
        if (!listed.has(id)) {
          list.removeChild(parts.row);
          rows.delete(id);
        }
      }
      const kept = before.filter((id) => listed.has(id));
      const drawn = next.items.map((item) => {
        let parts = rows.get(item.id);
        if (parts === undefined) {
          parts = makeRow(item.id);
          rows.set(item.id, parts);
        }
        updateRow(parts, item);
        return parts;
      });
      // New items join at the end, so the rows already there keep their
      // place and only the new ones are appended. Moving a row would drop
      // its focus, so the list is rebuilt only if Rust's order ever changes.
      if (kept.every((id, index) => ids[index] === id)) {
        list.append(...drawn.slice(kept.length).map((parts) => parts.row));
      } else {
        list.replaceChildren(...drawn.map((parts) => parts.row));
      }
      order = ids;
      drawChrome();

      if (focused !== null && !listed.has(focused)) {
        const target = focusAfterRemoval(before, ids, focused);
        const parts = target === null ? undefined : rows.get(target);
        (parts?.row ?? close).focus();
      }
    },
    dragEnded() {
      endDrag();
    },
  };
}

/**
 * Listens for `float://view` and `float://drag-ended`, guards each payload
 * and forwards it to `shelf`, then asks Rust for the current view once
 * (`float_view`), so a view sent before the page was listening isn't
 * missed. A view event that arrives while that answer is on its way is
 * at least as new, so the answer is dropped then; a malformed answer is
 * dropped too. Resolves to a function that stops listening and never
 * rejects, so `void wireFloat(shelf)` is safe.
 */
export async function wireFloat(
  shelf: FloatShelf,
  listenFn?: ListenFn,
  invoke: InvokeFn = tauriInvoke,
): Promise<() => void> {
  let heard = 0;
  const stop = await listenAll(
    [
      route(FLOAT_VIEW, isFloatView, (payload) => {
        heard += 1;
        shelf.show(payload);
      }),
      route(FLOAT_DRAG_ENDED, isFloatDragEnded, (payload) => shelf.dragEnded(payload)),
    ],
    "Grape shelf: could not listen for shelf events",
    listenFn,
  );
  const before = heard;
  let answer: unknown;
  try {
    answer = await invoke(FLOAT_GET_VIEW, {});
  } catch (error: unknown) {
    console.warn("Grape shelf: could not ask Rust for the shelf's items", error);
    return stop;
  }
  if (!isFloatView(answer)) {
    console.warn(`Grape shelf: ignored a ${FLOAT_GET_VIEW} answer that doesn't match the contract`);
  } else if (heard === before) {
    shelf.show(answer);
  }
  return stop;
}
