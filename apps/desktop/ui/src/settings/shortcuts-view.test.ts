import { afterEach, describe, expect, it, vi } from "vitest";
import type { InvokeFn } from "../shared/ipc";
import {
  byClass,
  byId,
  byTag,
  descendants,
  type FakeDocument,
  type FakeElement,
  fakeRoot,
  type FakeWindow,
  fakeWindow,
  oneByClass,
} from "../test-support/fake-dom";
import { fixtureJson } from "../test-support/fixtures";
import {
  type ChordWire,
  isShortcutsView,
  type ItemView,
  type MenuView,
  SHORTCUTS_COPY,
  SHORTCUTS_ERROR,
  type ShortcutsView,
  STEP_KIND_NAMES,
  STEP_KINDS,
  type StepsItem,
  type StepView,
} from "./shortcuts-model";
import { mountShortcuts, type ShortcutsPage } from "./shortcuts-view";

function viewFixture(name: string): ShortcutsView {
  const value = fixtureJson(name);
  if (!isShortcutsView(value)) {
    throw new Error(`${name} is not a ShortcutsView`);
  }
  return value;
}

const macos = viewFixture("shortcuts-view-macos.json");
const windows = viewFixture("shortcuts-view-windows.json");

const CHORD: ChordWire = { key: "KeyK", shift: true, ctrl: false, alt: false, meta: true };
const EVERYWHERE = 1;
const SAFARI = 20;

/**
 * A view with more in it than the fixtures: an emoji icon, a group, an
 * app's menu and a step of every kind.
 */
const sample: ShortcutsView = {
  ...macos,
  enabled: true,
  binding: "bound",
  menus: [
    {
      id: EVERYWHERE,
      scope: { kind: "everywhere" },
      withEverywhere: false,
      items: [
        {
          id: 2,
          name: "Copy",
          icon: { kind: "glyph", value: "clipboard" },
          kind: "steps",
          steps: [{ id: 3, kind: "clipboard", action: "copy" }],
        },
        {
          id: 4,
          name: "Sign",
          icon: { kind: "emoji", value: "✍️" },
          kind: "steps",
          steps: [
            { id: 5, kind: "typeText", text: "Best,\nMax" },
            { id: 6, kind: "keys", chord: CHORD, label: "Shift+Command+K" },
            { id: 7, kind: "delay", ms: 250 },
          ],
        },
        {
          id: 8,
          name: "Apps",
          icon: { kind: "none" },
          kind: "group",
          items: [
            {
              id: 9,
              name: "Browser",
              icon: { kind: "glyph", value: "app" },
              kind: "steps",
              steps: [
                { id: 10, kind: "openApp", name: null },
                { id: 11, kind: "openUrl", url: "https://example.com" },
              ],
            },
            { id: 12, name: "Notes", icon: { kind: "none" }, kind: "steps", steps: [] },
          ],
        },
      ],
    },
    {
      id: SAFARI,
      scope: { kind: "app", name: "Safari" },
      withEverywhere: true,
      items: [
        {
          id: 21,
          name: "Reload",
          icon: { kind: "none" },
          kind: "steps",
          steps: [
            { id: 22, kind: "keys", chord: null, label: null },
            { id: 23, kind: "window", placement: "rightHalf" },
            { id: 24, kind: "system", control: "toggleMute" },
            { id: 25, kind: "appleShortcut", name: "Start focus" },
            { id: 26, kind: "appleScript", source: "beep" },
            { id: 27, kind: "shell", source: "echo hello" },
            { id: 28, kind: "keyboardMaestro", name: "Tidy desktop" },
            { id: 29, kind: "notify", text: "Break time" },
            { id: 30, kind: "speak", text: "Stand up" },
            { id: 31, kind: "openFile", name: "Invoices", folder: true },
          ],
        },
      ],
    },
  ],
};

/** A deep copy of `view` with `change` applied. */
function changed(view: ShortcutsView, change: (copy: ShortcutsView) => void): ShortcutsView {
  const copy = JSON.parse(JSON.stringify(view)) as ShortcutsView;
  change(copy);
  if (!isShortcutsView(copy)) {
    throw new Error("the changed view doesn't pass the guard");
  }
  return copy;
}

function menuOf(view: ShortcutsView, id: number): MenuView {
  const menu = view.menus.find((candidate) => candidate.id === id);
  if (menu === undefined) throw new Error(`no menu ${id}`);
  return menu;
}

function itemOf(view: ShortcutsView, id: number): ItemView {
  for (const menu of view.menus) {
    for (const item of menu.items) {
      if (item.id === id) return item;
      const child = item.kind === "group" ? item.items.find((candidate) => candidate.id === id) : undefined;
      if (child !== undefined) return child;
    }
  }
  throw new Error(`no item ${id}`);
}

function stepsOf(view: ShortcutsView, id: number): StepsItem {
  const item = itemOf(view, id);
  if (item.kind !== "steps") throw new Error(`${id} is a group`);
  return item;
}

type Answer = (command: string, args: Record<string, unknown>) => Promise<unknown>;

interface Harness {
  readonly doc: FakeDocument;
  readonly root: FakeElement;
  readonly win: FakeWindow;
  readonly invoke: ReturnType<typeof vi.fn<InvokeFn>>;
  readonly page: ShortcutsPage;
}

/** Mounts the section, in a document with a window, with `answer` standing in for Rust. */
function harness(answer: Answer): Harness {
  const { doc, root } = fakeRoot();
  const win = fakeWindow(doc);
  const invoke = vi.fn<InvokeFn>(answer);
  const page = mountShortcuts(root as unknown as HTMLElement, { invoke });
  return { doc, root, win, invoke, page };
}

/** A page that has drawn `view`, and answers every later command with `next` (default: `view`). */
async function shown(view: ShortcutsView, next: Answer = () => Promise.resolve(view)): Promise<Harness> {
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

/** A page that has drawn `sample` with Safari's menu chosen. */
async function shownSafari(next: Answer = () => Promise.resolve(sample)): Promise<Harness> {
  const h = await shown(sample, next);
  change(byId(h.root, "sc-menu"), { value: String(SAFARI) });
  return h;
}

/** Answers each command with its own `Answer`; every other command rejects. */
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
  for (let node: FakeElement | null = element; node !== null; node = node.parent) {
    if (node.hasAttribute("hidden")) return true;
  }
  return false;
}

function buttonLabelled(root: FakeElement, label: string): FakeElement {
  const found = byTag(root, "button").filter((node) => node.getAttribute("aria-label") === label);
  const [only] = found;
  if (found.length !== 1 || only === undefined) throw new Error(`${found.length} buttons "${label}"`);
  return only;
}

/** Calls after the first `shortcuts_get`. */
function commands(h: Harness): unknown[][] {
  return h.invoke.mock.calls.slice(1);
}

function change(element: FakeElement, update: Partial<Pick<FakeElement, "value" | "checked">>): void {
  Object.assign(element, update);
  element.dispatch("change");
}

/** Types into a field without leaving it: no `change` yet. */
function type(element: FakeElement, value: string): void {
  element.value = value;
  element.dispatch("input");
}

function click(element: FakeElement): void {
  element.focus();
  element.dispatch("click");
}

/** The second click of a double click: the OS counts it, whatever is under the pointer by then. */
function clickAgain(element: FakeElement): void {
  element.focus();
  element.dispatch("click", { detail: 2 });
}

/** A press from the keyboard: the click it makes counts no clicks. */
function press(element: FakeElement): void {
  element.focus();
  element.dispatch("click", { detail: 0 });
}

function isWaiting(element: FakeElement): boolean {
  return element.getAttribute("aria-disabled") === "true";
}

const SET_SHORTCUT = "Set shortcut for the wheel";

/** The nearest ancestor (or the element itself) with this class. */
function closest(element: FakeElement, name: string): FakeElement {
  for (let node: FakeElement | null = element; node !== null; node = node.parent) {
    if (node.classList.contains(name)) return node;
  }
  throw new Error(`nothing .${name} around the element`);
}

function itemBox(root: FakeElement, id: number): FakeElement {
  return closest(byId(root, `sc-item-${id}-name`), "sc-item");
}

/** A row's own summary line (a group's row also holds its shortcuts' rows). */
function summaryOf(root: FakeElement, id: number): string {
  const lines = itemBox(root, id).children.filter((node) => node.classList.contains("sc-item-summary"));
  if (lines.length !== 1) throw new Error(`${lines.length} summaries in row ${id}`);
  return lines[0]?.textContent ?? "";
}

/** The step rows of shortcut `item`, in page order. */
function stepBoxes(root: FakeElement, item: number): FakeElement[] {
  return byClass(itemBox(root, item), "sc-step");
}

/** The names in the ring's rows, top level only, in page order. */
function ringNames(root: FakeElement): string[] {
  return oneByClass(root, "sc-items").children.map((box) => oneByClass(box.children[0] ?? box, "sc-name").value);
}

function options(select: FakeElement): string[] {
  return byTag(select, "option").map((node) => node.getAttribute("value") ?? "");
}

/**
 * The rules every state of the page keeps: one label per control, ids used
 * once, buttons that say what they do, and no reference to an element
 * that isn't there.
 */
