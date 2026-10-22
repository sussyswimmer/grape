import { describe, expect, it } from "vitest";
import {
  isToastHidePayload,
  isToastShowPayload,
  isWheelClosePayload,
  isWheelHighlightPayload,
  isWheelOpenPayload,
  TOAST_CLICKED,
  TOAST_HIDE,
  TOAST_SHOW,
  WHEEL_CLOSE,
  WHEEL_HIGHLIGHT,
  WHEEL_OPEN,
  WHEEL_PAINTED,
} from "./events";

// The golden payloads from docs/design/m1-drag-wheel.md §C. The Rust side
// `include_str!`s the same files, so both suites test one contract.
const raw = import.meta.glob<string>("./fixtures/*.json", {
  query: "?raw",
  import: "default",
  eager: true,
});

function fixture(name: string): string {
  const text = raw[`./fixtures/${name}`];
  if (text === undefined) {
    throw new Error(`missing fixture ${name}`);
  }
  return text;
}

function parsed(name: string): unknown {
  return JSON.parse(fixture(name));
}

type Guard = (value: unknown) => boolean;

const GUARDS: Readonly<Record<string, Guard>> = {
  isWheelOpenPayload,
  isWheelHighlightPayload,
  isWheelClosePayload,
  isToastShowPayload,
  isToastHidePayload,
};

/** Each fixture and the one guard that must accept it. */
const CASES: readonly (readonly [string, string])[] = [
  ["wheel-open-pending.json", "isWheelOpenPayload"],
  ["wheel-open-entries.json", "isWheelOpenPayload"],
  ["wheel-open-empty.json", "isWheelOpenPayload"],
  ["wheel-highlight.json", "isWheelHighlightPayload"],
  ["wheel-highlight-null.json", "isWheelHighlightPayload"],
  ["wheel-close.json", "isWheelClosePayload"],
  ["toast-show-working.json", "isToastShowPayload"],
  ["toast-show-done.json", "isToastShowPayload"],
  ["toast-hide.json", "isToastHidePayload"],
];

function guard(name: string): Guard {
  const found = GUARDS[name];
  if (found === undefined) {
    throw new Error(`no guard ${name}`);
  }
  return found;
}

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type Path = readonly (string | number)[];

function clone(value: unknown): Json {
  return JSON.parse(JSON.stringify(value)) as Json;
}

/** Every path to a value inside `value`, depth first (not the root itself). */
function paths(value: Json, prefix: Path = []): Path[] {
  const out: Path[] = [];
  if (Array.isArray(value)) {
    value.forEach((item, index) => {
      out.push([...prefix, index], ...paths(item, [...prefix, index]));
    });
  } else if (value !== null && typeof value === "object") {
    for (const [key, item] of Object.entries(value)) {
      out.push([...prefix, key], ...paths(item, [...prefix, key]));
    }
  }
  return out;
}

/** Every object (including the root) inside `value`, as paths. */
function objectPaths(value: Json): Path[] {
  return [[], ...paths(value)].filter((path) => {
    const target = get(value, path);
    return target !== null && typeof target === "object" && !Array.isArray(target);
  });
}

function get(value: Json, path: Path): Json {
  let node: Json = value;
  for (const step of path) {
    if (node === null || typeof node !== "object") {
      throw new Error(`bad path ${path.join(".")}`);
    }
    const next: Json | undefined = Array.isArray(node)
      ? node[Number(step)]
      : (node as { [key: string]: Json })[String(step)];
    if (next === undefined) {
      throw new Error(`bad path ${path.join(".")}`);
    }
    node = next;
  }
  return node;
}

/** A copy of `root` with the value at `path` replaced (or deleted when `replacement` is undefined). */
function mutate(root: unknown, path: Path, replacement: unknown): unknown {
  const copy = clone(root);
  const parentPath = path.slice(0, -1);
  const last = path[path.length - 1];
  const parent = get(copy, parentPath) as unknown as Record<string, unknown>;
  if (last === undefined) {
    throw new Error("empty path");
  }
  if (replacement === undefined) {
    delete parent[String(last)];
  } else {
    parent[String(last)] = replacement;
  }
  return copy;
}

/** A value of a different JSON type, so the field's type check must fail. */
function wrongType(value: Json): unknown {
  if (value === null) return {};
  if (typeof value === "number") return String(value);
  if (typeof value === "string") return 7;
  if (typeof value === "boolean") return String(value);
  return "not an object";
}

