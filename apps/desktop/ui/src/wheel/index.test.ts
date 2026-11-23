import { afterEach, describe, expect, it, vi } from "vitest";
import { WHEEL_CLOSE, WHEEL_HIGHLIGHT, WHEEL_OPEN } from "../shared/events";
import overlayEntry from "../overlay.ts?raw";
import { fakeRoot } from "../test-support/fake-dom";
import { fixtureJson } from "../test-support/fixtures";
import {
  type ListenFn,
  mountWheel,
  QUIET_EVENTS,
  quietPage,
  type WheelView,
  wireOverlay,
} from "./index";

type Handler = (event: { payload: unknown }) => void;

function fakeListen(): { listenFn: ListenFn; emit: (event: string, payload: unknown) => void } {
  const handlers = new Map<string, Handler>();
  const listenFn = (<T>(event: string, handler: (e: { payload: T }) => void) => {
    handlers.set(event, handler as unknown as Handler);
    return Promise.resolve(() => handlers.delete(event));
  }) as ListenFn;
  return {
    listenFn,
    emit: (event, payload) => handlers.get(event)?.({ payload }),
  };
}

function recordingView(): { view: WheelView; calls: string[] } {
  const calls: string[] = [];
  return {
    calls,
    view: {
      open: (p) => calls.push(`open:${p.seq}`),
      highlight: (p) => calls.push(`highlight:${p.seq}:${String(p.index)}`),
      close: (p) => calls.push(`close:${p.seq}:${p.reason}`),
    },
  };
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("wireOverlay", () => {
  it("forwards the golden payloads to the view", async () => {
    const { listenFn, emit } = fakeListen();
    const { view, calls } = recordingView();
    await wireOverlay(view, listenFn);
    emit(WHEEL_OPEN, fixtureJson("wheel-open-pending.json"));
    emit(WHEEL_HIGHLIGHT, fixtureJson("wheel-highlight.json"));
    emit(WHEEL_HIGHLIGHT, fixtureJson("wheel-highlight-null.json"));
    emit(WHEEL_CLOSE, fixtureJson("wheel-close.json"));
    expect(calls).toEqual(["open:1", "highlight:1:0", "highlight:1:null", "close:1:noFiles"]);
  });

  it("forwards the Shortcuts wheel's golden payloads, too", async () => {
    const { listenFn, emit } = fakeListen();
    const { view, calls } = recordingView();
    await wireOverlay(view, listenFn);
    emit(WHEEL_OPEN, fixtureJson("wheel-open-shortcuts.json"));
    emit(WHEEL_HIGHLIGHT, { seq: 4, index: 2 });
    emit(WHEEL_CLOSE, fixtureJson("wheel-close-picked.json"));
    emit(WHEEL_OPEN, fixtureJson("wheel-open-shortcuts-empty.json"));
    expect(calls).toEqual(["open:4", "highlight:4:2", "close:4:picked", "open:5"]);
  });

  it("drops a Shortcuts open whose mode and content disagree", async () => {
    vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const { listenFn, emit } = fakeListen();
    const { view, calls } = recordingView();
    await wireOverlay(view, listenFn);
    const open = fixtureJson("wheel-open-shortcuts.json") as Record<string, unknown>;
    emit(WHEEL_OPEN, { ...open, mode: "tools" });
    expect(calls).toEqual([]);
  });

  it("drops payloads that fail their guard", async () => {
    vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const { listenFn, emit } = fakeListen();
    const { view, calls } = recordingView();
    await wireOverlay(view, listenFn);
    emit(WHEEL_OPEN, fixtureJson("wheel-highlight.json"));
    emit(WHEEL_HIGHLIGHT, { seq: 1, index: -1 });
    emit(WHEEL_CLOSE, { seq: 1 });
    expect(calls).toEqual([]);
  });

  it("stops listening when asked", async () => {
    const { listenFn, emit } = fakeListen();
    const { view, calls } = recordingView();
    const stop = await wireOverlay(view, listenFn);
    stop();
    emit(WHEEL_CLOSE, fixtureJson("wheel-close.json"));
    expect(calls).toEqual([]);
  });

  it("re-exports mountWheel", () => {
    expect(typeof mountWheel).toBe("function");
  });
});

describe("quietPage", () => {
  it("cancels contextmenu, mousedown and dragstart anywhere on the page", () => {
    const { doc, root } = fakeRoot();
    const inner = doc.createElement("div");
    const deepest = doc.createElementNS("http://www.w3.org/2000/svg", "path");
    inner.append(deepest);
    root.append(inner);
    quietPage(root as unknown as EventTarget);
    for (const type of ["contextmenu", "mousedown", "dragstart"]) {
      expect(root.dispatch(type).defaultPrevented, type).toBe(true);
      expect(deepest.dispatch(type).defaultPrevented, type).toBe(true);
    }
  });

  it("listens for those three and nothing else", () => {
    const { doc, root } = fakeRoot();
    const inner = doc.createElement("div");
    root.append(inner);
    quietPage(root as unknown as EventTarget);
    expect(QUIET_EVENTS).toEqual(["contextmenu", "mousedown", "dragstart"]);
    expect([...root.listeners.keys()].sort()).toEqual([...QUIET_EVENTS].sort());
    for (const type of ["mouseup", "click", "pointerdown", "pointermove", "keydown", "wheel"]) {
      expect(inner.dispatch(type).defaultPrevented, type).toBe(false);
    }
  });

  it("is switched on by the overlay's entry, on the whole document", () => {
    expect(overlayEntry).toContain("quietPage(document);");
  });
});
