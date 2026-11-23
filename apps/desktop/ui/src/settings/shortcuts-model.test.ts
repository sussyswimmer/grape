import { describe, expect, it } from "vitest";
import { fixtureJson } from "../test-support/fixtures";
import { clone, get, type Json, mutate, objectPaths, paths, wrongType } from "../test-support/json";
import {
  addItemArgs,
  addStepArgs,
  allIds,
  bindingText,
  chordWire,
  delayEdit,
  findItem,
  findStep,
  glyphName,
  ICON_EMOJI,
  ICON_NONE,
  iconChoice,
  iconChoices,
  iconFromChoice,
  isOpensAt,
  isShortcutsView,
  isStepKind,
  itemSummary,
  menuChoices,
  menuName,
  menuOrEverywhere,
  menusFullText,
  MODIFIER_IDS,
  moveItemArgs,
  moveStepArgs,
  OPENS_AT_CHOICES,
  pickTargetArgs,
  removeItemArgs,
  removeMenuArgs,
  removeStepArgs,
  ringFullText,
  ringTitle,
  setEnabledArgs,
  setHotkeyArgs,
  setItemArgs,
  setMenuEverywhereArgs,
  setOpensAtArgs,
  setStepArgs,
  SHORTCUTS_ADD_ITEM,
  SHORTCUTS_ADD_MENU,
  SHORTCUTS_ADD_STEP,
  SHORTCUTS_COPY,
  SHORTCUTS_ERROR,
  SHORTCUTS_GET,
  SHORTCUTS_MOVE_ITEM,
  SHORTCUTS_MOVE_STEP,
  SHORTCUTS_PICK_TARGET,
  SHORTCUTS_REMOVE_ITEM,
  SHORTCUTS_REMOVE_MENU,
  SHORTCUTS_REMOVE_STEP,
  SHORTCUTS_REQUEST_ACCESS,
  SHORTCUTS_SET_ENABLED,
  SHORTCUTS_SET_HOTKEY,
  SHORTCUTS_SET_ITEM,
  SHORTCUTS_SET_MENU_EVERYWHERE,
  SHORTCUTS_SET_OPENS_AT,
  SHORTCUTS_SET_STEP,
  shortcutsErrorText,
  type ShortcutsView,
  STEP_FIELDS,
  STEP_GLYPHS,
  STEP_KIND_NAMES,
  STEP_KINDS,
  type StepEdit,
  stepEdit,
  stepPhrase,
  stepsFullText,
  type StepView,
} from "./shortcuts-model";

// The golden views and step edits the Rust model's tests build too, so both
// sides test one contract.
const raw = import.meta.glob<string>("../shared/fixtures/shortcuts-*.json", {
  query: "?raw",
  import: "default",
  eager: true,
});

const VIEWS = ["shortcuts-view-macos.json", "shortcuts-view-windows.json"] as const;

function text(name: string): string {
  const found = raw[`../shared/fixtures/${name}`];
  if (found === undefined) {
    throw new Error(`missing fixture ${name}`);
  }
  return found;
}

function viewFixture(name: string): ShortcutsView {
  const value = fixtureJson(name);
  if (!isShortcutsView(value)) {
    throw new Error(`${name} is not a ShortcutsView`);
  }
  return value;
}

const macos = viewFixture("shortcuts-view-macos.json");
const windows = viewFixture("shortcuts-view-windows.json");

const CHORD = chordWire("KeyK", { shift: true, ctrl: false, alt: false, meta: true });

/**
 * A view the fixtures don't show: an app's menu, a group, an emoji icon and
 * one step of every kind, set and unset.
 */
function richView(): ShortcutsView {
  const steps: StepView[] = [
    { id: 30, kind: "keys", chord: CHORD, label: "Shift+Command+K" },
    { id: 31, kind: "keys", chord: null, label: null },
    { id: 32, kind: "typeText", text: "Sent {date}\nMax" },
    { id: 33, kind: "clipboard", action: "clear" },
    { id: 34, kind: "openApp", name: "Safari" },
    { id: 35, kind: "openApp", name: null },
    { id: 36, kind: "openFile", name: "Invoices", folder: true },
    { id: 37, kind: "openFile", name: null, folder: false },
    { id: 38, kind: "openUrl", url: "https://example.com/inbox" },
    { id: 39, kind: "appleShortcut", name: "Start focus" },
    { id: 40, kind: "appleScript", source: 'tell application "Finder" to activate' },
    { id: 41, kind: "shell", source: "echo hello" },
    { id: 42, kind: "keyboardMaestro", name: "Tidy desktop" },
    { id: 43, kind: "window", placement: "rightHalf" },
    { id: 44, kind: "system", control: "toggleMute" },
    { id: 45, kind: "notify", text: "Break time" },
  ];
  return {
    ...clone(macos) as unknown as ShortcutsView,
    enabled: true,
    binding: "bound",
    access: "granted",
    opensAt: "center",
    note: "Everywhere didn't fit in this menu.",
    menus: [
      {
        id: 20,
        scope: { kind: "app", name: "Safari" },
        withEverywhere: true,
        items: [
          { id: 21, name: "Everything", icon: { kind: "emoji", value: "🍇" }, kind: "steps", steps },
          {
            id: 22,
            name: "Windows",
            icon: { kind: "none" },
            kind: "group",
            items: [
              {
                id: 23,
                name: "Say it",
                icon: { kind: "glyph", value: "speaker" },
                kind: "steps",
                steps: [
                  { id: 46, kind: "speak", text: "Stand up" },
                  { id: 47, kind: "delay", ms: 250 },
                ],
              },
              { id: 24, name: "Empty", icon: { kind: "none" }, kind: "steps", steps: [] },
            ],
          },
        ],
      },
      { id: 1, scope: { kind: "everywhere" }, withEverywhere: false, items: [] },
    ],
  };
}