describe("event names", () => {
  it("match the contract", () => {
    expect(WHEEL_OPEN).toBe("wheel://open");
    expect(WHEEL_HIGHLIGHT).toBe("wheel://highlight");
    expect(WHEEL_CLOSE).toBe("wheel://close");
    expect(TOAST_SHOW).toBe("toast://show");
    expect(TOAST_HIDE).toBe("toast://hide");
    expect(WHEEL_PAINTED).toBe("wheel_painted");
    expect(TOAST_CLICKED).toBe("toast_clicked");
  });
});

describe("golden fixtures", () => {
  it("are exactly the nine payloads from §C", () => {
    expect(Object.keys(raw).sort()).toEqual(CASES.map(([name]) => `./fixtures/${name}`).sort());
  });

  it.each(CASES)("%s is one line of JSON with a trailing newline", (name) => {
    const text = fixture(name);
    expect(text.endsWith("}\n")).toBe(true);
    expect(text.indexOf("\n")).toBe(text.length - 1);
    expect(text).not.toContain("\r");
    expect(text.startsWith("{")).toBe(true);
  });

  it.each(CASES)("%s passes %s and no other guard", (name, accepting) => {
    const value = parsed(name);
    for (const [guardName, check] of Object.entries(GUARDS)) {
      expect(check(value), `${guardName}(${name})`).toBe(guardName === accepting);
    }
  });

  it("keeps the toast title's ellipsis as one character", () => {
    const value = parsed("toast-show-working.json");
    expect(isToastShowPayload(value)).toBe(true);
    if (isToastShowPayload(value)) {
      expect(value.title).toBe("Making photo.png…");
    }
  });
});

describe("malformed variants fail", () => {
  describe.each(CASES)("%s", (name, accepting) => {
    const check = guard(accepting);
    const value = clone(parsed(name));

    it.each(paths(value).map((path) => [path.join(".")] as const))(
      "without %s",
      (dotted) => {
        const path = paths(value).find((candidate) => candidate.join(".") === dotted);
        if (path === undefined) throw new Error(dotted);
        const last = path[path.length - 1];
        // Removing an array element is a different variant (handled below).
        if (typeof last === "number") return;
        expect(check(mutate(value, path, undefined))).toBe(false);
      },
    );

    it.each(paths(value).map((path) => [path.join(".")] as const))(
      "with a wrong type at %s",
      (dotted) => {
        const path = paths(value).find((candidate) => candidate.join(".") === dotted);
        if (path === undefined) throw new Error(dotted);
        expect(check(mutate(value, path, wrongType(get(value, path))))).toBe(false);
      },
    );

    it("with an extra key on any object", () => {
      for (const path of objectPaths(value)) {
        const copy = clone(value);
        const target = get(copy, path) as { [key: string]: Json };
        target["extra"] = 1;
        expect(check(copy), `extra key at "${path.join(".")}"`).toBe(false);
      }
    });

    it("with NaN or ±Infinity at any number", () => {
      for (const path of paths(value)) {
        if (typeof get(value, path) !== "number") continue;
        for (const bad of [Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY]) {
          expect(check(mutate(value, path, bad)), `${String(bad)} at ${path.join(".")}`).toBe(
            false,
          );
        }
      }
    });

    it("when it is not an object", () => {
      for (const bad of [null, undefined, 4, "x", true, [], [value]]) {
        expect(check(bad)).toBe(false);
      }
    });
  });
});

