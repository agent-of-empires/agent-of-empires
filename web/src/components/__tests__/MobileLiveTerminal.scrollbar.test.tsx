// @vitest-environment jsdom
// The opt-in forward-mode scrollbar: where it appears, the width it reserves, and the wheel it forwards.

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent } from "@testing-library/react";
import { alt, installResizeObserver, liveFrame, renderLiveTerminal } from "./liveTerminalHarness";

const state = vi.hoisted(() => ({ coarse: false }));
vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({
    settings: { mobileFontSize: 14, desktopFontSize: 14, showForwardScrollbar: true },
    update: vi.fn(),
  }),
}));
vi.mock("../../hooks/useIsCoarsePointer", () => ({ useIsCoarsePointer: () => state.coarse }));
installResizeObserver();

const STRIP = "[data-forward-scrollbar]";

beforeEach(() => {
  state.coarse = false;
});

describe("MobileLiveTerminal forward scrollbar", () => {
  it("reserves a strip beside a full-screen mouse app and forwards its notches as wheels", () => {
    const { container, scroller, props } = renderLiveTerminal({ frame: liveFrame({ ...alt }) });
    expect(container.querySelector(STRIP)).not.toBeNull();
    // 6px edge + 18px strip + 2px gap, so no column hides under it.
    expect(scroller.style.right).toBe("26px");

    fireEvent.click(container.querySelector<HTMLElement>("[aria-label='Scroll down']")!);
    fireEvent.click(container.querySelector<HTMLElement>("[aria-label='Scroll up']")!);
    const wheel = props.forwardWheel as ReturnType<typeof vi.fn>;
    // Down then up, SGR-encoded, at pane 0's middle row.
    expect(wheel.mock.calls.map(([up, sgr, , row]) => [up, sgr, row])).toEqual([
      [false, true, 2],
      [true, true, 2],
    ]);
  });

  it.each([
    ["a normal-screen app", { frame: liveFrame() }, false],
    ["a read-only view", { frame: liveFrame({ ...alt }), readOnly: true }, false],
    ["a coarse pointer", { frame: liveFrame({ ...alt }) }, true],
  ])("shows no strip and no inset for %s", (_name, over, coarse) => {
    state.coarse = coarse;
    const { container, scroller } = renderLiveTerminal(over);
    expect(container.querySelector(STRIP)).toBeNull();
    expect(scroller.style.right).toBe("");
  });
});