describe("command names", () => {
  it("match the contract", () => {
    expect([
      SHORTCUTS_GET,
      SHORTCUTS_SET_ENABLED,
      SHORTCUTS_SET_HOTKEY,
      SHORTCUTS_SET_OPENS_AT,
      SHORTCUTS_REQUEST_ACCESS,
      SHORTCUTS_ADD_MENU,
      SHORTCUTS_REMOVE_MENU,
      SHORTCUTS_SET_MENU_EVERYWHERE,
      SHORTCUTS_ADD_ITEM,
      SHORTCUTS_SET_ITEM,
      SHORTCUTS_REMOVE_ITEM,
      SHORTCUTS_MOVE_ITEM,
      SHORTCUTS_ADD_STEP,
      SHORTCUTS_SET_STEP,
      SHORTCUTS_REMOVE_STEP,
      SHORTCUTS_MOVE_STEP,
      SHORTCUTS_PICK_TARGET,
    ]).toEqual([
      "shortcuts_get",
      "shortcuts_set_enabled",
      "shortcuts_set_hotkey",
      "shortcuts_set_opens_at",
      "shortcuts_request_access",
      "shortcuts_add_menu",
      "shortcuts_remove_menu",
      "shortcuts_set_menu_everywhere",
      "shortcuts_add_item",
      "shortcuts_set_item",
      "shortcuts_remove_item",
      "shortcuts_move_item",
      "shortcuts_add_step",
      "shortcuts_set_step",
      "shortcuts_remove_step",
      "shortcuts_move_step",
      "shortcuts_pick_target",
    ]);
  });
});

describe("command args", () => {
  it("carry exactly the contract's keys, in its order", () => {
    const icon = { kind: "glyph", value: "star" } as const;
    const cases: readonly (readonly [unknown, string])[] = [
      [setEnabledArgs(true), '{"on":true}'],
      [setHotkeyArgs(null), '{"chord":null}'],
      [
        setHotkeyArgs(CHORD),
        '{"chord":{"key":"KeyK","shift":true,"ctrl":false,"alt":false,"meta":true}}',
      ],
      [setOpensAtArgs("center"), '{"at":"center"}'],
      [removeMenuArgs(4), '{"menu":4}'],
      [setMenuEverywhereArgs(4, false), '{"menu":4,"on":false}'],
      [addItemArgs(1, null, "group"), '{"menu":1,"group":null,"kind":"group"}'],
      [addItemArgs(1, 9, "steps"), '{"menu":1,"group":9,"kind":"steps"}'],
      [setItemArgs(2, "Copy", icon), '{"item":2,"name":"Copy","icon":{"kind":"glyph","value":"star"}}'],
      [setItemArgs(2, "", { kind: "none" }), '{"item":2,"name":"","icon":{"kind":"none"}}'],
      [removeItemArgs(2), '{"item":2}'],
      [moveItemArgs(2, 0), '{"item":2,"to":0}'],
      [addStepArgs(2, "keyboardMaestro"), '{"item":2,"kind":"keyboardMaestro"}'],
      [setStepArgs(3, stepEdit.delay(250)), '{"step":3,"value":{"kind":"delay","ms":250}}'],
      [removeStepArgs(3), '{"step":3}'],
      [moveStepArgs(3, 5), '{"step":3,"to":5}'],
      [pickTargetArgs(3), '{"step":3}'],
    ];
    for (const [args, json] of cases) {
      expect(JSON.stringify(args)).toBe(json);
    }
  });

  it("writes a chord's fields in Rust's order whatever order they come in", () => {
    const chord = chordWire("F5", { meta: false, alt: true, ctrl: false, shift: false });
    expect(JSON.stringify(chord)).toBe('{"key":"F5","shift":false,"ctrl":false,"alt":true,"meta":false}');
  });
});