function expectWellFormed(root: FakeElement): void {
  const all = descendants(root);
  const ids = all.map((node) => node.getAttribute("id")).filter((id) => id !== null);
  expect(new Set(ids).size, "ids are unique").toBe(ids.length);

  const controls = all.filter((node) => ["select", "input", "textarea"].includes(node.localName));
  const labels = byTag(root, "label");
  for (const control of controls) {
    const id = control.getAttribute("id");
    expect(id).not.toBeNull();
    const own = labels.filter((label) => label.getAttribute("for") === id);
    expect(own, `label for ${String(id)}`).toHaveLength(1);
    expect(own[0]?.textContent).not.toBe("");
  }
  for (const label of labels) {
    const target = label.getAttribute("for");
    const targets = controls.filter((control) => control.getAttribute("id") === target);
    expect(targets, `control for ${String(target)}`).toHaveLength(1);
  }
  for (const button of byTag(root, "button")) {
    expect(button.getAttribute("type")).toBe("button");
    expect(button.textContent).not.toBe("");
    expect(button.getAttribute("aria-label")).toContain(button.textContent.replace("…", ""));
  }
  for (const node of all) {
    for (const name of ["aria-describedby", "aria-labelledby"]) {
      const targets = node.getAttribute(name);
      expect(targets, name).not.toBe("");
      for (const target of targets?.split(" ") ?? []) {
        expect(ids, `${name}="${target}"`).toContain(target);
      }
    }
    // A button that waits keeps its place in the Tab order: it is never both.
    if (node.localName === "button" && node.disabled) {
      expect(node.hasAttribute("aria-disabled")).toBe(false);
    }
    if (node.getAttribute("role") === "group") {
      expect(node.getAttribute("aria-label")).not.toBe("");
      expect(node.getAttribute("aria-label")).not.toBeNull();
    }
  }
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("mountShortcuts", () => {
  it("asks for the shortcuts once and shows only the heading until they arrive", async () => {
    const reply = deferred();
    const h = harness(() => reply.promise);
    await flush();
    expect(h.invoke.mock.calls).toEqual([["shortcuts_get", {}]]);
    expect(h.root.getAttribute("aria-busy")).toBe("true");
    const section = oneByClass(h.root, "shortcuts");
    expect(section.children.map((node) => node.localName)).toEqual(["h2", "p"]);
    expect(byTag(h.root, "input")).toEqual([]);

    reply.resolve(macos);
    await h.page.idle();
    expect(h.root.hasAttribute("aria-busy")).toBe(false);
    expect(h.invoke).toHaveBeenCalledTimes(1);
    expect(byTag(h.root, "input").length).toBeGreaterThan(0);
  });

  it("puts one section with an h2 in the root, then its own status line", async () => {
    const { root } = await shown(macos);
    expect(root.children.map((node) => node.localName)).toEqual(["section", "p"]);
    expect(byTag(root, "section")).toHaveLength(1);
    const section = root.children[0];
    expect(section?.getAttribute("aria-labelledby")).toBe("shortcuts-heading");
    expect(byTag(root, "h2").map((node) => node.textContent)).toEqual(["Shortcuts"]);
    expect(byId(root, "shortcuts-heading").localName).toBe("h2");
    expect(root.children[1]?.classList.contains("settings-status")).toBe(true);
    expect(byTag(root, "h3").map((node) => node.textContent)).toEqual([
      "Key combination",
      "Menus",
      "Shortcuts everywhere",
    ]);
  });

  it("makes the status line a polite live region from the start", () => {
    const h = harness(() => new Promise(() => undefined));
    const line = oneByClass(h.root, "sc-status");
    expect(line.getAttribute("role")).toBe("status");
    expect(line.getAttribute("aria-live")).toBe("polite");
    expect(line.textContent).toBe("");
  });

  it("shows the error and no controls when the first load fails", async () => {
    const h = await shown(macos);
    expect(oneByClass(h.root, "sc-status").textContent).toBe("");
    const failed = harness(() => Promise.reject("Couldn't read your shortcuts."));
    await failed.page.idle();
    expect(oneByClass(failed.root, "sc-status").textContent).toBe("Couldn't read your shortcuts.");
    expect(byTag(failed.root, "input")).toEqual([]);
    expect(failed.invoke).toHaveBeenCalledTimes(1);
  });
});

describe("drawing the macOS view", () => {
  it("shows the switch, the key combination and where the wheel opens", async () => {
    const { root } = await shown(macos);
    expect(oneByClass(root, "section-hint").textContent).toBe(SHORTCUTS_COPY.lead);
    const enabled = byId(root, "sc-enabled");
    expect(enabled.getAttribute("role")).toBe("switch");
    expect(enabled.getAttribute("type")).toBe("checkbox");
    expect(enabled.checked).toBe(false);
    expect(byId(root, enabled.getAttribute("aria-describedby") ?? "").textContent).not.toBe("");

    expect(byId(root, "sc-hotkey-shift").checked).toBe(false);
    expect(byId(root, "sc-hotkey-ctrl").checked).toBe(true);
    expect(byId(root, "sc-hotkey-alt").checked).toBe(true);
    expect(byId(root, "sc-hotkey-meta").checked).toBe(false);
    const modifierLabels = byTag(root, "label")
      .filter((label) => /^sc-hotkey-(shift|ctrl|alt|meta)$/.test(label.getAttribute("for") ?? ""))
      .map((label) => label.textContent);
    expect(modifierLabels).toEqual(["Shift", "Control", "Option", "Command"]);
    const key = byId(root, "sc-hotkey-key");
    expect(key.value).toBe("KeyG");
    expect(options(key)).toEqual(["", ...macos.keys.map((choice) => choice.id)]);
    expect(byTag(key, "option").slice(0, 2).map((node) => node.textContent)).toEqual(["None", "A"]);
    expect(oneByClass(closest(key, "sc-chord"), "sc-chord-label").textContent).toBe("Control+Option+G");
    expect(oneByClass(root, "sc-binding").textContent).toBe("The wheel is off.");

    const opensAt = byId(root, "sc-opens-at");
    expect(opensAt.value).toBe("cursor");
    expect(options(opensAt)).toEqual(["cursor", "center"]);
    expect(oneByClass(root, "sc-warning").textContent).toBe(
      "Shortcuts are saved as plain text on this Mac. Don't put passwords in them.",
    );
  });

  it("shows that access is missing, how to mend it, and a button to ask", async () => {
    const { root } = await shown(macos);
    const box = oneByClass(root, "sc-access");
    expect(isHidden(box)).toBe(false);
    expect(oneByClass(box, "sc-access-text").textContent).toBe(SHORTCUTS_COPY.accessDenied);
    expect(oneByClass(box, "sc-access-text").getAttribute("role")).toBe("status");
    expect(box.textContent).toContain("If Grape is already in that list, remove it with − and add it again.");
    expect(isHidden(buttonLabelled(root, "Ask for access to press keys"))).toBe(false);
  });

  it("shows access as granted without the button, and no access row where it isn't needed", async () => {
    const granted = await shown({ ...macos, access: "granted" });
    const box = oneByClass(granted.root, "sc-access");
    expect(isHidden(box)).toBe(false);
    expect(oneByClass(box, "sc-access-text").textContent).toBe(SHORTCUTS_COPY.accessGranted);
    expect(isHidden(buttonLabelled(granted.root, "Ask for access to press keys"))).toBe(true);
    expect(isHidden(byClass(box, "sc-hint")[0] ?? box)).toBe(true);

    const none = await shown({ ...macos, access: "notNeeded" });
    expect(isHidden(oneByClass(none.root, "sc-access"))).toBe(true);
  });

  it("shows the Everywhere menu's six shortcuts as rows keyed by Rust's ids", async () => {
    const { root } = await shown(macos);
    const menu = byId(root, "sc-menu");
    expect(byTag(menu, "option").map((node) => node.textContent)).toEqual(["Everywhere"]);
    expect(menu.value).toBe("1");
    expect(isHidden(buttonLabelled(root, "Remove menu: Everywhere"))).toBe(true);
    expect(isHidden(byId(root, "sc-menu-everywhere"))).toBe(true);
    expect(isHidden(buttonLabelled(root, "Add an app and give it a menu"))).toBe(false);

    expect(ringNames(root)).toEqual(["Copy", "Paste", "Screenshot", "Mission Control", "Mute", "Lock screen"]);
    expect(isHidden(oneByClass(root, "sc-items"))).toBe(false);
    const copy = itemBox(root, 2);
    expect(copy.getAttribute("role")).toBe("group");
    expect(copy.getAttribute("aria-label")).toBe("Shortcut: Copy");
    expect(summaryOf(root, 2)).toBe("Copy");
    expect(byId(root, "sc-item-2-icon").value).toBe("glyph:clipboard");
    expect(isHidden(byId(root, "sc-item-2-emoji"))).toBe(true);
    expect(byId(root, "sc-item-2-name").getAttribute("maxlength")).toBe("24");

    const steps = stepBoxes(root, 2);
    expect(steps).toHaveLength(1);
    expect(oneByClass(steps[0] ?? copy, "sc-step-title").textContent).toBe("1. Clipboard");
    expect(steps[0]?.getAttribute("aria-label")).toBe("Step 1: Clipboard");
    expect(byId(root, "sc-step-3-action").value).toBe("copy");
    expect(options(byId(root, "sc-step-3-action"))).toEqual(["copy", "paste", "clear"]);
    expect(byId(root, "sc-step-7-control").value).toBe("screenshotArea");
    expect(byId(root, "sc-step-13-control").value).toBe("lockScreen");
  });

  it("draws each icon: a glyph as our own strokes, an emoji as text, none as nothing", async () => {
    const { root } = await shown(sample);
    const picture = (id: number): FakeElement => oneByClass(itemBox(root, id).children[0] ?? root, "sc-item-icon");
    expect(picture(2).getAttribute("aria-hidden")).toBe("true");
    const svg = picture(2).children[0];
    expect(svg?.localName).toBe("svg");
    expect(svg?.namespaceURI).toBe("http://www.w3.org/2000/svg");
    expect(svg?.getAttribute("viewBox")).toBe("0 0 24 24");
    expect(byTag(picture(2), "path").length).toBeGreaterThan(0);
    expect(picture(4).textContent).toBe("✍️");
    expect(picture(4).children).toHaveLength(0);
    expect(picture(8).textContent).toBe("");
    expect(picture(8).children).toHaveLength(0);
  });

  it("offers no icon, emoji, then every glyph by name", async () => {
    const { root } = await shown(sample);
    const icon = byId(root, "sc-item-4-icon");
    expect(options(icon)).toEqual(["none", "emoji", ...macos.glyphs.map((id) => `glyph:${id}`)]);
    expect(byTag(icon, "option").slice(0, 3).map((node) => node.textContent)).toEqual(["No icon", "Emoji…", "Key"]);
    expect(icon.value).toBe("emoji");
    const emoji = byId(root, "sc-item-4-emoji");
    expect(isHidden(emoji)).toBe(false);
    expect(emoji.value).toBe("✍️");
    expect(emoji.getAttribute("maxlength")).toBe("16");
    expect(byId(root, "sc-item-8-icon").value).toBe("none");
  });

  it("shows a group's shortcuts inside it, with an Add shortcut of their own", async () => {
    const { root } = await shown(sample);
    const group = itemBox(root, 8);
    expect(group.classList.contains("is-group")).toBe(true);
    expect(group.getAttribute("aria-label")).toBe("Group: Apps");
    expect(summaryOf(root, 8)).toBe("Group of 2 shortcuts");
    const children = oneByClass(group, "sc-children");
    expect(children.children).toHaveLength(2);
    expect(closest(byId(root, "sc-item-9-name"), "sc-children")).toBe(children);
    expect(closest(byId(root, "sc-item-12-name"), "sc-children")).toBe(children);
    expect(isHidden(buttonLabelled(group, "Add shortcut to Apps"))).toBe(false);
    // A group has no steps of its own, and only the menu's ring adds groups.
    expect(byTag(group, "select").map((node) => node.getAttribute("id"))).not.toContain("sc-item-8-add-step");
    expect(byTag(root, "button").filter((node) => node.textContent === "Add group")).toHaveLength(1);
    expect(ringNames(root)).toEqual(["Copy", "Sign", "Apps"]);
  });

  it("gives every step kind its own field", async () => {
    const everywhere = await shown(sample);
    const text = byId(everywhere.root, "sc-step-5-text");
    expect(text.localName).toBe("textarea");
    expect(text.value).toBe("Best,\nMax");
    expect(text.getAttribute("maxlength")).toBe("4000");
    // Its hint, then the line that says when its text wasn't saved: empty, and so not shown, until then.
    expect(text.getAttribute("aria-describedby")).toBe("sc-step-5-text-hint sc-step-5-text-unsaved");
    expect(byId(everywhere.root, "sc-step-5-text-hint").textContent).toContain("{clipboard}");
    expect(byClass(everywhere.root, "sc-unsaved").map((line) => line.textContent).join("")).toBe("");
    expect(text.hasAttribute("aria-invalid")).toBe(false);
    expect(byId(everywhere.root, "sc-step-6-shift").checked).toBe(true);
    expect(byId(everywhere.root, "sc-step-6-meta").checked).toBe(true);
    expect(byId(everywhere.root, "sc-step-6-ctrl").checked).toBe(false);
    expect(byId(everywhere.root, "sc-step-6-key").value).toBe("KeyK");
    const delay = byId(everywhere.root, "sc-step-7-ms");
    expect(delay.getAttribute("type")).toBe("number");
    expect(delay.value).toBe("250");
    expect(delay.getAttribute("min")).toBe("10");
    expect(delay.getAttribute("max")).toBe("60000");
    expect(delay.getAttribute("aria-describedby")).toBe("sc-step-7-ms-unsaved");
    const app = stepBoxes(everywhere.root, 9)[0];
    expect(oneByClass(app ?? everywhere.root, "sc-picked").textContent).toBe("No app chosen yet");
    expect(byTag(app ?? everywhere.root, "input")).toEqual([]);
    const url = byId(everywhere.root, "sc-step-11-url");
    expect(url.localName).toBe("input");
    expect(url.value).toBe("https://example.com");
    expect(url.getAttribute("maxlength")).toBe("2048");
    expect(url.getAttribute("spellcheck")).toBe("false");

    const { root } = await shownSafari();
    expect(byId(root, "sc-step-22-key").value).toBe("");
    expect(byId(root, "sc-step-22-shift").checked).toBe(false);
    expect(byId(root, "sc-step-23-placement").value).toBe("rightHalf");
    expect(byId(root, "sc-step-24-control").value).toBe("toggleMute");
    expect(byId(root, "sc-step-25-name").value).toBe("Start focus");
    expect(byId(root, "sc-step-25-name").getAttribute("maxlength")).toBe("4000");
    expect(byId(root, "sc-step-26-source").value).toBe("beep");
    expect(byId(root, "sc-step-26-source").localName).toBe("textarea");
    expect(byId(root, "sc-step-26-source").getAttribute("maxlength")).toBe("20000");
    expect(byId(root, "sc-step-27-source").value).toBe("echo hello");
    expect(byId(root, "sc-step-28-name").value).toBe("Tidy desktop");
    expect(byId(root, "sc-step-29-text").value).toBe("Break time");
    expect(byId(root, "sc-step-30-text").value).toBe("Stand up");
    const file = stepBoxes(root, 21)[9];
    expect(oneByClass(file ?? root, "sc-picked").textContent).toBe("Invoices");
    expect(stepBoxes(root, 21).map((box) => oneByClass(box, "sc-step-title").textContent)).toEqual([
      "1. Press keys",
      "2. Move the window",
      "3. System",
      "4. Run an Apple Shortcut",
      "5. Run AppleScript",
      "6. Run a shell script",
      "7. Run a Keyboard Maestro macro",
      "8. Show a notice",
      "9. Speak",
      "10. Open a file or folder",
    ]);
  });

  it("lists the fifteen step kinds under a prompt in Add a step", async () => {
    const { root } = await shown(sample);
    const add = byId(root, "sc-item-2-add-step");
    expect(add.value).toBe("");
    expect(options(add)).toEqual(["", ...STEP_KINDS]);
    expect(byTag(add, "option").map((node) => node.textContent)).toEqual([
      "Choose a kind…",
      ...STEP_KINDS.map((kind) => STEP_KIND_NAMES[kind]),
    ]);
  });

  it("shows an app's menu with its switch and its Remove button", async () => {
    const { root } = await shownSafari();
    const menu = byId(root, "sc-menu");
    expect(byTag(menu, "option").map((node) => node.textContent)).toEqual(["Everywhere", "Safari"]);
    expect(menu.value).toBe("20");
    expect(byTag(root, "h3").map((node) => node.textContent)).toContain("Shortcuts in Safari");
    expect(isHidden(buttonLabelled(root, "Remove menu: Safari"))).toBe(false);
    const also = byId(root, "sc-menu-everywhere");
    expect(isHidden(also)).toBe(false);
    expect(also.checked).toBe(true);
    expect(also.getAttribute("role")).toBe("switch");
    expect(ringNames(root)).toEqual(["Reload"]);
  });

  it("lists Everywhere first even when Rust lists an app before it", async () => {
    const reversed = changed(sample, (view) => {
      view.menus.reverse();
    });
    const { root } = await shown(reversed);
    expect(options(byId(root, "sc-menu"))).toEqual(["1", "20"]);
    expect(byId(root, "sc-menu").value).toBe("1");
    expect(ringNames(root)).toEqual(["Copy", "Sign", "Apps"]);
  });

  it("says so when a menu is empty", async () => {
    const empty = changed(macos, (view) => {
      menuOf(view, EVERYWHERE).items = [];
    });
    const { root } = await shown(empty);
    const hint = byClass(root, "sc-hint").filter((node) => node.textContent === SHORTCUTS_COPY.emptyRing);
    expect(hint).toHaveLength(1);
    expect(isHidden(hint[0] ?? root)).toBe(false);
    const filled = await shown(macos);
    const other = byClass(filled.root, "sc-hint").filter((node) => node.textContent === SHORTCUTS_COPY.emptyRing);
    expect(isHidden(other[0] ?? filled.root)).toBe(true);
  });

  it("shows no key and a prompt when no key combination is set", async () => {
    const { root } = await shown({ ...macos, hotkey: null });
    expect(byId(root, "sc-hotkey-key").value).toBe("");
    for (const modifier of ["shift", "ctrl", "alt", "meta"]) {
      expect(byId(root, `sc-hotkey-${modifier}`).checked).toBe(false);
    }
    expect(oneByClass(closest(byId(root, "sc-hotkey-key"), "sc-chord"), "sc-chord-label").textContent).toBe("Not set");
    expect(oneByClass(root, "sc-binding").textContent).toContain("Choose a key");
  });

  it("shows the note that came with the view", async () => {
    const { root } = await shown({ ...sample, note: "Some shortcuts couldn't be read and were left out." });
    const note = oneByClass(root, "settings-note");
    expect(note.getAttribute("role")).toBe("status");
    expect(note.textContent).toBe("Some shortcuts couldn't be read and were left out.");
  });

  it("sets everything the user typed with textContent or a value, never markup", async () => {
    const hostile = changed(sample, (view) => {
      const sign = stepsOf(view, 4);
      sign.name = "<b>Sign</b>";
      sign.icon = { kind: "emoji", value: "<i>" };
      menuOf(view, SAFARI).scope = { kind: "app", name: "<img src=x>" };
      view.note = "<script>";
    });
    const { root } = await shown(hostile);
    expect(byTag(root, "b")).toEqual([]);
    expect(byTag(root, "i")).toEqual([]);
    expect(byTag(root, "img")).toEqual([]);
    expect(byTag(root, "script")).toEqual([]);
    expect(byId(root, "sc-item-4-name").value).toBe("<b>Sign</b>");
    expect(itemBox(root, 4).getAttribute("aria-label")).toBe("Shortcut: <b>Sign</b>");
    expect(byTag(byId(root, "sc-menu"), "option")[1]?.textContent).toBe("<img src=x>");
    expect(oneByClass(root, "settings-note").children).toHaveLength(0);
  });
});

describe("drawing the Windows view", () => {
  it("shows the heading and one line, and no controls", async () => {
    const { root, invoke } = await shown(windows);
    const section = oneByClass(root, "shortcuts");
    expect(section.children.map((node) => node.localName)).toEqual(["h2", "p"]);
    expect(section.children[0]?.textContent).toBe("Shortcuts");
    expect(section.children[1]?.textContent).toBe(SHORTCUTS_COPY.unavailable);
    expect(section.children[1]?.textContent).toContain("on the Mac first");
    for (const tag of ["input", "select", "textarea", "button", "label", "h3"]) {
      expect(byTag(root, tag), tag).toEqual([]);
    }
    expect(oneByClass(root, "sc-status").textContent).toBe("");
    expect(invoke).toHaveBeenCalledTimes(1);
  });

  it("draws the same from a view with no menu at all", async () => {
    const { root } = await shown({ ...windows, menus: [] });
    expect(oneByClass(root, "shortcuts").children.map((node) => node.localName)).toEqual(["h2", "p"]);
    expect(oneByClass(root, "section-hint").textContent).toBe(SHORTCUTS_COPY.unavailable);
    expect(oneByClass(root, "sc-status").textContent).toBe("");
  });

  it("takes the controls away again if a later view says the wheel isn't available", async () => {
    const h = await shown(macos, () => Promise.resolve(windows));
    expect(byTag(h.root, "input").length).toBeGreaterThan(0);
    change(byId(h.root, "sc-enabled"), { checked: true });
    await h.page.idle();
    expect(byTag(h.root, "input")).toEqual([]);
    expect(oneByClass(h.root, "section-hint").textContent).toBe(SHORTCUTS_COPY.unavailable);
  });
});

describe("labels and names", () => {
  it("hold in the macOS view", async () => {
    const { root } = await shown(macos);
    expectWellFormed(root);
  });

  it("hold in the Windows view, and before the first view", async () => {
    expectWellFormed((await shown(windows)).root);
    expectWellFormed(harness(() => new Promise(() => undefined)).root);
  });

  it("hold with a group, an emoji icon and every step kind, in either menu", async () => {
    const { root } = await shown(sample);
    expectWellFormed(root);
    change(byId(root, "sc-menu"), { value: String(SAFARI) });
    expectWellFormed(root);
    change(byId(root, "sc-menu"), { value: String(EVERYWHERE) });
    expectWellFormed(root);
  });

  it("hold while an emoji is being chosen, with no key combination, and at the limits", async () => {
    const limits = { ...sample.limits, items: 3, steps: 3, menus: 2 };
    const { root } = await shown({ ...sample, hotkey: null, limits });
    change(byId(root, "sc-item-2-icon"), { value: "emoji" });
    expectWellFormed(root);
  });

  it("name every row's buttons after the row", async () => {
    const { root } = await shown(sample);
    const labels = (box: FakeElement): (string | null)[] =>
      byTag(box.children[0] ?? box, "button").map((node) => node.getAttribute("aria-label"));
    expect(labels(itemBox(root, 4))).toEqual(["Move up: Sign", "Move down: Sign", "Remove: Sign"]);
    expect(labels(stepBoxes(root, 4)[1] ?? root)).toEqual([
      "Move up: step 2 of Sign",
      "Move down: step 2 of Sign",
      "Remove: step 2 of Sign",
    ]);
    expect(buttonLabelled(root, "Choose the app to open").textContent).toBe("Choose…");
    expect(closest(byId(root, "sc-step-6-key"), "sc-chord").getAttribute("aria-label")).toBe("Keys to press");
  });

  it("switch off Move up on the first row and Move down on the last", async () => {
    const { root } = await shown(sample);
    expect(buttonLabelled(root, "Move up: Copy").disabled).toBe(true);
    expect(buttonLabelled(root, "Move down: Copy").disabled).toBe(false);
    expect(buttonLabelled(root, "Move up: Sign").disabled).toBe(false);
    expect(buttonLabelled(root, "Move down: Apps").disabled).toBe(true);
    expect(buttonLabelled(root, "Move up: Browser").disabled).toBe(true);
    expect(buttonLabelled(root, "Move down: Notes").disabled).toBe(true);
    expect(buttonLabelled(root, "Move up: step 1 of Sign").disabled).toBe(true);
    expect(buttonLabelled(root, "Move down: step 3 of Sign").disabled).toBe(true);
    // A lone step can't move at all.
    expect(buttonLabelled(root, "Move up: step 1 of Copy").disabled).toBe(true);
    expect(buttonLabelled(root, "Move down: step 1 of Copy").disabled).toBe(true);
  });
});

describe("the wheel's own settings", () => {
  it("sends the switch", async () => {
    const h = await shown(macos, () => Promise.resolve({ ...macos, enabled: true, binding: "bound" }));
    change(byId(h.root, "sc-enabled"), { checked: true });
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_set_enabled", { on: true }]]);
    expect(byId(h.root, "sc-enabled").checked).toBe(true);
    expect(oneByClass(h.root, "sc-binding").textContent).toBe("Control+Option+G opens the wheel.");
  });

  it("puts the switch back, with Rust's sentence, when it's refused", async () => {
    const h = await shown(
      { ...macos, hotkey: null },
      answering({
        shortcuts_set_enabled: () => Promise.reject("Choose a key combination for the wheel first."),
        shortcuts_get: () => Promise.resolve({ ...macos, hotkey: null }),
      }),
    );
    change(byId(h.root, "sc-enabled"), { checked: true });
    await h.page.idle();
    expect(byId(h.root, "sc-enabled").checked).toBe(false);
    expect(oneByClass(h.root, "sc-status").textContent).toBe("Choose a key combination for the wheel first.");
  });

  it("shows Set shortcut in the key combination, off while the controls show what Rust holds", async () => {
    const { root } = await shown(macos);
    const set = buttonLabelled(root, SET_SHORTCUT);
    expect(set.textContent).toBe("Set shortcut");
    expect(closest(set, "sc-chord")).toBe(closest(byId(root, "sc-hotkey-key"), "sc-chord"));
    expect(set.disabled).toBe(true);
    // A Keys step still sends each chord as it is made: it has no button.
    const step = await shown(sample);
    expect(byTag(closest(byId(step.root, "sc-step-6-key"), "sc-chord"), "button")).toEqual([]);
  });

  it("sends nothing while a chord is being made, and the whole chord once from Set shortcut", async () => {
    const chord = { key: "F5", shift: false, ctrl: true, alt: false, meta: true };
    const h = await shown(macos, () =>
      Promise.resolve({ ...macos, hotkey: { chord, label: "Control+Command+F5" } }),
    );
    const set = buttonLabelled(h.root, SET_SHORTCUT);
    change(byId(h.root, "sc-hotkey-meta"), { checked: true });
    expect(set.disabled).toBe(false);
    change(byId(h.root, "sc-hotkey-alt"), { checked: false });
    change(byId(h.root, "sc-hotkey-key"), { value: "F5" });
    await h.page.idle();
    // Control+Option+Command+G and the chords after it were never Grape's to bind.
    expect(commands(h)).toEqual([]);
    expect(oneByClass(h.root, "sc-chord-label").textContent).toBe("Control+Option+G");

    click(set);
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_set_hotkey", { chord }]]);
    expect(JSON.stringify(commands(h)[0]?.[1])).toBe(
      '{"chord":{"key":"F5","shift":false,"ctrl":true,"alt":false,"meta":true}}',
    );
    expect(oneByClass(h.root, "sc-chord-label").textContent).toBe("Control+Command+F5");
    // Rust holds the draft now: the button is off, and focus didn't stay on it.
    expect(set.disabled).toBe(true);
    expect(h.doc.activeElement).toBe(byId(h.root, "sc-hotkey-key"));
  });

  it("switches Set shortcut off again when the draft goes back to what Rust holds", async () => {
    const h = await shown(macos);
    const set = buttonLabelled(h.root, SET_SHORTCUT);
    change(byId(h.root, "sc-hotkey-shift"), { checked: true });
    expect(set.disabled).toBe(false);
    change(byId(h.root, "sc-hotkey-shift"), { checked: false });
    expect(set.disabled).toBe(true);
    click(set);
    await h.page.idle();
    expect(commands(h)).toEqual([]);
  });

  it("sends one command for a double click on Set shortcut", async () => {
    const reply = deferred();
    const h = await shown(macos, () => reply.promise);
    change(byId(h.root, "sc-hotkey-key"), { value: "KeyH" });
    click(buttonLabelled(h.root, SET_SHORTCUT));
    clickAgain(buttonLabelled(h.root, SET_SHORTCUT));
    reply.resolve(macos);
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["shortcuts_set_hotkey", { chord: { key: "KeyH", shift: false, ctrl: true, alt: true, meta: false } }],
    ]);
  });

  it("clears the key combination with None and Set shortcut, and draws what Rust answers", async () => {
    const off = { ...macos, hotkey: null };
    const h = await shown(macos, () => Promise.resolve(off));
    change(byId(h.root, "sc-hotkey-key"), { value: "" });
    await h.page.idle();
    expect(commands(h)).toEqual([]);
    click(buttonLabelled(h.root, SET_SHORTCUT));
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_set_hotkey", { chord: null }]]);
    expect(byId(h.root, "sc-hotkey-key").value).toBe("");
    expect(oneByClass(h.root, "sc-binding").textContent).toContain("Choose a key");
    expect(buttonLabelled(h.root, SET_SHORTCUT).disabled).toBe(true);
  });

  it("holds modifiers ticked with no key as part of the draft, through a redraw", async () => {
    const off = { ...macos, hotkey: null };
    const h = await shown(
      off,
      answering({
        shortcuts_set_opens_at: () => Promise.resolve({ ...off, opensAt: "center" }),
        shortcuts_set_hotkey: (_command, args) =>
          Promise.resolve({ ...off, hotkey: { chord: args["chord"] as ChordWire, label: "Control+Shift+Space" } }),
      }),
    );
    const set = buttonLabelled(h.root, SET_SHORTCUT);
    change(byId(h.root, "sc-hotkey-ctrl"), { checked: true });
    change(byId(h.root, "sc-hotkey-shift"), { checked: true });
    // No key is no chord, which is what Rust holds: there is nothing to set yet.
    expect(set.disabled).toBe(true);

    // Another change redraws the page: the ticks stay.
    change(byId(h.root, "sc-opens-at"), { value: "center" });
    await h.page.idle();
    expect(byId(h.root, "sc-hotkey-ctrl").checked).toBe(true);
    expect(byId(h.root, "sc-hotkey-shift").checked).toBe(true);
    expect(byId(h.root, "sc-hotkey-key").value).toBe("");

    change(byId(h.root, "sc-hotkey-key"), { value: "Space" });
    expect(set.disabled).toBe(false);
    click(set);
    await h.page.idle();
    expect(commands(h)[1]).toEqual([
      "shortcuts_set_hotkey",
      { chord: { key: "Space", shift: true, ctrl: true, alt: false, meta: false } },
    ]);
    expect(commands(h)).toHaveLength(2);
  });

  it("keeps a draft through a redraw and a re-read, while the line shows what Rust holds", async () => {
    const h = await shown(macos, (command) =>
      Promise.resolve(command === "shortcuts_set_opens_at" ? { ...macos, opensAt: "center" } : macos),
    );
    change(byId(h.root, "sc-hotkey-meta"), { checked: true });
    change(byId(h.root, "sc-hotkey-key"), { value: "KeyJ" });
    change(byId(h.root, "sc-opens-at"), { value: "center" });
    await h.page.idle();
    h.win.dispatch("focus");
    await h.page.idle();
    expect(commands(h).map(([command]) => command)).toEqual(["shortcuts_set_opens_at", "shortcuts_get"]);
    expect(byId(h.root, "sc-hotkey-meta").checked).toBe(true);
    expect(byId(h.root, "sc-hotkey-key").value).toBe("KeyJ");
    expect(buttonLabelled(h.root, SET_SHORTCUT).disabled).toBe(false);
    expect(oneByClass(h.root, "sc-chord-label").textContent).toBe("Control+Option+G");
    expect(oneByClass(h.root, "sc-binding").textContent).toBe("The wheel is off.");
    expectWellFormed(h.root);
  });

  it("does nothing with an unsent draft when Settings closes or hides: the stored one stays", async () => {
    const h = await shown(macos);
    change(byId(h.root, "sc-hotkey-meta"), { checked: true });
    change(byId(h.root, "sc-hotkey-key"), { value: "KeyJ" });
    h.win.dispatch("pagehide");
    h.doc.visibilityState = "hidden";
    h.doc.dispatch("visibilitychange");
    await h.page.idle();
    expect(commands(h)).toEqual([]);
  });

  it("sends nothing for a key the view doesn't offer, and puts the select back", async () => {
    const h = await shown(macos);
    change(byId(h.root, "sc-hotkey-key"), { value: "Fn" });
    expect(byId(h.root, "sc-hotkey-key").value).toBe("KeyG");
    expect(buttonLabelled(h.root, SET_SHORTCUT).disabled).toBe(true);
    await h.page.idle();
    expect(commands(h)).toEqual([]);
  });

  it("keeps the draft, with Rust's sentence, when the chord is refused", async () => {
    const h = await shown(
      macos,
      answering({
        shortcuts_set_hotkey: () => Promise.reject("Grape couldn't use that shortcut. Try another."),
        shortcuts_get: () => Promise.resolve(macos),
      }),
    );
    const set = buttonLabelled(h.root, SET_SHORTCUT);
    change(byId(h.root, "sc-hotkey-meta"), { checked: true });
    change(byId(h.root, "sc-hotkey-key"), { value: "KeyQ" });
    click(set);
    await h.page.idle();
    expect(commands(h)).toEqual([
      [
        "shortcuts_set_hotkey",
        { chord: { key: "KeyQ", shift: false, ctrl: true, alt: true, meta: true } },
      ],
      ["shortcuts_get", {}],
    ]);
    // The draft is still there to mend, and can be sent again.
    expect(byId(h.root, "sc-hotkey-key").value).toBe("KeyQ");
    expect(byId(h.root, "sc-hotkey-meta").checked).toBe(true);
    expect(set.disabled).toBe(false);
    expect(h.doc.activeElement).toBe(set);
    expect(oneByClass(h.root, "sc-chord-label").textContent).toBe("Control+Option+G");
    expect(oneByClass(h.root, "sc-status").textContent).toBe("Grape couldn't use that shortcut. Try another.");
  });

  it("says when macOS wouldn't give Grape the chord", async () => {
    const { root } = await shown({ ...macos, enabled: true, binding: "failed" });
    expect(oneByClass(root, "sc-binding").textContent).toContain("macOS wouldn't give Grape that shortcut");
    expect(oneByClass(root, "sc-binding").getAttribute("role")).toBe("status");
  });

  it("draws a Mac whose hot keys couldn't start as a Mac: the whole section, and what mends it", async () => {
    // Rust reports it as available, with the binding failed.
    const failed: ShortcutsView = { ...sample, enabled: false, binding: "failed" };
    const h = await shown(
      failed,
      answering({
        shortcuts_set_enabled: () => Promise.reject("macOS wouldn't give Grape that shortcut."),
        shortcuts_get: () => Promise.resolve(failed),
      }),
    );
    expect(oneByClass(h.root, "section-hint").textContent).toBe(SHORTCUTS_COPY.lead);
    expect(h.root.textContent).not.toContain("on the Mac first");
    expect(ringNames(h.root)).toEqual(["Copy", "Sign", "Apps"]);
    expect(oneByClass(h.root, "sc-binding").textContent).toBe(
      "macOS wouldn't give Grape that shortcut. Try another, or quit Grape and open it again.",
    );
    const enabled = byId(h.root, "sc-enabled");
    expect(enabled.checked).toBe(false);
    expect(enabled.disabled).toBe(false);

    // Turning it on is Rust's to refuse: the switch goes back, under Rust's own sentence.
    change(enabled, { checked: true });
    await h.page.idle();
    expect(enabled.checked).toBe(false);
    expect(oneByClass(h.root, "sc-status").textContent).toBe("macOS wouldn't give Grape that shortcut.");
    expect(oneByClass(h.root, "sc-binding").textContent).toContain("quit Grape and open it again");

    // And it reads the view again on focus, as on every Mac.
    h.win.dispatch("focus");
    await h.page.idle();
    expect(commands(h).at(-1)).toEqual(["shortcuts_get", {}]);
    expectWellFormed(h.root);
  });

  it("reads the same line with the switch on", async () => {
    const { root } = await shown({ ...sample, enabled: true, binding: "failed" });
    expect(byId(root, "sc-enabled").checked).toBe(true);
    expect(oneByClass(root, "sc-binding").textContent).toContain("quit Grape and open it again");
    expect(oneByClass(root, "section-hint").textContent).toBe(SHORTCUTS_COPY.lead);
  });

  it("sends where the wheel opens", async () => {
    const h = await shown(macos, () => Promise.resolve({ ...macos, opensAt: "center" }));
    change(byId(h.root, "sc-opens-at"), { value: "center" });
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_set_opens_at", { at: "center" }]]);
    expect(byId(h.root, "sc-opens-at").value).toBe("center");

    change(byId(h.root, "sc-opens-at"), { value: "corner" });
    await h.page.idle();
    expect(commands(h)).toHaveLength(1);
    expect(byId(h.root, "sc-opens-at").value).toBe("center");
  });

  it("asks for access from its button, and moves focus when the button goes", async () => {
    const h = await shown(macos, () => Promise.resolve({ ...macos, access: "granted" }));
    const ask = buttonLabelled(h.root, "Ask for access to press keys");
    click(ask);
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_request_access", {}]]);
    expect(isHidden(ask)).toBe(true);
    expect(h.doc.activeElement).toBe(byId(h.root, "sc-enabled"));
  });
});

