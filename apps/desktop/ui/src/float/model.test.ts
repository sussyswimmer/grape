import { describe, expect, it } from "vitest";
import type { FloatView } from "../shared/events";
import { floatFixture } from "../test-support/fixtures";
import {
  DRAG_FALLBACK_MS,
  DRAG_THRESHOLD,
  displayName,
  dragAllText,
  dragIds,
  dragOver,
  focusAfterRemoval,
  type Gesture,
  hasFooter,
  IDLE,
  move,
  press,
  release,
  removeLabel,
  titleText,
} from "./model";

// photo.jpg (1, file), Invoices (2, folder), notes.txt (3, file, missing).
const shelf = floatFixture("float-view.json");
const empty = floatFixture("float-view-empty.json");

const PRIMARY = 1;

/** A press on item 1 at (10, 10) with pointer 7. */
function pressed(view: FloatView = shelf): Gesture {
  return press(IDLE, view, { kind: "item", id: 1 }, 7, 10, 10);
}

describe("dragIds", () => {
  it("drags the row's own item", () => {
    expect(dragIds(shelf, { kind: "item", id: 1 })).toEqual([1]);
    expect(dragIds(shelf, { kind: "item", id: 2 })).toEqual([2]);
  });

  it("never drags a missing item or one that isn't listed", () => {
    expect(dragIds(shelf, { kind: "item", id: 3 })).toEqual([]);
    expect(dragIds(shelf, { kind: "item", id: 9 })).toEqual([]);
  });

  it("drags every item that's still there for Drag all, in shelf order", () => {
    expect(dragIds(shelf, { kind: "all" })).toEqual([1, 2]);
    const reversed = { items: [...shelf.items].reverse() };
    expect(dragIds(reversed, { kind: "all" })).toEqual([2, 1]);
    expect(dragIds(empty, { kind: "all" })).toEqual([]);
  });
});

describe("press", () => {
  it("starts a press from idle on something draggable", () => {
    expect(pressed()).toEqual({
      kind: "pressed",
      source: { kind: "item", id: 1 },
      pointerId: 7,
      x: 10,
      y: 10,
    });
    expect(press(IDLE, shelf, { kind: "all" }, 1, 0, 0).kind).toBe("pressed");
  });

  it("does nothing on a missing row, or on Drag all with nothing to drag", () => {
    expect(press(IDLE, shelf, { kind: "item", id: 3 }, 1, 0, 0)).toBe(IDLE);
    expect(press(IDLE, empty, { kind: "all" }, 1, 0, 0)).toBe(IDLE);
    const allMissing = { items: shelf.items.map((item) => ({ ...item, missing: true })) };
    expect(press(IDLE, allMissing, { kind: "all" }, 1, 0, 0)).toBe(IDLE);
  });

  it("is ignored while a press or a drag is in progress", () => {
    const first = pressed();
    expect(press(first, shelf, { kind: "item", id: 2 }, 8, 0, 0)).toBe(first);
    const dragging: Gesture = { kind: "dragging" };
    expect(press(dragging, shelf, { kind: "item", id: 1 }, 7, 0, 0)).toBe(dragging);
  });
});

