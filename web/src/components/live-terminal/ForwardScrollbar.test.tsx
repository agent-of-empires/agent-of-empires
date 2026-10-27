// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { ForwardScrollbar } from "./ForwardScrollbar";

const PAGE = 39;

function setup(disabled = false) {
  const scrollNotches = vi.fn();
  const cancelScroll = vi.fn();
  const { unmount } = render(
    <ForwardScrollbar
      disabled={disabled}
      pageNotches={() => PAGE}
      scrollNotches={scrollNotches}
      cancelScroll={cancelScroll}
    />,
  );
  const group = screen.getByRole("group", { name: "Scroll controls" });
  // Thumb travel is (track - 28) / 2 = 86px.
  const track = group.children[1] as HTMLElement;
  track.getBoundingClientRect = () => ({ height: 200 }) as DOMRect;
  return { unmount, scrollNotches, cancelScroll, thumb: screen.getByLabelText("Drag to scroll") };
}
const down = (y: number) => ({ button: 0, pointerId: 1, clientY: y });
const sum = (fn: ReturnType<typeof vi.fn>) => fn.mock.calls.reduce((n, [k]) => n + (k as number), 0);

beforeEach(() => vi.useFakeTimers());
afterEach(() => vi.useRealTimers());

describe("ForwardScrollbar", () => {
  it("steps and pages through its buttons and keys, negative scrolling up", () => {
    const { scrollNotches, thumb } = setup();
    fireEvent.click(screen.getByLabelText("Scroll up"));
    fireEvent.click(screen.getByLabelText("Scroll down"));
    fireEvent.click(screen.getByLabelText("Page up"));
    fireEvent.click(screen.getByLabelText("Page down"));
    fireEvent.keyDown(thumb, { key: "ArrowUp" });
    fireEvent.keyDown(thumb, { key: "PageDown" });
    fireEvent.keyDown(thumb, { key: "Home" });
    expect(scrollNotches.mock.calls.map(([n]) => n)).toEqual([-1, 1, -PAGE, PAGE, -1, PAGE]);
  });

  it("repeats a held arrow", () => {
    const { scrollNotches } = setup();
    const up = screen.getByLabelText("Scroll up");
    fireEvent.pointerDown(up);
    act(() => vi.advanceTimersByTime(400 + 80 * 3));
    fireEvent.pointerUp(up);
    expect(scrollNotches.mock.calls.length).toBeGreaterThanOrEqual(3);
  });

  it("ignores every control while disabled", () => {
    const { scrollNotches, thumb } = setup(true);
    fireEvent.click(screen.getByLabelText("Scroll down"));
    fireEvent.click(screen.getByLabelText("Page down"));
    fireEvent.keyDown(thumb, { key: "ArrowDown" });
    fireEvent.pointerDown(thumb, down(100));
    fireEvent.pointerMove(thumb, { pointerId: 1, clientY: 186 });
    act(() => vi.advanceTimersByTime(500));
    expect(scrollNotches).not.toHaveBeenCalled();
  });

  it("scrolls at a rate set by the drag offset and stops dead on release", () => {
    const { scrollNotches, cancelScroll, thumb } = setup();
    fireEvent.pointerDown(thumb, down(100));
    // Inside the dead zone nothing moves.
    fireEvent.pointerMove(thumb, { pointerId: 1, clientY: 105 });
    act(() => vi.advanceTimersByTime(500));
    expect(scrollNotches).not.toHaveBeenCalled();

    fireEvent.pointerMove(thumb, { pointerId: 1, clientY: 186 });
    act(() => vi.advanceTimersByTime(500));
    const dragged = sum(scrollNotches);
    expect(dragged).toBeGreaterThanOrEqual(8);
    expect(dragged).toBeLessThanOrEqual(14);

    fireEvent.pointerUp(thumb, { pointerId: 1 });
    expect(cancelScroll).toHaveBeenCalledTimes(1);
    expect(thumb.style.transform).toContain("+ 0px");
    act(() => vi.advanceTimersByTime(500));
    expect(sum(scrollNotches)).toBe(dragged);
  });

  it("scrolls up when the thumb is dragged up, and cancels on pointercancel", () => {
    const { scrollNotches, cancelScroll, thumb } = setup();
    fireEvent.pointerDown(thumb, down(100));
    fireEvent.pointerMove(thumb, { pointerId: 1, clientY: 14 });
    act(() => vi.advanceTimersByTime(300));
    expect(sum(scrollNotches)).toBeLessThan(0);
    fireEvent.pointerCancel(thumb, { pointerId: 1 });
    expect(cancelScroll).toHaveBeenCalledTimes(1);
  });

  it("cancels an active drag on unmount and exposes no scrollbar semantics", () => {
    const { unmount, cancelScroll, thumb } = setup();
    expect(screen.queryByRole("scrollbar")).toBeNull();
    expect(thumb.hasAttribute("aria-valuenow")).toBe(false);
    fireEvent.pointerDown(thumb, down(100));
    unmount();
    expect(cancelScroll).toHaveBeenCalledTimes(1);
  });
});
