import { describe, expect, it } from "vitest";
import { clone, get, type Json, mutate, objectPaths, paths, wrongType } from "../test-support/json";
import { settingsFixture } from "../test-support/fixtures";
import {
  choicesSignature,
  errorText,
  GENERIC_ERROR,
  isKeyId,
  isSettingsView,
  isTerminalId,
  keyNames,
  leadText,
  outputFolder,
  SETTINGS_DISMISS_WELCOME,
  SETTINGS_GET,
  SETTINGS_PICK_FOLDER,
  SETTINGS_SET_KEY,
  SETTINGS_SET_OUTPUT_NEXT_TO_SOURCE,
  SETTINGS_SET_TERMINAL,
  SETTINGS_SET_TOGGLE,
  SETTINGS_USE_DEFAULT_FALLBACK,
  type SettingsView,
  terminalHint,
  terminalSignature,
  toggleRows,
  UNKNOWN_STATE,
  WELCOME_COPY,
  welcomeSteps,
  WHEEL_LABELS,
  WHEELS,
} from "./model";

// The golden views the Rust settings tests build too, so both sides test
// one contract.
const raw = import.meta.glob<string>("../shared/fixtures/settings-view-*.json", {
  query: "?raw",
  import: "default",
  eager: true,
});

const NAMES = ["settings-view-windows.json", "settings-view-macos.json"] as const;

function text(name: string): string {
  const found = raw[`../shared/fixtures/${name}`];
  if (found === undefined) {
    throw new Error(`missing fixture ${name}`);
  }
  return found;
}

const windows = settingsFixture("settings-view-windows.json");
const macos = settingsFixture("settings-view-macos.json");

describe("command names", () => {
  it("match the contract", () => {
    expect(SETTINGS_GET).toBe("settings_get");
    expect(SETTINGS_SET_KEY).toBe("settings_set_key");
    expect(SETTINGS_SET_OUTPUT_NEXT_TO_SOURCE).toBe("settings_set_output_next_to_source");
    expect(SETTINGS_PICK_FOLDER).toBe("settings_pick_folder");
    expect(SETTINGS_USE_DEFAULT_FALLBACK).toBe("settings_use_default_fallback");
    expect(SETTINGS_SET_TOGGLE).toBe("settings_set_toggle");
    expect(SETTINGS_SET_TERMINAL).toBe("settings_set_terminal");
    expect(SETTINGS_DISMISS_WELCOME).toBe("settings_dismiss_welcome");
  });
});

describe("golden views", () => {
  it("are exactly the Windows and macOS views", () => {
    expect(Object.keys(raw).sort()).toEqual(NAMES.map((name) => `../shared/fixtures/${name}`).sort());
  });

  it.each(NAMES)("%s is one line of JSON with a trailing newline", (name) => {
    const body = text(name);
    expect(body.endsWith("}\n")).toBe(true);
    expect(body.indexOf("\n")).toBe(body.length - 1);
    expect(body).not.toContain("\r");
  });

  it.each(NAMES)("%s passes the guard", (name) => {
    expect(isSettingsView(JSON.parse(text(name)))).toBe(true);
  });

  it("reads the Windows view", () => {
    expect(windows.platform).toBe("windows");
    expect(windows.keys).toEqual({ conversions: "shift", tools: "ctrl", actions: "alt" });
    expect(windows.keyChoices.map((choice) => choice.name)).toEqual(["Shift", "Ctrl", "Alt"]);
    expect(windows.output).toEqual({ kind: "nextToSource" });
    expect(windows.fallbackDir).toEqual({ path: "C:\\Users\\max\\Downloads\\Grape", isDefault: true });
    expect(windows.haptics).toBeNull();
    expect(windows.launchAtLogin).toBe(true);
    expect(windows.note).toBeNull();
    expect(windows.terminal).toEqual({
      choice: "windowsTerminal",
      choices: [
        { id: "windowsTerminal", name: "Windows Terminal" },
        { id: "powershell", name: "PowerShell" },
      ],
    });
    expect(windows.welcome).toBe(true);
  });

  it("reads the macOS view", () => {
    expect(macos.platform).toBe("macos");
    expect(macos.keys).toEqual({ conversions: "meta", tools: "shift", actions: "ctrl" });
    expect(macos.keyChoices.map((choice) => choice.name)).toEqual([
      "Shift",
      "Control",
      "Option",
      "Command",
    ]);
    expect(macos.output).toEqual({ kind: "folder", path: "/Users/max/Pictures/Grape out" });
    expect(macos.fallbackDir.isDefault).toBe(false);
    expect(macos.sounds).toBe(false);
    expect(macos.haptics).toBe(true);
    expect(macos.launchAtLogin).toBeNull();
    expect(macos.note).toBe("Command now opens Conversions, and Shift opens Tools.");
    expect(macos.terminal).toEqual({
      choice: "iterm2",
      choices: [
        { id: "terminal", name: "Terminal" },
        { id: "iterm2", name: "iTerm2" },
      ],
    });
    expect(macos.welcome).toBe(false);
  });
});

