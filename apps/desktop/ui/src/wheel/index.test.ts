import { afterEach, describe, expect, it, vi } from "vitest";
import { WHEEL_CLOSE, WHEEL_HIGHLIGHT, WHEEL_OPEN } from "../shared/events";
import { fixtureJson } from "../test-support/fixtures";
import { type ListenFn, mountWheel, type WheelView, wireOverlay } from "./index";

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
