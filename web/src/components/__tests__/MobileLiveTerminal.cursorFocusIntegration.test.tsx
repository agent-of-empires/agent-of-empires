// @vitest-environment jsdom
// Input focus must reach the rendered cursor cell, not only the parent's chrome ring (#2684).

import { describe, expect, it, vi } from "vitest";
import { fireEvent } from "@testing-library/react";
import { installResizeObserver, liveFrame, renderLiveTerminal } from "./liveTerminalHarness";

vi.mock("../../hooks/useWebSettings", () => ({
  useWebSettings: () => ({ settings: { mobileFontSize: 14, desktopFontSize: 14 }, update: vi.fn() }),
}));
installResizeObserver();

describe("MobileLiveTerminal cursor fill on focus", () => {
  it("starts hollow, fills and blinks on focus, and reverts on blur", () => {
    const { container, input } = renderLiveTerminal({ frame: liveFrame({ cursor: { x: 2, y: 0 } }) });
    const cell = () => container.querySelector("[data-live-cursor]") as HTMLElement;
    const expectHollow = () => {
      expect(cell().style.backgroundColor).toBe("");
      expect(cell().style.outline).toContain("var(--term-cursor");
      expect(cell().className).toBe("");
    };
    expectHollow();
    fireEvent.focus(input());
    expect(cell().style.backgroundColor).toContain("var(--term-cursor");
    expect(cell().style.outline).toBe("");
    expect(cell().className).toContain("animate-term-cursor-blink");
    fireEvent.blur(input());
    expectHollow();
  });
});