describe("step edits", () => {
  // Each fixture, and the builder call that must write it byte for byte.
  const EDITS: Readonly<Record<string, StepEdit>> = {
    "shortcuts-step-keys.json": stepEdit.keys(CHORD),
    "shortcuts-step-keys-unset.json": stepEdit.keys(null),
    "shortcuts-step-type-text.json": stepEdit.typeText("Sent {date} at {time}\nMax"),
    "shortcuts-step-clipboard.json": stepEdit.clipboard("paste"),
    "shortcuts-step-open-url.json": stepEdit.openUrl("https://example.com/inbox"),
    "shortcuts-step-apple-shortcut.json": stepEdit.appleShortcut("Start focus"),
    "shortcuts-step-apple-script.json": stepEdit.appleScript('tell application "Finder" to activate'),
    "shortcuts-step-shell.json": stepEdit.shell("echo hello"),
    "shortcuts-step-keyboard-maestro.json": stepEdit.keyboardMaestro("Tidy desktop"),
    "shortcuts-step-window.json": stepEdit.window("rightHalf"),
    "shortcuts-step-system.json": stepEdit.system("toggleMute"),
    "shortcuts-step-notify.json": stepEdit.notify("Break time"),
    "shortcuts-step-speak.json": stepEdit.speak("Stand up"),
    "shortcuts-step-delay.json": stepEdit.delay(250),
  };

  it("has a builder for every step fixture, and a fixture for every builder", () => {
    const steps = Object.keys(raw)
      .map((path) => path.replace("../shared/fixtures/", ""))
      .filter((name) => name.startsWith("shortcuts-step-"));
    expect(steps.sort()).toEqual(Object.keys(EDITS).sort());
    const built = new Set(Object.values(EDITS).map((edit) => edit.kind));
    expect([...built].sort()).toEqual(Object.keys(stepEdit).sort());
  });

  it.each(Object.keys(EDITS))("%s is what its builder writes, byte for byte", (name) => {
    expect(`${JSON.stringify(EDITS[name])}\n`).toBe(text(name));
  });

  it("has no edit for an app, a file or a folder: Rust picks those", () => {
    expect(Object.keys(stepEdit)).not.toContain("openApp");
    expect(Object.keys(stepEdit)).not.toContain("openFile");
    expect(STEP_FIELDS.openApp.control).toBe("pick");
    expect(STEP_FIELDS.openFile.control).toBe("pick");
  });

  it("builds the same edits from each kind's field", () => {
    const fromField = (kind: keyof typeof STEP_FIELDS, value: string): StepEdit | null => {
      const field = STEP_FIELDS[kind];
      return field.control === "text" || field.control === "choice" || field.control === "number"
        ? field.edit(value)
        : null;
    };
    expect(fromField("typeText", "hi")).toEqual(stepEdit.typeText("hi"));
    expect(fromField("openUrl", "mailto:max@example.com")).toEqual(stepEdit.openUrl("mailto:max@example.com"));
    expect(fromField("appleShortcut", "Start focus")).toEqual(stepEdit.appleShortcut("Start focus"));
    expect(fromField("appleScript", "beep")).toEqual(stepEdit.appleScript("beep"));
    expect(fromField("shell", "ls")).toEqual(stepEdit.shell("ls"));
    expect(fromField("keyboardMaestro", "Tidy")).toEqual(stepEdit.keyboardMaestro("Tidy"));
    expect(fromField("notify", "Done")).toEqual(stepEdit.notify("Done"));
    expect(fromField("speak", "Done")).toEqual(stepEdit.speak("Done"));
    expect(fromField("clipboard", "copy")).toEqual(stepEdit.clipboard("copy"));
    expect(fromField("window", "topLeft")).toEqual(stepEdit.window("topLeft"));
    expect(fromField("system", "lockScreen")).toEqual(stepEdit.system("lockScreen"));
    expect(fromField("delay", "250")).toEqual(stepEdit.delay(250));
    // A value the select doesn't offer makes no edit.
    expect(fromField("clipboard", "cut")).toBeNull();
    expect(fromField("window", "copy")).toBeNull();
    expect(fromField("system", "")).toBeNull();
  });

  it("makes a delay only from a whole number Rust can read", () => {
    expect(delayEdit("250")).toEqual({ kind: "delay", ms: 250 });
    expect(delayEdit(" 10 ")).toEqual({ kind: "delay", ms: 10 });
    // The range is Rust's to judge, and it says why.
    expect(delayEdit("0")).toEqual({ kind: "delay", ms: 0 });
    expect(delayEdit("4294967295")).toEqual({ kind: "delay", ms: 4294967295 });
    for (const bad of ["", " ", "-5", "2.5", "1e3", "abc", "4294967296", "99999999999", "0x10"]) {
      expect(delayEdit(bad), bad).toBeNull();
    }
  });
});