describe("malformed views fail", () => {
  describe.each(NAMES)("%s", (name) => {
    const value = clone(JSON.parse(text(name)));

    it.each(paths(value).map((path) => [path.join(".")] as const))("without %s", (dotted) => {
      const path = paths(value).find((candidate) => candidate.join(".") === dotted);
      if (path === undefined) throw new Error(dotted);
      // Removing a key or terminal choice is a semantic case (below), not a missing field.
      if (typeof path[path.length - 1] === "number") return;
      expect(isSettingsView(mutate(value, path, undefined))).toBe(false);
    });

    it.each(paths(value).map((path) => [path.join(".")] as const))(
      "with a wrong type at %s",
      (dotted) => {
        const path = paths(value).find((candidate) => candidate.join(".") === dotted);
        if (path === undefined) throw new Error(dotted);
        expect(isSettingsView(mutate(value, path, wrongType(get(value, path))))).toBe(false);
      },
    );

    it("with an extra key on any object", () => {
      for (const path of objectPaths(value)) {
        const copy = clone(value);
        const target = get(copy, path) as { [key: string]: Json };
        target["extra"] = 1;
        expect(isSettingsView(copy), `extra key at "${path.join(".")}"`).toBe(false);
      }
    });

    it("when it is not an object", () => {
      for (const bad of [null, undefined, 4, "x", true, [], [value]]) {
        expect(isSettingsView(bad)).toBe(false);
      }
    });
  });
});