describe("move", () => {
  it(`stays a press until it has moved ${DRAG_THRESHOLD} px`, () => {
    const start = pressed();
    expect(move(start, shelf, 7, 13, 12, PRIMARY)).toEqual({ gesture: start, drag: null });
    expect(move(start, shelf, 7, 10, 13.9, PRIMARY)).toEqual({ gesture: start, drag: null });
  });

  it("becomes a drag of the row's item at the threshold", () => {
    expect(move(pressed(), shelf, 7, 14, 10, PRIMARY)).toEqual({
      gesture: { kind: "dragging" },
      drag: [1],
    });
    // Diagonal: 3 px across and 3 down is more than 4 px.
    expect(move(pressed(), shelf, 7, 13, 13, PRIMARY).drag).toEqual([1]);
    expect(move(pressed(), shelf, 7, 6, 10, PRIMARY | 2).drag).toEqual([1]);
  });

  it("drags what's there when it crosses the threshold, not what was there at the press", () => {
    const start = press(IDLE, shelf, { kind: "all" }, 7, 10, 10);
    const later = { items: shelf.items.filter((item) => item.id !== 2) };
    expect(move(start, later, 7, 30, 10, PRIMARY).drag).toEqual([1]);
    const gone = { items: shelf.items.filter((item) => item.id !== 1) };
    expect(move(pressed(), gone, 7, 30, 10, PRIMARY)).toEqual({ gesture: IDLE, drag: null });
  });

  it("ends the press when the primary button is no longer held", () => {
    expect(move(pressed(), shelf, 7, 30, 10, 0)).toEqual({ gesture: IDLE, drag: null });
    expect(move(pressed(), shelf, 7, 10, 10, 2)).toEqual({ gesture: IDLE, drag: null });
  });

  it("ignores other pointers, and moves while idle or dragging", () => {
    const start = pressed();
    expect(move(start, shelf, 8, 40, 40, PRIMARY)).toEqual({ gesture: start, drag: null });
    expect(move(IDLE, shelf, 7, 40, 40, PRIMARY)).toEqual({ gesture: IDLE, drag: null });
    const dragging: Gesture = { kind: "dragging" };
    expect(move(dragging, shelf, 7, 40, 40, 0)).toEqual({ gesture: dragging, drag: null });
  });
});

describe("release and dragOver", () => {
  it("pointerup or pointercancel ends a press on that pointer only", () => {
    const start = pressed();
    expect(release(start, 7)).toBe(IDLE);
    expect(release(start, 8)).toBe(start);
    expect(release(IDLE, 7)).toBe(IDLE);
  });

  it("never ends a drag on a pointer event: only drag-ended or the fallback does", () => {
    const dragging: Gesture = { kind: "dragging" };
    expect(release(dragging, 7)).toBe(dragging);
    expect(dragOver(dragging)).toBe(IDLE);
    const start = pressed();
    expect(dragOver(start)).toBe(start);
    expect(dragOver(IDLE)).toBe(IDLE);
  });

  it("waits ten seconds for drag-ended before giving up", () => {
    expect(DRAG_FALLBACK_MS).toBe(10_000);
  });
});

describe("focusAfterRemoval", () => {
  it("moves to the next row that's still there", () => {
    expect(focusAfterRemoval([1, 2, 3], [1, 3], 2)).toBe(3);
    expect(focusAfterRemoval([1, 2, 3, 4], [1, 4], 2)).toBe(4);
  });

  it("falls back to the previous row, then to the close button", () => {
    expect(focusAfterRemoval([1, 2, 3], [1, 2], 3)).toBe(2);
    expect(focusAfterRemoval([1, 2, 3], [1], 3)).toBe(1);
    expect(focusAfterRemoval([1], [], 1)).toBeNull();
    expect(focusAfterRemoval([1, 2], [], 2)).toBeNull();
  });

  it("keeps the focus on a row that's still listed", () => {
    expect(focusAfterRemoval([1, 2, 3], [1, 2, 3, 4], 2)).toBe(2);
  });

  it("goes to the first row when the focused id was never listed", () => {
    expect(focusAfterRemoval([1, 2], [2, 5], 9)).toBe(2);
    expect(focusAfterRemoval([], [], 9)).toBeNull();
  });
});

describe("copy", () => {
  it("counts the items in the title once there are some", () => {
    expect(titleText(0)).toBe("Shelf");
    expect(titleText(1)).toBe("Shelf · 1");
    expect(titleText(50)).toBe("Shelf · 50");
  });

  it("names each row's × after its item", () => {
    expect(removeLabel("photo.jpg")).toBe("Remove photo.jpg");
    expect(removeLabel("")).toBe("Remove Unnamed item");
    expect(displayName("Invoices")).toBe("Invoices");
    expect(displayName("")).toBe("Unnamed item");
  });

  it("counts what Drag all will take", () => {
    expect(dragAllText(2)).toBe("Drag all 2");
    expect(dragAllText(0)).toBe("Nothing to drag");
  });

  it("shows the Drag all row from two items, as Rust's window height does", () => {
    expect(hasFooter(0)).toBe(false);
    expect(hasFooter(1)).toBe(false);
    expect(hasFooter(2)).toBe(true);
    expect(hasFooter(50)).toBe(true);
  });
});
