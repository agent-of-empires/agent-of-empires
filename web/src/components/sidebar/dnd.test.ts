// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook } from "@testing-library/react";

import { dragEnded, dragStarted, newDragClickGuard, useSuppressClickAfterDrag } from "./dnd";

type Step = "start" | "end" | "press" | "click" | "key click" | "wait 1s";

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
    ];
    for (const [steps, want] of cases) {
      const ref = { current: newDragClickGuard() };
      const { unmount } = renderHook(() => useSuppressClickAfterDrag(ref));
      const button = document.body.appendChild(document.createElement("button"));
      const reached: boolean[] = [];
      for (const step of steps) {
        if (step === "start") dragStarted(ref.current);
        else if (step === "end") dragEnded(ref.current);
        else if (step === "press") button.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
        else if (step === "wait 1s") vi.advanceTimersByTime(1000);
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
