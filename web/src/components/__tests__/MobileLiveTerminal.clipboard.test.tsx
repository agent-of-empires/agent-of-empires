// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent } from "@testing-library/react";
import { alt, installResizeObserver, liveFrame, renderLiveTerminal } from "./liveTerminalHarness";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));
installResizeObserver();

describe("MobileLiveTerminal OSC 52 clipboard", () => {
  it("arms the parent clipboard bridge on a forwarded left-button release", () => {
    const armAgentClipboard = vi.fn();
    const { scroller } = renderLiveTerminal({
      frame: liveFrame({ content: "copy me\n", history: 0, ...alt }),
      armAgentClipboard,
    });
    const pointer = { pointerType: "mouse", pointerId: 1, button: 0, clientY: 10 };
    fireEvent.pointerDown(scroller, { ...pointer, clientX: 10 });
    fireEvent.pointerUp(scroller, { ...pointer, clientX: 80 });
    expect(armAgentClipboard).toHaveBeenCalledTimes(1);
  });
});
