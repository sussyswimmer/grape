import { afterEach, describe, expect, it, vi } from "vitest";
import type { InvokeFn } from "../shared/ipc";
import {
  byId,
  byTag,
  type FakeDocument,
  type FakeElement,
  fakeRoot,
  oneByClass,
} from "../test-support/fake-dom";
import { settingsFixture } from "../test-support/fixtures";
import { GENERIC_ERROR, type SettingsView } from "./model";
import { isStepKey, mountSettings, type SettingsPage } from "./view";

const windows = settingsFixture("settings-view-windows.json");
const macos = settingsFixture("settings-view-macos.json");

type Answer = (command: string, args: Record<string, unknown>) => Promise<unknown>;

interface Harness {
  readonly doc: FakeDocument;
  readonly root: FakeElement;
  readonly invoke: ReturnType<typeof vi.fn<InvokeFn>>;
  readonly page: SettingsPage;
}

/** Mounts the page with `answer` standing in for Rust. */
function harness(answer: Answer): Harness {
  const { doc, root } = fakeRoot();
  const invoke = vi.fn<InvokeFn>(answer);
  const page = mountSettings(root as unknown as HTMLElement, { invoke });
  return { doc, root, invoke, page };
}

/** A page that has drawn `view`, and answers every later command with `next` (default: `view`). */
async function shown(view: SettingsView, next: Answer = () => Promise.resolve(view)): Promise<Harness> {
  let first = true;
  const h = harness((command, args) => {
    if (first) {
      first = false;
      return Promise.resolve(view);
    }
    return next(command, args);
  });
  await h.page.idle();
  return h;
}

/** Answers each command with its own `Answer`; any other command rejects. */
function answering(answers: Readonly<Record<string, Answer>>): Answer {
  return (command, args) => answers[command]?.(command, args) ?? Promise.reject(`unexpected ${command}`);
}

