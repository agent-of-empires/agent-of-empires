// @vitest-environment jsdom

import { useRef } from "react";
import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { useTerminalGestureBoundary } from "./useTerminalGestureBoundary";

function Probe({ forwardMode }: { forwardMode: boolean }) {
  const ref = useRef<HTMLDivElement>(null);
  useTerminalGestureBoundary({ scrollerRef: ref, forwardMode, mouseSgr: true });
  return <div ref={ref} />;
}

describe("useTerminalGestureBoundary", () => {
  it("cancels alternate-screen touch moves and leaves normal scrolling alone", () => {
    const { container, rerender } = render(<Probe forwardMode />);
    const scroller = container.firstElementChild!;
    const forwardedMove = new Event("touchmove", { cancelable: true });
    scroller.dispatchEvent(forwardedMove);
    expect(forwardedMove.defaultPrevented).toBe(true);

    rerender(<Probe forwardMode={false} />);
    const nativeMove = new Event("touchmove", { cancelable: true });
    scroller.dispatchEvent(nativeMove);
    expect(nativeMove.defaultPrevented).toBe(false);
  });
});