describe("menus", () => {
  it("adds an app through Rust's picker and shows the new menu", async () => {
    const withMail = changed(sample, (view) => {
      view.menus.push({ id: 40, scope: { kind: "app", name: "Mail" }, withEverywhere: false, items: [] });
    });
    const h = await shown(sample, () => Promise.resolve(withMail));
    const add = buttonLabelled(h.root, "Add an app and give it a menu");
    expect(add.textContent).toBe("Add an app…");
    click(add);
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_add_menu", {}]]);
    expect(byId(h.root, "sc-menu").value).toBe("40");
    expect(byTag(byId(h.root, "sc-menu"), "option").map((node) => node.textContent)).toEqual([
      "Everywhere",
      "Safari",
      "Mail",
    ]);
    expect(byTag(h.root, "h3").map((node) => node.textContent)).toContain("Shortcuts in Mail");
    expect(ringNames(h.root)).toEqual([]);
    expect(h.doc.activeElement).toBe(add);
  });

  it("stays on the same menu when the picker is cancelled", async () => {
    const h = await shownSafari();
    click(buttonLabelled(h.root, "Add an app and give it a menu"));
    await h.page.idle();
    expect(byId(h.root, "sc-menu").value).toBe("20");
    expect(ringNames(h.root)).toEqual(["Reload"]);
  });

  it("removes the chosen app's menu, goes back to Everywhere and keeps focus on the page", async () => {
    const without = changed(sample, (view) => {
      view.menus.pop();
    });
    const h = await shownSafari(() => Promise.resolve(without));
    click(buttonLabelled(h.root, "Remove menu: Safari"));
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_remove_menu", { menu: 20 }]]);
    expect(byId(h.root, "sc-menu").value).toBe("1");
    expect(options(byId(h.root, "sc-menu"))).toEqual(["1"]);
    expect(ringNames(h.root)).toEqual(["Copy", "Sign", "Apps"]);
    expect(isHidden(buttonLabelled(h.root, "Remove menu: Everywhere"))).toBe(true);
    expect(h.doc.activeElement).toBe(byId(h.root, "sc-menu"));
  });

  it("ignores a second click on Remove menu until the first has answered, and a double click's second", async () => {
    const reply = deferred();
    const h = await shownSafari(() => reply.promise);
    const remove = buttonLabelled(h.root, "Remove menu: Safari");
    clickAgain(remove);
    await flush();
    expect(commands(h)).toEqual([]);
    click(remove);
    click(remove);
    await flush();
    expect(commands(h)).toEqual([["shortcuts_remove_menu", { menu: 20 }]]);
    expect(isWaiting(remove)).toBe(true);
    reply.resolve(sample);
    await h.page.idle();
    expect(isWaiting(remove)).toBe(false);
  });

  it("sends the Everywhere switch for the chosen menu, and shows Rust's note", async () => {
    const off = changed(sample, (view) => {
      menuOf(view, SAFARI).withEverywhere = false;
    });
    const h = await shownSafari(
      answering({
        shortcuts_set_menu_everywhere: (_command, args) =>
          Promise.resolve(args["on"] === true ? { ...sample, note: "Everywhere didn't fit in this menu." } : off),
      }),
    );
    change(byId(h.root, "sc-menu-everywhere"), { checked: false });
    await h.page.idle();
    expect(byId(h.root, "sc-menu-everywhere").checked).toBe(false);
    expect(oneByClass(h.root, "settings-note").textContent).toBe("");
    change(byId(h.root, "sc-menu-everywhere"), { checked: true });
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["shortcuts_set_menu_everywhere", { menu: 20, on: false }],
      ["shortcuts_set_menu_everywhere", { menu: 20, on: true }],
    ]);
    expect(oneByClass(h.root, "settings-note").textContent).toBe("Everywhere didn't fit in this menu.");
  });

  it("changes menus without a command, swapping the rows", async () => {
    const h = await shown(sample);
    const copy = itemBox(h.root, 2);
    change(byId(h.root, "sc-menu"), { value: String(SAFARI) });
    await h.page.idle();
    expect(commands(h)).toEqual([]);
    expect(ringNames(h.root)).toEqual(["Reload"]);
    expect(copy.parent).toBeNull();
    change(byId(h.root, "sc-menu"), { value: "999" });
    expect(byId(h.root, "sc-menu").value).toBe("20");
    expect(ringNames(h.root)).toEqual(["Reload"]);
  });

  it("sends what was typed in a row before another menu's rows replace it", async () => {
    const h = await shown(sample);
    type(byId(h.root, "sc-item-2-name"), "Duplicate");
    change(byId(h.root, "sc-menu"), { value: String(SAFARI) });
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["shortcuts_set_item", { item: 2, name: "Duplicate", icon: { kind: "glyph", value: "clipboard" } }],
    ]);
    expect(ringNames(h.root)).toEqual(["Reload"]);
  });
});