/** A promise the test settles by hand. */
function deferred(): {
  readonly promise: Promise<unknown>;
  resolve(value: unknown): void;
  reject(reason: unknown): void;
} {
  let resolve: (value: unknown) => void = () => undefined;
  let reject: (reason: unknown) => void = () => undefined;
  const promise = new Promise<unknown>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

/** Lets every queued promise callback run. */
function flush(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

function isHidden(element: FakeElement): boolean {
  return element.hasAttribute("hidden");
}

function buttonLabelled(root: FakeElement, label: string): FakeElement {
  const found = byTag(root, "button").filter((node) => node.getAttribute("aria-label") === label);
  const [only] = found;
  if (found.length !== 1 || only === undefined) throw new Error(`no button "${label}"`);
  return only;
}

function toggleRow(root: FakeElement, name: string): FakeElement {
  const row = byId(root, `toggle-${name}`).parent;
  if (row === null) throw new Error(`no row for ${name}`);
  return row;
}

/** Calls after the initial `settings_get`. */
function commands(h: Harness): unknown[][] {
  return h.invoke.mock.calls.slice(1);
}

function change(element: FakeElement, update: Partial<Pick<FakeElement, "value" | "checked">>): void {
  Object.assign(element, update);
  element.dispatch("change");
}

/** Picks `key` for `wheel` and moves on, so the select sends it at once. */
function pickKey(root: FakeElement, wheel: string, key: string): void {
  const select = byId(root, `key-${wheel}`);
  change(select, { value: key });
  select.dispatch("blur");
}

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("mountSettings", () => {
  it("asks for the settings once and hides the sections until they arrive", async () => {
    const reply = deferred();
    const h = harness(() => reply.promise);
    await flush();
    expect(h.invoke.mock.calls).toEqual([["settings_get", {}]]);
    const sections = oneByClass(h.root, "settings-sections");
    expect(isHidden(sections)).toBe(true);
    expect(isHidden(oneByClass(h.root, "lead"))).toBe(true);
    expect(h.root.getAttribute("aria-busy")).toBe("true");

    reply.resolve(windows);
    await h.page.idle();
    expect(isHidden(sections)).toBe(false);
    expect(h.root.hasAttribute("aria-busy")).toBe(false);
    expect(h.invoke).toHaveBeenCalledTimes(1);
  });

  it("makes the status and note lines polite live regions from the start", () => {
    const h = harness(() => new Promise(() => undefined));
    for (const name of ["settings-status", "settings-note"]) {
      const line = oneByClass(h.root, name);
      expect(line.getAttribute("role")).toBe("status");
      expect(line.getAttribute("aria-live")).toBe("polite");
      expect(line.textContent).toBe("");
    }
  });

  it("shows the error and no sections when the first load fails", async () => {
    const h = harness(() => Promise.reject("Couldn't read your settings."));
    await h.page.idle();
    expect(oneByClass(h.root, "settings-status").textContent).toBe("Couldn't read your settings.");
    expect(isHidden(oneByClass(h.root, "settings-sections"))).toBe(true);
    expect(h.invoke).toHaveBeenCalledTimes(1);
  });

  it("binds every control to a label, and every switch to its detail line", async () => {
    const { root } = await shown(macos);
    for (const control of [...byTag(root, "select"), ...byTag(root, "input")]) {
      const id = control.getAttribute("id");
      expect(id).not.toBeNull();
      const labels = byTag(root, "label").filter((label) => label.getAttribute("for") === id);
      expect(labels, `label for ${String(id)}`).toHaveLength(1);
      expect(labels[0]?.textContent).not.toBe("");
    }
    for (const name of ["sounds", "haptics", "launchAtLogin"]) {
      const input = byId(root, `toggle-${name}`);
      expect(input.getAttribute("role")).toBe("switch");
      expect(input.getAttribute("type")).toBe("checkbox");
      expect(input.getAttribute("aria-describedby")).toBe(`toggle-${name}-detail`);
      expect(byId(root, `toggle-${name}-detail`).textContent).not.toBe("");
    }
    for (const button of byTag(root, "button")) {
      expect(button.getAttribute("type")).toBe("button");
      expect(button.getAttribute("aria-label")).toContain(button.textContent.replace("…", ""));
    }
    const sections = byTag(root, "section");
    // The welcome card and the five settings sections.
    expect(sections).toHaveLength(6);
    for (const section of sections) {
      const heading = byId(root, section.getAttribute("aria-labelledby") ?? "");
      expect(heading.localName).toBe("h2");
    }
  });
});

describe("drawing the Windows view", () => {
  it("shows the keys with Windows names", async () => {
    const { root } = await shown(windows);
    expect(oneByClass(root, "lead").textContent).toContain("notification area");
    const expected = { conversions: "shift", tools: "ctrl", actions: "alt" };
    for (const [wheel, key] of Object.entries(expected)) {
      const select = byId(root, `key-${wheel}`);
      expect(select.value).toBe(key);
      expect(byTag(select, "option").map((option) => option.textContent)).toEqual([
        "Shift",
        "Ctrl",
        "Alt",
      ]);
      expect(byTag(select, "option").map((option) => option.getAttribute("value"))).toEqual([
        "shift",
        "ctrl",
        "alt",
      ]);
    }
    const labels = byTag(root, "label").filter((label) =>
      (label.getAttribute("for") ?? "").startsWith("key-"),
    );
    expect(labels.map((label) => label.textContent)).toEqual(["Conversions", "Tools", "File actions"]);
    expect(oneByClass(root, "settings-note").textContent).toBe("");
  });

  it("shows next to the original, and the default fallback folder", async () => {
    const { root } = await shown(windows);
    expect(byId(root, "output-next").checked).toBe(true);
    expect(byId(root, "output-folder").checked).toBe(false);
    expect(isHidden(oneByClass(byId(root, "output-folder").parent ?? root, "folder-row"))).toBe(true);
    expect(byId(root, "output-path").textContent).toBe("");

    const fallback = byId(root, "fallback-heading").parent ?? root;
    expect(oneByClass(fallback, "folder-path").textContent).toBe("C:\\Users\\max\\Downloads\\Grape");
    expect(isHidden(oneByClass(fallback, "folder-badge"))).toBe(false);
    expect(oneByClass(fallback, "folder-badge").textContent).toBe("(default)");
    expect(isHidden(buttonLabelled(root, "Use the default fallback folder"))).toBe(true);
    expect(isHidden(buttonLabelled(root, "Choose the fallback folder"))).toBe(false);
  });

  it("shows sounds and launch at login, and hides haptics", async () => {
    const { root } = await shown(windows);
    expect(isHidden(toggleRow(root, "sounds"))).toBe(false);
    expect(byId(root, "toggle-sounds").checked).toBe(true);
    expect(isHidden(toggleRow(root, "haptics"))).toBe(true);
    expect(isHidden(toggleRow(root, "launchAtLogin"))).toBe(false);
    expect(byId(root, "toggle-launchAtLogin").checked).toBe(true);
    expect(byId(root, "toggle-launchAtLogin").hasAttribute("aria-disabled")).toBe(false);
    expect(byId(root, "toggle-launchAtLogin").hasAttribute("disabled")).toBe(false);
    expect(byId(root, "toggle-launchAtLogin-detail").textContent).toBe("Start Grape when you sign in.");
    expect(oneByClass(root, "settings-status").textContent).toBe("");
  });
});

describe("drawing the macOS view", () => {
  it("shows the keys with Mac names and the swap note", async () => {
    const { root } = await shown(macos);
    expect(oneByClass(root, "lead").textContent).toContain("menu bar");
    expect(byId(root, "key-conversions").value).toBe("meta");
    expect(byId(root, "key-tools").value).toBe("shift");
    expect(byId(root, "key-actions").value).toBe("ctrl");
    expect(byTag(byId(root, "key-tools"), "option").map((option) => option.textContent)).toEqual([
      "Shift",
      "Control",
      "Option",
      "Command",
    ]);
    expect(oneByClass(root, "settings-note").textContent).toBe(
      "Command now opens Conversions, and Shift opens Tools.",
    );
  });

  it("shows the chosen folder and a fallback that isn't the default", async () => {
    const { root } = await shown(macos);
    expect(byId(root, "output-folder").checked).toBe(true);
    expect(byId(root, "output-next").checked).toBe(false);
    expect(byId(root, "output-path").textContent).toBe("/Users/max/Pictures/Grape out");
    expect(isHidden(oneByClass(byId(root, "output-folder").parent ?? root, "folder-row"))).toBe(false);
    expect(isHidden(oneByClass(root, "folder-badge"))).toBe(true);
    expect(isHidden(buttonLabelled(root, "Use the default fallback folder"))).toBe(false);
  });

  it("shows haptics, and launch at login as unusable when it can't be read", async () => {
    const { root } = await shown(macos);
    expect(byId(root, "toggle-sounds").checked).toBe(false);
    expect(isHidden(toggleRow(root, "haptics"))).toBe(false);
    expect(byId(root, "toggle-haptics").checked).toBe(true);
    const launch = byId(root, "toggle-launchAtLogin");
    expect(launch.checked).toBe(false);
    // Not `disabled`: it stays in the Tab order and reads "Couldn't check".
    expect(launch.getAttribute("aria-disabled")).toBe("true");
    expect(launch.hasAttribute("disabled")).toBe(false);
    expect(launch.hasAttribute("tabindex")).toBe(false);
    expect(launch.getAttribute("aria-describedby")).toBe("toggle-launchAtLogin-detail");
    expect(byId(root, "toggle-launchAtLogin-detail").textContent).toBe("Couldn't check");
  });

  it("sets text with textContent, never markup", async () => {
    const view: SettingsView = {
      ...macos,
      output: { kind: "folder", path: "/Users/max/<b>out</b>" },
      note: "<img src=x>",
    };
    const { root } = await shown(view);
    expect(byId(root, "output-path").textContent).toBe("/Users/max/<b>out</b>");
    expect(byId(root, "output-path").children).toHaveLength(0);
    expect(oneByClass(root, "settings-note").children).toHaveLength(0);
  });
});

describe("keys", () => {
  it("sends the new key and draws Rust's answer, swap note included", async () => {
    const swapped: SettingsView = {
      ...windows,
      keys: { conversions: "ctrl", tools: "shift", actions: "alt" },
      note: "Ctrl now opens Conversions, and Shift opens Tools.",
    };
    const h = await shown(windows, () => Promise.resolve(swapped));
    pickKey(h.root, "conversions", "ctrl");
    await h.page.idle();
    expect(commands(h)).toEqual([["settings_set_key", { wheel: "conversions", key: "ctrl" }]]);
    expect(byId(h.root, "key-conversions").value).toBe("ctrl");
    expect(byId(h.root, "key-tools").value).toBe("shift");
    expect(oneByClass(h.root, "settings-note").textContent).toBe(
      "Ctrl now opens Conversions, and Shift opens Tools.",
    );
  });

  it("clears the note when the next view has none", async () => {
    const h = await shown(macos, () => Promise.resolve({ ...macos, note: null, sounds: true }));
    change(byId(h.root, "toggle-sounds"), { checked: true });
    await h.page.idle();
    expect(oneByClass(h.root, "settings-note").textContent).toBe("");
  });

  it("sends nothing for a value that isn't a key, and puts the select back", async () => {
    const h = await shown(windows);
    pickKey(h.root, "tools", "win");
    await h.page.idle();
    expect(commands(h)).toEqual([]);
    expect(byId(h.root, "key-tools").value).toBe("ctrl");
  });

  it("keeps the options when the choices don't change", async () => {
    const h = await shown(windows);
    const before = byTag(byId(h.root, "key-tools"), "option");
    change(byId(h.root, "toggle-sounds"), { checked: false });
    await h.page.idle();
    expect(byTag(byId(h.root, "key-tools"), "option")).toEqual(before);
    expect(byTag(byId(h.root, "key-tools"), "option")[0]).toBe(before[0]);
  });
});

describe("isStepKey", () => {
  const plain = { ctrlKey: false, metaKey: false, altKey: false };
  const moves = ["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Home", "End", "PageUp", "PageDown"];
  it("counts arrows, Home, End and page keys on Windows, and typed characters on both", () => {
    for (const key of [...moves, "a", "S"]) {
      expect(isStepKey({ ...plain, key }, "windows"), key).toBe(true);
    }
    for (const key of ["a", "S"]) {
      expect(isStepKey({ ...plain, key }, "macos"), key).toBe(true);
    }
  });
  it("doesn't count keys that open the list: Space, F4, Alt+arrows, and the arrows on macOS", () => {
    for (const platform of ["windows", "macos"] as const) {
      expect(isStepKey({ ...plain, key: " " }, platform)).toBe(false);
      expect(isStepKey({ ...plain, key: "F4" }, platform)).toBe(false);
      expect(isStepKey({ ...plain, key: "ArrowDown", altKey: true }, platform)).toBe(false);
      expect(isStepKey({ ...plain, key: "ArrowUp", altKey: true }, platform)).toBe(false);
    }
    for (const key of moves) {
      expect(isStepKey({ ...plain, key }, "macos"), key).toBe(false);
    }
  });
  it("doesn't count Enter, Tab, Escape or shortcuts", () => {
    for (const key of ["Enter", "Tab", "Escape", "Shift"]) {
      expect(isStepKey({ ...plain, key }, "windows"), key).toBe(false);
    }
    expect(isStepKey({ ...plain, key: "c", ctrlKey: true }, "windows")).toBe(false);
    expect(isStepKey({ ...plain, key: "c", metaKey: true }, "macos")).toBe(false);
    expect(isStepKey({ ...plain, key: "c", altKey: true }, "windows")).toBe(false);
  });
});

describe("key selects send the key the user settles on", () => {
  // Chromium on Windows fires `change` for every arrow-key step while a
  // select is closed; only the key the user settles on may reach Rust's
  // swap rule.
  const swapped: SettingsView = {
    ...windows,
    keys: { conversions: "alt", tools: "ctrl", actions: "shift" },
    note: "Alt now opens Conversions, and Shift opens File actions.",
  };

  it("sends a pick from the list at once", async () => {
    const h = await shown(windows, () => Promise.resolve(swapped));
    const select = byId(h.root, "key-conversions");
    select.dispatch("mousedown");
    change(select, { value: "alt" });
    await h.page.idle();
    expect(commands(h)).toEqual([["settings_set_key", { wheel: "conversions", key: "alt" }]]);
    expect(select.value).toBe("alt");
  });

  it("sends one command for keyboard steps, however slow, once the select loses focus", async () => {
    const h = await shown(windows, () => Promise.resolve(swapped));
    vi.useFakeTimers();
    const select = byId(h.root, "key-conversions");
    select.dispatch("keydown", { key: "ArrowDown" });
    change(select, { value: "ctrl" });
    // A screen reader user may wait on each option.
    vi.advanceTimersByTime(60_000);
    select.dispatch("keydown", { key: "ArrowDown" });
    change(select, { value: "alt" });
    vi.advanceTimersByTime(60_000);
    await h.page.idle();
    expect(commands(h)).toEqual([]);
    expect(select.value).toBe("alt");

    select.dispatch("blur");
    await h.page.idle();
    expect(commands(h)).toEqual([["settings_set_key", { wheel: "conversions", key: "alt" }]]);
    expect(byId(h.root, "key-tools").value).toBe("ctrl");
    expect(byId(h.root, "key-actions").value).toBe("shift");

    select.dispatch("blur");
    await h.page.idle();
    expect(commands(h)).toHaveLength(1);
  });

  it("sends a keyboard step on Enter", async () => {
    const h = await shown(windows, () => Promise.resolve(swapped));
    const select = byId(h.root, "key-conversions");
    select.dispatch("keydown", { key: "ArrowDown" });
    change(select, { value: "ctrl" });
    select.dispatch("keydown", { key: "ArrowDown" });
    change(select, { value: "alt" });
    await h.page.idle();
    expect(commands(h)).toEqual([]);

    select.dispatch("keydown", { key: "Enter" });
    await h.page.idle();
    expect(commands(h)).toEqual([["settings_set_key", { wheel: "conversions", key: "alt" }]]);
  });

  it("treats type-ahead as a step, but not a shortcut", async () => {
    const h = await shown(windows, () => Promise.resolve(swapped));
    const select = byId(h.root, "key-conversions");
    select.dispatch("keydown", { key: "a" });
    change(select, { value: "alt" });
    await h.page.idle();
    expect(commands(h)).toEqual([]);
    select.dispatch("blur");
    await h.page.idle();
    expect(commands(h)).toHaveLength(1);

    select.dispatch("keydown", { key: "c", ctrlKey: true });
    change(select, { value: "ctrl" });
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["settings_set_key", { wheel: "conversions", key: "alt" }],
      ["settings_set_key", { wheel: "conversions", key: "ctrl" }],
    ]);
  });

  it("sends a pick from a list opened with the keyboard at once", async () => {
    const h = await shown(windows, () => Promise.resolve(swapped));
    const select = byId(h.root, "key-conversions");
    select.dispatch("keydown", { key: "ArrowDown" });
    select.dispatch("keydown", { key: "ArrowDown", altKey: true });
    change(select, { value: "alt" });
    await h.page.idle();
    expect(commands(h)).toEqual([["settings_set_key", { wheel: "conversions", key: "alt" }]]);
  });

  it("sends an arrow-key change at once on macOS, where the arrows open the menu", async () => {
    const h = await shown(macos, () => Promise.resolve(macos));
    const select = byId(h.root, "key-actions");
    select.dispatch("keydown", { key: "ArrowDown" });
    change(select, { value: "alt" });
    await h.page.idle();
    expect(commands(h)).toEqual([["settings_set_key", { wheel: "actions", key: "alt" }]]);
  });

  it("sends a mouse pick at once after keyboard steps, and only that key", async () => {
    const h = await shown(windows, () => Promise.resolve(swapped));
    const select = byId(h.root, "key-conversions");
    select.dispatch("keydown", { key: "ArrowDown" });
    change(select, { value: "ctrl" });
    select.dispatch("mousedown");
    change(select, { value: "alt" });
    await h.page.idle();
    expect(commands(h)).toEqual([["settings_set_key", { wheel: "conversions", key: "alt" }]]);
  });

  it("sends nothing on blur or Enter without a change", async () => {
    const h = await shown(windows);
    const select = byId(h.root, "key-tools");
    select.dispatch("blur");
    select.dispatch("keydown", { key: "Enter" });
    await h.page.idle();
    expect(commands(h)).toEqual([]);
  });

  it("keeps showing the picked key through other redraws until Rust answers", async () => {
    const reply = deferred();
    const h = await shown(
      windows,
      answering({
        settings_set_toggle: () => Promise.resolve({ ...windows, sounds: false }),
        settings_set_key: () => reply.promise,
      }),
    );
    const select = byId(h.root, "key-tools");
    select.dispatch("keydown", { key: "ArrowDown" });
    change(select, { value: "alt" });
    // Another change answers and the page redraws while the key waits.
    change(byId(h.root, "toggle-sounds"), { checked: false });
    await h.page.idle();
    expect(byId(h.root, "toggle-sounds").checked).toBe(false);
    expect(select.value).toBe("alt");

    select.dispatch("blur");
    // Sent, not answered: a redraw still shows it.
    change(byId(h.root, "key-actions"), { value: "win" });
    expect(select.value).toBe("alt");
    expect(byId(h.root, "key-actions").value).toBe("alt");

    reply.resolve({
      ...windows,
      sounds: false,
      keys: { conversions: "shift", tools: "alt", actions: "ctrl" },
      note: "Alt now opens Tools, and Ctrl opens File actions.",
    });
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["settings_set_toggle", { name: "sounds", on: false }],
      ["settings_set_key", { wheel: "tools", key: "alt" }],
    ]);
    expect(select.value).toBe("alt");
    expect(byId(h.root, "key-actions").value).toBe("ctrl");
  });

  it("shows the view's key again once Rust refuses the pick", async () => {
    const h = await shown(
      windows,
      answering({
        settings_set_key: () => Promise.reject("That key can't open a wheel here."),
        settings_get: () => Promise.resolve(windows),
      }),
    );
    const select = byId(h.root, "key-tools");
    change(select, { value: "alt" });
    await h.page.idle();
    expect(select.value).toBe("ctrl");
    expect(oneByClass(h.root, "settings-status").textContent).toBe("That key can't open a wheel here.");
  });
});

