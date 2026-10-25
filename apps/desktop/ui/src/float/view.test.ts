import { afterEach, describe, expect, it, vi } from "vitest";
import {
  FLOAT_CLEAR,
  FLOAT_DRAG_ENDED,
  FLOAT_DRAG_OUT,
  FLOAT_GET_VIEW,
  FLOAT_REMOVE,
  FLOAT_REVEAL,
  FLOAT_VIEW,
  type FloatView,
} from "../shared/events";
import type { InvokeFn } from "../shared/ipc";
import type { ListenFn } from "../shared/listen";
import {
  byClass,
  byTag,
  descendants,
  type FakeDocument,
  type FakeElement,
  fakeRoot,
  oneByClass,
} from "../test-support/fake-dom";
import { fixtureJson, floatFixture } from "../test-support/fixtures";
import { DRAG_FALLBACK_MS } from "./model";
import { type FloatShelf, mountFloat, wireFloat } from "./view";

// photo.jpg (1, file), Invoices (2, folder), notes.txt (3, file, missing).
const shelf = floatFixture("float-view.json");
const empty = floatFixture("float-view-empty.json");

interface Harness {
  readonly doc: FakeDocument;
  readonly root: FakeElement;
  readonly view: FloatShelf;
  readonly invoke: ReturnType<typeof vi.fn<InvokeFn>>;
}

function harness(answer: InvokeFn = () => Promise.resolve(null)): Harness {
  const { doc, root } = fakeRoot();
  const invoke = vi.fn<InvokeFn>(answer);
  const view = mountFloat(root as unknown as HTMLElement, { invoke });
  return { doc, root, view, invoke };
}

function shown(view: FloatView = shelf, answer?: InvokeFn): Harness {
  const h = harness(answer);
  h.view.show(view);
  return h;
}

function rows(root: FakeElement): FakeElement[] {
  return byClass(root, "float-row");
}

function row(root: FakeElement, id: number): FakeElement {
  const found = rows(root).filter((node) => node.getAttribute("data-id") === String(id));
  const [only] = found;
  if (found.length !== 1 || only === undefined) throw new Error(`expected one row ${id}`);
  return only;
}

function closeButton(root: FakeElement): FakeElement {
  return oneByClass(root, "float-close");
}

function dragAll(root: FakeElement): FakeElement {
  return oneByClass(root, "float-drag-all");
}

function isHidden(element: FakeElement): boolean {
  return element.hasAttribute("hidden");
}

function commands(h: Harness): unknown[][] {
  return h.invoke.mock.calls.map((call) => [...call]);
}

/** A primary-button press at (x, y). */
function down(target: FakeElement, x = 20, y = 20, fields: Record<string, unknown> = {}): void {
  target.dispatch("pointerdown", {
    button: 0,
    buttons: 1,
    isPrimary: true,
    pointerId: 1,
    clientX: x,
    clientY: y,
    ...fields,
  });
}

/** A move to (x, y) with the primary button held (or `buttons`). */
function moveTo(target: FakeElement, x: number, y: number, buttons = 1): void {
  target.dispatch("pointermove", { buttons, pointerId: 1, clientX: x, clientY: y });
}

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("mountFloat", () => {
  it("builds the header, an empty list and a hidden Drag all row", () => {
    const { root } = harness();
    const panel = oneByClass(root, "float-panel");
    expect(root.children).toEqual([panel]);
    const title = oneByClass(root, "float-title");
    expect(title.localName).toBe("h1");
    expect(title.textContent).toBe("Shelf");
    const close = closeButton(root);
    expect(close.localName).toBe("button");
    expect(close.getAttribute("type")).toBe("button");
    expect(close.getAttribute("aria-label")).toBe("Close the shelf");
    const list = oneByClass(root, "float-list");
    expect(list.getAttribute("role")).toBe("list");
    expect(list.getAttribute("aria-label")).toBe("Items on the shelf");
    expect(list.children).toEqual([]);
    expect(isHidden(oneByClass(root, "float-footer"))).toBe(true);
    expect(root.getAttribute("data-gesture")).toBe("idle");
  });

  it("makes only the title a window-move region, and never the close button", () => {
    const { root } = harness();
    const regions = descendants(root).filter((node) => node.hasAttribute("data-tauri-drag-region"));
    expect(regions).toEqual([oneByClass(root, "float-title")]);
    expect(regions[0]?.getAttribute("data-tauri-drag-region")).toBe("deep");
    expect(oneByClass(root, "float-header").children).toContain(closeButton(root));
  });

  it("keeps a double-click on the title from Tauri's maximize, but not a single press", () => {
    const { root } = harness();
    const bubbled: number[] = [];
    root.addEventListener("mousedown", (event) => bubbled.push(Number(event["detail"])));
    const title = oneByClass(root, "float-title");
    title.dispatch("mousedown", { detail: 1, button: 0 });
    title.dispatch("mousedown", { detail: 2, button: 0 });
    expect(bubbled).toEqual([1]);
  });
});