describe("shortcuts and groups", () => {
  it("adds a shortcut to the chosen menu's ring and moves focus to its name", async () => {
    const more = changed(sample, (view) => {
      const fresh: StepsItem = { id: 50, name: "New shortcut", icon: { kind: "none" }, kind: "steps", steps: [] };
      menuOf(view, EVERYWHERE).items.push(fresh);
    });
    const h = await shown(sample, () => Promise.resolve(more));
    click(buttonLabelled(h.root, "Add shortcut"));
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_add_item", { menu: 1, group: null, kind: "steps" }]]);
    expect(ringNames(h.root)).toEqual(["Copy", "Sign", "Apps", "New shortcut"]);
    expect(h.doc.activeElement).toBe(byId(h.root, "sc-item-50-name"));
    expect(summaryOf(h.root, 50)).toBe("No steps yet");
  });

  it("adds a group to the ring, and a shortcut to a group", async () => {
    const h = await shownSafari();
    click(buttonLabelled(h.root, "Add group"));
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_add_item", { menu: 20, group: null, kind: "group" }]]);

    const inGroup = changed(sample, (view) => {
      const apps = itemOf(view, 8);
      if (apps.kind !== "group") throw new Error("not a group");
      apps.items.push({ id: 51, name: "New shortcut", icon: { kind: "none" }, kind: "steps", steps: [] });
    });
    const g = await shown(sample, () => Promise.resolve(inGroup));
    click(buttonLabelled(g.root, "Add shortcut to Apps"));
    await g.page.idle();
    expect(commands(g)).toEqual([["shortcuts_add_item", { menu: 1, group: 8, kind: "steps" }]]);
    expect(closest(byId(g.root, "sc-item-51-name"), "sc-children")).toBe(oneByClass(itemBox(g.root, 8), "sc-children"));
    expect(g.doc.activeElement).toBe(byId(g.root, "sc-item-51-name"));
    expect(ringNames(g.root)).toEqual(["Copy", "Sign", "Apps"]);
  });

  it("sends a new name with the icon it has, when the field is left", async () => {
    const renamed = changed(sample, (view) => {
      itemOf(view, 2).name = "Duplicate";
    });
    const h = await shown(sample, () => Promise.resolve(renamed));
    const name = byId(h.root, "sc-item-2-name");
    type(name, "  Duplicate ");
    await h.page.idle();
    expect(commands(h)).toEqual([]);
    name.dispatch("change");
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["shortcuts_set_item", { item: 2, name: "  Duplicate ", icon: { kind: "glyph", value: "clipboard" } }],
    ]);
    // Rust trimmed it, and the row follows.
    expect(name.value).toBe("Duplicate");
    expect(itemBox(h.root, 2).getAttribute("aria-label")).toBe("Shortcut: Duplicate");
    expect(buttonLabelled(h.root, "Remove: Duplicate").textContent).toBe("Remove");
  });

  it("sends an emoji icon's name change with the emoji", async () => {
    const h = await shown(sample);
    change(byId(h.root, "sc-item-4-name"), { value: "Signature" });
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["shortcuts_set_item", { item: 4, name: "Signature", icon: { kind: "emoji", value: "✍️" } }],
    ]);
  });

  it("sends a glyph or no icon as soon as it's chosen", async () => {
    const starred = changed(sample, (view) => {
      itemOf(view, 2).icon = { kind: "glyph", value: "star" };
    });
    const h = await shown(sample, () => Promise.resolve(starred));
    change(byId(h.root, "sc-item-2-icon"), { value: "glyph:star" });
    await h.page.idle();
    change(byId(h.root, "sc-item-4-icon"), { value: "none" });
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["shortcuts_set_item", { item: 2, name: "Copy", icon: { kind: "glyph", value: "star" } }],
      ["shortcuts_set_item", { item: 4, name: "Sign", icon: { kind: "none" } }],
    ]);
    expect(byId(h.root, "sc-item-2-icon").value).toBe("glyph:star");
  });

  it("waits for the emoji to be typed before it sends an emoji icon", async () => {
    const grape = changed(sample, (view) => {
      itemOf(view, 2).icon = { kind: "emoji", value: "🍇" };
    });
    const h = await shown(sample, answering({ shortcuts_set_item: () => Promise.resolve(grape) }));
    const icon = byId(h.root, "sc-item-2-icon");
    const emoji = byId(h.root, "sc-item-2-emoji");
    change(icon, { value: "emoji" });
    await h.page.idle();
    expect(commands(h)).toEqual([]);
    expect(isHidden(emoji)).toBe(false);
    expect(h.doc.activeElement).toBe(emoji);

    change(emoji, { value: "🍇" });
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["shortcuts_set_item", { item: 2, name: "Copy", icon: { kind: "emoji", value: "🍇" } }],
    ]);
    expect(icon.value).toBe("emoji");
    expect(emoji.value).toBe("🍇");
    expect(oneByClass(itemBox(h.root, 2).children[0] ?? h.root, "sc-item-icon").textContent).toBe("🍇");
  });

  it("keeps Emoji chosen through a redraw and after the empty field is left, sending nothing", async () => {
    const h = await shown(sample);
    const icon = byId(h.root, "sc-item-2-icon");
    const emoji = byId(h.root, "sc-item-2-emoji");
    change(icon, { value: "emoji" });
    change(byId(h.root, "sc-enabled"), { checked: true });
    await h.page.idle();
    expect(icon.value).toBe("emoji");
    expect(isHidden(emoji)).toBe(false);

    // The window stops being the key one (the system's emoji picker, another app): the field blurs.
    emoji.dispatch("blur");
    emoji.dispatch("change");
    await h.page.idle();
    expect(icon.value).toBe("emoji");
    expect(isHidden(emoji)).toBe(false);
    expect(h.doc.activeElement).toBe(emoji);

    // So does a real move to another control, and a re-read when the window comes back.
    byId(h.root, "sc-item-2-name").focus();
    emoji.dispatch("blur");
    h.win.dispatch("focus");
    await h.page.idle();
    expect(icon.value).toBe("emoji");
    expect(isHidden(emoji)).toBe(false);
    expect(commands(h)).toEqual([["shortcuts_set_enabled", { on: true }], ["shortcuts_get", {}]]);
    // The shortcut keeps the icon it has until an emoji is typed: a rename sends that one.
    change(byId(h.root, "sc-item-2-name"), { value: "Duplicate" });
    await h.page.idle();
    expect(commands(h).at(-1)).toEqual([
      "shortcuts_set_item",
      { item: 2, name: "Duplicate", icon: { kind: "glyph", value: "clipboard" } },
    ]);
    expectWellFormed(h.root);
  });

  it("gives Emoji up only when another icon is chosen", async () => {
    const h = await shown(sample);
    const icon = byId(h.root, "sc-item-2-icon");
    const emoji = byId(h.root, "sc-item-2-emoji");
    change(icon, { value: "emoji" });
    emoji.dispatch("blur");
    change(icon, { value: "glyph:clipboard" });
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["shortcuts_set_item", { item: 2, name: "Copy", icon: { kind: "glyph", value: "clipboard" } }],
    ]);
    expect(icon.value).toBe("glyph:clipboard");
    expect(isHidden(emoji)).toBe(true);
    expect(h.doc.activeElement).not.toBe(emoji);
  });

  it("leaves Emoji chosen, with the field empty again, when Rust refuses what was typed", async () => {
    const h = await shown(
      sample,
      answering({
        shortcuts_set_item: () => Promise.reject("An emoji icon is one emoji, with no spaces."),
        shortcuts_get: () => Promise.resolve(sample),
      }),
    );
    const icon = byId(h.root, "sc-item-2-icon");
    const emoji = byId(h.root, "sc-item-2-emoji");
    change(icon, { value: "emoji" });
    change(emoji, { value: "a b" });
    await h.page.idle();
    expect(icon.value).toBe("emoji");
    expect(isHidden(emoji)).toBe(false);
    // A short field goes back to what Rust holds: it travels with every rename.
    expect(emoji.value).toBe("");
    expect(oneByClass(h.root, "sc-status").textContent).toBe("An emoji icon is one emoji, with no spaces.");
  });

  it("puts an emptied emoji back instead of sending it", async () => {
    const h = await shown(sample);
    change(byId(h.root, "sc-item-4-emoji"), { value: "" });
    await h.page.idle();
    expect(commands(h)).toEqual([]);
    expect(byId(h.root, "sc-item-4-emoji").value).toBe("✍️");
  });

  it("removes a shortcut and moves focus to Add shortcut", async () => {
    const without = changed(sample, (view) => {
      menuOf(view, EVERYWHERE).items.splice(1, 1);
    });
    const h = await shown(sample, () => Promise.resolve(without));
    const apps = itemBox(h.root, 8);
    click(buttonLabelled(h.root, "Remove: Sign"));
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_remove_item", { item: 4 }]]);
    expect(ringNames(h.root)).toEqual(["Copy", "Apps"]);
    expect(itemBox(h.root, 8)).toBe(apps);
    expect(h.doc.activeElement).toBe(buttonLabelled(h.root, "Add shortcut"));
  });

  it("ignores the second click of a double click on Remove, whichever row it lands on", async () => {
    const without = changed(sample, (view) => {
      menuOf(view, EVERYWHERE).items.splice(0, 1);
    });
    const h = await shown(sample, answering({ shortcuts_remove_item: () => Promise.resolve(without) }));
    click(buttonLabelled(h.root, "Remove: Copy"));
    await h.page.idle();
    expect(ringNames(h.root)).toEqual(["Sign", "Apps"]);
    // Sign slid up under the pointer, and the second click of the pair lands on its Remove.
    clickAgain(buttonLabelled(h.root, "Remove: Sign"));
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_remove_item", { item: 2 }]]);
    expect(ringNames(h.root)).toEqual(["Sign", "Apps"]);

    // A click of its own, or a press from the keyboard, still removes.
    press(buttonLabelled(h.root, "Remove: Sign"));
    await h.page.idle();
    expect(commands(h)).toHaveLength(2);
    expect(commands(h)[1]).toEqual(["shortcuts_remove_item", { item: 4 }]);
  });

  it("does nothing more with a Remove whose command hasn't answered, and wakes it if Rust refuses", async () => {
    const reply = deferred();
    const h = await shown(
      sample,
      answering({ shortcuts_remove_item: () => reply.promise, shortcuts_get: () => Promise.resolve(sample) }),
    );
    const remove = buttonLabelled(h.root, "Remove: Sign");
    click(remove);
    expect(isWaiting(remove)).toBe(true);
    // It keeps the focus and its place in the Tab order.
    expect(remove.disabled).toBe(false);
    expect(h.doc.activeElement).toBe(remove);
    click(remove);
    press(remove);
    await flush();
    expect(commands(h)).toEqual([["shortcuts_remove_item", { item: 4 }]]);
    // Other rows' buttons are their own.
    expect(isWaiting(buttonLabelled(h.root, "Remove: Copy"))).toBe(false);

    reply.reject("Couldn't save your shortcuts.");
    await h.page.idle();
    expect(isWaiting(remove)).toBe(false);
    click(remove);
    await flush();
    expect(commands(h).filter(([command]) => command === "shortcuts_remove_item")).toHaveLength(2);
  });

  it("removes a group's shortcut and moves focus to the group's Add shortcut", async () => {
    const without = changed(sample, (view) => {
      const apps = itemOf(view, 8);
      if (apps.kind === "group") apps.items.pop();
    });
    const h = await shown(sample, () => Promise.resolve(without));
    click(buttonLabelled(h.root, "Remove: Notes"));
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_remove_item", { item: 12 }]]);
    expect(h.doc.activeElement).toBe(buttonLabelled(h.root, "Add shortcut to Apps"));
  });

  it("moves a shortcut up, keeps its row, and puts focus on the button that still works", async () => {
    const moved = changed(sample, (view) => {
      const items = menuOf(view, EVERYWHERE).items;
      items.unshift(...items.splice(1, 1));
    });
    const h = await shown(sample, () => Promise.resolve(moved));
    const sign = itemBox(h.root, 4);
    const list = oneByClass(h.root, "sc-items");
    const removed = vi.spyOn(list, "removeChild");
    click(buttonLabelled(h.root, "Move up: Sign"));
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_move_item", { item: 4, to: 0 }]]);
    expect(ringNames(h.root)).toEqual(["Sign", "Copy", "Apps"]);
    expect(itemBox(h.root, 4)).toBe(sign);
    // The row with focus stays in the page: the others move around it.
    expect(removed.mock.calls.map(([node]) => node)).not.toContain(sign);
    // It's first now, so Move up is off: focus is on its Move down.
    expect(buttonLabelled(h.root, "Move up: Sign").disabled).toBe(true);
    expect(h.doc.activeElement).toBe(buttonLabelled(h.root, "Move down: Sign"));
  });

  it("moves a shortcut down, and focus stays on Move down while it can go further", async () => {
    const moved = changed(sample, (view) => {
      const items = menuOf(view, EVERYWHERE).items;
      items.splice(1, 0, ...items.splice(0, 1));
    });
    const h = await shown(sample, () => Promise.resolve(moved));
    const copy = itemBox(h.root, 2);
    const removed = vi.spyOn(oneByClass(h.root, "sc-items"), "removeChild");
    const down = buttonLabelled(h.root, "Move down: Copy");
    click(down);
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_move_item", { item: 2, to: 1 }]]);
    expect(ringNames(h.root)).toEqual(["Sign", "Copy", "Apps"]);
    expect(removed.mock.calls.map(([node]) => node)).not.toContain(copy);
    expect(h.doc.activeElement).toBe(down);
    expect(down.disabled).toBe(false);
  });

  it("moves a shortcut inside its group, by its place in the group", async () => {
    const h = await shown(sample);
    click(buttonLabelled(h.root, "Move up: Notes"));
    await h.page.idle();
    click(buttonLabelled(h.root, "Move down: Browser"));
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["shortcuts_move_item", { item: 12, to: 0 }],
      ["shortcuts_move_item", { item: 9, to: 1 }],
    ]);
  });

  it("holds a list's Move buttons until its move has answered: the next click counts from the new order", async () => {
    const order = (ids: readonly number[]): ShortcutsView =>
      changed(sample, (view) => {
        const menu = menuOf(view, EVERYWHERE);
        menu.items = ids.map((id) => itemOf(sample, id));
      });
    const first = deferred();
    const replies = [first.promise, Promise.resolve(order([4, 8, 2]))];
    const h = await shown(sample, () => replies.shift() ?? Promise.reject("unexpected"));
    const down = buttonLabelled(h.root, "Move down: Copy");
    click(down);
    // Key repeat, or a second click before the first answer: from the old order it would be place 1 again.
    click(down);
    press(down);
    click(buttonLabelled(h.root, "Move up: Apps"));
    await flush();
    expect(commands(h)).toEqual([["shortcuts_move_item", { item: 2, to: 1 }]]);
    // Every Move button of the ring that was on waits. None is disabled for it: the pressed one keeps the focus.
    const ringMoves = ["Copy", "Sign", "Apps"].flatMap((name) => [
      buttonLabelled(h.root, `Move up: ${name}`),
      buttonLabelled(h.root, `Move down: ${name}`),
    ]);
    expect(ringMoves.map((node) => isWaiting(node) || node.disabled)).toEqual(Array(6).fill(true));
    expect(ringMoves.filter(isWaiting)).toHaveLength(4);
    expect(down.disabled).toBe(false);
    expect(isWaiting(down)).toBe(true);
    // Another list is its own: the group's shortcuts and a shortcut's steps can still move.
    expect(isWaiting(buttonLabelled(h.root, "Move up: Notes"))).toBe(false);
    expect(isWaiting(buttonLabelled(h.root, "Move down: step 1 of Sign"))).toBe(false);
    expect(isWaiting(buttonLabelled(h.root, "Remove: Copy"))).toBe(false);
    expectWellFormed(h.root);

    first.resolve(order([4, 2, 8]));
    await h.page.idle();
    expect(ringNames(h.root)).toEqual(["Sign", "Copy", "Apps"]);
    expect(byTag(h.root, "button").filter(isWaiting)).toEqual([]);
    expect(h.doc.activeElement).toBe(down);

    // The next click is counted from the order Rust answered with.
    click(down);
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["shortcuts_move_item", { item: 2, to: 1 }],
      ["shortcuts_move_item", { item: 2, to: 2 }],
    ]);
    expect(ringNames(h.root)).toEqual(["Sign", "Apps", "Copy"]);
  });

  it("frees the Move buttons again when the move is refused", async () => {
    const h = await shown(
      sample,
      answering({
        shortcuts_move_item: () => Promise.reject("That shortcut can't move there."),
        shortcuts_get: () => Promise.resolve(sample),
      }),
    );
    click(buttonLabelled(h.root, "Move down: Copy"));
    await h.page.idle();
    expect(byTag(h.root, "button").filter(isWaiting)).toEqual([]);
    expect(oneByClass(h.root, "sc-status").textContent).toBe("That shortcut can't move there.");
  });
});