describe("semantic checks", () => {
  const win = clone(windows);
  const mac = clone(macos);

  it("rejects unknown enum values", () => {
    expect(isSettingsView(mutate(win, ["platform"], "linux"))).toBe(false);
    expect(isSettingsView(mutate(win, ["platform"], "Windows"))).toBe(false);
    expect(isSettingsView(mutate(win, ["keys", "tools"], "win"))).toBe(false);
    expect(isSettingsView(mutate(win, ["keys", "tools"], "Ctrl"))).toBe(false);
    expect(isSettingsView(mutate(win, ["keyChoices", 0, "key"], "capsLock"))).toBe(false);
    expect(isSettingsView(mutate(win, ["output", "kind"], "nextToOriginal"))).toBe(false);
    expect(isSettingsView(mutate(mac, ["output", "kind"], "Folder"))).toBe(false);
    // Rust's ids, not the store's snake case or the display name.
    expect(isSettingsView(mutate(win, ["terminal", "choice"], "windows_terminal"))).toBe(false);
    expect(isSettingsView(mutate(win, ["terminal", "choice"], "PowerShell"))).toBe(false);
    expect(isSettingsView(mutate(win, ["terminal", "choices", 1, "id"], "cmd"))).toBe(false);
    expect(isSettingsView(mutate(mac, ["terminal", "choices", 0, "id"], "iTerm2"))).toBe(false);
  });

  it("rejects two wheels on one key", () => {
    expect(isSettingsView(mutate(win, ["keys", "actions"], "shift"))).toBe(false);
    expect(isSettingsView(mutate(mac, ["keys", "tools"], "meta"))).toBe(false);
  });

  it("rejects a key that isn't offered", () => {
    // Windows offers no "meta".
    expect(isSettingsView(mutate(win, ["keys", "actions"], "meta"))).toBe(false);
    // macOS without its Command choice, while Conversions uses it.
    const choices = get(mac, ["keyChoices"]) as Json[];
    expect(isSettingsView(mutate(mac, ["keyChoices"], choices.slice(0, 3)))).toBe(false);
  });

  it("accepts the choices in any order, and an unused one missing", () => {
    const choices = get(mac, ["keyChoices"]) as Json[];
    expect(isSettingsView(mutate(mac, ["keyChoices"], [...choices].reverse()))).toBe(true);
    // Option (alt) is offered but unused.
    const withoutOption = choices.filter((_, index) => index !== 2);
    expect(isSettingsView(mutate(mac, ["keyChoices"], withoutOption))).toBe(true);
  });

  it("rejects empty or repeated key choices and empty names", () => {
    expect(isSettingsView(mutate(win, ["keyChoices"], []))).toBe(false);
    const choices = get(win, ["keyChoices"]) as Json[];
    expect(isSettingsView(mutate(win, ["keyChoices"], [...choices, choices[0] ?? null]))).toBe(
      false,
    );
    expect(
      isSettingsView(mutate(win, ["keyChoices", 1], { key: "shift", name: "Shift again" })),
    ).toBe(false);
    expect(isSettingsView(mutate(win, ["keyChoices", 1, "name"], ""))).toBe(false);
  });

  it("rejects a terminal app that isn't one of its choices", () => {
    // Known ids, but not offered here.
    expect(isSettingsView(mutate(win, ["terminal", "choice"], "iterm2"))).toBe(false);
    expect(isSettingsView(mutate(mac, ["terminal", "choice"], "powershell"))).toBe(false);
    // macOS without its iTerm2 choice, while iTerm2 is picked.
    const choices = get(mac, ["terminal", "choices"]) as Json[];
    expect(isSettingsView(mutate(mac, ["terminal", "choices"], choices.slice(0, 1)))).toBe(false);
  });

  it("accepts the terminal choices in any order, and one alone", () => {
    const choices = get(win, ["terminal", "choices"]) as Json[];
    expect(isSettingsView(mutate(win, ["terminal", "choices"], [...choices].reverse()))).toBe(true);
    expect(isSettingsView(mutate(win, ["terminal", "choices"], choices.slice(0, 1)))).toBe(true);
    expect(isSettingsView(mutate(win, ["terminal", "choice"], "powershell"))).toBe(true);
  });

  it("rejects empty or repeated terminal choices and empty names", () => {
    expect(isSettingsView(mutate(win, ["terminal", "choices"], []))).toBe(false);
    const choices = get(win, ["terminal", "choices"]) as Json[];
    expect(
      isSettingsView(mutate(win, ["terminal", "choices"], [...choices, choices[0] ?? null])),
    ).toBe(false);
    expect(
      isSettingsView(
        mutate(win, ["terminal", "choices", 1], { id: "windowsTerminal", name: "Windows Terminal (again)" }),
      ),
    ).toBe(false);
    expect(isSettingsView(mutate(win, ["terminal", "choices", 1, "name"], ""))).toBe(false);
    expect(isSettingsView(mutate(win, ["terminal", "choices"], {}))).toBe(false);
  });

  it("takes welcome only as a bool", () => {
    for (const bad of [null, "true", 1, {}]) {
      expect(isSettingsView(mutate(win, ["welcome"], bad)), String(bad)).toBe(false);
    }
    expect(isSettingsView(mutate(win, ["welcome"], false))).toBe(true);
    expect(isSettingsView(mutate(mac, ["welcome"], true))).toBe(true);
  });

  it("rejects output fields that belong to the other kind", () => {
    expect(
      isSettingsView(mutate(win, ["output"], { kind: "nextToSource", path: "C:\\out" })),
    ).toBe(false);
    expect(isSettingsView(mutate(win, ["output"], { kind: "folder" }))).toBe(false);
    expect(isSettingsView(mutate(win, ["output"], { kind: "folder", path: "C:\\out" }))).toBe(true);
  });

  it("rejects empty paths", () => {
    expect(isSettingsView(mutate(mac, ["output", "path"], ""))).toBe(false);
    expect(isSettingsView(mutate(win, ["fallbackDir", "path"], ""))).toBe(false);
  });

  it("accepts each nullable field as a bool or null, never missing", () => {
    for (const field of ["haptics", "launchAtLogin"]) {
      for (const ok of [true, false, null]) {
        expect(isSettingsView(mutate(win, [field], ok)), `${field}: ${String(ok)}`).toBe(true);
      }
      expect(isSettingsView(mutate(win, [field], undefined))).toBe(false);
    }
    expect(isSettingsView(mutate(win, ["note"], "Alt now opens Tools, and Ctrl opens File actions."))).toBe(
      true,
    );
    expect(isSettingsView(mutate(mac, ["note"], null))).toBe(true);
  });
});