describe("errors", () => {
  it("shows Rust's sentence, and the last good view when the settings can't be read again", async () => {
    const h = await shown(windows, () => Promise.reject("That key can't open a wheel here."));
    pickKey(h.root, "actions", "shift");
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["settings_set_key", { wheel: "actions", key: "shift" }],
      ["settings_get", {}],
    ]);
    expect(oneByClass(h.root, "settings-status").textContent).toBe("That key can't open a wheel here.");
    expect(byId(h.root, "key-actions").value).toBe("alt");
    expect(byId(h.root, "key-conversions").value).toBe("shift");
  });

  it("reads the settings again after a failure and draws them, keeping the message", async () => {
    // A failed save still changes the setting in Rust.
    const h = await shown(
      windows,
      answering({
        settings_set_toggle: () => Promise.reject("Couldn't save your settings."),
        settings_get: () => Promise.resolve({ ...windows, sounds: false }),
      }),
    );
    change(byId(h.root, "toggle-sounds"), { checked: false });
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["settings_set_toggle", { name: "sounds", on: false }],
      ["settings_get", {}],
    ]);
    expect(byId(h.root, "toggle-sounds").checked).toBe(false);
    expect(oneByClass(h.root, "settings-status").textContent).toBe("Couldn't save your settings.");
  });

  it("doesn't bring back a swap note when it reads the settings again", async () => {
    // The macOS fixture carries a note, as if Rust repeated it.
    const h = await shown(
      macos,
      answering({
        settings_set_toggle: () => Promise.reject("Couldn't save your settings."),
        settings_get: () => Promise.resolve({ ...macos, sounds: true }),
      }),
    );
    expect(oneByClass(h.root, "settings-note").textContent).not.toBe("");
    change(byId(h.root, "toggle-sounds"), { checked: true });
    await h.page.idle();
    expect(byId(h.root, "toggle-sounds").checked).toBe(true);
    expect(oneByClass(h.root, "settings-note").textContent).toBe("");
    expect(oneByClass(h.root, "settings-status").textContent).toBe("Couldn't save your settings.");
  });

  it("keeps the last good view and the message when the re-read can't be used", async () => {
    const warned = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const h = await shown(
      windows,
      answering({
        settings_set_toggle: () => Promise.reject("Couldn't save your settings."),
        settings_get: () => Promise.resolve({ ...windows, sounds: false, secret: "C:\\private" }),
      }),
    );
    change(byId(h.root, "toggle-sounds"), { checked: false });
    await h.page.idle();
    expect(byId(h.root, "toggle-sounds").checked).toBe(true);
    expect(oneByClass(h.root, "settings-status").textContent).toBe("Couldn't save your settings.");
    expect(warned).toHaveBeenCalledTimes(1);
    expect(JSON.stringify(warned.mock.calls)).not.toContain("private");
  });

  it("clears the message once the next change goes through", async () => {
    let fail = true;
    const h = await shown(
      windows,
      answering({
        settings_set_toggle: () => {
          if (fail) {
            fail = false;
            return Promise.reject("Couldn't save your settings.");
          }
          return Promise.resolve({ ...windows, sounds: false });
        },
        settings_get: () => Promise.resolve(windows),
      }),
    );
    change(byId(h.root, "toggle-sounds"), { checked: false });
    await h.page.idle();
    expect(oneByClass(h.root, "settings-status").textContent).toBe("Couldn't save your settings.");
    expect(byId(h.root, "toggle-sounds").checked).toBe(true);

    change(byId(h.root, "toggle-sounds"), { checked: false });
    await h.page.idle();
    expect(oneByClass(h.root, "settings-status").textContent).toBe("");
    expect(byId(h.root, "toggle-sounds").checked).toBe(false);
  });

  it("shows a generic message for a rejection without a sentence, or a throw", async () => {
    const logged = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const h = await shown(
      windows,
      answering({
        settings_set_toggle: () => Promise.reject(new Error("no ipc")),
        settings_get: () => Promise.resolve(windows),
      }),
    );
    change(byId(h.root, "toggle-sounds"), { checked: false });
    await h.page.idle();
    expect(oneByClass(h.root, "settings-status").textContent).toBe(GENERIC_ERROR);
    expect(byId(h.root, "toggle-sounds").checked).toBe(true);
    expect(logged).toHaveBeenCalledTimes(1);

    const throwing = await shown(windows, () => {
      throw new Error("no ipc");
    });
    change(byId(throwing.root, "toggle-sounds"), { checked: false });
    await throwing.page.idle();
    expect(oneByClass(throwing.root, "settings-status").textContent).toBe(GENERIC_ERROR);
  });

  it("drops a reply that fails the guard, keeps the last good view, and never logs the reply", async () => {
    const warned = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const h = await shown(windows, () =>
      Promise.resolve({ ...windows, sounds: false, keys: { conversions: "shift" }, secret: "C:\\private" }),
    );
    change(byId(h.root, "toggle-sounds"), { checked: false });
    await h.page.idle();
    expect(oneByClass(h.root, "settings-status").textContent).toBe(GENERIC_ERROR);
    expect(byId(h.root, "toggle-sounds").checked).toBe(true);
    expect(warned).toHaveBeenCalledTimes(1);
    expect(JSON.stringify(warned.mock.calls)).not.toContain("private");
  });
});

