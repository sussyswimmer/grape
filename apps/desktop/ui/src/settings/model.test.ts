import { describe, expect, it } from "vitest";
import { clone, get, type Json, mutate, objectPaths, paths, wrongType } from "../test-support/json";
import { settingsFixture } from "../test-support/fixtures";
import {
  choicesSignature,
  errorText,
  GENERIC_ERROR,
  isKeyId,
  isSettingsView,
  leadText,
  outputFolder,
  SETTINGS_GET,
  SETTINGS_PICK_FOLDER,
  SETTINGS_SET_KEY,
  SETTINGS_SET_OUTPUT_NEXT_TO_SOURCE,
  SETTINGS_SET_TOGGLE,
  SETTINGS_USE_DEFAULT_FALLBACK,
  toggleRows,
  UNKNOWN_STATE,
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
  });
});

describe("malformed views fail", () => {
  describe.each(NAMES)("%s", (name) => {
    const value = clone(JSON.parse(text(name)));

    it.each(paths(value).map((path) => [path.join(".")] as const))("without %s", (dotted) => {
      const path = paths(value).find((candidate) => candidate.join(".") === dotted);
      if (path === undefined) throw new Error(dotted);
      // Removing a key choice is a semantic case (below), not a missing field.
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