describe("golden views", () => {
  it("are exactly the macOS and Windows views", () => {
    const views = Object.keys(raw).filter((path) => path.includes("shortcuts-view-"));
    expect(views.sort()).toEqual(VIEWS.map((name) => `../shared/fixtures/${name}`).sort());
  });

  it.each(VIEWS)("%s is one line of JSON with a trailing newline", (name) => {
    const body = text(name);
    expect(body.endsWith("}\n")).toBe(true);
    expect(body.indexOf("\n")).toBe(body.length - 1);
    expect(body).not.toContain("\r");
  });

  it.each(VIEWS)("%s passes the guard", (name) => {
    expect(isShortcutsView(JSON.parse(text(name)))).toBe(true);
  });

  it("reads the macOS view", () => {
    expect(macos.available).toBe(true);
    expect(macos.enabled).toBe(false);
    expect(macos.hotkey).toEqual({
      chord: { key: "KeyG", shift: false, ctrl: true, alt: true, meta: false },
      label: "Control+Option+G",
    });
    expect(macos.binding).toBe("off");
    expect(macos.access).toBe("denied");
    expect(macos.opensAt).toBe("cursor");
    expect(macos.note).toBeNull();
    expect(macos.limits).toEqual({
      items: 12,
      steps: 16,
      name: 24,
      icon: 16,
      text: 4000,
      script: 20000,
      url: 2048,
      menus: 40,
      delayMinMs: 10,
      delayMaxMs: 60000,
    });
    expect(macos.keys).toHaveLength(81);
    expect(macos.keys[0]).toEqual({ id: "KeyA", label: "A" });
    expect(macos.modifiers).toEqual({ shift: "Shift", ctrl: "Control", alt: "Option", meta: "Command" });
    expect(macos.glyphs).toHaveLength(24);
    expect(macos.menus).toHaveLength(1);
    expect(macos.menus[0]?.scope).toEqual({ kind: "everywhere" });
    expect(macos.menus[0]?.items.map((item) => item.name)).toEqual([
      "Copy",
      "Paste",
      "Screenshot",
      "Mission Control",
      "Mute",
      "Lock screen",
    ]);
    expect(macos.menus[0]?.items[0]).toEqual({
      id: 2,
      name: "Copy",
      icon: { kind: "glyph", value: "clipboard" },
      kind: "steps",
      steps: [{ id: 3, kind: "clipboard", action: "copy" }],
    });
  });

  it("reads the Windows view", () => {
    expect(windows.available).toBe(false);
    expect(windows.enabled).toBe(false);
    expect(windows.hotkey?.label).toBe("Ctrl+Alt+G");
    expect(windows.binding).toBe("off");
    expect(windows.access).toBe("notNeeded");
    expect(windows.modifiers).toEqual({ shift: "Shift", ctrl: "Ctrl", alt: "Alt", meta: "Win" });
    expect(windows.menus).toEqual([
      { id: 1, scope: { kind: "everywhere" }, withEverywhere: false, items: [] },
    ]);
  });

  it("takes a view with an app's menu, a group and a step of every kind", () => {
    const rich = richView();
    expect(isShortcutsView(rich)).toBe(true);
    const kinds = new Set<string>();
    for (const id of allIds(rich)) {
      const place = findStep(rich, id);
      if (place !== null) {
        kinds.add(place.step.kind);
      }
    }
    expect([...kinds].sort()).toEqual([...STEP_KINDS].sort());
  });
});

describe("malformed views fail", () => {
  const SAMPLES: readonly (readonly [string, Json])[] = [
    ...VIEWS.map((name) => [name, clone(JSON.parse(text(name)))] as const),
    ["a view with every step kind", clone(richView())],
  ];

  describe.each(SAMPLES)("%s", (_name, value) => {
    const all = paths(value);

    it("without each field", () => {
      for (const path of all) {
        // Removing a list's entry is a semantic case (below), not a missing field.
        if (typeof path[path.length - 1] === "number") continue;
        expect(isShortcutsView(mutate(value, path, undefined)), `without ${path.join(".")}`).toBe(false);
      }
    });

    it("with a wrong type at each field", () => {
      for (const path of all) {
        const broken = mutate(value, path, wrongType(get(value, path)));
        expect(isShortcutsView(broken), `wrong type at ${path.join(".")}`).toBe(false);
      }
    });

    it("with an extra key on each object", () => {
      for (const path of objectPaths(value)) {
        const copy = clone(value);
        const target = get(copy, path) as { [key: string]: Json };
        target["extra"] = 1;
        expect(isShortcutsView(copy), `extra key at "${path.join(".")}"`).toBe(false);
      }
    });

    it("when it is not an object", () => {
      for (const bad of [null, undefined, 4, "x", true, [], [value]]) {
        expect(isShortcutsView(bad)).toBe(false);
      }
    });
  });
});

