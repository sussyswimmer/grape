import { afterEach, describe, expect, it, vi } from "vitest";
import { TOAST_CLICKED, TOAST_HIDE, TOAST_SHOW } from "../shared/events";
import type { ListenFn } from "../shared/listen";
import { byClass, type FakeElement, fakeRoot, oneByClass } from "../test-support/fake-dom";
import { fixtureJson, toastFixture } from "../test-support/fixtures";
import { mountToast, type ToastView, wireToast } from "./view";

const working = toastFixture("toast-show-working.json");
const done = toastFixture("toast-show-done.json");

function harness(): { root: FakeElement; view: ToastView; invoke: ReturnType<typeof vi.fn> } {
  const { root } = fakeRoot();
  const invoke = vi.fn(() => Promise.resolve());
  const view = mountToast(root as unknown as HTMLElement, { invoke });
  return { root, view, invoke };
}

function pill(root: FakeElement): FakeElement {
  expect(root.children).toHaveLength(1);
  const [first] = root.children;
  if (first === undefined) throw new Error("no toast");
  return first;
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("mountToast", () => {
  it("makes the root a polite live region and starts empty", () => {
    const { root } = harness();
    expect(root.getAttribute("role")).toBe("status");
    expect(root.getAttribute("aria-live")).toBe("polite");
    expect(root.children).toEqual([]);
  });
});

describe("show", () => {
  it("renders the working toast: tone dot, title, no detail, no bar", () => {
    const { root, view } = harness();
    view.show(working);
    const toast = pill(root);
    expect(toast.classList.contains("toast")).toBe(true);
    expect(toast.getAttribute("data-tone")).toBe("working");
    expect(oneByClass(toast, "toast-dot").getAttribute("aria-hidden")).toBe("true");
    expect(oneByClass(toast, "toast-title").textContent).toBe("Making photo.png…");
    expect(byClass(toast, "toast-detail")).toHaveLength(0);
    expect(byClass(toast, "toast-progress")).toHaveLength(0);
  });

  it("renders the done toast with its detail", () => {
    const { root, view } = harness();
    view.show(done);
    const toast = pill(root);
    expect(toast.getAttribute("data-tone")).toBe("done");
    expect(oneByClass(toast, "toast-title").textContent).toBe("Done");
    expect(oneByClass(toast, "toast-detail").textContent).toBe("photo.png");
  });

  it("draws a thin progress bar only when progress is set", () => {
    const { root, view } = harness();
    view.show({ ...working, progress: 0.25 });
    const track = oneByClass(pill(root), "toast-progress");
    expect(track.getAttribute("role")).toBe("progressbar");
    expect(track.getAttribute("aria-valuenow")).toBe("25");
    expect(oneByClass(track, "toast-progress-bar").style.getPropertyValue("width")).toBe("25%");
    view.show({ ...working, progress: null });
    expect(byClass(pill(root), "toast-progress")).toHaveLength(0);
  });

  it("uses each tone", () => {
    const { root, view } = harness();
    for (const tone of ["working", "done", "error", "info"] as const) {
      view.show({ ...done, tone });
      expect(pill(root).getAttribute("data-tone")).toBe(tone);
    }
  });

  it("sets text with textContent, never markup", () => {
    const { root, view } = harness();
    view.show({ ...done, title: "<b>x</b>", detail: "<img src=x>" });
    expect(oneByClass(pill(root), "toast-title").textContent).toBe("<b>x</b>");
    expect(oneByClass(pill(root), "toast-title").children).toHaveLength(0);
  });
});

describe("hide", () => {
  it("clears the toast it names and ignores others", () => {
    const { root, view } = harness();
    view.show(done);
    view.hide({ id: 3 });
    expect(root.children).toHaveLength(1);
    view.hide({ id: 4 });
    expect(root.children).toEqual([]);
    expect(root.classList.contains("is-revealable")).toBe(false);
  });
});

describe("click", () => {
  it("asks Rust to reveal only when the toast is revealable, with a pointer only then", () => {
    const { root, view, invoke } = harness();
    view.show(working);
    expect(root.classList.contains("is-revealable")).toBe(false);
    root.dispatch("click");
    expect(invoke).not.toHaveBeenCalled();

    view.show(done);
    expect(root.classList.contains("is-revealable")).toBe(true);
    oneByClass(root, "toast-title").dispatch("click");
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(invoke).toHaveBeenCalledWith(TOAST_CLICKED, { id: 4 });
  });

  it("does nothing once the toast is hidden", () => {
    const { root, view, invoke } = harness();
    view.show(done);
    view.hide({ id: 4 });
    root.dispatch("click");
    expect(invoke).not.toHaveBeenCalled();
  });

  it("swallows a failing invoke", async () => {
    const { root } = fakeRoot();
    const view = mountToast(root as unknown as HTMLElement, {
      invoke: () => Promise.reject(new Error("no ipc")),
    });
    view.show(done);
    expect(() => root.dispatch("click")).not.toThrow();
    await Promise.resolve();
  });
});

describe("wireToast", () => {
  type Handler = (event: { payload: unknown }) => void;

  it("forwards guarded payloads and drops malformed ones", async () => {
    vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const handlers = new Map<string, Handler>();
    const listenFn = (<T>(event: string, handler: (e: { payload: T }) => void) => {
      handlers.set(event, handler as unknown as Handler);
      return Promise.resolve(() => handlers.delete(event));
    }) as ListenFn;
    const calls: string[] = [];
    const view: ToastView = {
      show: (p) => calls.push(`show:${p.id}:${p.tone}`),
      hide: (p) => calls.push(`hide:${p.id}`),
    };
    const stop = await wireToast(view, listenFn);
    handlers.get(TOAST_SHOW)?.({ payload: fixtureJson("toast-show-working.json") });
    handlers.get(TOAST_SHOW)?.({ payload: fixtureJson("toast-show-done.json") });
    handlers.get(TOAST_SHOW)?.({ payload: { id: 4, tone: "loud" } });
    handlers.get(TOAST_HIDE)?.({ payload: fixtureJson("toast-hide.json") });
    handlers.get(TOAST_HIDE)?.({ payload: { id: "4" } });
    expect(calls).toEqual(["show:4:working", "show:4:done", "hide:4"]);
    stop();
    expect(handlers.size).toBe(0);
  });
});