describe("where files go", () => {
  it("sends next-to-the-original from its radio", async () => {
    const h = await shown(macos, () => Promise.resolve({ ...macos, output: { kind: "nextToSource" } }));
    change(byId(h.root, "output-next"), { checked: true });
    await h.page.idle();
    expect(commands(h)).toEqual([["settings_set_output_next_to_source", {}]]);
    expect(byId(h.root, "output-next").checked).toBe(true);
    expect(byId(h.root, "output-folder").checked).toBe(false);
    expect(byId(h.root, "output-path").textContent).toBe("");
  });

  it("opens the picker from the folder radio, and puts the radio back when it's cancelled", async () => {
    const h = await shown(windows);
    change(byId(h.root, "output-folder"), { checked: true });
    await h.page.idle();
    expect(commands(h)).toEqual([["settings_pick_folder", { target: "output" }]]);
    expect(byId(h.root, "output-next").checked).toBe(true);
    expect(byId(h.root, "output-folder").checked).toBe(false);
  });

  it("shows the picked folder", async () => {
    const picked: SettingsView = { ...windows, output: { kind: "folder", path: "D:\\Converted" } };
    const h = await shown(windows, () => Promise.resolve(picked));
    change(byId(h.root, "output-folder"), { checked: true });
    await h.page.idle();
    expect(byId(h.root, "output-folder").checked).toBe(true);
    expect(byId(h.root, "output-path").textContent).toBe("D:\\Converted");
  });

  it("opens the picker again from Choose…", async () => {
    const h = await shown(macos);
    buttonLabelled(h.root, "Choose the folder for new files").dispatch("click");
    await h.page.idle();
    expect(commands(h)).toEqual([["settings_pick_folder", { target: "output" }]]);
  });

  it("opens one picker at a time", async () => {
    const reply = deferred();
    const h = await shown(macos, () => reply.promise);
    const choose = buttonLabelled(h.root, "Choose the folder for new files");
    choose.dispatch("click");
    choose.dispatch("click");
    buttonLabelled(h.root, "Choose the fallback folder").dispatch("click");
    await flush();
    expect(commands(h)).toEqual([["settings_pick_folder", { target: "output" }]]);
    reply.resolve(macos);
    await h.page.idle();

    buttonLabelled(h.root, "Choose the fallback folder").dispatch("click");
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["settings_pick_folder", { target: "output" }],
      ["settings_pick_folder", { target: "fallback" }],
    ]);
  });
});

