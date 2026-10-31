import { afterEach, describe, expect, it, vi } from "vitest";
import { isWheelClosePayload } from "./events";
import { type ListenFn, listenAll, route } from "./listen";

type Handler = (event: { payload: unknown }) => void;

/** A fake `listen` that records handlers and can be told to fail for one event. */
function fakeListen(failFor: string | null = null): {
  listenFn: ListenFn;
  handlers: Map<string, Handler>;
  unlistened: string[];
} {
  const handlers = new Map<string, Handler>();
  const unlistened: string[] = [];
  const listenFn = (<T>(event: string, handler: (e: { payload: T }) => void) => {
    if (event === failFor) {
      return Promise.reject(new Error(`cannot listen to ${event}`));
    }
    handlers.set(event, handler as unknown as Handler);
    return Promise.resolve(() => {
      unlistened.push(event);
      handlers.delete(event);
    });
  }) as ListenFn;
  return { listenFn, handlers, unlistened };
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("route", () => {
  it("delivers payloads its guard accepts and warns about the rest", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const seen: unknown[] = [];
    const r = route("wheel://close", isWheelClosePayload, (p) => seen.push(p));
    r.deliver({ seq: 1, reason: "drop" });
    r.deliver({ seq: 1, reason: "nope", secret: "C:\\Users\\me\\photo.png" });
    expect(seen).toEqual([{ seq: 1, reason: "drop" }]);
    expect(warn).toHaveBeenCalledTimes(1);
    // The rejected payload (which could hold a file name) is never logged.
    expect(JSON.stringify(warn.mock.calls)).not.toContain("photo.png");
  });
});

describe("listenAll", () => {
  it("listens to every route and stops them all", async () => {
    const { listenFn, handlers, unlistened } = fakeListen();
    const seen: string[] = [];
    const stop = await listenAll(
      [
        { event: "a", deliver: () => seen.push("a") },
        { event: "b", deliver: () => seen.push("b") },
      ],
      "failed",
      listenFn,
    );
    handlers.get("b")?.({ payload: 1 });
    handlers.get("a")?.({ payload: 1 });
    expect(seen).toEqual(["b", "a"]);
    stop();
    expect(unlistened.sort()).toEqual(["a", "b"]);
    stop();
    expect(unlistened).toHaveLength(2);
  });

  it("never rejects: logs, undoes the listeners that started and returns a no-op", async () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const { listenFn, unlistened } = fakeListen("b");
    const stop = await listenAll(
      [
        { event: "a", deliver: () => undefined },
        { event: "b", deliver: () => undefined },
      ],
      "could not listen",
      listenFn,
    );
    expect(error).toHaveBeenCalledWith("could not listen", expect.any(Error));
    expect(unlistened).toEqual(["a"]);
    expect(() => stop()).not.toThrow();
  });

  it("survives a listen that throws synchronously", async () => {
    vi.spyOn(console, "error").mockImplementation(() => undefined);
    const throwing = (() => {
      throw new Error("no tauri");
    }) as ListenFn;
    const stop = await listenAll([{ event: "a", deliver: () => undefined }], "x", throwing);
    expect(() => stop()).not.toThrow();
  });
});