describe("steps", () => {
  it("adds a step of the chosen kind, resets the select and moves focus to the new step", async () => {
    const more = changed(sample, (view) => {
      stepsOf(view, 2).steps.push({ id: 60, kind: "openUrl", url: "" });
    });
    const h = await shown(sample, () => Promise.resolve(more));
    const add = byId(h.root, "sc-item-2-add-step");
    change(add, { value: "openUrl" });
    expect(add.value).toBe("");
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_add_step", { item: 2, kind: "openUrl" }]]);
    expect(stepBoxes(h.root, 2)).toHaveLength(2);
    expect(h.doc.activeElement).toBe(byId(h.root, "sc-step-60-url"));
    expect(summaryOf(h.root, 2)).toBe("Copy, then Open a link");
  });

  it("sends each of the fifteen kinds from Add a step, and nothing for the prompt", async () => {
    const h = await shown(sample);
    const add = byId(h.root, "sc-item-12-add-step");
    for (const kind of STEP_KINDS) {
      change(add, { value: kind });
    }
    change(add, { value: "" });
    change(add, { value: "launch" });
    await h.page.idle();
    expect(commands(h)).toEqual(STEP_KINDS.map((kind) => ["shortcuts_add_step", { item: 12, kind }]));
  });

  it("moves focus to the key select of a new Keys step, and to Choose… of a new Open app step", async () => {
    const withKeys = changed(sample, (view) => {
      stepsOf(view, 12).steps.push({ id: 61, kind: "keys", chord: null, label: null });
    });
    const h = await shown(sample, () => Promise.resolve(withKeys));
    change(byId(h.root, "sc-item-12-add-step"), { value: "keys" });
    await h.page.idle();
    expect(h.doc.activeElement).toBe(byId(h.root, "sc-step-61-key"));

    const withApp = changed(sample, (view) => {
      stepsOf(view, 12).steps.push({ id: 62, kind: "openApp", name: null });
    });
    const g = await shown(sample, () => Promise.resolve(withApp));
    change(byId(g.root, "sc-item-12-add-step"), { value: "openApp" });
    await g.page.idle();
    expect(g.doc.activeElement?.textContent).toBe("Choose…");
    expect(closest(g.doc.activeElement ?? g.root, "sc-item")).toBe(itemBox(g.root, 12));
  });

  it("sends text steps when their field is left, as the fixtures write them", async () => {
    const h = await shown(sample);
    change(byId(h.root, "sc-step-5-text"), { value: "Sent {date} at {time}\nMax" });
    change(byId(h.root, "sc-step-11-url"), { value: "https://example.com/inbox" });
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["shortcuts_set_step", { step: 5, value: fixtureJson("shortcuts-step-type-text.json") }],
      ["shortcuts_set_step", { step: 11, value: fixtureJson("shortcuts-step-open-url.json") }],
    ]);

    const s = await shownSafari();
    change(byId(s.root, "sc-step-25-name"), { value: "Start focus!" });
    change(byId(s.root, "sc-step-26-source"), { value: 'tell application "Finder" to activate' });
    change(byId(s.root, "sc-step-27-source"), { value: "echo hello\n" });
    change(byId(s.root, "sc-step-28-name"), { value: "Tidy" });
    change(byId(s.root, "sc-step-29-text"), { value: "Break" });
    change(byId(s.root, "sc-step-30-text"), { value: "Up" });
    await s.page.idle();
    expect(commands(s)).toEqual([
      ["shortcuts_set_step", { step: 25, value: { kind: "appleShortcut", name: "Start focus!" } }],
      ["shortcuts_set_step", { step: 26, value: fixtureJson("shortcuts-step-apple-script.json") }],
      ["shortcuts_set_step", { step: 27, value: { kind: "shell", source: "echo hello\n" } }],
      ["shortcuts_set_step", { step: 28, value: { kind: "keyboardMaestro", name: "Tidy" } }],
      ["shortcuts_set_step", { step: 29, value: { kind: "notify", text: "Break" } }],
      ["shortcuts_set_step", { step: 30, value: { kind: "speak", text: "Up" } }],
    ]);
  });

  it("sends a step's choice as soon as it's made", async () => {
    const h = await shown(sample);
    change(byId(h.root, "sc-step-3-action"), { value: "paste" });
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["shortcuts_set_step", { step: 3, value: fixtureJson("shortcuts-step-clipboard.json") }],
    ]);
    expect(JSON.stringify(commands(h)[0]?.[1])).toBe('{"step":3,"value":{"kind":"clipboard","action":"paste"}}');

    const moved = changed(sample, (view) => {
      const step = stepsOf(view, 21).steps[1];
      if (step?.kind === "window") step.placement = "topLeft";
    });
    const s = await shownSafari(() => Promise.resolve(moved));
    change(byId(s.root, "sc-step-23-placement"), { value: "topLeft" });
    await s.page.idle();
    change(byId(s.root, "sc-step-24-control"), { value: "lockScreen" });
    await s.page.idle();
    expect(commands(s)).toEqual([
      ["shortcuts_set_step", { step: 23, value: { kind: "window", placement: "topLeft" } }],
      ["shortcuts_set_step", { step: 24, value: { kind: "system", control: "lockScreen" } }],
    ]);
    expect(byId(s.root, "sc-step-23-placement").value).toBe("topLeft");

    change(byId(s.root, "sc-step-24-control"), { value: "shutDown" });
    await s.page.idle();
    expect(commands(s)).toHaveLength(2);
    expect(byId(s.root, "sc-step-24-control").value).toBe("toggleMute");
  });

  it("sends a Keys step's chord, and no chord for None", async () => {
    const h = await shown(sample);
    change(byId(h.root, "sc-step-6-shift"), { checked: false });
    await h.page.idle();
    change(byId(h.root, "sc-step-6-key"), { value: "" });
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["shortcuts_set_step", { step: 6, value: { kind: "keys", chord: { ...CHORD, shift: false } } }],
      ["shortcuts_set_step", { step: 6, value: fixtureJson("shortcuts-step-keys-unset.json") }],
    ]);
  });

  it("builds an unset Keys step's chord from held modifiers and the key", async () => {
    const h = await shownSafari();
    change(byId(h.root, "sc-step-22-meta"), { checked: true });
    change(byId(h.root, "sc-step-22-shift"), { checked: true });
    await h.page.idle();
    expect(commands(h)).toEqual([]);
    change(byId(h.root, "sc-step-22-key"), { value: "KeyK" });
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_set_step", { step: 22, value: fixtureJson("shortcuts-step-keys.json") }]]);
    expect(JSON.stringify(commands(h)[0]?.[1])).toBe(
      '{"step":22,"value":{"kind":"keys","chord":{"key":"KeyK","shift":true,"ctrl":false,"alt":false,"meta":true}}}',
    );
  });

  it("sends a delay as a number, and puts back what isn't one", async () => {
    const h = await shown(sample);
    const delay = byId(h.root, "sc-step-7-ms");
    change(delay, { value: "abc" });
    change(delay, { value: "2.5" });
    change(delay, { value: "" });
    await h.page.idle();
    expect(commands(h)).toEqual([]);
    expect(delay.value).toBe("250");

    change(delay, { value: "5" });
    await h.page.idle();
    // Too short is Rust's to say.
    expect(commands(h)).toEqual([["shortcuts_set_step", { step: 7, value: { kind: "delay", ms: 5 } }]]);
  });

  it("opens Rust's picker for an app, a file or a folder, and shows the name it picked", async () => {
    const picked = changed(sample, (view) => {
      const step = stepsOf(view, 9).steps[0];
      if (step?.kind === "openApp") step.name = "Safari";
    });
    const h = await shown(sample, () => Promise.resolve(picked));
    const choose = buttonLabelled(h.root, "Choose the app to open");
    click(choose);
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_pick_target", { step: 10 }]]);
    expect(oneByClass(stepBoxes(h.root, 9)[0] ?? h.root, "sc-picked").textContent).toBe("Safari");
    expect(summaryOf(h.root, 9)).toBe("Open Safari, then Open a link");
    expect(h.doc.activeElement).toBe(choose);

    const s = await shownSafari();
    click(buttonLabelled(s.root, "Choose the file or folder to open"));
    await s.page.idle();
    expect(commands(s)).toEqual([["shortcuts_pick_target", { step: 31 }]]);
  });

  it("opens one picker at a time, whether for a step or for a new menu", async () => {
    const reply = deferred();
    const h = await shown(sample, () => reply.promise);
    const choose = buttonLabelled(h.root, "Choose the app to open");
    click(choose);
    click(choose);
    click(buttonLabelled(h.root, "Add an app and give it a menu"));
    await flush();
    expect(commands(h)).toEqual([["shortcuts_pick_target", { step: 10 }]]);
    reply.resolve(sample);
    await h.page.idle();

    click(buttonLabelled(h.root, "Add an app and give it a menu"));
    await flush();
    expect(commands(h)).toHaveLength(2);
    expect(commands(h)[1]).toEqual(["shortcuts_add_menu", {}]);
  });

  it("removes a step and moves focus to Add a step", async () => {
    const without = changed(sample, (view) => {
      stepsOf(view, 4).steps.splice(1, 1);
    });
    const h = await shown(sample, () => Promise.resolve(without));
    const last = stepBoxes(h.root, 4)[2];
    click(buttonLabelled(h.root, "Remove: step 2 of Sign"));
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_remove_step", { step: 6 }]]);
    expect(stepBoxes(h.root, 4)).toHaveLength(2);
    expect(stepBoxes(h.root, 4)[1]).toBe(last);
    expect(oneByClass(last ?? h.root, "sc-step-title").textContent).toBe("2. Wait");
    expect(h.doc.activeElement).toBe(byId(h.root, "sc-item-4-add-step"));
  });

  it("moves a step inside its shortcut, keeps its row and keeps focus on it", async () => {
    const moved = changed(sample, (view) => {
      const steps = stepsOf(view, 4).steps;
      steps.push(...steps.splice(1, 1));
    });
    const h = await shown(sample, () => Promise.resolve(moved));
    const keys = stepBoxes(h.root, 4)[1];
    click(buttonLabelled(h.root, "Move down: step 2 of Sign"));
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_move_step", { step: 6, to: 2 }]]);
    const after = stepBoxes(h.root, 4);
    expect(after[2]).toBe(keys);
    expect(after.map((box) => oneByClass(box, "sc-step-title").textContent)).toEqual([
      "1. Type text",
      "2. Wait",
      "3. Press keys",
    ]);
    // Last now, so Move down is off: focus is on its Move up.
    expect(h.doc.activeElement).toBe(buttonLabelled(h.root, "Move up: step 3 of Sign"));
    expect(closest(h.doc.activeElement ?? h.root, "sc-step")).toBe(keys);

    click(buttonLabelled(h.root, "Move up: step 2 of Sign"));
    await h.page.idle();
    expect(commands(h)[1]).toEqual(["shortcuts_move_step", { step: 7, to: 0 }]);
  });

  it("holds a shortcut's step Move buttons until its move has answered", async () => {
    const moved = changed(sample, (view) => {
      const steps = stepsOf(view, 4).steps;
      steps.push(...steps.splice(0, 1));
    });
    const first = deferred();
    const replies = [first.promise, Promise.resolve(moved)];
    const h = await shown(sample, () => replies.shift() ?? Promise.reject("unexpected"));
    const down = buttonLabelled(h.root, "Move down: step 1 of Sign");
    click(down);
    click(down);
    await flush();
    expect(commands(h)).toEqual([["shortcuts_move_step", { step: 5, to: 1 }]]);
    // Four of the six: Move up of the first step and Move down of the last are off as it is.
    expect(stepBoxes(h.root, 4).flatMap((box) => byTag(box, "button").filter(isWaiting))).toHaveLength(4);
    // Another shortcut's steps, and this shortcut's own row, are free.
    expect(isWaiting(buttonLabelled(h.root, "Move down: step 1 of Browser"))).toBe(false);
    expect(isWaiting(buttonLabelled(h.root, "Move up: Sign"))).toBe(false);

    first.resolve(
      changed(sample, (view) => {
        const steps = stepsOf(view, 4).steps;
        steps.splice(1, 0, ...steps.splice(0, 1));
      }),
    );
    await h.page.idle();
    expect(byTag(h.root, "button").filter(isWaiting)).toEqual([]);
    click(buttonLabelled(h.root, "Move down: step 2 of Sign"));
    await h.page.idle();
    expect(commands(h)[1]).toEqual(["shortcuts_move_step", { step: 5, to: 2 }]);
  });

  it("ignores the second click of a double click on a step's Remove", async () => {
    const without = changed(sample, (view) => {
      stepsOf(view, 4).steps.splice(0, 1);
    });
    const h = await shown(sample, answering({ shortcuts_remove_step: () => Promise.resolve(without) }));
    click(buttonLabelled(h.root, "Remove: step 1 of Sign"));
    await h.page.idle();
    clickAgain(buttonLabelled(h.root, "Remove: step 1 of Sign"));
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_remove_step", { step: 5 }]]);
    expect(stepBoxes(h.root, 4)).toHaveLength(2);
  });

  it("builds a new row when Rust reuses an id for a step of another kind", async () => {
    const other = changed(sample, (view) => {
      stepsOf(view, 2).steps = [{ id: 3, kind: "notify", text: "Copied" }];
    });
    const h = await shown(sample, () => Promise.resolve(other));
    const before = stepBoxes(h.root, 2)[0];
    change(byId(h.root, "sc-enabled"), { checked: true });
    await h.page.idle();
    expect(stepBoxes(h.root, 2)).toHaveLength(1);
    expect(stepBoxes(h.root, 2)[0]).not.toBe(before);
    expect(byId(h.root, "sc-step-3-text").value).toBe("Copied");
    expectWellFormed(h.root);
  });
});