describe("show", () => {
  it("lists each item: a focusable row with its glyph, name and × button", () => {
    const { root } = shown();
    expect(rows(root).map((node) => node.getAttribute("data-id"))).toEqual(["1", "2", "3"]);
    for (const node of rows(root)) {
      expect(node.getAttribute("role")).toBe("listitem");
      expect(node.getAttribute("tabindex")).toBe("0");
    }
    const photo = row(root, 1);
    expect(oneByClass(photo, "float-name").textContent).toBe("photo.jpg");
    expect(photo.getAttribute("title")).toBe("photo.jpg");
    expect(photo.getAttribute("data-kind")).toBe("file");
    expect(oneByClass(photo, "float-glyph").getAttribute("aria-hidden")).toBe("true");
    expect(byTag(oneByClass(photo, "float-glyph"), "svg")).toHaveLength(1);
    const remove = oneByClass(photo, "float-remove");
    expect(remove.localName).toBe("button");
    expect(remove.getAttribute("aria-label")).toBe("Remove photo.jpg");
    expect(row(root, 2).getAttribute("data-kind")).toBe("folder");
  });

  it("draws file and folder glyphs differently", () => {
    const { root } = shown();
    const paths = (id: number): (string | null)[] =>
      byTag(oneByClass(row(root, id), "float-glyph"), "path").map((node) => node.getAttribute("d"));
    expect(paths(1)).not.toEqual(paths(2));
    expect(paths(1)).toEqual(paths(3));
  });

  it("dims a missing item and says so", () => {
    const { root } = shown();
    const notes = row(root, 3);
    expect(notes.classList.contains("is-missing")).toBe(true);
    const label = oneByClass(notes, "float-missing");
    expect(label.textContent).toBe("Missing");
    expect(isHidden(label)).toBe(false);
    expect(row(root, 1).classList.contains("is-missing")).toBe(false);
    expect(isHidden(oneByClass(row(root, 1), "float-missing"))).toBe(true);
  });

  it("counts the items in the title and offers Drag all for those still there", () => {
    const { root, view } = shown();
    expect(oneByClass(root, "float-title").textContent).toBe("Shelf · 3");
    expect(isHidden(oneByClass(root, "float-footer"))).toBe(false);
    expect(dragAll(root).textContent).toBe("Drag all 2");
    expect(dragAll(root).hasAttribute("aria-disabled")).toBe(false);

    view.show({ items: shelf.items.map((item) => ({ ...item, missing: true })) });
    expect(dragAll(root).textContent).toBe("Nothing to drag");
    expect(dragAll(root).getAttribute("aria-disabled")).toBe("true");
    expect(dragAll(root).classList.contains("is-disabled")).toBe(true);
  });

  it("shows Drag all from two items only", () => {
    const { root, view } = shown({ items: shelf.items.slice(0, 1) });
    expect(isHidden(oneByClass(root, "float-footer"))).toBe(true);
    view.show({ items: shelf.items.slice(0, 2) });
    expect(isHidden(oneByClass(root, "float-footer"))).toBe(false);
    view.show(empty);
    expect(rows(root)).toEqual([]);
    expect(oneByClass(root, "float-title").textContent).toBe("Shelf");
    expect(isHidden(oneByClass(root, "float-footer"))).toBe(true);
  });

  it("makes nothing HTML-draggable", () => {
    const { root } = shown();
    const draggable = descendants(root).filter((node) => node.getAttribute("draggable") === "true");
    expect(draggable).toEqual([]);
    expect(row(root, 1).getAttribute("draggable")).toBe("false");
  });

  it("sets names with textContent, never markup", () => {
    const { root } = shown({ items: [{ id: 4, name: "<b>x</b>", kind: "file", missing: false }] });
    const name = oneByClass(row(root, 4), "float-name");
    expect(name.textContent).toBe("<b>x</b>");
    expect(name.children).toEqual([]);
  });

  it("keeps each row's element across redraws, so focus survives", () => {
    const { doc, root, view } = shown();
    const photo = row(root, 1);
    photo.focus();
    view.show({ items: [...shelf.items, { id: 4, name: "song.mp3", kind: "file", missing: false }] });
    expect(row(root, 1)).toBe(photo);
    expect(doc.activeElement).toBe(photo);
    expect(rows(root).map((node) => node.getAttribute("data-id"))).toEqual(["1", "2", "3", "4"]);
    view.show({ items: shelf.items.map((item) => (item.id === 3 ? { ...item, missing: false } : item)) });
    expect(row(root, 1)).toBe(photo);
    expect(row(root, 3).classList.contains("is-missing")).toBe(false);
  });

  it("follows Rust's order if it ever changes", () => {
    const { root, view } = shown();
    view.show({ items: [...shelf.items].reverse() });
    expect(rows(root).map((node) => node.getAttribute("data-id"))).toEqual(["3", "2", "1"]);
  });

  it("moves focus to the next row when the focused one leaves", () => {
    const { doc, root, view } = shown();
    row(root, 2).focus();
    view.show({ items: shelf.items.filter((item) => item.id !== 2) });
    expect(doc.activeElement).toBe(row(root, 3));
  });

  it("moves focus to the previous row, then to the close button", () => {
    const { doc, root, view } = shown();
    oneByClass(row(root, 3), "float-remove").focus();
    view.show({ items: shelf.items.slice(0, 2) });
    expect(doc.activeElement).toBe(row(root, 2));
    view.show({ items: shelf.items.slice(0, 1) });
    expect(doc.activeElement).toBe(row(root, 1));
    view.show(empty);
    expect(doc.activeElement).toBe(closeButton(root));
  });

  it("leaves focus alone when another row leaves", () => {
    const { doc, root, view } = shown();
    const close = closeButton(root);
    close.focus();
    view.show({ items: shelf.items.slice(1) });
    expect(doc.activeElement).toBe(close);
    const invoices = row(root, 2);
    invoices.focus();
    view.show({ items: shelf.items.slice(1, 2) });
    expect(doc.activeElement).toBe(invoices);
  });
});