describe("fallback folder", () => {
  it("opens the picker for the fallback", async () => {
    const h = await shown(windows);
    buttonLabelled(h.root, "Choose the fallback folder").dispatch("click");
    await h.page.idle();
    expect(commands(h)).toEqual([["settings_pick_folder", { target: "fallback" }]]);
  });

  it("goes back to the default, moving focus off the button it hides", async () => {
    const h = await shown(macos, () =>
      Promise.resolve({ ...macos, fallbackDir: { path: "/Users/max/Downloads/Grape", isDefault: true } }),
    );
    const useDefault = buttonLabelled(h.root, "Use the default fallback folder");
    useDefault.focus();
    useDefault.dispatch("click");
    await h.page.idle();
    expect(commands(h)).toEqual([["settings_use_default_fallback", {}]]);
    expect(isHidden(useDefault)).toBe(true);
    expect(isHidden(oneByClass(h.root, "folder-badge"))).toBe(false);
    expect(h.doc.activeElement).toBe(buttonLabelled(h.root, "Choose the fallback folder"));
  });
});

describe("switches", () => {
  it.each([
    ["sounds", false],
    ["haptics", false],
  ] as const)("sends %s turned to %s", async (name, on) => {
    const h = await shown(macos, () => Promise.resolve({ ...macos, [name]: on }));
    change(byId(h.root, `toggle-${name}`), { checked: on });
    await h.page.idle();
    expect(commands(h)).toEqual([["settings_set_toggle", { name, on }]]);
    expect(byId(h.root, `toggle-${name}`).checked).toBe(on);
  });

  it("sends launch at login, and shows the real state Rust reads back after a failure", async () => {
    const h = await shown(
      windows,
      answering({
        settings_set_toggle: () => Promise.reject("Couldn't change launch at login."),
        settings_get: () => Promise.resolve({ ...windows, launchAtLogin: null }),
      }),
    );
    change(byId(h.root, "toggle-launchAtLogin"), { checked: false });
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["settings_set_toggle", { name: "launchAtLogin", on: false }],
      ["settings_get", {}],
    ]);
    const launch = byId(h.root, "toggle-launchAtLogin");
    expect(launch.checked).toBe(false);
    expect(launch.getAttribute("aria-disabled")).toBe("true");
    expect(byId(h.root, "toggle-launchAtLogin-detail").textContent).toBe("Couldn't check");
    expect(oneByClass(h.root, "settings-status").textContent).toBe("Couldn't change launch at login.");
  });

  it("ignores a change while launch at login can't be checked, and puts the switch back", async () => {
    const h = await shown(macos);
    const launch = byId(h.root, "toggle-launchAtLogin");
    change(launch, { checked: true });
    await h.page.idle();
    expect(commands(h)).toEqual([]);
    expect(launch.checked).toBe(false);
    expect(launch.getAttribute("aria-disabled")).toBe("true");
  });

  it("keeps launch at login focused and unusable across a redraw", async () => {
    const h = await shown(macos, () => Promise.resolve({ ...macos, sounds: true }));
    const launch = byId(h.root, "toggle-launchAtLogin");
    launch.focus();
    change(byId(h.root, "toggle-sounds"), { checked: true });
    await h.page.idle();
    expect(h.doc.activeElement).toBe(launch);
    expect(launch.getAttribute("aria-disabled")).toBe("true");
    expect(launch.hasAttribute("disabled")).toBe(false);
  });

  it("re-enables launch at login once its state can be read", async () => {
    const h = await shown(macos, () => Promise.resolve({ ...macos, launchAtLogin: true }));
    change(byId(h.root, "toggle-sounds"), { checked: true });
    await h.page.idle();
    expect(byId(h.root, "toggle-launchAtLogin").hasAttribute("aria-disabled")).toBe(false);
    expect(byId(h.root, "toggle-launchAtLogin").checked).toBe(true);
    expect(byId(h.root, "toggle-launchAtLogin-detail").textContent).toBe("Start Grape when you sign in.");
  });
});