describe("text fields", () => {
  it("send on change, never while the user types", async () => {
    const h = await shown(sample);
    const text = byId(h.root, "sc-step-5-text");
    type(text, "B");
    type(text, "Be");
    text.dispatch("keydown", { key: "Enter" });
    await h.page.idle();
    // Enter in a text area is a line break, not a send.
    expect(commands(h)).toEqual([]);
    text.dispatch("change");
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_set_step", { step: 5, value: { kind: "typeText", text: "Be" } }]]);
  });

  it("send nothing when the field is left as it was", async () => {
    const h = await shown(sample);
    byId(h.root, "sc-item-2-name").dispatch("change");
    change(byId(h.root, "sc-step-5-text"), { value: "Best,\nMax" });
    await h.page.idle();
    expect(commands(h)).toEqual([]);
  });

  it("send a one-line field on Enter, once", async () => {
    const reply = deferred();
    const h = await shown(sample, () => reply.promise);
    const name = byId(h.root, "sc-item-2-name");
    type(name, "Duplicate");
    name.dispatch("keydown", { key: "a" });
    await flush();
    expect(commands(h)).toEqual([]);
    name.dispatch("keydown", { key: "Enter" });
    // A webview that also fires `change` on Enter, and then on blur, sends nothing more.
    name.dispatch("change");
    name.dispatch("change");
    reply.resolve(
      changed(sample, (view) => {
        itemOf(view, 2).name = "Duplicate";
      }),
    );
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["shortcuts_set_item", { item: 2, name: "Duplicate", icon: { kind: "glyph", value: "clipboard" } }],
    ]);
    expect(name.value).toBe("Duplicate");
  });

  it("keep what the user is typing through a redraw from another command", async () => {
    const h = await shown(
      sample,
      answering({ shortcuts_set_enabled: () => Promise.resolve({ ...sample, enabled: false }) }),
    );
    const name = byId(h.root, "sc-item-4-name");
    const script = byId(h.root, "sc-step-5-text");
    type(name, "Signat");
    type(script, "Kind regards");
    change(byId(h.root, "sc-enabled"), { checked: false });
    await h.page.idle();
    expect(byId(h.root, "sc-enabled").checked).toBe(false);
    expect(name.value).toBe("Signat");
    expect(script.value).toBe("Kind regards");
    // The fields the user didn't touch follow the view.
    expect(byId(h.root, "sc-item-2-name").value).toBe("Copy");
    expect(commands(h)).toEqual([["shortcuts_set_enabled", { on: false }]]);
  });

  it("keep what the user typed after sending, until that newer text has answered too", async () => {
    const first = deferred();
    const second = deferred();
    const replies = [first, second];
    const h = await shown(sample, () => replies.shift()?.promise ?? Promise.resolve(sample));
    const name = byId(h.root, "sc-item-4-name");
    change(name, { value: "Sig" });
    type(name, "Signature");
    first.resolve(
      changed(sample, (view) => {
        itemOf(view, 4).name = "Sig";
      }),
    );
    await h.page.idle();
    // The answer to "Sig" must not wipe out "Signature".
    expect(name.value).toBe("Signature");
    expect(itemBox(h.root, 4).getAttribute("aria-label")).toBe("Shortcut: Sig");

    name.dispatch("change");
    second.resolve(
      changed(sample, (view) => {
        itemOf(view, 4).name = "Signature";
      }),
    );
    await h.page.idle();
    expect(commands(h).map((call) => (call[1] as { name: string }).name)).toEqual(["Sig", "Signature"]);
    expect(name.value).toBe("Signature");
  });

  it("show Rust's value once the field's own command has answered", async () => {
    const tidy = changed(sample, (view) => {
      const step = stepsOf(view, 4).steps[0];
      if (step?.kind === "typeText") step.text = "Kind regards";
    });
    const h = await shown(sample, () => Promise.resolve(tidy));
    const text = byId(h.root, "sc-step-5-text");
    change(text, { value: "Kind regards\r\n" });
    await h.page.idle();
    expect(text.value).toBe("Kind regards");
  });

  const LINK_REFUSED = "A link starts with its kind, like https: or mailto:, and has no spaces.";

  it("keep the text Rust refused, marked as not saved under Rust's sentence", async () => {
    const h = await shown(
      sample,
      answering({
        shortcuts_set_step: () => Promise.reject(LINK_REFUSED),
        shortcuts_set_enabled: () => Promise.resolve(sample),
        shortcuts_get: () => Promise.resolve(sample),
      }),
    );
    const url = byId(h.root, "sc-step-11-url");
    const line = byId(h.root, "sc-step-11-url-unsaved");
    change(url, { value: "example.com/some/long/path?with=query" });
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["shortcuts_set_step", { step: 11, value: { kind: "openUrl", url: "example.com/some/long/path?with=query" } }],
      ["shortcuts_get", {}],
    ]);
    // The typing isn't thrown away: it is there to mend.
    expect(url.value).toBe("example.com/some/long/path?with=query");
    expect(url.getAttribute("aria-invalid")).toBe("true");
    expect(url.getAttribute("aria-describedby")?.split(" ")).toContain("sc-step-11-url-unsaved");
    expect(line.textContent).toBe(`Not saved. ${LINK_REFUSED}`);
    expect(oneByClass(h.root, "sc-status").textContent).toBe(LINK_REFUSED);
    expectWellFormed(h.root);

    // It outlives the status line, a redraw and a re-read.
    change(byId(h.root, "sc-enabled"), { checked: true });
    await h.page.idle();
    h.win.dispatch("focus");
    await h.page.idle();
    expect(oneByClass(h.root, "sc-status").textContent).toBe("");
    expect(url.value).toBe("example.com/some/long/path?with=query");
    expect(url.getAttribute("aria-invalid")).toBe("true");
    expect(line.textContent).toBe(`Not saved. ${LINK_REFUSED}`);
  });

  it("send refused text again only once the user has changed it", async () => {
    const mended = changed(sample, (view) => {
      const step = stepsOf(view, 9).steps[1];
      if (step?.kind === "openUrl") step.url = "https://example.com/inbox";
    });
    const h = await shown(sample, (command, args) => {
      if (command === "shortcuts_get") return Promise.resolve(sample);
      const value = args["value"] as { url: string };
      return value.url.startsWith("https:") ? Promise.resolve(mended) : Promise.reject(LINK_REFUSED);
    });
    const url = byId(h.root, "sc-step-11-url");
    change(url, { value: "example.com/inbox" });
    await h.page.idle();
    expect(commands(h)).toHaveLength(2);

    // Leaving it again, Enter, and closing or hiding Settings don't repeat the command or its error.
    url.dispatch("change");
    url.dispatch("keydown", { key: "Enter" });
    h.win.dispatch("pagehide");
    h.doc.visibilityState = "hidden";
    h.doc.dispatch("visibilitychange");
    await h.page.idle();
    expect(commands(h)).toHaveLength(2);
    expect(url.value).toBe("example.com/inbox");

    // While it is being mended the line stays, so the sentence can be read.
    type(url, "https://example.com/inbox");
    expect(url.getAttribute("aria-invalid")).toBe("true");
    url.dispatch("change");
    await h.page.idle();
    expect(commands(h)[2]).toEqual([
      "shortcuts_set_step",
      { step: 11, value: { kind: "openUrl", url: "https://example.com/inbox" } },
    ]);
    expect(commands(h)).toHaveLength(3);
    expect(url.value).toBe("https://example.com/inbox");
    expect(url.hasAttribute("aria-invalid")).toBe(false);
    expect(byId(h.root, "sc-step-11-url-unsaved").textContent).toBe("");
  });

  it("drop the mark, and send nothing, when refused text is changed back to what Rust holds", async () => {
    const h = await shown(
      sample,
      answering({
        shortcuts_set_step: () => Promise.reject("Grape can't keep some of those characters."),
        shortcuts_get: () => Promise.resolve(sample),
      }),
    );
    const text = byId(h.root, "sc-step-5-text");
    change(text, { value: "Best,\u000bMax" });
    await h.page.idle();
    expect(text.value).toBe("Best,\u000bMax");
    expect(text.getAttribute("aria-invalid")).toBe("true");
    change(text, { value: "Best,\nMax" });
    await h.page.idle();
    expect(commands(h)).toHaveLength(2);
    expect(text.hasAttribute("aria-invalid")).toBe(false);
    expect(byId(h.root, "sc-step-5-text-unsaved").textContent).toBe("");
  });

  it("keep a refused number too, and a text whose answer couldn't be read", async () => {
    const h = await shown(
      sample,
      answering({
        shortcuts_set_step: (_command, args) =>
          (args["value"] as { kind: string }).kind === "delay"
            ? Promise.reject("A wait is 10 to 60000 milliseconds.")
            : Promise.resolve({ not: "a view" }),
        shortcuts_get: () => Promise.resolve(sample),
      }),
    );
    vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const delay = byId(h.root, "sc-step-7-ms");
    change(delay, { value: "5" });
    await h.page.idle();
    expect(delay.value).toBe("5");
    expect(delay.getAttribute("aria-invalid")).toBe("true");
    expect(byId(h.root, "sc-step-7-ms-unsaved").textContent).toBe("Not saved. A wait is 10 to 60000 milliseconds.");

    const text = byId(h.root, "sc-step-5-text");
    change(text, { value: "Kind regards" });
    await h.page.idle();
    expect(text.value).toBe("Kind regards");
    expect(byId(h.root, "sc-step-5-text-unsaved").textContent).toBe(`Not saved. ${SHORTCUTS_ERROR}`);
  });

  it("show Rust's value, unmarked, when only the save failed: the change is in", async () => {
    const tidy = changed(sample, (view) => {
      const step = stepsOf(view, 4).steps[0];
      if (step?.kind === "typeText") step.text = "Kind regards";
    });
    const h = await shown(
      sample,
      answering({
        shortcuts_set_step: () => Promise.reject("Couldn't save your shortcuts."),
        shortcuts_get: () => Promise.resolve(tidy),
      }),
    );
    const text = byId(h.root, "sc-step-5-text");
    change(text, { value: "Kind regards\r\n" });
    await h.page.idle();
    expect(text.value).toBe("Kind regards");
    expect(text.hasAttribute("aria-invalid")).toBe(false);
    expect(byId(h.root, "sc-step-5-text-unsaved").textContent).toBe("");
    expect(oneByClass(h.root, "sc-status").textContent).toBe("Couldn't save your shortcuts.");
    // Rust holds it, so leaving the field again sends nothing.
    text.dispatch("change");
    await h.page.idle();
    expect(commands(h)).toHaveLength(2);
  });

  it("put a refused name back, with Rust's sentence: it travels with every icon change", async () => {
    const h = await shown(
      sample,
      answering({
        shortcuts_set_item: (_command, args) =>
          args["name"] === "" ? Promise.reject("A name needs 1 to 24 characters.") : Promise.resolve(sample),
        shortcuts_get: () => Promise.resolve(sample),
      }),
    );
    const name = byId(h.root, "sc-item-2-name");
    change(name, { value: "" });
    await h.page.idle();
    expect(name.value).toBe("Copy");
    expect(name.hasAttribute("aria-invalid")).toBe(false);
    expect(oneByClass(h.root, "sc-status").textContent).toBe("A name needs 1 to 24 characters.");
    // So the next icon change isn't refused for the name's sake.
    change(byId(h.root, "sc-item-2-icon"), { value: "glyph:star" });
    await h.page.idle();
    expect(commands(h).at(-1)).toEqual([
      "shortcuts_set_item",
      { item: 2, name: "Copy", icon: { kind: "glyph", value: "star" } },
    ]);
    expect(oneByClass(h.root, "sc-status").textContent).toBe("");
  });

  it("send an edit back to the old value when Rust has moved on under typed text", async () => {
    const named = (name: string): ShortcutsView =>
      changed(sample, (view) => {
        itemOf(view, 2).name = name;
      });
    const first = deferred();
    const replies = [first.promise, Promise.resolve(named("Copy"))];
    const h = await shown(sample, () => replies.shift() ?? Promise.reject("unexpected"));
    const name = byId(h.root, "sc-item-2-name");
    type(name, "B");
    name.dispatch("keydown", { key: "Enter" });
    // One more letter before the answer: the redraw leaves it alone, but Rust holds "B" now.
    type(name, "Bx");
    first.resolve(named("B"));
    await h.page.idle();
    expect(name.value).toBe("Bx");
    expect(itemBox(h.root, 2).getAttribute("aria-label")).toBe("Shortcut: B");

    change(name, { value: "Copy" });
    await h.page.idle();
    expect(commands(h).map((call) => (call[1] as { name: string }).name)).toEqual(["B", "Copy"]);
    expect(name.value).toBe("Copy");
    expect(itemBox(h.root, 2).getAttribute("aria-label")).toBe("Shortcut: Copy");
  });

  it("send an edit back to the old value while the newer one is still on its way", async () => {
    const named = (name: string): ShortcutsView =>
      changed(sample, (view) => {
        itemOf(view, 2).name = name;
      });
    const first = deferred();
    const replies = [first.promise, Promise.resolve(named("Copy"))];
    const h = await shown(sample, () => replies.shift() ?? Promise.reject("unexpected"));
    const name = byId(h.root, "sc-item-2-name");
    change(name, { value: "B" });
    // Rust will hold "B" once it lands: "Copy" is a change again, and is sent once.
    change(name, { value: "Copy" });
    name.dispatch("change");
    first.resolve(named("B"));
    await h.page.idle();
    expect(commands(h).map((call) => (call[1] as { name: string }).name)).toEqual(["B", "Copy"]);
    expect(name.value).toBe("Copy");
  });

  it("send nothing for typed text that equals what Rust came to hold", async () => {
    const h = await shown(
      sample,
      answering({
        shortcuts_set_enabled: () =>
          Promise.resolve(
            changed(sample, (view) => {
              itemOf(view, 2).name = "Bx";
            }),
          ),
      }),
    );
    const name = byId(h.root, "sc-item-2-name");
    type(name, "B");
    change(byId(h.root, "sc-enabled"), { checked: false });
    await h.page.idle();
    expect(name.value).toBe("B");
    type(name, "Bx");
    name.dispatch("change");
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_set_enabled", { on: false }]]);
  });

  it("are all sent when the window closes, each once", async () => {
    const h = await shown(sample);
    type(byId(h.root, "sc-step-11-url"), "https://example.com/inbox");
    type(byId(h.root, "sc-item-2-name"), "Duplicate");
    type(byId(h.root, "sc-step-5-text"), "Kind regards");
    type(byId(h.root, "sc-step-7-ms"), "500");
    h.win.dispatch("pagehide");
    // A second event finds them on their way already.
    h.win.dispatch("pagehide");
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["shortcuts_set_item", { item: 2, name: "Duplicate", icon: { kind: "glyph", value: "clipboard" } }],
      ["shortcuts_set_step", { step: 5, value: { kind: "typeText", text: "Kind regards" } }],
      ["shortcuts_set_step", { step: 7, value: { kind: "delay", ms: 500 } }],
      ["shortcuts_set_step", { step: 11, value: { kind: "openUrl", url: "https://example.com/inbox" } }],
    ]);
  });

  it("go to Rust at once when the window closes, not behind a command that hasn't answered", async () => {
    const first = deferred();
    const h = await shown(sample, (command) =>
      command === "shortcuts_set_enabled" ? first.promise : Promise.resolve(sample),
    );
    change(byId(h.root, "sc-enabled"), { checked: false });
    change(byId(h.root, "sc-opens-at"), { value: "center" });
    await flush();
    // One at a time: the second waits for the first's answer.
    expect(commands(h)).toEqual([["shortcuts_set_enabled", { on: false }]]);
    type(byId(h.root, "sc-item-2-name"), "Duplicate");
    type(byId(h.root, "sc-step-5-text"), "Kind regards");

    h.win.dispatch("pagehide");
    // No answer has come, and none may: everything is with Rust before the handler returns, in order.
    expect(commands(h)).toEqual([
      ["shortcuts_set_enabled", { on: false }],
      ["shortcuts_set_opens_at", { at: "center" }],
      ["shortcuts_set_item", { item: 2, name: "Duplicate", icon: { kind: "glyph", value: "clipboard" } }],
      ["shortcuts_set_step", { step: 5, value: { kind: "typeText", text: "Kind regards" } }],
    ]);

    // If the page lives on (it was only hidden), each answer is read in turn and nothing is sent twice.
    first.resolve(sample);
    await h.page.idle();
    expect(commands(h)).toHaveLength(4);
    expect(h.root.hasAttribute("aria-busy")).toBe(false);
  });

  it("go to Rust inside the handler when the page is hidden, and a refusal among them is still read", async () => {
    const h = await shown(
      sample,
      answering({
        shortcuts_set_item: () => Promise.reject("A name needs 1 to 24 characters."),
        shortcuts_set_step: () => Promise.resolve(sample),
        shortcuts_get: () => Promise.resolve(sample),
      }),
    );
    type(byId(h.root, "sc-item-2-name"), "Duplicate");
    type(byId(h.root, "sc-step-11-url"), "https://example.com");
    type(byId(h.root, "sc-step-7-ms"), "500");
    h.doc.visibilityState = "hidden";
    h.doc.dispatch("visibilitychange");
    expect(commands(h).map(([command]) => command)).toEqual(["shortcuts_set_item", "shortcuts_set_step"]);
    await h.page.idle();
    expect(commands(h).map(([command]) => command)).toEqual([
      "shortcuts_set_item",
      "shortcuts_set_step",
      "shortcuts_get",
    ]);
    expect(oneByClass(h.root, "sc-status").textContent).toBe("A name needs 1 to 24 characters.");
  });

  it("are sent when the page is hidden, without a re-read", async () => {
    const h = await shown(sample);
    type(byId(h.root, "sc-item-4-name"), "Signature");
    h.doc.visibilityState = "hidden";
    h.doc.dispatch("visibilitychange");
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["shortcuts_set_item", { item: 4, name: "Signature", icon: { kind: "emoji", value: "✍️" } }],
    ]);
  });

  it("are left alone, unsent, when the window only loses and regains the focus", async () => {
    const h = await shown(sample);
    const name = byId(h.root, "sc-item-4-name");
    type(name, "Signature");
    h.win.dispatch("focus");
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_get", {}]]);
    expect(name.value).toBe("Signature");
  });

  it("of a removed row are forgotten", async () => {
    const without = changed(sample, (view) => {
      menuOf(view, EVERYWHERE).items.splice(1, 1);
    });
    const h = await shown(sample, answering({ shortcuts_remove_item: () => Promise.resolve(without) }));
    const name = byId(h.root, "sc-item-4-name");
    const text = byId(h.root, "sc-step-5-text");
    click(buttonLabelled(h.root, "Remove: Sign"));
    await h.page.idle();
    name.value = "Gone";
    text.value = "Gone";
    h.win.dispatch("pagehide");
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_remove_item", { item: 4 }]]);
  });

  it("take their length limits from the view", async () => {
    const tight = { ...sample, limits: { ...sample.limits, name: 10, icon: 4, text: 100, script: 200, url: 50 } };
    const { root } = await shown(tight);
    expect(byId(root, "sc-item-2-name").getAttribute("maxlength")).toBe("10");
    expect(byId(root, "sc-item-4-emoji").getAttribute("maxlength")).toBe("4");
    expect(byId(root, "sc-step-5-text").getAttribute("maxlength")).toBe("100");
    expect(byId(root, "sc-step-11-url").getAttribute("maxlength")).toBe("50");
    change(byId(root, "sc-menu"), { value: String(SAFARI) });
    expect(byId(root, "sc-step-26-source").getAttribute("maxlength")).toBe("200");
    expect(byId(root, "sc-step-27-source").getAttribute("maxlength")).toBe("200");
    expect(byId(root, "sc-step-28-name").getAttribute("maxlength")).toBe("100");
  });
});

