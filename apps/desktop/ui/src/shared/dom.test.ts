import { describe, expect, it } from "vitest";
import { type IdLookup, mustGetElementById } from "./dom";

// A stand-in for `document`: the tests run in Node, without a DOM.
function stubDocument(elements: Record<string, HTMLElement>): IdLookup {
  return {
    getElementById: (id: string) => elements[id] ?? null,
  };
}

function fakeElement(id: string): HTMLElement {
  return { id } as unknown as HTMLElement;
}

describe("mustGetElementById", () => {
  it("returns the element when it exists", () => {
    const wheel = fakeElement("wheel");
    const root = stubDocument({ wheel });
    expect(mustGetElementById(root, "wheel")).toBe(wheel);
  });

  it("throws and names the id when the element is missing", () => {
    const root = stubDocument({});
    expect(() => mustGetElementById(root, "wheel")).toThrow(/"wheel"/);
  });

  it("does not match a different id", () => {
    const root = stubDocument({ settings: fakeElement("settings") });
    expect(() => mustGetElementById(root, "wheel")).toThrow();
  });
});