describe("ordering", () => {
  it("sends one command at a time and draws only the last answer", async () => {
    const first = deferred();
    const second = deferred();
    const replies = [first, second];
    const h = await shown(windows, () => replies.shift()?.promise ?? Promise.reject("unexpected"));
    change(byId(h.root, "toggle-sounds"), { checked: false });
    pickKey(h.root, "tools", "alt");
    await flush();
    expect(commands(h)).toEqual([["settings_set_toggle", { name: "sounds", on: false }]]);
    expect(h.root.getAttribute("aria-busy")).toBe("true");

    first.resolve({ ...windows, sounds: false });
    await flush();
    expect(commands(h)).toHaveLength(2);
    expect(commands(h)[1]).toEqual(["settings_set_key", { wheel: "tools", key: "alt" }]);
    // Not drawn yet: the select still shows what the user picked.
    expect(byId(h.root, "key-tools").value).toBe("alt");

    second.resolve({
      ...windows,
      sounds: false,
      keys: { conversions: "shift", tools: "alt", actions: "ctrl" },
      note: "Alt now opens Tools, and Ctrl opens File actions.",
    });
    await h.page.idle();
    expect(h.root.hasAttribute("aria-busy")).toBe(false);
    expect(byId(h.root, "toggle-sounds").checked).toBe(false);
    expect(byId(h.root, "key-actions").value).toBe("ctrl");
    expect(oneByClass(h.root, "settings-note").textContent).toBe(
      "Alt now opens Tools, and Ctrl opens File actions.",
    );
  });

  it("keeps the first failure's message when a later change goes through", async () => {
    const h = await shown(
      windows,
      answering({
        settings_set_key: () => Promise.reject("Couldn't save your settings."),
        settings_get: () => Promise.resolve(windows),
        settings_set_toggle: () => Promise.resolve({ ...windows, sounds: false }),
      }),
    );
    pickKey(h.root, "tools", "alt");
    change(byId(h.root, "toggle-sounds"), { checked: false });
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["settings_set_key", { wheel: "tools", key: "alt" }],
      ["settings_get", {}],
      ["settings_set_toggle", { name: "sounds", on: false }],
    ]);
    expect(oneByClass(h.root, "settings-status").textContent).toBe("Couldn't save your settings.");
    expect(byId(h.root, "toggle-sounds").checked).toBe(false);
    expect(byId(h.root, "key-tools").value).toBe("ctrl");
  });
});