describe("semantic checks", () => {
  const entries = clone(parsed("wheel-open-entries.json"));
  const pending = clone(parsed("wheel-open-pending.json"));

  it("rejects ids and seqs that are not non-negative integers", () => {
    for (const bad of [-1, 1.5, 2 ** 53]) {
      expect(isWheelOpenPayload(mutate(entries, ["seq"], bad))).toBe(false);
      expect(isWheelHighlightPayload({ seq: bad, index: 0 })).toBe(false);
      expect(isWheelHighlightPayload({ seq: 1, index: bad })).toBe(false);
      expect(isWheelClosePayload({ seq: bad, reason: "drop" })).toBe(false);
      expect(isToastHidePayload({ id: bad })).toBe(false);
    }
    expect(isWheelHighlightPayload({ seq: 0, index: 11 })).toBe(true);
  });

  it("rejects unknown enum values", () => {
    expect(isWheelOpenPayload(mutate(entries, ["mode"], "convert"))).toBe(false);
    expect(isWheelOpenPayload(mutate(entries, ["mode"], "Conversions"))).toBe(false);
    expect(
      isWheelOpenPayload(mutate(entries, ["content", "segments", 0, "entry", "kind"], "file")),
    ).toBe(false);
    expect(isWheelOpenPayload(mutate(pending, ["content", "kind"], "loading"))).toBe(false);
    expect(
      isWheelOpenPayload(
        mutate(pending, ["content"], { kind: "empty", state: "noActions" }),
      ),
    ).toBe(false);
    expect(isWheelClosePayload({ seq: 1, reason: "dropped" })).toBe(false);
    expect(isWheelClosePayload({ seq: 1, reason: "NoFiles" })).toBe(false);
    for (const reason of ["drop", "cancel", "escape", "noFiles", "dragEnded", "watchdog", "paused"]) {
      expect(isWheelClosePayload({ seq: 1, reason })).toBe(true);
    }
    const done = clone(parsed("toast-show-done.json"));
    expect(isToastShowPayload(mutate(done, ["tone"], "success"))).toBe(false);
    for (const tone of ["working", "done", "error", "info"]) {
      expect(isToastShowPayload(mutate(done, ["tone"], tone))).toBe(true);
    }
  });

  it("rejects grape-core's own WheelContent shape (kind + value)", () => {
    expect(
      isWheelOpenPayload(
        mutate(pending, ["content"], {
          kind: "entries",
          value: [{ kind: "convert", key: "png" }],
        }),
      ),
    ).toBe(false);
    expect(
      isWheelOpenPayload(mutate(pending, ["content"], { kind: "empty", value: "noTools" })),
    ).toBe(false);
  });

  it("rejects content fields that belong to another kind", () => {
    expect(
      isWheelOpenPayload(mutate(pending, ["content"], { kind: "pending", dense: false })),
    ).toBe(false);
    expect(
      isWheelOpenPayload(
        mutate(pending, ["content"], { kind: "empty", state: "noTools", segments: [] }),
      ),
    ).toBe(false);
  });

  it("rejects entries with no segments or with indexes out of order", () => {
    expect(isWheelOpenPayload(mutate(entries, ["content", "segments"], []))).toBe(false);
    expect(isWheelOpenPayload(mutate(entries, ["content", "segments", 0, "index"], 1))).toBe(
      false,
    );
    const segment = get(entries, ["content", "segments", 0]);
    const two = mutate(entries, ["content", "segments"], [segment, { ...(segment as object), index: 1 }]);
    expect(isWheelOpenPayload(two)).toBe(true);
    const swapped = mutate(entries, ["content", "segments"], [{ ...(segment as object), index: 1 }, segment]);
    expect(isWheelOpenPayload(swapped)).toBe(false);
  });

  it("rejects an empty entry key", () => {
    expect(
      isWheelOpenPayload(mutate(entries, ["content", "segments", 0, "entry", "key"], "")),
    ).toBe(false);
  });

  it("rejects geometry that would break the segment math", () => {
    const bad: [string, number][] = [
      ["innerR", 130], // inside-out ring
      ["outerR", 48], // zero-width ring
      ["backdropR", 100], // backdrop smaller than the ring
      ["gap", -1],
      ["gap", 100], // gap wider than the dead zone
      ["corner", -1],
      ["labelR", -1],
      ["screenMargin", -1],
      ["denseAt", 6.5],
    ];
    for (const [key, number] of bad) {
      expect(isWheelOpenPayload(mutate(entries, ["geometry", key], number)), key).toBe(false);
    }
  });

  it("rejects a non-positive scale", () => {
    expect(isWheelOpenPayload(mutate(entries, ["scale"], 0))).toBe(false);
    expect(isWheelOpenPayload(mutate(entries, ["scale"], -1.5))).toBe(false);
    expect(isWheelOpenPayload(mutate(entries, ["scale"], 2.25))).toBe(true);
  });

  it("accepts any key names, including empty or custom ones", () => {
    expect(isWheelOpenPayload(mutate(entries, ["keys", "actions"], "Caps"))).toBe(true);
    expect(isWheelOpenPayload(mutate(entries, ["keys", "tools"], "Right Ctrl"))).toBe(true);
  });

  it("accepts toast progress only as a fraction in 0..=1", () => {
    const working = clone(parsed("toast-show-working.json"));
    for (const ok of [0, 0.25, 1]) {
      expect(isToastShowPayload(mutate(working, ["progress"], ok))).toBe(true);
    }
    for (const bad of [-0.01, 1.01, 50]) {
      expect(isToastShowPayload(mutate(working, ["progress"], bad))).toBe(false);
    }
  });

  it("accepts a toast detail as text or null, never missing", () => {
    const working = clone(parsed("toast-show-working.json"));
    expect(isToastShowPayload(mutate(working, ["detail"], "photo.png"))).toBe(true);
    expect(isToastShowPayload(mutate(working, ["detail"], undefined))).toBe(false);
  });
});
