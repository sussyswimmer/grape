/**
 * Helpers for guard tests: walk a parsed fixture and make broken copies of
 * it (a field removed, retyped or replaced). Test code only.
 */

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
export type Path = readonly (string | number)[];

/** A deep copy of a JSON value. */
export function clone(value: unknown): Json {
  return JSON.parse(JSON.stringify(value)) as Json;
}

/** Every path to a value inside `value`, depth first (not the root itself). */
export function paths(value: Json, prefix: Path = []): Path[] {
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
export function objectPaths(value: Json): Path[] {
  return [[], ...paths(value)].filter((path) => {
    const target = get(value, path);
    return target !== null && typeof target === "object" && !Array.isArray(target);
  });
}

export function get(value: Json, path: Path): Json {
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
export function mutate(root: unknown, path: Path, replacement: unknown): unknown {
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
export function wrongType(value: Json): unknown {
  if (value === null) return {};
  if (typeof value === "number") return String(value);
  if (typeof value === "string") return 7;
  if (typeof value === "boolean") return String(value);
  return "not an object";
}
