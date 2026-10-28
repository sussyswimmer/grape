import { afterEach, describe, expect, it, vi } from "vitest";
import {
  PANEL_CANCEL,
  PANEL_CLOSE,
  PANEL_CURRENT,
  PANEL_OPEN,
  PANEL_RUN,
  type PanelOpen,
} from "../shared/events";
import type { InvokeFn } from "../shared/ipc";
import type { ListenFn } from "../shared/listen";
import {
  byClass,
  byId,
  byTag,
  descendants,
  type FakeDocument,
  type FakeElement,
  fakeRoot,
  oneByClass,
} from "../test-support/fake-dom";
import { fixtureJson, panelOpenFixture } from "../test-support/fixtures";
import { ROW_HEIGHT } from "./model";
import { mountPanel, type PanelHost, type PanelKey, type PanelView, wirePanel } from "./view";

const compress = panelOpenFixture("panel-open-compress.json");
const channels = panelOpenFixture("panel-open-channels.json");
const normalize = panelOpenFixture("panel-open-normalize.json");
const merge = panelOpenFixture("panel-open-merge.json");

/** A key event the stub host hands the panel; the test reads `defaultPrevented` after. */
type SentKey = PanelKey & { defaultPrevented: boolean };

interface Harness {
  readonly doc: FakeDocument;
  readonly root: FakeElement;
  readonly view: PanelView;
  readonly invoke: ReturnType<typeof vi.fn<InvokeFn>>;
  /** Sends a keydown through the host, as the document would. */
  key(key: string, fields?: Partial<Omit<PanelKey, "key" | "preventDefault">>): SentKey;
  /** The window gains or loses the keyboard. */
  setFocus(focused: boolean): void;
}

function harness(options: { focused?: boolean; answer?: InvokeFn } = {}): Harness {
  const { doc, root } = fakeRoot();
  const keyListeners: ((event: PanelKey) => void)[] = [];
  const focusListeners: ((focused: boolean) => void)[] = [];
  let focused = options.focused ?? false;
  const host: PanelHost = {
    onKey: (listener) => keyListeners.push(listener),
    onFocusChange: (listener) => focusListeners.push(listener),
    hasFocus: () => focused,
  };
  const invoke = vi.fn<InvokeFn>(options.answer ?? (() => Promise.resolve(null)));
  const view = mountPanel(root as unknown as HTMLElement, { invoke, host });
  return {
    doc,
    root,
    view,
    invoke,
    key(key, fields = {}) {
      const event: SentKey = {
        key,
        altKey: false,
        shiftKey: false,
        repeat: false,
        isComposing: false,
        target: doc.activeElement ?? doc.body,
        ...fields,
        defaultPrevented: false,
        preventDefault() {
          event.defaultPrevented = true;
        },
      };
      for (const listener of keyListeners) listener(event);
      return event;
    },
    setFocus(next) {
      focused = next;
      for (const listener of focusListeners) listener(next);
    },
  };
}

function opened(request: PanelOpen, options: { focused?: boolean; answer?: InvokeFn } = {}): Harness {
  const h = harness(options);
  h.view.open(request);
  return h;
}

function commands(h: Harness): unknown[][] {
  return h.invoke.mock.calls.map((call) => [...call]);
}

function runs(h: Harness): unknown[][] {
  return commands(h).filter(([command]) => command === PANEL_RUN);
}

