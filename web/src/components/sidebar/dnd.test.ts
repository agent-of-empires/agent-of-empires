// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook } from "@testing-library/react";

import type { CollisionDetection } from "@dnd-kit/core";

import {
  dragEnded,
  dragStarted,
  newDragClickGuard,
  typedClosestCenter,
  useReleaseDragOnUnmount,
  useSuppressClickAfterDrag,
} from "./dnd";

type Step = "start" | "end" | "press" | "click" | "key click" | "wait 1s" | "list unmounts";

beforeEach(() => vi.useFakeTimers());
afterEach(() => vi.useRealTimers());

describe("drag click guard", () => {
  it("swallows clicks during a drag and the release click only", () => {
    // (steps, which clicks reach the page, in order)
    const cases: [Step[], boolean[]][] = [
      [["click"], [true]],
      [["start", "click"], [false]],
      // The release click arrives with no press after pointer-up.
      [["start", "end", "click"], [false]],
      [
        ["start", "end", "click", "click"],
        [false, true],
      ],
      // A deliberate click right after a drop starts with a press.
      [["start", "end", "press", "click"], [true]],
      // A browser that sends no release click can't swallow a later one.
      [["start", "end", "wait 1s", "click"], [true]],
      // A press during a drag doesn't let its click through.
      [["start", "press", "click"], [false]],
      // Enter on a focused control, or `element.click()`, never presses.
      [["start", "end", "key click"], [true]],
      // The drag's list goes away before any end or cancel.
      [["start", "list unmounts", "click"], [false]],
      [["start", "list unmounts", "press", "click"], [true]],
      [
        ["start", "list unmounts", "click", "click"],
        [false, true],
      ],
      [["list unmounts", "click"], [true]],
    ];
    for (const [steps, want] of cases) {
      const ref = { current: newDragClickGuard() };
      const { unmount } = renderHook(() => useSuppressClickAfterDrag(ref));
      const list = renderHook(() => useReleaseDragOnUnmount(ref));
      const button = document.body.appendChild(document.createElement("button"));
      const reached: boolean[] = [];
      for (const step of steps) {
        if (step === "start") dragStarted(ref.current);
        else if (step === "end") dragEnded(ref.current);
        else if (step === "press") button.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
        else if (step === "wait 1s") vi.advanceTimersByTime(1000);
        else if (step === "list unmounts") list.unmount();
        else {
          let got = false;
          button.addEventListener("click", () => (got = true), { once: true });
          const detail = step === "click" ? 1 : 0;
          button.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true, detail }));
          reached.push(got);
        }
      }
      expect(reached, steps.join(" ")).toEqual(want);
      button.remove();
      unmount();
    }
  });
});

describe("typedClosestCenter", () => {
  it("targets the closest droppable of the dragged type in its own list, and none once the pointer leaves it", () => {
    // Rows of lists A and B and one header, top to bottom; A's first row, or the header, is dragged.
    const items = [
      ["a1", "workspace", "A", 0],
      ["a2", "workspace", "A", 20],
      ["header", "group", "repos", 40],
      ["b1", "workspace", "B", 60],
    ] as const;
    const rect = (top: number) => ({ top, left: 0, width: 100, height: 20, bottom: top + 20, right: 100 });
    const droppableContainers = items.map(([id, type, list]) => ({
      id,
      data: { current: { type, sortable: { containerId: list } } },
    }));
    const target = (y: number, active = droppableContainers[0]) =>
      typedClosestCenter({
        active,
        collisionRect: rect(y - 10),
        droppableRects: new Map(items.map(([id, , , top]) => [id, rect(top)])),
        droppableContainers,
        pointerCoordinates: { x: 50, y },
      } as unknown as Parameters<CollisionDetection>[0])[0]?.id;
    expect(target(34)).toBe("a2");
    // Over the header or B's row, both closer to A's last row than anything else of A's.
    expect(target(48)).toBeUndefined();
    expect(target(68)).toBeUndefined();
    // No other list holds headers, so a header dragged anywhere has its one list to land in.
    expect(target(80, droppableContainers[2])).toBe("header");
  });
});