describe("buttons and keys", () => {
  it("the header's × empties the shelf", () => {
    const h = shown();
    closeButton(h.root).dispatch("click");
    expect(commands(h)).toEqual([[FLOAT_CLEAR, {}]]);
  });

  it("a row's × takes that item off the shelf", () => {
    const h = shown();
    oneByClass(row(h.root, 2), "float-remove").dispatch("click");
    expect(commands(h)).toEqual([[FLOAT_REMOVE, { id: 2 }]]);
  });

  it("double-click or Enter on a row reveals it", () => {
    const h = shown();
    row(h.root, 1).dispatch("dblclick");
    const enter = row(h.root, 2).dispatch("keydown", { key: "Enter" });
    expect(enter.defaultPrevented).toBe(true);
    row(h.root, 2).dispatch("keydown", { key: "Enter", repeat: true });
    expect(commands(h)).toEqual([
      [FLOAT_REVEAL, { id: 1 }],
      [FLOAT_REVEAL, { id: 2 }],
    ]);
  });

  it("Delete or Backspace on a row removes it", () => {
    const h = shown();
    row(h.root, 1).dispatch("keydown", { key: "Delete" });
    const back = row(h.root, 3).dispatch("keydown", { key: "Backspace" });
    expect(back.defaultPrevented).toBe(true);
    row(h.root, 2).dispatch("keydown", { key: "a" });
    expect(commands(h)).toEqual([
      [FLOAT_REMOVE, { id: 1 }],
      [FLOAT_REMOVE, { id: 3 }],
    ]);
  });

  it("keys and double-clicks on a row's × stay with the ×", () => {
    const h = shown();
    const remove = oneByClass(row(h.root, 1), "float-remove");
    remove.dispatch("keydown", { key: "Enter" });
    remove.dispatch("keydown", { key: "Delete" });
    remove.dispatch("dblclick");
    expect(commands(h)).toEqual([]);
  });

  it("swallows a failing invoke", async () => {
    const h = shown(shelf, () => Promise.reject(new Error("no ipc")));
    expect(() => closeButton(h.root).dispatch("click")).not.toThrow();
    expect(() => row(h.root, 1).dispatch("dblclick")).not.toThrow();
    await Promise.resolve();
  });
});

