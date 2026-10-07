// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { installResizeObserver, renderLiveTerminal } from "./liveTerminalHarness";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));
installResizeObserver();

describe("MobileLiveTerminal screen alignment", () => {
  it.each([
    [true, true],
    // Paired shells top-align like a normal terminal.
    [false, false],
  ])("bottomAlign=%s uses mt-auto: %s", (bottomAlign, mtAuto) => {
    expect(renderLiveTerminal({ bottomAlign }).content().className.includes("mt-auto")).toBe(mtAuto);
  });
});