describe("semantic checks", () => {
  /** A copy of the rich view with `change` applied. */
  function changed(change: (view: ShortcutsView) => void): ShortcutsView {
    const copy = clone(richView()) as unknown as ShortcutsView;
    change(copy);
    return copy;
  }

  function withValue(path: readonly (string | number)[], value: unknown): unknown {
    return mutate(richView(), path, value);
  }

  it("rejects unknown enum values", () => {
    const cases: readonly (readonly [readonly (string | number)[], unknown])[] = [
      [["binding"], "pending"],
      [["access"], "asked"],
      [["opensAt"], "corner"],
      [["menus", 0, "scope", "kind"], "window"],
      [["menus", 0, "items", 0, "kind"], "folder"],
      [["menus", 0, "items", 0, "icon", "kind"], "image"],
      [["menus", 0, "items", 0, "steps", 2, "kind"], "paste"],
      [["menus", 0, "items", 0, "steps", 3, "action"], "cut"],
      [["menus", 0, "items", 0, "steps", 13, "placement"], "thirds"],
      [["menus", 0, "items", 0, "steps", 14, "control"], "shutDown"],
    ];
    for (const [path, value] of cases) {
      expect(isShortcutsView(withValue(path, value)), path.join(".")).toBe(false);
    }
  });

  it("rejects an id used twice, anywhere", () => {
    // A step with its shortcut's id, a shortcut with a menu's, two steps alike.
    expect(isShortcutsView(withValue(["menus", 0, "items", 0, "steps", 0, "id"], 21))).toBe(false);
    expect(isShortcutsView(withValue(["menus", 0, "items", 0, "id"], 1))).toBe(false);
    expect(isShortcutsView(withValue(["menus", 0, "items", 0, "steps", 1, "id"], 30))).toBe(false);
    expect(isShortcutsView(withValue(["menus", 1, "id"], 20))).toBe(false);
  });

  it("rejects ids that aren't positive whole numbers", () => {
    for (const bad of [0, -3, 2.5, Number.MAX_SAFE_INTEGER + 2, Number.NaN]) {
      expect(isShortcutsView(withValue(["menus", 0, "id"], bad)), String(bad)).toBe(false);
      expect(isShortcutsView(withValue(["menus", 0, "items", 0, "steps", 0, "id"], bad))).toBe(false);
    }
  });

  it("rejects a group inside a group", () => {
    const nested = changed((view) => {
      const group = view.menus[0]?.items[1];
      if (group?.kind !== "group") throw new Error("no group");
      const inner = { id: 90, name: "Deeper", icon: { kind: "none" }, kind: "group", items: [] };
      (group.items as unknown[]).push(inner);
    });
    expect(isShortcutsView(nested)).toBe(false);
  });

  it("wants one Everywhere menu, never two", () => {
    const none = changed((view) => {
      view.menus.pop();
    });
    expect(isShortcutsView(none)).toBe(false);
    const two = changed((view) => {
      view.menus.push({ id: 91, scope: { kind: "everywhere" }, withEverywhere: false, items: [] });
    });
    expect(isShortcutsView(two)).toBe(false);
    expect(isShortcutsView({ ...richView(), menus: [] })).toBe(false);
  });

  it("takes a view with no menu at all only where the wheel isn't available", () => {
    expect(isShortcutsView({ ...windows, menus: [] })).toBe(true);
    expect(isShortcutsView({ ...macos, menus: [] })).toBe(false);
    const two = [...windows.menus, { id: 91, scope: { kind: "everywhere" }, withEverywhere: false, items: [] }];
    expect(isShortcutsView({ ...windows, menus: two })).toBe(false);
  });

  it("wants every chord's key to be one the view offers", () => {
    expect(isShortcutsView(withValue(["hotkey", "chord", "key"], "KeyÅ"))).toBe(false);
    expect(isShortcutsView(withValue(["menus", 0, "items", 0, "steps", 0, "chord", "key"], "Fn"))).toBe(false);
    // No key is offered, so no chord can be shown.
    expect(isShortcutsView({ ...richView(), keys: [] })).toBe(false);
    const noChords = changed((view) => {
      view.hotkey = null;
      view.keys = [];
      view.menus = [{ id: 1, scope: { kind: "everywhere" }, withEverywhere: false, items: [] }];
    });
    expect(isShortcutsView(noChords)).toBe(true);
  });

  it("wants a Keys step's chord and label together, or neither", () => {
    expect(isShortcutsView(withValue(["menus", 0, "items", 0, "steps", 0, "label"], null))).toBe(false);
    expect(isShortcutsView(withValue(["menus", 0, "items", 0, "steps", 1, "label"], "Command+K"))).toBe(false);
    expect(isShortcutsView(withValue(["menus", 0, "items", 0, "steps", 1, "chord"], CHORD))).toBe(false);
  });

  it("wants a glyph icon to be one the view offers, and each key and glyph once", () => {
    expect(isShortcutsView(withValue(["menus", 0, "items", 1, "items", 0, "icon", "value"], "rocket"))).toBe(false);
    expect(isShortcutsView({ ...richView(), glyphs: [...macos.glyphs, "key"] })).toBe(false);
    expect(isShortcutsView({ ...richView(), glyphs: ["", ...macos.glyphs] })).toBe(false);
    expect(isShortcutsView({ ...richView(), keys: [...macos.keys, { id: "KeyA", label: "A again" }] })).toBe(
      false,
    );
    expect(isShortcutsView(withValue(["keys", 0, "label"], ""))).toBe(false);
    expect(isShortcutsView(withValue(["modifiers", "alt"], ""))).toBe(false);
  });

  it("wants limits that are positive whole numbers, the shortest delay not over the longest", () => {
    for (const name of Object.keys(macos.limits)) {
      for (const bad of [0, -1, 1.5]) {
        expect(isShortcutsView(withValue(["limits", name], bad)), `${name} ${bad}`).toBe(false);
      }
    }
    expect(isShortcutsView(withValue(["limits", "delayMinMs"], 60001))).toBe(false);
    expect(isShortcutsView(withValue(["limits", "delayMinMs"], 60000))).toBe(true);
  });

  it("rejects a delay that isn't a whole number of milliseconds", () => {
    const delay = ["menus", 0, "items", 1, "items", 0, "steps", 1, "ms"] as const;
    for (const bad of [-1, 0.5]) {
      expect(isShortcutsView(withValue(delay, bad))).toBe(false);
    }
    // The range is Rust's: the page shows what it's given.
    expect(isShortcutsView(withValue(delay, 0))).toBe(true);
    expect(isShortcutsView(withValue(delay, 600000))).toBe(true);
  });

  it("checks what the user typed for its type only, never its length", () => {
    const long = "x".repeat(50_000);
    const typed: readonly (readonly (string | number)[])[] = [
      ["menus", 0, "scope", "name"],
      ["menus", 0, "items", 0, "name"],
      ["menus", 0, "items", 0, "icon", "value"],
      ["menus", 0, "items", 0, "steps", 2, "text"],
      ["menus", 0, "items", 0, "steps", 4, "name"],
      ["menus", 0, "items", 0, "steps", 6, "name"],
      ["menus", 0, "items", 0, "steps", 8, "url"],
      ["menus", 0, "items", 0, "steps", 9, "name"],
      ["menus", 0, "items", 0, "steps", 10, "source"],
      ["menus", 0, "items", 0, "steps", 11, "source"],
      ["menus", 0, "items", 0, "steps", 12, "name"],
      ["menus", 0, "items", 0, "steps", 15, "text"],
      ["note"],
      ["hotkey", "label"],
    ];
    for (const path of typed) {
      expect(isShortcutsView(withValue(path, long)), path.join(".")).toBe(true);
      expect(isShortcutsView(withValue(path, "")), path.join(".")).toBe(true);
    }
  });

  it("takes more shortcuts or steps than the limits say: Rust keeps the limits", () => {
    const crowded = changed((view) => {
      const everywhere = view.menus[1];
      if (everywhere === undefined) throw new Error("no Everywhere");
      for (let n = 0; n < 20; n += 1) {
        everywhere.items.push({ id: 100 + n, name: `S${n}`, icon: { kind: "none" }, kind: "steps", steps: [] });
      }
    });
    expect(isShortcutsView(crowded)).toBe(true);
  });
});