describe("isKeyId", () => {
  it("knows the four key ids and nothing else", () => {
    for (const key of ["shift", "ctrl", "alt", "meta"]) {
      expect(isKeyId(key)).toBe(true);
    }
    for (const bad of ["Shift", "win", "", null, 1, undefined]) {
      expect(isKeyId(bad)).toBe(false);
    }
  });
});

describe("isTerminalId", () => {
  it("knows the four terminal ids and nothing else", () => {
    for (const id of ["windowsTerminal", "powershell", "terminal", "iterm2"]) {
      expect(isTerminalId(id)).toBe(true);
    }
    for (const bad of ["windows_terminal", "iTerm2", "cmd", "", null, 1, undefined]) {
      expect(isTerminalId(bad)).toBe(false);
    }
  });
});

describe("copy", () => {
  it("labels the wheels in page order", () => {
    expect(WHEELS).toEqual(["conversions", "tools", "actions"]);
    expect(WHEELS.map((wheel) => WHEEL_LABELS[wheel])).toEqual([
      "Conversions",
      "Tools",
      "File actions",
    ]);
  });

  it("names where Grape lives on each platform", () => {
    expect(leadText("windows")).toBe("Grape runs in the notification area. Changes apply right away.");
    expect(leadText("macos")).toBe("Grape runs in the menu bar. Changes apply right away.");
  });

  it("shows Rust's own error sentence, or a generic one", () => {
    expect(errorText("Couldn't save your settings.")).toBe("Couldn't save your settings.");
    expect(errorText("That key can't open a wheel here.")).toBe("That key can't open a wheel here.");
    for (const other of ["", "   ", new Error("no ipc"), undefined, null, 4, { message: "x" }]) {
      expect(errorText(other)).toBe(GENERIC_ERROR);
    }
  });
});

describe("toggleRows", () => {
  it("shows sounds and launch at login on Windows, with no haptics row", () => {
    expect(toggleRows(windows)).toEqual([
      {
        name: "sounds",
        label: "Sounds",
        detail: "A soft tick as you move between choices.",
        on: true,
        enabled: true,
      },
      {
        name: "launchAtLogin",
        label: "Launch at login",
        detail: "Start Grape when you sign in.",
        on: true,
        enabled: true,
      },
    ]);
  });

  it("shows haptics on macOS, and an unreadable launch state as off and unusable", () => {
    const rows = toggleRows(macos);
    expect(rows.map((row) => row.name)).toEqual(["sounds", "haptics", "launchAtLogin"]);
    expect(rows[0]).toMatchObject({ on: false, enabled: true });
    expect(rows[1]).toMatchObject({ label: "Haptics", on: true, enabled: true });
    expect(rows[2]).toMatchObject({ detail: UNKNOWN_STATE, on: false, enabled: false });
    expect(UNKNOWN_STATE).toBe("Couldn't check");
  });

  it("keeps haptics separate from sounds", () => {
    const rows = toggleRows({ ...macos, sounds: true, haptics: false });
    expect(rows.map((row) => [row.name, row.on])).toEqual([
      ["sounds", true],
      ["haptics", false],
      ["launchAtLogin", false],
    ]);
  });
});

