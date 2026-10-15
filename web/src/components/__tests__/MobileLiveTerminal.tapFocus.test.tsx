// @vitest-environment jsdom
// Tapping the terminal focuses the hidden input, which raises the soft keyboard (#2243).

import { describe, expect, it, vi } from "vitest";
import { fireEvent } from "@testing-library/react";
import { installResizeObserver, renderLiveTerminal } from "./liveTerminalHarness";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));
installResizeObserver();

describe("MobileLiveTerminal tap-to-focus", () => {
  it("focuses the hidden input when tapped", () => {
    const { scroller, input } = renderLiveTerminal();
    fireEvent.click(scroller);
    expect(document.activeElement).toBe(input());
  });

  it("leaves an active text selection alone so select-to-copy works", () => {
    const { scroller, input } = renderLiveTerminal();
    const range = document.createRange();
    range.selectNodeContents(scroller);
    window.getSelection()!.removeAllRanges();
    window.getSelection()!.addRange(range);
    fireEvent.click(scroller);
    expect(document.activeElement).not.toBe(input());
  });
});