describe("page structure", () => {
  it("puts the welcome card, the lead, the sections and the status line in the root, in order", async () => {
    const { root } = await shown(windows);
    expect(root.children.map((child) => child.classList.toString())).toEqual([
      "welcome",
      "lead",
      "settings-sections",
      "settings-status",
    ]);
    const headings = byTag(root, "h2").map((heading) => heading.textContent);
    expect(headings).toEqual([
      "Welcome to Grape",
      "Keys",
      "Where files go",
      "Fallback folder",
      "File actions",
      "General",
    ]);
  });
});

describe("terminal app", () => {
  function terminalSelect(root: FakeElement): FakeElement {
    return byId(root, "terminal-app");
  }

  it("shows the Windows apps in the File actions section, with the PowerShell fallback", async () => {
    const { root } = await shown(windows);
    const select = terminalSelect(root);
    const section = byId(root, "file-actions-heading").parent ?? root;
    expect(section.localName).toBe("section");
    expect(byTag(section, "select")).toEqual([select]);
    expect(select.value).toBe("windowsTerminal");
    const options = byTag(select, "option");
    expect(options.map((option) => [option.getAttribute("value"), option.textContent])).toEqual([
      ["windowsTerminal", "Windows Terminal"],
      ["powershell", "PowerShell"],
    ]);
    const labels = byTag(root, "label").filter((label) => label.getAttribute("for") === "terminal-app");
    expect(labels.map((label) => label.textContent)).toEqual(["Terminal"]);
    expect(oneByClass(section, "section-hint").textContent).toContain(
      "If Windows Terminal can't start, PowerShell opens instead.",
    );
  });

  it("shows the Mac apps, iTerm2 picked, with the Terminal fallback", async () => {
    const { root } = await shown(macos);
    const select = terminalSelect(root);
    expect(select.value).toBe("iterm2");
    expect(byTag(select, "option").map((option) => option.textContent)).toEqual(["Terminal", "iTerm2"]);
    const section = byId(root, "file-actions-heading").parent ?? root;
    expect(oneByClass(section, "section-hint").textContent).toContain(
      "If iTerm2 isn't installed, Terminal opens instead.",
    );
  });

  it("sends a pick at once and draws Rust's answer", async () => {
    const h = await shown(windows, () =>
      Promise.resolve({ ...windows, terminal: { ...windows.terminal, choice: "powershell" } }),
    );
    const select = terminalSelect(h.root);
    select.dispatch("mousedown");
    change(select, { value: "powershell" });
    await h.page.idle();
    expect(commands(h)).toEqual([["settings_set_terminal", { terminal: "powershell" }]]);
    expect(select.value).toBe("powershell");
  });

  it("sends keyboard steps on Windows only once the user settles", async () => {
    const h = await shown(windows, () =>
      Promise.resolve({ ...windows, terminal: { ...windows.terminal, choice: "powershell" } }),
    );
    const select = terminalSelect(h.root);
    select.dispatch("keydown", { key: "ArrowDown" });
    change(select, { value: "powershell" });
    select.dispatch("keydown", { key: "ArrowUp" });
    change(select, { value: "windowsTerminal" });
    select.dispatch("keydown", { key: "ArrowDown" });
    change(select, { value: "powershell" });
    await h.page.idle();
    expect(commands(h)).toEqual([]);
    expect(select.value).toBe("powershell");

    select.dispatch("keydown", { key: "Enter" });
    await h.page.idle();
    expect(commands(h)).toEqual([["settings_set_terminal", { terminal: "powershell" }]]);
    select.dispatch("blur");
    await h.page.idle();
    expect(commands(h)).toHaveLength(1);
  });

  it("keeps showing a pick through other redraws until Rust answers", async () => {
    const reply = deferred();
    const h = await shown(
      macos,
      answering({
        settings_set_toggle: () => Promise.resolve({ ...macos, sounds: true }),
        settings_set_terminal: () => reply.promise,
      }),
    );
    const select = terminalSelect(h.root);
    // Type-ahead is a step on macOS too, so the pick waits for the user to settle.
    select.dispatch("keydown", { key: "t" });
    change(select, { value: "terminal" });
    change(byId(h.root, "toggle-sounds"), { checked: true });
    await h.page.idle();
    expect(byId(h.root, "toggle-sounds").checked).toBe(true);
    expect(select.value).toBe("terminal");

    select.dispatch("blur");
    // Sent, not answered: a redraw still shows it.
    change(select, { value: "cmd" });
    expect(select.value).toBe("terminal");

    reply.resolve({ ...macos, sounds: true, terminal: { ...macos.terminal, choice: "terminal" } });
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["settings_set_toggle", { name: "sounds", on: true }],
      ["settings_set_terminal", { terminal: "terminal" }],
    ]);
    expect(select.value).toBe("terminal");
  });

  it("sends nothing for a value that isn't an app, and puts the select back", async () => {
    const h = await shown(windows);
    const select = terminalSelect(h.root);
    change(select, { value: "cmd" });
    select.dispatch("blur");
    await h.page.idle();
    expect(commands(h)).toEqual([]);
    expect(select.value).toBe("windowsTerminal");
  });

  it("shows Rust's refusal and the app it still has", async () => {
    const h = await shown(
      macos,
      answering({
        settings_set_terminal: () => Promise.reject("That app isn't offered here."),
        settings_get: () => Promise.resolve(macos),
      }),
    );
    const select = terminalSelect(h.root);
    change(select, { value: "terminal" });
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["settings_set_terminal", { terminal: "terminal" }],
      ["settings_get", {}],
    ]);
    expect(select.value).toBe("iterm2");
    expect(oneByClass(h.root, "settings-status").textContent).toBe("That app isn't offered here.");
  });

  it("keeps the options when the choices don't change", async () => {
    const h = await shown(windows, () => Promise.resolve({ ...windows, sounds: false }));
    const before = byTag(terminalSelect(h.root), "option");
    change(byId(h.root, "toggle-sounds"), { checked: false });
    await h.page.idle();
    expect(byTag(terminalSelect(h.root), "option")[0]).toBe(before[0]);
  });
});