describe("rows and focus", () => {
  it("keeps every row and control across a redraw, so focus stays", async () => {
    const h = await shown(sample, () => Promise.resolve({ ...sample, opensAt: "center" }));
    const boxes = byClass(h.root, "sc-item");
    const controls = [...byTag(h.root, "input"), ...byTag(h.root, "select"), ...byTag(h.root, "textarea")];
    const text = byId(h.root, "sc-step-5-text");
    text.focus();
    const list = oneByClass(h.root, "sc-items");
    const removed = vi.spyOn(list, "removeChild");
    const inserted = vi.spyOn(list, "insertBefore");
    const appended = vi.spyOn(list, "append");
    change(byId(h.root, "sc-opens-at"), { value: "center" });
    await h.page.idle();
    expect(byClass(h.root, "sc-item")).toEqual(boxes);
    expect([...byTag(h.root, "input"), ...byTag(h.root, "select"), ...byTag(h.root, "textarea")]).toEqual(controls);
    expect(h.doc.activeElement).toBe(text);
    // Nothing was taken out and put back: that would drop focus in a real page.
    expect(removed).not.toHaveBeenCalled();
    expect(inserted).not.toHaveBeenCalled();
    expect(appended).not.toHaveBeenCalled();
  });

  it("keeps the options of every select when the choices don't change", async () => {
    const h = await shown(sample, () => Promise.resolve({ ...sample, opensAt: "center" }));
    const firsts = byTag(h.root, "select").map((select) => byTag(select, "option")[0]);
    change(byId(h.root, "sc-opens-at"), { value: "center" });
    await h.page.idle();
    expect(byTag(h.root, "select").map((select) => byTag(select, "option")[0])).toEqual(firsts);
  });

  it("leaves a row with focus where it is while rows above and below it change", async () => {
    const shuffled = changed(sample, (view) => {
      const items = menuOf(view, EVERYWHERE).items;
      items.reverse();
      items.push({ id: 70, name: "Fresh", icon: { kind: "none" }, kind: "steps", steps: [] });
    });
    const h = await shown(sample, () => Promise.resolve(shuffled));
    const sign = itemBox(h.root, 4);
    const text = byId(h.root, "sc-step-5-text");
    text.focus();
    const removed = vi.spyOn(oneByClass(h.root, "sc-items"), "removeChild");
    h.win.dispatch("focus");
    await h.page.idle();
    expect(ringNames(h.root)).toEqual(["Apps", "Sign", "Copy", "Fresh"]);
    expect(removed.mock.calls.map(([node]) => node)).not.toContain(sign);
    expect(h.doc.activeElement).toBe(text);
  });

  it("moves focus off a row that Rust no longer lists", async () => {
    const without = changed(sample, (view) => {
      menuOf(view, EVERYWHERE).items.splice(1, 1);
    });
    const h = await shown(sample, () => Promise.resolve(without));
    byId(h.root, "sc-step-5-text").focus();
    h.win.dispatch("focus");
    await h.page.idle();
    expect(ringNames(h.root)).toEqual(["Copy", "Apps"]);
    expect(h.doc.activeElement).toBe(buttonLabelled(h.root, "Add shortcut"));
  });
});