describe("drag out", () => {
  it("starts one drag of the row's item once the press has moved 4 px", () => {
    const h = shown();
    const photo = row(h.root, 1);
    down(photo, 20, 20);
    expect(h.root.getAttribute("data-gesture")).toBe("pressed");
    expect(photo.hasPointerCapture(1)).toBe(true);
    moveTo(photo, 22, 21);
    expect(commands(h)).toEqual([]);
    moveTo(photo, 24, 20);
    expect(commands(h)).toEqual([[FLOAT_DRAG_OUT, { ids: [1] }]]);
    expect(h.root.getAttribute("data-gesture")).toBe("dragging");
    expect(photo.hasPointerCapture(1)).toBe(false);
    moveTo(h.root, 60, 60);
    moveTo(h.root, 90, 90);
    expect(commands(h)).toHaveLength(1);
  });

  it("Drag all drags every item that's still there", () => {
    const h = shown();
    down(dragAll(h.root));
    moveTo(dragAll(h.root), 20, 30);
    expect(commands(h)).toEqual([[FLOAT_DRAG_OUT, { ids: [1, 2] }]]);
  });

  it("never drags a missing row, and never from a press on a ×", () => {
    const h = shown();
    down(row(h.root, 3));
    moveTo(row(h.root, 3), 40, 40);
    expect(h.root.getAttribute("data-gesture")).toBe("idle");
    down(oneByClass(row(h.root, 1), "float-remove"));
    moveTo(h.root, 40, 40);
    expect(commands(h)).toEqual([]);
    expect(h.root.getAttribute("data-gesture")).toBe("idle");
  });

  it("only the primary button presses", () => {
    const h = shown();
    down(row(h.root, 1), 20, 20, { button: 2, buttons: 2 });
    down(row(h.root, 1), 20, 20, { isPrimary: false });
    moveTo(row(h.root, 1), 40, 40);
    expect(commands(h)).toEqual([]);
  });

  it("a press ends when the button comes up, seen or not", () => {
    const h = shown();
    const photo = row(h.root, 1);
    down(photo);
    photo.dispatch("pointerup", { pointerId: 1 });
    expect(h.root.getAttribute("data-gesture")).toBe("idle");
    expect(photo.hasPointerCapture(1)).toBe(false);
    down(photo);
    photo.dispatch("pointercancel", { pointerId: 1 });
    expect(h.root.getAttribute("data-gesture")).toBe("idle");
    down(photo);
    moveTo(photo, 40, 40, 0);
    expect(h.root.getAttribute("data-gesture")).toBe("idle");
    expect(commands(h)).toEqual([]);
  });

  it("ignores every gesture until drag-ended arrives, then takes them again", () => {
    const h = shown();
    down(row(h.root, 1));
    moveTo(row(h.root, 1), 40, 20);
    // WebView2 sends no mouseup after the OS drag; whatever does arrive changes nothing.
    h.root.dispatch("pointerup", { pointerId: 1 });
    down(row(h.root, 2));
    moveTo(row(h.root, 2), 80, 80);
    expect(commands(h)).toHaveLength(1);
    expect(h.root.getAttribute("data-gesture")).toBe("dragging");

    h.view.dragEnded({ outcome: "copied" });
    expect(h.root.getAttribute("data-gesture")).toBe("idle");
    down(row(h.root, 2));
    moveTo(row(h.root, 2), 40, 20);
    expect(commands(h)).toEqual([
      [FLOAT_DRAG_OUT, { ids: [1] }],
      [FLOAT_DRAG_OUT, { ids: [2] }],
    ]);
  });

  it("gives up waiting for drag-ended after ten seconds", () => {
    vi.useFakeTimers();
    const h = shown();
    down(row(h.root, 1));
    moveTo(row(h.root, 1), 40, 20);
    vi.advanceTimersByTime(DRAG_FALLBACK_MS - 1);
    expect(h.root.getAttribute("data-gesture")).toBe("dragging");
    vi.advanceTimersByTime(1);
    expect(h.root.getAttribute("data-gesture")).toBe("idle");
  });

  it("a drag-ended clears the fallback, so it can't end the next drag early", () => {
    vi.useFakeTimers();
    const h = shown();
    down(row(h.root, 1));
    moveTo(row(h.root, 1), 40, 20);
    vi.advanceTimersByTime(DRAG_FALLBACK_MS / 2);
    h.view.dragEnded({ outcome: "cancelled" });
    down(row(h.root, 2));
    moveTo(row(h.root, 2), 40, 20);
    vi.advanceTimersByTime(DRAG_FALLBACK_MS - 1);
    expect(h.root.getAttribute("data-gesture")).toBe("dragging");
  });

  it("takes gestures again at once when the drag command fails", async () => {
    const h = shown(shelf, () => Promise.reject(new Error("no ipc")));
    down(row(h.root, 1));
    moveTo(row(h.root, 1), 40, 20);
    expect(h.root.getAttribute("data-gesture")).toBe("dragging");
    await Promise.resolve();
    await Promise.resolve();
    expect(h.root.getAttribute("data-gesture")).toBe("idle");
  });

  it("takes gestures again when the drag command throws", () => {
    const h = shown(shelf, () => {
      throw new Error("no ipc");
    });
    down(row(h.root, 1));
    moveTo(row(h.root, 1), 40, 20);
    expect(h.root.getAttribute("data-gesture")).toBe("idle");
  });

  it("an item removed during the press isn't dragged", () => {
    const h = shown();
    down(row(h.root, 1));
    h.view.show({ items: shelf.items.slice(1) });
    moveTo(h.root, 40, 20);
    expect(commands(h)).toEqual([]);
    expect(h.root.getAttribute("data-gesture")).toBe("idle");
  });

  it("cancels every HTML drag", () => {
    const h = shown();
    expect(row(h.root, 1).dispatch("dragstart").defaultPrevented).toBe(true);
    const name = oneByClass(row(h.root, 2), "float-name");
    expect(name.dispatch("dragstart").defaultPrevented).toBe(true);
  });
});