describe("value checks before a command", () => {
  it("knows the fifteen step kinds, in Rust's order", () => {
    expect(STEP_KINDS).toEqual([
      "keys",
      "typeText",
      "clipboard",
      "openApp",
      "openFile",
      "openUrl",
      "appleShortcut",
      "appleScript",
      "shell",
      "keyboardMaestro",
      "window",
      "system",
      "notify",
      "speak",
      "delay",
    ]);
    for (const kind of STEP_KINDS) {
      expect(isStepKind(kind)).toBe(true);
    }
    for (const bad of ["", "Keys", "open", null, 3, undefined]) {
      expect(isStepKind(bad)).toBe(false);
    }
  });

  it("knows where the wheel can open", () => {
    expect(OPENS_AT_CHOICES.map((choice) => choice.value)).toEqual(["cursor", "center"]);
    expect(isOpensAt("cursor")).toBe(true);
    expect(isOpensAt("center")).toBe(true);
    expect(isOpensAt("corner")).toBe(false);
    expect(isOpensAt(null)).toBe(false);
  });

  it("lists the modifiers in page order", () => {
    expect(MODIFIER_IDS).toEqual(["shift", "ctrl", "alt", "meta"]);
  });
});

describe("copy", () => {
  it("says the section's fixed sentences", () => {
    expect(SHORTCUTS_COPY.title).toBe("Shortcuts");
    expect(SHORTCUTS_COPY.plainText).toBe(
      "Shortcuts are saved as plain text on this Mac. Don't put passwords in them.",
    );
    expect(SHORTCUTS_COPY.accessRepair).toBe(
      "If Grape is already in that list, remove it with − and add it again.",
    );
    expect(SHORTCUTS_COPY.accessDenied).toContain("Accessibility");
    expect(SHORTCUTS_COPY.accessGranted).toContain("Accessibility");
    expect(SHORTCUTS_COPY.unavailable).toContain("on the Mac first");
    expect(SHORTCUTS_COPY.lead.length).toBeGreaterThan(0);
  });

  it("puts each button's text inside its longer accessible name", () => {
    expect(SHORTCUTS_COPY.askAccessLabel).toContain(SHORTCUTS_COPY.askAccess);
    expect(SHORTCUTS_COPY.addMenuLabel).toContain(SHORTCUTS_COPY.addMenu.replace("…", ""));
    for (const kind of ["openApp", "openFile"] as const) {
      const field = STEP_FIELDS[kind];
      expect(field.control === "pick" && field.label).toContain(SHORTCUTS_COPY.choose.replace("…", ""));
    }
  });

  it("names every step kind once", () => {
    const names = STEP_KINDS.map((kind) => STEP_KIND_NAMES[kind]);
    expect(new Set(names).size).toBe(STEP_KINDS.length);
    for (const name of names) {
      expect(name.length).toBeGreaterThan(0);
    }
    expect(Object.keys(STEP_KIND_NAMES).sort()).toEqual([...STEP_KINDS].sort());
  });

  it("gives every step kind one field, with a label", () => {
    expect(Object.keys(STEP_FIELDS).sort()).toEqual([...STEP_KINDS].sort());
    for (const kind of STEP_KINDS) {
      const field = STEP_FIELDS[kind];
      expect(field.label.length, kind).toBeGreaterThan(0);
      if (field.control === "text") {
        expect(field.placeholder.length, kind).toBeGreaterThan(0);
        expect(field.lines, kind).toBeGreaterThan(0);
      }
      if (field.control === "choice") {
        const values = field.choices.map((choice) => choice.value);
        expect(new Set(values).size, kind).toBe(values.length);
        expect(field.choices.every((choice) => choice.label.length > 0)).toBe(true);
      }
    }
    expect(STEP_FIELDS.keys.control).toBe("chord");
    expect(STEP_FIELDS.delay.control).toBe("number");
  });

  it("takes each text field's length limit from the right place", () => {
    const limit = (kind: keyof typeof STEP_FIELDS): string | null => {
      const field = STEP_FIELDS[kind];
      return field.control === "text" ? field.limit : null;
    };
    expect(limit("typeText")).toBe("text");
    expect(limit("notify")).toBe("text");
    expect(limit("speak")).toBe("text");
    expect(limit("appleShortcut")).toBe("text");
    expect(limit("keyboardMaestro")).toBe("text");
    expect(limit("appleScript")).toBe("script");
    expect(limit("shell")).toBe("script");
    expect(limit("openUrl")).toBe("url");
  });

  it("reads each field's value from a step of its kind", () => {
    const rich = richView();
    const read = (id: number): string | null => {
      const place = findStep(rich, id);
      if (place === null) throw new Error(`no step ${id}`);
      const field = STEP_FIELDS[place.step.kind];
      return field.control === "chord" ? null : field.read(place.step);
    };
    expect(read(32)).toBe("Sent {date}\nMax");
    expect(read(33)).toBe("clear");
    expect(read(34)).toBe("Safari");
    expect(read(35)).toBeNull();
    expect(read(36)).toBe("Invoices");
    expect(read(38)).toBe("https://example.com/inbox");
    expect(read(39)).toBe("Start focus");
    expect(read(40)).toBe('tell application "Finder" to activate');
    expect(read(41)).toBe("echo hello");
    expect(read(42)).toBe("Tidy desktop");
    expect(read(43)).toBe("rightHalf");
    expect(read(44)).toBe("toggleMute");
    expect(read(45)).toBe("Break time");
    expect(read(46)).toBe("Stand up");
    expect(read(47)).toBe("250");
  });

  it("gives every step kind a glyph Rust offers", () => {
    for (const kind of STEP_KINDS) {
      expect(macos.glyphs, kind).toContain(STEP_GLYPHS[kind]);
    }
  });

  it("words the binding line", () => {
    expect(bindingText(macos)).toBe("The wheel is off.");
    expect(bindingText({ ...macos, enabled: true, binding: "bound" })).toBe("Control+Option+G opens the wheel.");
    expect(bindingText({ ...macos, enabled: true, binding: "failed" })).toContain("macOS wouldn't give Grape");
    expect(bindingText({ ...macos, enabled: true, binding: "off" })).not.toBe("The wheel is off.");
    expect(bindingText({ ...macos, hotkey: null })).toContain("Choose a key");
  });

  it("words the limits with the view's numbers", () => {
    expect(ringFullText(12)).toContain("12");
    expect(stepsFullText(16)).toContain("16");
    expect(menusFullText(40)).toContain("40");
  });

  it("shows Rust's sentence for a refusal, and its own for the rest", () => {
    expect(shortcutsErrorText("A name needs 1 to 24 characters.")).toBe("A name needs 1 to 24 characters.");
    for (const reason of ["", "  ", null, undefined, 4, new Error("no ipc"), { message: "x" }]) {
      expect(shortcutsErrorText(reason)).toBe(SHORTCUTS_ERROR);
    }
  });
});

