/** Anything that can look elements up by id: a `Document`, or a stub in tests. */
export type IdLookup = Pick<Document, "getElementById">;

/**
 * Returns the element with this id, or throws if the page doesn't have it.
 *
 * Use this instead of a bare `getElementById` or `querySelector`, so every
 * DOM lookup is null-checked and a missing element fails loudly in one place.
 */
export function mustGetElementById(root: IdLookup, id: string): HTMLElement {
  const element = root.getElementById(id);
  if (element === null) {
    throw new Error(`Grape UI: the page has no element with id "${id}"`);
  }
  return element;
}