describe("wireFloat", () => {
  type Handler = (event: { payload: unknown }) => void;

  function fakeListen(handlers: Map<string, Handler>): ListenFn {
    return (<T>(event: string, handler: (e: { payload: T }) => void) => {
      handlers.set(event, handler as unknown as Handler);
      return Promise.resolve(() => handlers.delete(event));
    }) as ListenFn;
  }

  function recorder(): { calls: string[]; shelf: FloatShelf } {
    const calls: string[] = [];
    return {
      calls,
      shelf: {
        show: (view) => calls.push(`show:${view.items.map((item) => item.id).join(",")}`),
        dragEnded: (payload) => calls.push(`ended:${payload.outcome}`),
      },
    };
  }

  it("listens first, then asks for the current view once and draws it", async () => {
    const handlers = new Map<string, Handler>();
    const { calls, shelf: view } = recorder();
    const invoke = vi.fn<InvokeFn>(() => {
      expect([...handlers.keys()].sort()).toEqual([FLOAT_DRAG_ENDED, FLOAT_VIEW].sort());
      return Promise.resolve(fixtureJson("float-view.json"));
    });
    const stop = await wireFloat(view, fakeListen(handlers), invoke);
    expect(invoke.mock.calls).toEqual([[FLOAT_GET_VIEW, {}]]);
    expect(calls).toEqual(["show:1,2,3"]);
    stop();
    expect(handlers.size).toBe(0);
  });

  it("forwards guarded events and drops malformed ones", async () => {
    vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const handlers = new Map<string, Handler>();
    const { calls, shelf: view } = recorder();
    await wireFloat(view, fakeListen(handlers), () => Promise.resolve(fixtureJson("float-view-empty.json")));
    handlers.get(FLOAT_VIEW)?.({ payload: fixtureJson("float-view.json") });
    handlers.get(FLOAT_VIEW)?.({ payload: { items: [{ id: 1, name: "/home/a", kind: "file" }] } });
    handlers.get(FLOAT_DRAG_ENDED)?.({ payload: fixtureJson("float-drag-ended.json") });
    handlers.get(FLOAT_DRAG_ENDED)?.({ payload: { outcome: "moved" } });
    expect(calls).toEqual(["show:", "show:1,2,3", "ended:copied"]);
  });

  it("drops the answer when a view event came while it was on its way", async () => {
    const handlers = new Map<string, Handler>();
    const { calls, shelf: view } = recorder();
    const answer = Promise.resolve(fixtureJson("float-view-empty.json"));
    await wireFloat(view, fakeListen(handlers), () => {
      handlers.get(FLOAT_VIEW)?.({ payload: fixtureJson("float-view.json") });
      return answer;
    });
    expect(calls).toEqual(["show:1,2,3"]);
  });

  it("ignores a malformed answer and survives a failed one", async () => {
    vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const { calls, shelf: view } = recorder();
    const handlers = new Map<string, Handler>();
    await wireFloat(view, fakeListen(handlers), () => Promise.resolve({ items: "none" }));
    await expect(
      wireFloat(view, fakeListen(new Map()), () => Promise.reject(new Error("no ipc"))),
    ).resolves.toBeTypeOf("function");
    await expect(
      wireFloat(view, fakeListen(new Map()), () => {
        throw new Error("no ipc");
      }),
    ).resolves.toBeTypeOf("function");
    expect(calls).toEqual([]);
  });
});