describe("icons", () => {
  it("offers no icon, emoji, then each glyph by a name of its own", () => {
    const choices = iconChoices(macos.glyphs);
    expect(choices).toHaveLength(macos.glyphs.length + 2);
    expect(choices[0]).toEqual({ value: ICON_NONE, label: "No icon" });
    expect(choices[1]).toEqual({ value: ICON_EMOJI, label: "Emoji…" });
    expect(choices[2]).toEqual({ value: "glyph:key", label: "Key" });
    expect(new Set(choices.map((choice) => choice.value)).size).toBe(choices.length);
    expect(new Set(choices.map((choice) => choice.label)).size).toBe(choices.length);
    for (const id of macos.glyphs) {
      // A name, not the bare id.
      expect(glyphName(id)).toMatch(/^[A-Z]/);
    }
  });

  it("names a glyph it doesn't know by its id, and never by an inherited key", () => {
    expect(glyphName("rocket")).toBe("Rocket");
    expect(glyphName("constructor")).toBe("Constructor");
    expect(glyphName("")).toBe("");
  });

  it("goes from an icon to its choice and back", () => {
    const icons = [
      { kind: "none" },
      { kind: "emoji", value: "🍇" },
      { kind: "glyph", value: "camera" },
    ] as const;
    for (const icon of icons) {
      const emoji = icon.kind === "emoji" ? icon.value : "";
      expect(iconFromChoice(iconChoice(icon), emoji, macos.glyphs)).toEqual(icon);
    }
  });

  it("makes no icon from Emoji with nothing typed, or from a choice it doesn't offer", () => {
    expect(iconFromChoice(ICON_EMOJI, "", macos.glyphs)).toBeNull();
    expect(iconFromChoice("glyph:rocket", "", macos.glyphs)).toBeNull();
    expect(iconFromChoice("camera", "", macos.glyphs)).toBeNull();
    expect(iconFromChoice("", "🍇", macos.glyphs)).toBeNull();
    // A glyph id that happens to be called like another choice is still a glyph.
    expect(iconFromChoice("glyph:none", "", ["none"])).toEqual({ kind: "glyph", value: "none" });
  });
});