describe("limits", () => {
  it("switch off Add shortcut and Add group when the ring is full, and say why", async () => {
    const full = { ...sample, limits: { ...sample.limits, items: 3 } };
    const { root } = await shown(full);
    const add = buttonLabelled(root, "Add shortcut");
    const group = buttonLabelled(root, "Add group");
    expect(add.disabled).toBe(true);
    expect(group.disabled).toBe(true);
    const reason = byId(root, add.getAttribute("aria-describedby") ?? "");
    expect(group.getAttribute("aria-describedby")).toBe(add.getAttribute("aria-describedby"));
    expect(reason.textContent).toBe("That's 3 shortcuts, the most one ring holds.");
    // The group has two of three, so it can still take one.
    expect(buttonLabelled(root, "Add shortcut to Apps").disabled).toBe(false);
    expect(byId(root, "sc-item-8-full").textContent).toBe("");
  });

  it("leave the add buttons on, with no reason, under the limit", async () => {
    const { root } = await shown(sample);
    expect(buttonLabelled(root, "Add shortcut").disabled).toBe(false);
    expect(buttonLabelled(root, "Add group").disabled).toBe(false);
    expect(byId(root, "sc-ring-full").textContent).toBe("");
    expect(byId(root, "sc-item-4-add-step").disabled).toBe(false);
    expect(byId(root, "sc-item-4-full").textContent).toBe("");
    expect(buttonLabelled(root, "Add an app and give it a menu").disabled).toBe(false);
    expect(byId(root, "sc-menus-full").textContent).toBe("");
  });

  it("switch off a full group's Add shortcut, and a full shortcut's Add a step", async () => {
    const { root } = await shown({ ...sample, limits: { ...sample.limits, items: 2, steps: 3 } });
    const add = buttonLabelled(root, "Add shortcut to Apps");
    expect(add.disabled).toBe(true);
    expect(byId(root, add.getAttribute("aria-describedby") ?? "").textContent).toBe(
      "That's 2 shortcuts, the most one ring holds.",
    );
    const step = byId(root, "sc-item-4-add-step");
    expect(step.disabled).toBe(true);
    expect(byId(root, step.getAttribute("aria-describedby") ?? "").textContent).toBe(
      "That's 3 steps, the most one shortcut holds.",
    );
    expect(byId(root, "sc-item-2-add-step").disabled).toBe(false);
  });

  it("switch off Add an app… when Grape keeps no more menus", async () => {
    const { root } = await shown({ ...sample, limits: { ...sample.limits, menus: 2 } });
    const add = buttonLabelled(root, "Add an app and give it a menu");
    expect(add.disabled).toBe(true);
    expect(byId(root, add.getAttribute("aria-describedby") ?? "").textContent).toBe(
      "That's 2 menus, the most Grape keeps.",
    );
  });

  it("switch the add buttons on again once there's room", async () => {
    const full = { ...sample, limits: { ...sample.limits, items: 3 } };
    const roomy = changed(full, (view) => {
      menuOf(view, EVERYWHERE).items.pop();
    });
    const h = await shown(full, () => Promise.resolve(roomy));
    click(buttonLabelled(h.root, "Remove: Apps"));
    await h.page.idle();
    expect(buttonLabelled(h.root, "Add shortcut").disabled).toBe(false);
    expect(byId(h.root, "sc-ring-full").textContent).toBe("");
    expect(h.doc.activeElement).toBe(buttonLabelled(h.root, "Add shortcut"));
  });
});

describe("reading the view again", () => {
  it("happens when the window gets the focus, which is how granted access shows", async () => {
    const h = await shown(macos, () => Promise.resolve({ ...macos, access: "granted" }));
    expect(isHidden(buttonLabelled(h.root, "Ask for access to press keys"))).toBe(false);
    h.win.dispatch("focus");
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_get", {}]]);
    expect(oneByClass(h.root, "sc-access-text").textContent).toBe(SHORTCUTS_COPY.accessGranted);
    expect(isHidden(buttonLabelled(h.root, "Ask for access to press keys"))).toBe(true);
  });

  it("happens when the page becomes visible, once even if both events come", async () => {
    const h = await shown(macos);
    h.doc.visibilityState = "visible";
    h.doc.dispatch("visibilitychange");
    h.win.dispatch("focus");
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_get", {}]]);
  });

  it("is skipped while a command is on its way: its answer brings the view", async () => {
    const reply = deferred();
    const h = await shown(macos, () => reply.promise);
    change(byId(h.root, "sc-enabled"), { checked: true });
    h.win.dispatch("focus");
    reply.resolve({ ...macos, enabled: true });
    await h.page.idle();
    expect(commands(h)).toEqual([["shortcuts_set_enabled", { on: true }]]);
  });

  it("is skipped where the wheel isn't available", async () => {
    const h = await shown(windows);
    h.win.dispatch("focus");
    h.doc.dispatch("visibilitychange");
    await h.page.idle();
    expect(commands(h)).toEqual([]);
  });

  it("needs a window: a document without one mounts and listens to nothing", async () => {
    const { root } = fakeRoot();
    const invoke = vi.fn<InvokeFn>(() => Promise.resolve(macos));
    const page = mountShortcuts(root as unknown as HTMLElement, { invoke });
    await page.idle();
    expect(byId(root, "sc-enabled").checked).toBe(false);
    expect(invoke).toHaveBeenCalledTimes(1);
  });

  it("keeps the note and the error on show", async () => {
    const noted = { ...sample, note: "Some shortcuts couldn't be read and were left out." };
    const h = await shown(
      noted,
      answering({
        shortcuts_set_opens_at: () => Promise.reject("Couldn't save your shortcuts."),
        shortcuts_get: () => Promise.resolve({ ...sample, opensAt: "center" }),
      }),
    );
    h.win.dispatch("focus");
    await h.page.idle();
    expect(oneByClass(h.root, "settings-note").textContent).toBe(noted.note);

    change(byId(h.root, "sc-opens-at"), { value: "center" });
    await h.page.idle();
    expect(oneByClass(h.root, "sc-status").textContent).toBe("Couldn't save your shortcuts.");
    // The re-read after a failure drops the note: it belonged to an earlier change.
    expect(oneByClass(h.root, "settings-note").textContent).toBe("");
    h.win.dispatch("focus");
    await h.page.idle();
    expect(oneByClass(h.root, "sc-status").textContent).toBe("Couldn't save your shortcuts.");
  });

  it("says nothing when it fails, and keeps the view", async () => {
    const logged = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const h = await shown(macos, () => Promise.reject("Couldn't read your shortcuts."));
    h.win.dispatch("focus");
    await h.page.idle();
    expect(oneByClass(h.root, "sc-status").textContent).toBe("");
    expect(byId(h.root, "sc-hotkey-key").value).toBe("KeyG");
    expect(logged).not.toHaveBeenCalled();
  });

  it("brings the first view after a load that failed, and clears that error", async () => {
    let fail = true;
    const h = harness(() => {
      if (fail) {
        fail = false;
        return Promise.reject("Couldn't read your shortcuts.");
      }
      return Promise.resolve(macos);
    });
    await h.page.idle();
    expect(oneByClass(h.root, "sc-status").textContent).toBe("Couldn't read your shortcuts.");
    h.win.dispatch("focus");
    await h.page.idle();
    expect(oneByClass(h.root, "sc-status").textContent).toBe("");
    expect(byId(h.root, "sc-hotkey-key").value).toBe("KeyG");
  });
});

describe("errors", () => {
  it("shows Rust's sentence and reads the view again, which a failed save has changed", async () => {
    const h = await shown(
      macos,
      answering({
        shortcuts_set_opens_at: () => Promise.reject("Couldn't save your shortcuts."),
        shortcuts_get: () => Promise.resolve({ ...macos, opensAt: "center" }),
      }),
    );
    change(byId(h.root, "sc-opens-at"), { value: "center" });
    await h.page.idle();
    expect(commands(h)).toEqual([
      ["shortcuts_set_opens_at", { at: "center" }],
      ["shortcuts_get", {}],
    ]);
    expect(byId(h.root, "sc-opens-at").value).toBe("center");
    expect(oneByClass(h.root, "sc-status").textContent).toBe("Couldn't save your shortcuts.");
  });

  it("keeps the last good view and the sentence when the re-read fails too", async () => {
    const h = await shown(macos, () => Promise.reject("Couldn't save your shortcuts."));
    change(byId(h.root, "sc-opens-at"), { value: "center" });
    await h.page.idle();
    expect(byId(h.root, "sc-opens-at").value).toBe("cursor");
    expect(oneByClass(h.root, "sc-status").textContent).toBe("Couldn't save your shortcuts.");
  });

  it("clears the sentence once the next change goes through", async () => {
    let fail = true;
    const h = await shown(
      macos,
      answering({
        shortcuts_set_opens_at: () => {
          if (fail) {
            fail = false;
            return Promise.reject("Couldn't save your shortcuts.");
          }
          return Promise.resolve({ ...macos, opensAt: "center" });
        },
        shortcuts_get: () => Promise.resolve(macos),
      }),
    );
    change(byId(h.root, "sc-opens-at"), { value: "center" });
    await h.page.idle();
    expect(oneByClass(h.root, "sc-status").textContent).toBe("Couldn't save your shortcuts.");
    change(byId(h.root, "sc-opens-at"), { value: "center" });
    await h.page.idle();
    expect(oneByClass(h.root, "sc-status").textContent).toBe("");
    expect(byId(h.root, "sc-opens-at").value).toBe("center");
  });

  it("shows its own sentence for a rejection without one, or a throw, and logs only its kind", async () => {
    const logged = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const h = await shown(
      sample,
      answering({
        shortcuts_set_step: () => Promise.reject(new TypeError("MARKER in the message")),
        shortcuts_get: () => Promise.resolve(sample),
      }),
    );
    change(byId(h.root, "sc-step-5-text"), { value: "MARKER typed" });
    await h.page.idle();
    expect(oneByClass(h.root, "sc-status").textContent).toBe(SHORTCUTS_ERROR);
    expect(logged).toHaveBeenCalledTimes(1);
    expect(logged.mock.calls[0]).toEqual(["Grape shortcuts: shortcuts_set_step failed (TypeError)"]);
    expect(JSON.stringify(logged.mock.calls)).not.toContain("MARKER");

    const throwing = await shown(macos, () => {
      throw new Error("no ipc");
    });
    change(byId(throwing.root, "sc-enabled"), { checked: true });
    await throwing.page.idle();
    expect(oneByClass(throwing.root, "sc-status").textContent).toBe(SHORTCUTS_ERROR);
    expect(byId(throwing.root, "sc-enabled").checked).toBe(false);
  });

  it("drops a reply that fails the guard, keeps the last good view, and never logs the reply", async () => {
    const warned = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const logged = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const info = vi.spyOn(console, "log").mockImplementation(() => undefined);
    const h = await shown(sample, () =>
      Promise.resolve({ ...sample, enabled: false, note: "MARKER note", secret: "MARKER script" }),
    );
    change(byId(h.root, "sc-enabled"), { checked: false });
    await h.page.idle();
    expect(oneByClass(h.root, "sc-status").textContent).toBe(SHORTCUTS_ERROR);
    expect(byId(h.root, "sc-enabled").checked).toBe(true);
    expect(oneByClass(h.root, "settings-note").textContent).toBe("");
    // One fixed line that names the command, and nothing of the reply.
    expect(warned.mock.calls).toEqual([
      ["Grape shortcuts: shortcuts_set_enabled answered with a view that doesn't match the contract"],
    ]);
    expect(logged).not.toHaveBeenCalled();
    expect(info).not.toHaveBeenCalled();
  });

  it("drops a first reply that fails the guard, and a re-read that does", async () => {
    const warned = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const first = harness(() => Promise.resolve({ ...macos, menus: [] }));
    await first.page.idle();
    expect(oneByClass(first.root, "sc-status").textContent).toBe(SHORTCUTS_ERROR);
    expect(byTag(first.root, "input")).toEqual([]);

    const h = await shown(
      macos,
      answering({
        shortcuts_set_opens_at: () => Promise.reject("Couldn't save your shortcuts."),
        shortcuts_get: () => Promise.resolve({ ...macos, opensAt: "center", secret: "MARKER" }),
      }),
    );
    change(byId(h.root, "sc-opens-at"), { value: "center" });
    await h.page.idle();
    expect(byId(h.root, "sc-opens-at").value).toBe("cursor");
    expect(oneByClass(h.root, "sc-status").textContent).toBe("Couldn't save your shortcuts.");
    h.win.dispatch("focus");
    await h.page.idle();
    expect(oneByClass(h.root, "sc-status").textContent).toBe("Couldn't save your shortcuts.");
    expect(warned).toHaveBeenCalledTimes(3);
    expect(JSON.stringify(warned.mock.calls)).not.toContain("MARKER");
  });
});

describe("ordering", () => {
  it("sends one command at a time and draws only the last answer", async () => {
    const first = deferred();
    const second = deferred();
    const replies = [first, second];
    const h = await shown(macos, () => replies.shift()?.promise ?? Promise.reject("unexpected"));
    change(byId(h.root, "sc-enabled"), { checked: true });
    change(byId(h.root, "sc-opens-at"), { value: "center" });
    await flush();
    expect(commands(h)).toEqual([["shortcuts_set_enabled", { on: true }]]);
    expect(h.root.getAttribute("aria-busy")).toBe("true");

    first.resolve({ ...macos, enabled: true });
    await flush();
    expect(commands(h)).toHaveLength(2);
    // The first answer isn't drawn: it would put the select back for a moment.
    expect(byId(h.root, "sc-opens-at").value).toBe("center");

    second.resolve({ ...macos, enabled: true, opensAt: "center" });
    await h.page.idle();
    expect(byId(h.root, "sc-enabled").checked).toBe(true);
    expect(byId(h.root, "sc-opens-at").value).toBe("center");
    expect(h.root.hasAttribute("aria-busy")).toBe(false);
  });

  it("uses a queue of its own, apart from the rest of Settings", async () => {
    const h = await shown(macos);
    expect(h.invoke.mock.calls.every(([command]) => command.startsWith("shortcuts_"))).toBe(true);
    expect(byClass(h.root, "settings-status")).toHaveLength(1);
  });
});

describe("the sample view", () => {
  it("has every step kind, so the tests above cover each one", () => {
    const kinds = new Set<StepView["kind"]>();
    for (const menu of sample.menus) {
      for (const top of menu.items) {
        for (const item of top.kind === "group" ? top.items : [top]) {
          for (const step of item.steps) kinds.add(step.kind);
        }
      }
    }
    expect([...kinds].sort()).toEqual([...STEP_KINDS].sort());
    expect(isShortcutsView(sample)).toBe(true);
  });
});
