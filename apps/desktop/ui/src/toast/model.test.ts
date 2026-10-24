import { describe, expect, it } from "vitest";
import { toastFixture } from "../test-support/fixtures";
import { applyHide, applyShow, clickTarget, progressPercent } from "./model";

const working = toastFixture("toast-show-working.json"); // id 4, not revealable
const done = toastFixture("toast-show-done.json"); // id 4, revealable

describe("applyShow", () => {
  it("replaces whatever is shown", () => {
    expect(applyShow(null, working)).toBe(working);
    expect(applyShow(working, done)).toBe(done);
    const other = { ...done, id: 5 };
    expect(applyShow(done, other)).toBe(other);
  });
});

describe("applyHide", () => {
  it("hides the toast it names", () => {
    expect(applyHide(done, { id: 4 })).toBeNull();
  });

  it("keeps a newer toast", () => {
    const newer = { ...working, id: 5 };
    expect(applyHide(newer, { id: 4 })).toBe(newer);
    expect(applyHide(null, { id: 4 })).toBeNull();
  });
});

describe("clickTarget", () => {
  it("reports the id only for a revealable toast", () => {
    expect(clickTarget(done)).toBe(4);
    expect(clickTarget(working)).toBeNull();
    expect(clickTarget(null)).toBeNull();
  });
});

describe("progressPercent", () => {
  it("turns a fraction into a percentage with one decimal", () => {
    expect(progressPercent(null)).toBeNull();
    expect(progressPercent(0)).toBe(0);
    expect(progressPercent(0.5)).toBe(50);
    expect(progressPercent(1 / 3)).toBe(33.3);
    expect(progressPercent(1)).toBe(100);
  });

  it("clamps out-of-range values and drops non-finite ones", () => {
    expect(progressPercent(-0.2)).toBe(0);
    expect(progressPercent(1.7)).toBe(100);
    expect(progressPercent(Number.NaN)).toBeNull();
  });
});