describe("summaries", () => {
  const rich = richView();

  function step(id: number): StepView {
    const place = findStep(rich, id);
    if (place === null) throw new Error(`no step ${id}`);
    return place.step;
  }

  it("says what a step does in a few words", () => {
    expect(stepPhrase(step(30))).toBe("Press Shift+Command+K");
    expect(stepPhrase(step(31))).toBe("Press keys");
    expect(stepPhrase(step(33))).toBe("Clear the clipboard");
    expect(stepPhrase(step(34))).toBe("Open Safari");
    expect(stepPhrase(step(35))).toBe("Open an app");
    expect(stepPhrase(step(36))).toBe("Open Invoices");
    expect(stepPhrase(step(37))).toBe("Open a file or folder");
    expect(stepPhrase(step(43))).toBe("Window: Right half");
    expect(stepPhrase(step(44))).toBe("Mute or unmute");
    expect(stepPhrase(step(47))).toBe("Wait 250 ms");
  });

  it("never repeats typed text, a script or a link", () => {
    for (const id of [32, 38, 39, 40, 41, 42, 45, 46]) {
      const phrase = stepPhrase(step(id));
      expect(phrase).toBe(STEP_KIND_NAMES[step(id).kind]);
    }
  });

  it("sums up a shortcut's steps on one line", () => {
    const item = (id: number) => {
      const place = findItem(rich, id);
      if (place === null) throw new Error(`no item ${id}`);
      return place.item;
    };
    expect(itemSummary(item(24))).toBe("No steps yet");
    expect(itemSummary(item(23))).toBe("Speak, then Wait 250 ms");
    expect(itemSummary(item(21))).toBe("Press Shift+Command+K, then Press keys and 14 more");
    expect(itemSummary(item(22))).toBe("Group of 2 shortcuts");
    const copy = macos.menus[0]?.items[0];
    expect(copy === undefined ? "" : itemSummary(copy)).toBe("Copy");
    expect(itemSummary({ id: 9, name: "G", icon: { kind: "none" }, kind: "group", items: [] })).toBe("Empty group");
    const only = item(24);
    if (only.kind !== "steps") throw new Error("not a shortcut");
    expect(itemSummary({ id: 9, name: "G", icon: { kind: "none" }, kind: "group", items: [only] })).toBe(
      "Group of 1 shortcut",
    );
  });
});

describe("finding things in a view", () => {
  const rich = richView();

  it("lists Everywhere first, then the apps as Rust has them", () => {
    expect(menuChoices(rich).map((menu) => menu.id)).toEqual([1, 20]);
    expect(menuChoices(rich).map((menu) => menuName(menu.scope))).toEqual(["Everywhere", "Safari"]);
    expect(ringTitle({ kind: "everywhere" })).toBe("Shortcuts everywhere");
    expect(ringTitle({ kind: "app", name: "Safari" })).toBe("Shortcuts in Safari");
  });

  it("falls back to Everywhere for a menu that isn't there", () => {
    expect(menuOrEverywhere(rich, 20)?.id).toBe(20);
    expect(menuOrEverywhere(rich, null)?.id).toBe(1);
    expect(menuOrEverywhere(rich, 999)?.id).toBe(1);
  });

  it("finds a shortcut in a ring or in a group, with its place", () => {
    const top = findItem(rich, 22);
    expect(top?.menu.id).toBe(20);
    expect(top?.group).toBeNull();
    expect(top?.index).toBe(1);
    expect(top?.count).toBe(2);
    const child = findItem(rich, 24);
    expect(child?.group?.id).toBe(22);
    expect(child?.index).toBe(1);
    expect(child?.count).toBe(2);
    expect(findItem(rich, 30)).toBeNull();
    expect(findItem(rich, 999)).toBeNull();
  });

  it("finds a step with its shortcut and its place", () => {
    const first = findStep(rich, 31);
    expect(first?.item.id).toBe(21);
    expect(first?.index).toBe(1);
    expect(first?.count).toBe(16);
    const inGroup = findStep(rich, 47);
    expect(inGroup?.item.id).toBe(23);
    expect(inGroup?.index).toBe(1);
    expect(findStep(rich, 21)).toBeNull();
  });

  it("collects every id once", () => {
    const ids = allIds(rich);
    expect(ids.size).toBe(2 + 4 + 18);
    expect(ids.has(1) && ids.has(20) && ids.has(24) && ids.has(47)).toBe(true);
    expect(allIds(macos).size).toBe(13);
  });
});
