// @vitest-environment jsdom
// A plain Enter puts the soft keyboard away; a hardware keyboard, which never occludes, keeps focus for the next line.

import { describe, expect, it, vi } from "vitest";
import { act, fireEvent } from "@testing-library/react";
import { installResizeObserver, renderLiveTerminal } from "./liveTerminalHarness";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));
vi.mock("../../hooks/useIsCoarsePointer", () => ({ useIsCoarsePointer: () => true }));
installResizeObserver();

describe("MobileLiveTerminal Enter and the keyboard", () => {
  it.each([
    ["the soft keyboard is put away", true, false],
    ["a hardware keyboard keeps focus", false, true],
  ])("%s", async (_name, keyboardOpen, keepsFocus) => {
    const { scroller, input } = renderLiveTerminal({ keyboardOpen, sendData: vi.fn(() => true) });
    fireEvent.click(scroller);
    fireEvent.keyDown(input(), { key: "Enter", code: "Enter" });
    await act(() => new Promise((resolve) => setTimeout(resolve, 0)));
    expect(document.activeElement === input()).toBe(keepsFocus);
  });
});