describe("welcome card", () => {
  function card(root: FakeElement): FakeElement {
    return oneByClass(root, "welcome");
  }

  function steps(root: FakeElement): [string, string][] {
    return byTag(card(root), "li").map((item) => [
      oneByClass(item, "welcome-step-title").textContent,
      oneByClass(item, "welcome-step-detail").textContent,
    ]);
  }

  function gotIt(root: FakeElement): FakeElement {
    return buttonLabelled(root, "Got it, hide this card");
  }

  const welcomeMac: SettingsView = { ...macos, welcome: true };

  it("stays hidden until the settings arrive, then shows when the view asks", async () => {
    const reply = deferred();
    const h = harness(() => reply.promise);
    expect(isHidden(card(h.root))).toBe(true);
    reply.resolve(windows);
    await h.page.idle();
    expect(isHidden(card(h.root))).toBe(false);
  });

  it("is hidden when the view doesn't ask for it", async () => {
    const { root } = await shown(macos);
    expect(isHidden(card(root))).toBe(true);
  });

  it("walks through drag, tap and drop with the Windows keys", async () => {
    const { root } = await shown(windows);
    const welcome = card(root);
    expect(welcome.localName).toBe("section");
    expect(byId(root, welcome.getAttribute("aria-labelledby") ?? "").textContent).toBe("Welcome to Grape");
    expect(oneByClass(welcome, "welcome-intro").textContent).toBe(
      "Grape works while you drag a file, in three moves.",
    );
    expect(oneByClass(welcome, "welcome-steps").localName).toBe("ol");
    // Kept as a list for VoiceOver, though it shows no markers.
    expect(oneByClass(welcome, "welcome-steps").getAttribute("role")).toBe("list");
    expect(steps(root)).toEqual([
      ["Drag a file", "Start dragging any file in File Explorer or on the desktop."],
      ["Tap Shift", "Ctrl for tools, Alt for file actions."],
      ["Drop it on a segment", "Drop in the middle to cancel."],
    ]);
    const button = gotIt(root);
    expect(button.textContent).toBe("Got it");
    expect(button.parent).toBe(welcome);
  });

  it("names Finder and the Mac keys on macOS", async () => {
    const { root } = await shown(welcomeMac);
    expect(steps(root)).toEqual([
      ["Drag a file", "Start dragging any file in Finder or on the desktop."],
      ["Tap Command", "Shift for tools, Control for file actions."],
      ["Drop it on a segment", "Drop in the middle to cancel."],
    ]);
  });

  it("draws one decorative illustration per step", async () => {
    const { root } = await shown(windows);
    const items = byTag(card(root), "li");
    expect(items.map((item) => byTag(item, "svg").map((svg) => svg.classList.toString()))).toEqual([
      ["welcome-art welcome-art-drag"],
      ["welcome-art welcome-art-tap"],
      ["welcome-art welcome-art-drop"],
    ]);
    for (const svg of byTag(card(root), "svg")) {
      expect(svg.getAttribute("aria-hidden")).toBe("true");
    }
  });

  it("follows a key change, and stays: only Got it dismisses it", async () => {
    const swapped: SettingsView = {
      ...windows,
      keys: { conversions: "ctrl", tools: "shift", actions: "alt" },
      note: "Ctrl now opens Conversions, and Shift opens Tools.",
    };
    const h = await shown(windows, () => Promise.resolve(swapped));
    pickKey(h.root, "conversions", "ctrl");
    await h.page.idle();
    expect(isHidden(card(h.root))).toBe(false);
    expect(steps(h.root)[1]).toEqual(["Tap Ctrl", "Shift for tools, Alt for file actions."]);
  });

  it("sends the dismissal once, hides on Rust's answer, and moves focus to the first key", async () => {
    const reply = deferred();
    const h = await shown(windows, () => reply.promise);
    const button = gotIt(h.root);
    button.focus();
    button.dispatch("click");
    button.dispatch("click");
    await flush();
    expect(commands(h)).toEqual([["settings_dismiss_welcome", {}]]);
    // The page doesn't decide: the card stays until Rust says so.
    expect(isHidden(card(h.root))).toBe(false);

    reply.resolve({ ...windows, welcome: false });
    await h.page.idle();
    expect(isHidden(card(h.root))).toBe(true);
    expect(h.doc.activeElement).toBe(byId(h.root, "key-conversions"));
    expect(commands(h)).toHaveLength(1);
  });

  it("leaves focus alone when it isn't on the card", async () => {
    const h = await shown(windows, () => Promise.resolve({ ...windows, welcome: false }));
    const sounds = byId(h.root, "toggle-sounds");
    gotIt(h.root).dispatch("click");
    sounds.focus();
    await h.page.idle();
    expect(isHidden(card(h.root))).toBe(true);
    expect(h.doc.activeElement).toBe(sounds);
  });

  it("stays, with Rust's message, when the dismissal fails", async () => {
    const h = await shown(
      windows,
      answering({
        settings_dismiss_welcome: () => Promise.reject("Couldn't save your settings."),
        settings_get: () => Promise.resolve(windows),
      }),
    );
    const button = gotIt(h.root);
    button.focus();
    button.dispatch("click");
    await h.page.idle();
    expect(isHidden(card(h.root))).toBe(false);
    expect(h.doc.activeElement).toBe(button);
    expect(oneByClass(h.root, "settings-status").textContent).toBe("Couldn't save your settings.");

    // It can be tried again.
    button.dispatch("click");
    await h.page.idle();
    expect(commands(h).filter(([command]) => command === "settings_dismiss_welcome")).toHaveLength(2);
  });

  it("sets its text with textContent, never markup", async () => {
    const view: SettingsView = {
      ...windows,
      keyChoices: [
        { key: "shift", name: "<b>Shift</b>" },
        { key: "ctrl", name: "Ctrl" },
        { key: "alt", name: "Alt" },
      ],
    };
    const { root } = await shown(view);
    const title = oneByClass(byTag(card(root), "li")[1] ?? root, "welcome-step-title");
    expect(title.textContent).toBe("Tap <b>Shift</b>");
    expect(title.children).toHaveLength(0);
  });
});