/** Lets every queued promise callback run. */
function flush(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

/** A promise the test settles by hand. */
function deferred(): { readonly promise: Promise<unknown>; reject(reason: unknown): void } {
  let reject: (reason: unknown) => void = () => undefined;
  const promise = new Promise<unknown>((_resolve, rej) => {
    reject = rej;
  });
  return { promise, reject };
}

function radios(root: FakeElement): FakeElement[] {
  return byTag(root, "input").filter((node) => node.getAttribute("type") === "radio");
}

function radio(root: FakeElement, value: string): FakeElement {
  const found = radios(root).find((node) => node.getAttribute("value") === value);
  if (found === undefined) throw new Error(`no radio ${value}`);
  return found;
}

function segmentOf(input: FakeElement): FakeElement {
  if (input.parent === null) throw new Error("radio without a label");
  return input.parent;
}

function check(root: FakeElement, value: string): void {
  for (const node of radios(root)) node.checked = false;
  const input = radio(root, value);
  input.checked = true;
  input.dispatch("change");
}

function rowIds(root: FakeElement): number[] {
  return byId(root, "panel-order").children.map((row) => Number(row.getAttribute("id")?.replace("panel-item-", "")));
}

function row(root: FakeElement, id: number): FakeElement {
  return byId(root, `panel-item-${id}`);
}

function positions(root: FakeElement): string[] {
  return byId(root, "panel-order").children.map((node) => oneByClass(node, "panel-position").textContent);
}

/** A primary-button press on `target` at y. */
function down(target: FakeElement, y: number, fields: Record<string, unknown> = {}): void {
  target.dispatch("pointerdown", {
    button: 0,
    buttons: 1,
    isPrimary: true,
    pointerId: 1,
    clientX: 40,
    clientY: y,
    ...fields,
  });
}

function moveTo(target: FakeElement, y: number, buttons = 1, x = 40): void {
  target.dispatch("pointermove", { buttons, pointerId: 1, clientX: x, clientY: y });
}

function up(target: FakeElement): void {
  target.dispatch("pointerup", { pointerId: 1, button: 0, buttons: 0 });
}

function mergeOf(n: number): PanelOpen {
  const items = Array.from({ length: n }, (_, index) => ({ id: index + 1, name: `f${index + 1}.pdf` }));
  return {
    seq: 20,
    title: "Merge PDFs",
    items,
    panel: { tool: "mergePDF", mergedNames: items.map((item) => `f${item.id} Merged.pdf`) },
  };
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("mountPanel", () => {
  it("starts empty and hidden, and says whether the window has the keyboard", () => {
    const h = harness();
    expect(h.root.children).toEqual([]);
    expect(h.root.hidden).toBe(true);
    expect(h.root.getAttribute("data-active")).toBe("false");
    expect(harness({ focused: true }).root.getAttribute("data-active")).toBe("true");
  });

  it("ignores keys and focus while nothing is shown", () => {
    const h = harness();
    h.key("Enter");
    h.key("Escape");
    h.setFocus(true);
    expect(commands(h)).toEqual([]);
    expect(h.root.children).toEqual([]);
  });
});

describe("open", () => {
  it("draws the dialog: title, count, ×, hint, Cancel and Run", () => {
    const h = opened(compress);
    expect(h.root.hidden).toBe(false);
    expect(h.root.getAttribute("data-tool")).toBe("compress");
    expect(h.root.getAttribute("data-state")).toBe("idle");
    const surface = oneByClass(h.root, "panel-surface");
    expect(h.root.children).toEqual([surface]);
    expect(surface.getAttribute("role")).toBe("dialog");
    expect(surface.getAttribute("aria-labelledby")).toBe("panel-title");
    expect(surface.getAttribute("aria-describedby")).toBe("panel-count panel-hint");
    expect(surface.hasAttribute("aria-modal")).toBe(false);
    const title = byId(h.root, "panel-title");
    expect(title.localName).toBe("h1");
    expect(title.textContent).toBe("Compress");
    expect(byId(h.root, "panel-count").textContent).toBe("2 files");
    const close = byId(h.root, "panel-close");
    expect(close.localName).toBe("button");
    expect(close.getAttribute("type")).toBe("button");
    expect(close.getAttribute("aria-label")).toBe("Close");
    expect(close.getAttribute("title")).toBe("Close");
    expect(byTag(close, "svg")).toHaveLength(1);
    expect(byId(h.root, "panel-hint").textContent).toBe("Click here to use the keyboard.");
    expect(byId(h.root, "panel-cancel").textContent).toBe("Cancel");
    const run = byId(h.root, "panel-run");
    expect(run.textContent).toBe("Compress");
    expect(run.classList.contains("is-primary")).toBe(true);
  });

  it("names the one file in the count, with the full name as its title", () => {
    const h = opened(channels);
    const count = byId(h.root, "panel-count");
    expect(count.textContent).toBe("1 file · interview.wav");
    expect(count.getAttribute("title")).toBe("interview.wav");
    expect(byId(h.root, "panel-run").textContent).toBe("Apply");
  });

  it("gives every element a unique id and makes nothing HTML-draggable", () => {
    for (const request of [compress, channels, normalize, merge]) {
      const h = opened(request);
      const ids = descendants(h.root).flatMap((node) => {
        const id = node.getAttribute("id");
        return id === null ? [] : [id];
      });
      expect(new Set(ids).size, request.title).toBe(ids.length);
      expect(descendants(h.root).filter((node) => node.getAttribute("draggable") === "true")).toEqual([]);
    }
  });

  it("sets names with textContent, never markup", () => {
    const h = opened({ ...channels, items: [{ id: 1, name: "<b>x</b>.wav" }] });
    expect(byId(h.root, "panel-count").textContent).toBe("1 file · <b>x</b>.wav");
    expect(byId(h.root, "panel-count").children).toEqual([]);
  });
});

describe("Compress and Audio Channels", () => {
  it("draw a radiogroup with the remembered preset checked", () => {
    const h = opened(compress);
    expect(byId(h.root, "panel-choice-label").textContent).toBe("Quality");
    const group = byId(h.root, "panel-choice");
    expect(group.getAttribute("role")).toBe("radiogroup");
    expect(group.getAttribute("aria-labelledby")).toBe("panel-choice-label");
    expect(radios(h.root).map((node) => node.getAttribute("value"))).toEqual(["small", "balanced", "high"]);
    expect(byClass(h.root, "panel-segment").map((node) => node.textContent)).toEqual(["Small", "Balanced", "High"]);
    for (const node of radios(h.root)) {
      expect(node.getAttribute("name")).toBe("panel-choice");
      expect(segmentOf(node).localName).toBe("label");
    }
    expect(radio(h.root, "balanced").checked).toBe(true);
    expect(radios(h.root).filter((node) => node.checked)).toHaveLength(1);
    expect(byClass(h.root, "is-checked")).toEqual([segmentOf(radio(h.root, "balanced"))]);
    const note = byId(h.root, "panel-note");
    expect(note.getAttribute("aria-live")).toBe("polite");
    expect(note.textContent).toBe("Smaller, and hard to tell apart.");
  });

  it("Audio Channels offers Mono and Stereo, starting at Mono", () => {
    const h = opened(channels);
    expect(byId(h.root, "panel-choice-label").textContent).toBe("Channels");
    expect(radios(h.root).map((node) => node.getAttribute("value"))).toEqual(["mono", "stereo"]);
    expect(radio(h.root, "mono").checked).toBe(true);
    expect(segmentOf(radio(h.root, "mono")).classList.contains("is-checked")).toBe(true);
    expect(byId(h.root, "panel-note").textContent).toBe("Mixes everything into one channel.");
  });

  it("a changed radio moves is-checked and the note", () => {
    const h = opened(compress);
    check(h.root, "high");
    expect(byClass(h.root, "is-checked")).toEqual([segmentOf(radio(h.root, "high"))]);
    expect(byId(h.root, "panel-note").textContent).toBe("Best quality. Saves the least space.");
    check(h.root, "small");
    expect(byId(h.root, "panel-note").textContent).toBe("Smallest file. Large photos and videos are scaled down.");
  });

  it("Enter runs with the chosen preset (panel-run-compress.json)", () => {
    const h = opened(compress);
    check(h.root, "small");
    const enter = h.key("Enter", { target: radio(h.root, "small") });
    expect(enter.defaultPrevented).toBe(true);
    expect(commands(h)).toEqual([[PANEL_RUN, fixtureJson("panel-run-compress.json")]]);
  });

  it("Run sends the channel layout (panel-run-channels.json)", () => {
    const h = opened(channels);
    check(h.root, "stereo");
    byId(h.root, "panel-run").dispatch("click");
    expect(commands(h)).toEqual([[PANEL_RUN, fixtureJson("panel-run-channels.json")]]);
  });

  it("puts a radio back when it changes while running", () => {
    const h = opened(compress, { answer: () => new Promise(() => undefined) });
    h.key("Enter", { target: h.root });
    check(h.root, "high");
    expect(radio(h.root, "balanced").checked).toBe(true);
    expect(radio(h.root, "high").checked).toBe(false);
    expect(byId(h.root, "panel-note").textContent).toBe("Smaller, and hard to tell apart.");
  });
});

describe("Normalize Volume", () => {
  it("draws a range from the request, with the value in LUFS", () => {
    const h = opened(normalize);
    const slider = byId(h.root, "panel-lufs");
    expect(slider.localName).toBe("input");
    expect(slider.getAttribute("type")).toBe("range");
    expect(slider.getAttribute("min")).toBe("-24");
    expect(slider.getAttribute("max")).toBe("-10");
    expect(slider.getAttribute("step")).toBe("1");
    expect(slider.value).toBe("-16");
    expect(slider.getAttribute("aria-valuetext")).toBe("−16 LUFS");
    const label = byId(h.root, "panel-lufs-label");
    expect(label.localName).toBe("label");
    expect(label.getAttribute("for")).toBe("panel-lufs");
    expect(label.textContent).toBe("Target loudness");
    const output = byId(h.root, "panel-lufs-value");
    expect(output.localName).toBe("output");
    expect(output.getAttribute("for")).toBe("panel-lufs");
    expect(output.textContent).toBe("−16 LUFS");
    // An <output> is a polite live region: the slider's aria-valuetext says
    // the value, so the visual copy stays out of the accessibility tree.
    expect(output.getAttribute("aria-hidden")).toBe("true");
    expect(oneByClass(h.root, "panel-scale").children.map((node) => node.textContent)).toEqual(["Quieter", "Louder"]);
    expect(byId(h.root, "panel-note").textContent).toBe("Each file ends up about this loud.");
    expect(byId(h.root, "panel-run").textContent).toBe("Normalize");
  });

  it("follows the slider, snapped, and runs with it (panel-run-normalize.json)", () => {
    const h = opened(normalize);
    const slider = byId(h.root, "panel-lufs");
    slider.value = "-14";
    slider.dispatch("input");
    expect(byId(h.root, "panel-lufs-value").textContent).toBe("−14 LUFS");
    expect(slider.getAttribute("aria-valuetext")).toBe("−14 LUFS");
    slider.value = "-14.4";
    slider.dispatch("change");
    expect(slider.value).toBe("-14");
    h.key("Enter", { target: slider });
    expect(commands(h)).toEqual([[PANEL_RUN, fixtureJson("panel-run-normalize.json")]]);
  });
});

describe("Merge PDFs", () => {
  it("draws a listbox of the files, the first selected", () => {
    const h = opened(merge);
    expect(byId(h.root, "panel-order-label").textContent).toBe("Merge order");
    const help = byId(h.root, "panel-order-help");
    expect(help.classList.contains("visually-hidden")).toBe(true);
    expect(help.textContent).toContain("The first file names the new PDF.");
    const list = byId(h.root, "panel-order");
    expect(list.localName).toBe("ul");
    expect(list.getAttribute("role")).toBe("listbox");
    expect(list.tabIndex).toBe(0);
    expect(list.getAttribute("aria-labelledby")).toBe("panel-order-label");
    expect(list.getAttribute("aria-describedby")).toBe("panel-order-help");
    expect(list.getAttribute("aria-activedescendant")).toBe("panel-item-1");
    expect(rowIds(h.root)).toEqual([1, 2, 3]);
    expect(positions(h.root)).toEqual(["1", "2", "3"]);
    for (const node of list.children) {
      expect(node.localName).toBe("li");
      expect(node.getAttribute("role")).toBe("option");
      expect(byClass(node, "panel-grip")).toHaveLength(1);
    }
    expect(row(h.root, 1).getAttribute("aria-selected")).toBe("true");
    expect(row(h.root, 2).getAttribute("aria-selected")).toBe("false");
    expect(oneByClass(row(h.root, 2), "panel-name").textContent).toBe("cover.pdf");
    expect(row(h.root, 2).getAttribute("title")).toBe("cover.pdf");
    expect(byId(h.root, "panel-output").textContent).toBe("Saves as report Merged.pdf");
    const announce = byId(h.root, "panel-announce");
    expect(announce.getAttribute("role")).toBe("status");
    expect(announce.classList.contains("visually-hidden")).toBe(true);
    expect(byId(h.root, "panel-run").textContent).toBe("Merge");
  });

  it("offers Move up and Move down, each disabled at its end", () => {
    const h = opened(merge);
    const moveUp = byId(h.root, "panel-move-up");
    const moveDown = byId(h.root, "panel-move-down");
    expect(moveUp.getAttribute("aria-label")).toBe("Move up");
    expect(moveUp.getAttribute("title")).toBe("Move up");
    expect(moveUp.getAttribute("aria-keyshortcuts")).toBe("Alt+ArrowUp");
    expect(moveDown.getAttribute("aria-label")).toBe("Move down");
    expect(moveDown.getAttribute("aria-keyshortcuts")).toBe("Alt+ArrowDown");
    expect(moveUp.disabled).toBe(true);
    expect(moveDown.disabled).toBe(false);
    h.key("End", { target: byId(h.root, "panel-order") });
    expect(moveUp.disabled).toBe(false);
    expect(moveDown.disabled).toBe(true);
  });

  it("arrow keys, Home and End select without wrapping", () => {
    const h = opened(merge);
    const list = byId(h.root, "panel-order");
    const arrow = h.key("ArrowDown", { target: list });
    expect(arrow.defaultPrevented).toBe(true);
    expect(list.getAttribute("aria-activedescendant")).toBe("panel-item-2");
    expect(row(h.root, 2).getAttribute("aria-selected")).toBe("true");
    expect(row(h.root, 1).getAttribute("aria-selected")).toBe("false");
    h.key("End", { target: list });
    h.key("ArrowDown", { target: list });
    expect(list.getAttribute("aria-activedescendant")).toBe("panel-item-3");
    h.key("Home", { target: list });
    h.key("ArrowUp", { target: list });
    expect(list.getAttribute("aria-activedescendant")).toBe("panel-item-1");
    expect(rowIds(h.root)).toEqual([1, 2, 3]);
  });

  it("Alt+ArrowUp moves the selected file, with its position, Saves as and an announcement", () => {
    const h = opened(merge);
    const list = byId(h.root, "panel-order");
    h.key("ArrowDown", { target: list });
    const move = h.key("ArrowUp", { target: list, altKey: true });
    expect(move.defaultPrevented).toBe(true);
    expect(rowIds(h.root)).toEqual([2, 1, 3]);
    expect(positions(h.root)).toEqual(["1", "2", "3"]);
    expect(list.getAttribute("aria-activedescendant")).toBe("panel-item-2");
    expect(byId(h.root, "panel-output").textContent).toBe("Saves as cover Merged.pdf");
    expect(byId(h.root, "panel-announce").textContent).toBe("cover.pdf moved to 1 of 3.");
    expect(byId(h.root, "panel-move-up").disabled).toBe(true);
    // At the top, another Alt+ArrowUp changes nothing.
    h.key("ArrowUp", { target: list, altKey: true });
    expect(rowIds(h.root)).toEqual([2, 1, 3]);
  });

  it("Enter on the list runs in the new order (panel-run-merge.json)", () => {
    const h = opened(merge);
    const list = byId(h.root, "panel-order");
    h.key("ArrowDown", { target: list });
    h.key("ArrowUp", { target: list, altKey: true });
    h.key("Enter", { target: list });
    expect(commands(h)).toEqual([[PANEL_RUN, fixtureJson("panel-run-merge.json")]]);
  });

  it("Move up and Move down move the selected file", () => {
    const h = opened(merge);
    byId(h.root, "panel-move-down").dispatch("click");
    expect(rowIds(h.root)).toEqual([2, 1, 3]);
    expect(byId(h.root, "panel-announce").textContent).toBe("report.pdf moved to 2 of 3.");
    byId(h.root, "panel-move-down").dispatch("click");
    expect(rowIds(h.root)).toEqual([2, 3, 1]);
    byId(h.root, "panel-move-up").dispatch("click");
    expect(rowIds(h.root)).toEqual([2, 1, 3]);
  });

  it("hands the keyboard to the list when the focused Move button reaches its end", () => {
    const h = opened(merge);
    const moveDown = byId(h.root, "panel-move-down");
    h.key("ArrowDown", { target: byId(h.root, "panel-order") });
    moveDown.focus();
    moveDown.dispatch("click");
    expect(moveDown.disabled).toBe(true);
    expect(h.doc.activeElement).toBe(byId(h.root, "panel-order"));
  });

  it("keeps the selected row in view from the row math", () => {
    const h = opened(mergeOf(10));
    const list = byId(h.root, "panel-order");
    h.key("End", { target: list });
    expect(list.scrollTop).toBe(4 * ROW_HEIGHT);
    h.key("Home", { target: list });
    expect(list.scrollTop).toBe(0);
    for (let step = 0; step < 6; step += 1) h.key("ArrowDown", { target: list });
    expect(list.scrollTop).toBe(ROW_HEIGHT);
  });
});

describe("pointer reorder", () => {
  it("a press selects the row and focuses the list", () => {
    const h = opened(merge);
    down(row(h.root, 3), 100);
    up(row(h.root, 3));
    expect(byId(h.root, "panel-order").getAttribute("aria-activedescendant")).toBe("panel-item-3");
    expect(h.doc.activeElement).toBe(byId(h.root, "panel-order"));
    expect(rowIds(h.root)).toEqual([1, 2, 3]);
  });

  it("moving 4 px lifts the row; letting go puts it where the line was", () => {
    const h = opened(merge);
    const list = byId(h.root, "panel-order");
    down(row(h.root, 3), 100);
    moveTo(list, 28);
    expect(list.hasPointerCapture(1)).toBe(true);
    expect(row(h.root, 3).classList.contains("is-lifted")).toBe(true);
    expect(h.root.getAttribute("data-gesture")).toBe("lifted");
    const line = oneByClass(h.root, "panel-drop-line");
    expect(line.hidden).toBe(false);
    expect(line.style.getPropertyValue("top")).toBe("0px");
    up(list);
    expect(rowIds(h.root)).toEqual([3, 1, 2]);
    expect(list.hasPointerCapture(1)).toBe(false);
    expect(row(h.root, 3).classList.contains("is-lifted")).toBe(false);
    expect(line.hidden).toBe(true);
    expect(h.root.getAttribute("data-gesture")).toBe("idle");
    expect(byId(h.root, "panel-announce").textContent).toBe("appendix.pdf moved to 1 of 3.");
    expect(byId(h.root, "panel-output").textContent).toBe("Saves as appendix Merged.pdf");
  });

  it("marks a move down under the row it lands after", () => {
    const h = opened(merge);
    const list = byId(h.root, "panel-order");
    down(row(h.root, 1), 20);
    moveTo(list, 92);
    const line = oneByClass(h.root, "panel-drop-line");
    expect(line.style.getPropertyValue("top")).toBe(`${3 * ROW_HEIGHT - 2}px`);
    up(list);
    expect(rowIds(h.root)).toEqual([2, 3, 1]);
  });

  it("a move under 4 px is only a selection", () => {
    const h = opened(merge);
    const list = byId(h.root, "panel-order");
    down(row(h.root, 3), 100);
    moveTo(list, 97);
    expect(list.hasPointerCapture(1)).toBe(false);
    expect(row(h.root, 3).classList.contains("is-lifted")).toBe(false);
    up(list);
    expect(rowIds(h.root)).toEqual([1, 2, 3]);
    expect(list.getAttribute("aria-activedescendant")).toBe("panel-item-3");
  });

  it("pointercancel or a lost capture leaves the order alone", () => {
    const h = opened(merge);
    const list = byId(h.root, "panel-order");
    down(row(h.root, 3), 100);
    moveTo(list, 28);
    list.dispatch("pointercancel", { pointerId: 1 });
    expect(list.hasPointerCapture(1)).toBe(false);
    up(list);
    expect(rowIds(h.root)).toEqual([1, 2, 3]);

    down(row(h.root, 3), 100);
    moveTo(list, 28);
    list.dispatch("lostpointercapture", { pointerId: 1 });
    up(list);
    expect(rowIds(h.root)).toEqual([1, 2, 3]);
    expect(oneByClass(h.root, "panel-drop-line").hidden).toBe(true);
  });

  it("ends when the button came up unseen", () => {
    const h = opened(merge);
    const list = byId(h.root, "panel-order");
    down(row(h.root, 3), 100);
    moveTo(list, 28);
    moveTo(list, 20, 0);
    up(list);
    expect(rowIds(h.root)).toEqual([1, 2, 3]);
    expect(h.root.getAttribute("data-gesture")).toBe("idle");
  });

  it("only the primary button reorders", () => {
    const h = opened(merge);
    const list = byId(h.root, "panel-order");
    down(row(h.root, 3), 100, { button: 2, buttons: 2 });
    moveTo(list, 28, 2);
    down(row(h.root, 3), 100, { isPrimary: false });
    moveTo(list, 28);
    up(list);
    expect(rowIds(h.root)).toEqual([1, 2, 3]);
    expect(list.getAttribute("aria-activedescendant")).toBe("panel-item-1");
  });

  it("Esc during the gesture only ends the gesture", () => {
    const h = opened(merge);
    const list = byId(h.root, "panel-order");
    down(row(h.root, 3), 100);
    moveTo(list, 28);
    const esc = h.key("Escape", { target: list });
    expect(esc.defaultPrevented).toBe(true);
    expect(commands(h)).toEqual([]);
    expect(list.hasPointerCapture(1)).toBe(false);
    expect(row(h.root, 3).classList.contains("is-lifted")).toBe(false);
    up(list);
    expect(rowIds(h.root)).toEqual([1, 2, 3]);
    // The next Esc cancels.
    h.key("Escape", { target: list });
    expect(commands(h)).toEqual([[PANEL_CANCEL, { seq: 4 }]]);
  });

  it("Enter during the gesture doesn't run", () => {
    const h = opened(merge);
    const list = byId(h.root, "panel-order");
    down(row(h.root, 3), 100);
    moveTo(list, 28);
    h.key("Enter", { target: list });
    expect(runs(h)).toEqual([]);
  });

  it("scrolls a quarter row per move near the list's bottom edge", () => {
    const h = opened(mergeOf(10));
    const list = byId(h.root, "panel-order");
    list.scrollTop = 2 * ROW_HEIGHT;
    // Row 6 (index 5) sits at 108..144 in the visible part; its middle is 126.
    down(row(h.root, 6), 300);
    moveTo(list, 380);
    expect(list.scrollTop).toBe(2 * ROW_HEIGHT + ROW_HEIGHT / 4);
    moveTo(list, 381);
    expect(list.scrollTop).toBe(2 * ROW_HEIGHT + ROW_HEIGHT / 2);
    up(list);
    expect(rowIds(h.root)).toEqual([1, 2, 3, 4, 5, 7, 8, 9, 6, 10]);
  });

  it("cancels every HTML drag and the context menu", () => {
    const h = opened(merge);
    expect(row(h.root, 1).dispatch("dragstart").defaultPrevented).toBe(true);
    expect(oneByClass(row(h.root, 2), "panel-name").dispatch("dragstart").defaultPrevented).toBe(true);
    expect(row(h.root, 1).dispatch("contextmenu").defaultPrevented).toBe(true);
    expect(byId(h.root, "panel-title").dispatch("contextmenu").defaultPrevented).toBe(true);
  });
});

describe("run and cancel", () => {
  it("Enter on the root runs once; a second Enter or Run while running sends nothing", () => {
    const h = opened(compress, { answer: () => new Promise(() => undefined) });
    h.key("Enter", { target: h.root });
    expect(h.root.getAttribute("data-state")).toBe("running");
    h.key("Enter", { target: h.root });
    byId(h.root, "panel-run").dispatch("click");
    expect(commands(h)).toEqual([[PANEL_RUN, { seq: 1, order: [1, 2], choice: { tool: "compress", preset: "balanced" } }]]);
    for (const id of ["panel-close", "panel-cancel", "panel-run"]) {
      expect(byId(h.root, id).disabled, id).toBe(true);
    }
  });

  it("ignores a repeated or composing Enter", () => {
    const h = opened(compress);
    h.key("Enter", { target: h.root, repeat: true });
    h.key("Enter", { target: h.root, isComposing: true });
    expect(commands(h)).toEqual([]);
  });

  it("leaves Enter on a panel button to the button", () => {
    const h = opened(merge);
    for (const id of ["panel-cancel", "panel-close", "panel-run", "panel-move-down"]) {
      const enter = h.key("Enter", { target: byId(h.root, id) });
      expect(enter.defaultPrevented, id).toBe(false);
    }
    expect(commands(h)).toEqual([]);
  });

  it("Esc, Cancel or × cancels the shown seq once", () => {
    const esc = opened(merge);
    expect(esc.key("Escape").defaultPrevented).toBe(true);
    esc.key("Escape");
    expect(commands(esc)).toEqual([[PANEL_CANCEL, fixtureJson("panel-cancel.json")]]);
    expect(esc.root.getAttribute("data-state")).toBe("closing");

    const close = opened(compress);
    byId(close.root, "panel-close").dispatch("click");
    expect(commands(close)).toEqual([[PANEL_CANCEL, { seq: 1 }]]);

    const cancel = opened(channels);
    byId(cancel.root, "panel-cancel").dispatch("click");
    byId(cancel.root, "panel-run").dispatch("click");
    expect(commands(cancel)).toEqual([[PANEL_CANCEL, { seq: 2 }]]);
  });

  it("a refused cancel (a mouse button is down: a drag's Esc) keeps the panel and its order", async () => {
    const h = opened(merge, {
      focused: true,
      answer: (command) => (command === PANEL_CANCEL ? Promise.reject("A mouse button is down.") : Promise.resolve(null)),
    });
    const list = byId(h.root, "panel-order");
    h.key("ArrowDown", { target: list });
    h.key("ArrowUp", { target: list, altKey: true });
    expect(rowIds(h.root)).toEqual([2, 1, 3]);
    h.key("Escape");
    expect(h.root.getAttribute("data-state")).toBe("closing");
    await flush();
    expect(h.root.getAttribute("data-state")).toBe("idle");
    expect(h.root.hidden).toBe(false);
    for (const id of ["panel-close", "panel-cancel", "panel-run"]) {
      expect(byId(h.root, id).disabled, id).toBe(false);
    }
    // Nothing went wrong, so the hint doesn't change.
    expect(byId(h.root, "panel-hint").textContent).toBe("Enter runs · Esc cancels");
    expect(rowIds(h.root)).toEqual([2, 1, 3]);
    // Esc works again, and so does Run, with the order the user made.
    h.key("Escape");
    await flush();
    byId(h.root, "panel-run").dispatch("click");
    expect(commands(h)).toEqual([
      [PANEL_CANCEL, { seq: 4 }],
      [PANEL_CANCEL, { seq: 4 }],
      [PANEL_RUN, fixtureJson("panel-run-merge.json")],
    ]);
  });

  it("a cancel refused after a newer panel opened leaves the newer one alone", async () => {
    const pending = deferred();
    const h = opened(compress, { answer: (command) => (command === PANEL_CANCEL ? pending.promise : Promise.resolve(null)) });
    h.key("Escape");
    h.view.open(merge);
    pending.reject("A mouse button is down.");
    await flush();
    expect(h.root.getAttribute("data-state")).toBe("idle");
    expect(byId(h.root, "panel-title").textContent).toBe("Merge PDFs");
  });

  it("Esc while running doesn't cancel", () => {
    const h = opened(compress, { answer: () => new Promise(() => undefined) });
    h.key("Enter", { target: h.root });
    h.key("Escape");
    expect(commands(h).map(([command]) => command)).toEqual([PANEL_RUN]);
  });

  it("a rejection re-enables the buttons and shows its sentence in the hint", async () => {
    const h = opened(compress, {
      focused: true,
      answer: (command) =>
        command === PANEL_RUN ? Promise.reject("Grape couldn't use that choice.") : Promise.resolve(null),
    });
    h.key("Enter", { target: h.root });
    await flush();
    expect(h.root.getAttribute("data-state")).toBe("idle");
    for (const id of ["panel-close", "panel-cancel", "panel-run"]) {
      expect(byId(h.root, id).disabled, id).toBe(false);
    }
    const hint = byId(h.root, "panel-hint");
    expect(hint.textContent).toBe("Grape couldn't use that choice.");
    expect(hint.classList.contains("is-error")).toBe(true);
    // A change clears it; Run can be tried again.
    check(h.root, "high");
    expect(hint.textContent).toBe("Enter runs · Esc cancels");
    expect(hint.classList.contains("is-error")).toBe(false);
    byId(h.root, "panel-run").dispatch("click");
    expect(runs(h)).toHaveLength(2);
  });

  it("shows the generic sentence for a rejection without one, and survives a throwing invoke", async () => {
    const h = opened(compress, { answer: () => Promise.reject(new Error("no ipc")) });
    h.key("Enter", { target: h.root });
    await flush();
    expect(byId(h.root, "panel-hint").textContent).toBe("Grape couldn't use that choice.");

    const thrown = opened(compress, {
      answer: () => {
        throw new Error("no ipc");
      },
    });
    expect(() => thrown.key("Enter", { target: thrown.root })).not.toThrow();
    await flush();
    expect(thrown.root.getAttribute("data-state")).toBe("idle");
    expect(() => byId(thrown.root, "panel-cancel").dispatch("click")).not.toThrow();
  });
});

describe("close and replace", () => {
  it("close with the shown seq clears and hides the panel; another seq is ignored", () => {
    const h = opened(merge);
    h.view.close({ seq: 3 });
    expect(h.root.hidden).toBe(false);
    expect(byId(h.root, "panel-title").textContent).toBe("Merge PDFs");
    h.view.close({ seq: 4 });
    expect(h.root.hidden).toBe(true);
    expect(h.root.children).toEqual([]);
    expect(h.root.hasAttribute("data-tool")).toBe(false);
    h.key("Enter", { target: h.root });
    expect(commands(h)).toEqual([]);
  });

  it("a new open replaces everything, a running state too, and a late rejection can't touch it", async () => {
    const pending = deferred();
    const h = opened(compress, { answer: (command) => (command === PANEL_RUN ? pending.promise : Promise.resolve(null)) });
    h.key("Enter", { target: h.root });
    expect(h.root.getAttribute("data-state")).toBe("running");
    h.view.open(merge);
    expect(h.root.getAttribute("data-state")).toBe("idle");
    expect(h.root.getAttribute("data-tool")).toBe("mergePDF");
    expect(byId(h.root, "panel-title").textContent).toBe("Merge PDFs");
    expect(byId(h.root, "panel-run").disabled).toBe(false);
    expect(radios(h.root)).toEqual([]);
    pending.reject("Grape couldn't use that choice.");
    await flush();
    expect(byId(h.root, "panel-hint").textContent).toBe("Click here to use the keyboard.");
    h.key("Escape");
    expect(commands(h).at(-1)).toEqual([PANEL_CANCEL, { seq: 4 }]);
  });

  it("a new open ends a reorder gesture and releases the pointer", () => {
    const h = opened(merge);
    const list = byId(h.root, "panel-order");
    down(row(h.root, 3), 100);
    moveTo(list, 28);
    h.view.open({ ...merge, seq: 5 });
    expect(list.hasPointerCapture(1)).toBe(false);
    expect(h.root.getAttribute("data-gesture")).toBe("idle");
    expect(rowIds(h.root)).toEqual([1, 2, 3]);
  });
});

describe("focus", () => {
  it("unfocused, it dims and says how to take the keyboard; focused, it says what the keys do", () => {
    const h = opened(compress);
    expect(h.root.getAttribute("data-active")).toBe("false");
    expect(byId(h.root, "panel-hint").textContent).toBe("Click here to use the keyboard.");
    expect(h.doc.activeElement).toBeNull();
    h.setFocus(true);
    expect(h.root.getAttribute("data-active")).toBe("true");
    expect(byId(h.root, "panel-hint").textContent).toBe("Enter runs · Esc cancels");
    expect(h.doc.activeElement).toBe(radio(h.root, "balanced"));
    h.setFocus(false);
    expect(byId(h.root, "panel-hint").textContent).toBe("Click here to use the keyboard.");
  });

  it("on gaining focus, the main control takes it only when nothing in the panel has it", () => {
    const h = opened(normalize);
    const run = byId(h.root, "panel-run");
    run.focus();
    h.setFocus(true);
    expect(h.doc.activeElement).toBe(run);

    const fromBody = opened(merge);
    fromBody.doc.activeElement = fromBody.doc.body;
    fromBody.setFocus(true);
    expect(fromBody.doc.activeElement).toBe(byId(fromBody.root, "panel-order"));

    const fromRoot = opened(normalize);
    fromRoot.doc.activeElement = fromRoot.root;
    fromRoot.setFocus(true);
    expect(fromRoot.doc.activeElement).toBe(byId(fromRoot.root, "panel-lufs"));
  });

  it("opening while the window has the keyboard focuses the main control", () => {
    const h = opened(merge, { focused: true });
    expect(h.root.getAttribute("data-active")).toBe("true");
    expect(byId(h.root, "panel-hint").textContent).toBe("Enter runs · Esc cancels");
    expect(h.doc.activeElement).toBe(byId(h.root, "panel-order"));
  });

  it("Tab cycles × → choice → Cancel → Run and wraps both ways", () => {
    const h = opened(compress, { focused: true });
    const close = byId(h.root, "panel-close");
    const run = byId(h.root, "panel-run");
    run.focus();
    expect(h.key("Tab").defaultPrevented).toBe(true);
    expect(h.doc.activeElement).toBe(close);
    h.key("Tab");
    expect(h.doc.activeElement).toBe(radio(h.root, "balanced"));
    h.key("Tab");
    expect(h.doc.activeElement).toBe(byId(h.root, "panel-cancel"));
    h.key("Tab");
    expect(h.doc.activeElement).toBe(run);
    h.key("Tab");
    expect(h.doc.activeElement).toBe(close);
    h.key("Tab", { shiftKey: true });
    expect(h.doc.activeElement).toBe(run);
  });

  it("Tab in Merge visits the list and the Move buttons that are enabled", () => {
    const h = opened(merge, { focused: true });
    const order: string[] = [];
    byId(h.root, "panel-close").focus();
    for (let step = 0; step < 5; step += 1) {
      h.key("Tab");
      order.push(h.doc.activeElement?.getAttribute("id") ?? "none");
    }
    expect(order).toEqual(["panel-order", "panel-move-down", "panel-cancel", "panel-run", "panel-close"]);
  });

  it("Tab from outside the panel's controls starts at either end", () => {
    const h = opened(compress);
    h.doc.activeElement = h.doc.body;
    h.key("Tab");
    expect(h.doc.activeElement).toBe(byId(h.root, "panel-close"));
    h.doc.activeElement = h.doc.body;
    h.key("Tab", { shiftKey: true });
    expect(h.doc.activeElement).toBe(byId(h.root, "panel-run"));
  });

  it("a rejection gives the focus back to the main control when it was lost", async () => {
    const h = opened(channels, {
      focused: true,
      answer: (command) => (command === PANEL_RUN ? Promise.reject("No.") : Promise.resolve(null)),
    });
    h.key("Enter", { target: h.root });
    h.doc.activeElement = h.doc.body;
    await flush();
    expect(h.doc.activeElement).toBe(radio(h.root, "mono"));
  });
});

describe("wirePanel", () => {
  type Handler = (event: { payload: unknown }) => void;

  function fakeListen(handlers: Map<string, Handler>): ListenFn {
    return (<T>(event: string, handler: (e: { payload: T }) => void) => {
      handlers.set(event, handler as unknown as Handler);
      return Promise.resolve(() => handlers.delete(event));
    }) as ListenFn;
  }

  function recorder(): { calls: string[]; view: PanelView } {
    const calls: string[] = [];
    return {
      calls,
      view: {
        open: (payload) => calls.push(`open:${payload.seq}`),
        close: (payload) => calls.push(`close:${payload.seq}`),
      },
    };
  }

  it("listens first, then asks for the open request once and shows it", async () => {
    const handlers = new Map<string, Handler>();
    const { calls, view } = recorder();
    const invoke = vi.fn<InvokeFn>(() => {
      expect([...handlers.keys()].sort()).toEqual([PANEL_CLOSE, PANEL_OPEN].sort());
      return Promise.resolve(fixtureJson("panel-open-merge.json"));
    });
    const stop = await wirePanel(view, { listen: fakeListen(handlers), invoke });
    expect(invoke.mock.calls).toEqual([[PANEL_CURRENT, {}]]);
    expect(calls).toEqual(["open:4"]);
    stop();
    expect(handlers.size).toBe(0);
  });

  it("shows nothing for a null answer", async () => {
    const { calls, view } = recorder();
    const invoke = vi.fn<InvokeFn>(() => Promise.resolve(null));
    await wirePanel(view, { listen: fakeListen(new Map()), invoke });
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(calls).toEqual([]);
  });

  it("forwards guarded events and drops malformed ones with a warning", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const handlers = new Map<string, Handler>();
    const { calls, view } = recorder();
    await wirePanel(view, { listen: fakeListen(handlers), invoke: () => Promise.resolve(null) });
    handlers.get(PANEL_OPEN)?.({ payload: fixtureJson("panel-open-compress.json") });
    handlers.get(PANEL_OPEN)?.({ payload: { ...compress, items: [{ id: 1, name: "a", path: "/a" }] } });
    handlers.get(PANEL_CLOSE)?.({ payload: fixtureJson("panel-close.json") });
    handlers.get(PANEL_CLOSE)?.({ payload: { seq: 0 } });
    expect(calls).toEqual(["open:1", "close:4"]);
    expect(warn).toHaveBeenCalledTimes(2);
  });

  it("ignores an answer no newer than an event that came while it was on its way", async () => {
    const cases: [string, string, unknown, string[]][] = [
      ["an open of the same seq", PANEL_OPEN, fixtureJson("panel-open-merge.json"), ["open:4"]],
      ["a newer open", PANEL_OPEN, { ...merge, seq: 5 }, ["open:5"]],
      ["the close of that seq", PANEL_CLOSE, fixtureJson("panel-close.json"), ["close:4"]],
    ];
    for (const [label, event, payload, expected] of cases) {
      const handlers = new Map<string, Handler>();
      const { calls, view } = recorder();
      await wirePanel(view, {
        listen: fakeListen(handlers),
        invoke: () => {
          handlers.get(event)?.({ payload });
          return Promise.resolve(fixtureJson("panel-open-merge.json"));
        },
      });
      expect(calls, label).toEqual(expected);
    }
  });

  it("shows an answer newer than every event seen", async () => {
    const handlers = new Map<string, Handler>();
    const { calls, view } = recorder();
    await wirePanel(view, {
      listen: fakeListen(handlers),
      invoke: () => {
        handlers.get(PANEL_CLOSE)?.({ payload: { seq: 3 } });
        return Promise.resolve(fixtureJson("panel-open-merge.json"));
      },
    });
    expect(calls).toEqual(["close:3", "open:4"]);
  });

  it("ignores a malformed answer with a warning and survives a failed one", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const { calls, view } = recorder();
    await wirePanel(view, { listen: fakeListen(new Map()), invoke: () => Promise.resolve({ seq: 1 }) });
    expect(warn).toHaveBeenCalledTimes(1);
    await expect(
      wirePanel(view, { listen: fakeListen(new Map()), invoke: () => Promise.reject(new Error("no ipc")) }),
    ).resolves.toBeTypeOf("function");
    await expect(
      wirePanel(view, {
        listen: fakeListen(new Map()),
        invoke: () => {
          throw new Error("no ipc");
        },
      }),
    ).resolves.toBeTypeOf("function");
    expect(calls).toEqual([]);
  });

  it("drives a real panel: the answer renders, a close hides it", async () => {
    const handlers = new Map<string, Handler>();
    const h = harness();
    await wirePanel(h.view, {
      listen: fakeListen(handlers),
      invoke: () => Promise.resolve(fixtureJson("panel-open-channels.json")),
    });
    expect(h.root.hidden).toBe(false);
    expect(byId(h.root, "panel-title").textContent).toBe("Audio Channels");
    handlers.get(PANEL_CLOSE)?.({ payload: { seq: 2 } });
    expect(h.root.hidden).toBe(true);
  });
});