describe("outputFolder", () => {
  it("is the folder, or null next to the original", () => {
    expect(outputFolder(windows)).toBeNull();
    expect(outputFolder(macos)).toBe("/Users/max/Pictures/Grape out");
  });
});

describe("terminalHint", () => {
  it("names this OS's fallback app", () => {
    expect(terminalHint("windows")).toBe(
      "Drop a file on Terminal to open its folder in the app you pick. If Windows Terminal can't start, PowerShell opens instead.",
    );
    expect(terminalHint("macos")).toBe(
      "Drop a file on Terminal to open its folder in the app you pick. If iTerm2 isn't installed, Terminal opens instead.",
    );
  });
});

describe("keyNames", () => {
  it("names each wheel's key as this OS spells it", () => {
    expect(keyNames(windows)).toEqual({ conversions: "Shift", tools: "Ctrl", actions: "Alt" });
    expect(keyNames(macos)).toEqual({ conversions: "Command", tools: "Shift", actions: "Control" });
  });

  it("follows the current keys and names", () => {
    const view: SettingsView = {
      ...windows,
      keys: { conversions: "alt", tools: "shift", actions: "ctrl" },
      keyChoices: [
        { key: "shift", name: "Left Shift" },
        { key: "ctrl", name: "Ctrl" },
        { key: "alt", name: "Right Alt" },
      ],
    };
    expect(keyNames(view)).toEqual({ conversions: "Right Alt", tools: "Left Shift", actions: "Ctrl" });
  });
});

describe("welcome copy", () => {
  it("is Grape's own heading, intro and button", () => {
    expect(WELCOME_COPY).toEqual({
      heading: "Welcome to Grape",
      intro: "Grape works while you drag a file, in three moves.",
      done: "Got it",
    });
  });

  it("walks through drag, tap and drop on Windows, with its keys and File Explorer", () => {
    expect(welcomeSteps("windows", keyNames(windows))).toEqual([
      {
        art: "drag",
        title: "Drag a file",
        detail: "Start dragging any file in File Explorer or on the desktop.",
      },
      { art: "tap", title: "Tap Shift", detail: "Ctrl for tools, Alt for file actions." },
      { art: "drop", title: "Drop it on a segment", detail: "Drop in the middle to cancel." },
    ]);
  });

  it("names Finder and the Mac keys on macOS", () => {
    const steps = welcomeSteps("macos", keyNames(macos));
    expect(steps.map((step) => step.art)).toEqual(["drag", "tap", "drop"]);
    expect(steps[0]?.detail).toBe("Start dragging any file in Finder or on the desktop.");
    expect(steps[1]).toEqual({
      art: "tap",
      title: "Tap Command",
      detail: "Shift for tools, Control for file actions.",
    });
    expect(steps[2]?.title).toBe("Drop it on a segment");
  });

  it("uses the names as they are, never as patterns", () => {
    const steps = welcomeSteps("windows", { conversions: "$&", tools: "{tools}", actions: "$1" });
    expect(steps[1]).toMatchObject({ title: "Tap $&", detail: "{tools} for tools, $1 for file actions." });
  });
});

describe("choicesSignature", () => {
  it("changes when a key or a name changes, not otherwise", () => {
    const base = choicesSignature(windows.keyChoices);
    expect(choicesSignature(windows.keyChoices.map((choice) => ({ ...choice })))).toBe(base);
    expect(choicesSignature(macos.keyChoices)).not.toBe(base);
    expect(
      choicesSignature([{ key: "shift", name: "Left Shift" }, ...windows.keyChoices.slice(1)]),
    ).not.toBe(base);
  });
});

describe("terminalSignature", () => {
  it("changes when an id or a name changes, not otherwise", () => {
    const base = terminalSignature(windows.terminal.choices);
    expect(terminalSignature(windows.terminal.choices.map((choice) => ({ ...choice })))).toBe(base);
    expect(terminalSignature(macos.terminal.choices)).not.toBe(base);
    expect(terminalSignature([...windows.terminal.choices].reverse())).not.toBe(base);
    expect(
      terminalSignature([{ id: "windowsTerminal", name: "Terminal" }, ...windows.terminal.choices.slice(1)]),
    ).not.toBe(base);
  });
});
