/**
 * Building blocks for the runtime guards in `events.ts` and
 * `settings/model.ts`. Everything Rust sends arrives as `unknown`; these
 * check its shape without trusting it.
 */

/** A plain object's fields, still unchecked. */
export type Fields = Readonly<Record<string, unknown>>;

/**
 * True when `value` is a plain object with exactly these keys: none
 * missing, none extra. The Rust structs serialize every field (an `Option`
 * becomes `null`), so an unknown key means the two sides drifted apart.
 */
export function hasExactKeys(value: unknown, keys: readonly string[]): value is Fields {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return false;
  }
  const own = Object.keys(value);
  return own.length === keys.length && keys.every((key) => Object.hasOwn(value, key));
}

/** True when `value` is one of these strings. */
export function isOneOf<T extends string>(value: unknown, options: readonly T[]): value is T {
  return typeof value === "string" && (options as readonly string[]).includes(value);
}
