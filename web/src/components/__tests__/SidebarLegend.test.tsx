// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";

import { IdleDecayWindowContext } from "../../lib/idleDecay";
import { RATTLES } from "../../lib/rattles";
import { SidebarLegend } from "../sidebar/SidebarLegend";

function stubReducedMotion(reduce: boolean) {
  vi.stubGlobal(
    "matchMedia",
    (query: string) =>
      ({
        matches: query.includes("prefers-reduced-motion") ? reduce : false,
        media: query,
        addEventListener: () => {},
        removeEventListener: () => {},
      }) as unknown as MediaQueryList,
  );
}

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("SidebarLegend", () => {
  it("animates its glyphs only when reduced motion is off", () => {
    for (const reduce of [true, false]) {
      vi.useFakeTimers();
      stubReducedMotion(reduce);
      render(<SidebarLegend />);
      fireEvent.click(screen.getByRole("button", { name: "What do these symbols mean?" }));
      const running = () => screen.getAllByTestId("sidebar-legend-status")[0]!.textContent ?? "";
      expect(running()).toContain("Running");
      const frames = new Set<string>();
      for (let i = 0; i < 6; i++) {
        frames.add(running());
        act(() => vi.advanceTimersByTime(RATTLES.dots.interval));
      }
      expect(frames.size > 1, `reduced motion ${reduce}`).toBe(!reduce);
      cleanup();
      vi.useRealTimers();
    }
  });

  it("draws the just-finished sample as fresh Idle whatever the decay window, and no held glyph blank", () => {
    const glyphs = () =>
      screen
        .getAllByTestId("sidebar-legend-status")
        .map((row) => [row.textContent ?? "", row.querySelector("[aria-hidden]")?.textContent ?? ""] as const);
    // (decay window in ms, reduced motion): 0 is the default, when nothing reads as fresh.
    const cases: [number, boolean][] = [
      [0, false],
      [0, true],
      [60_000, false],
      [60_000, true],
    ];
    for (const [windowMs, reduce] of cases) {
      vi.useFakeTimers();
      stubReducedMotion(reduce);
      render(
        <IdleDecayWindowContext.Provider value={windowMs}>
          <SidebarLegend />
        </IdleDecayWindowContext.Provider>,
      );
      fireEvent.click(screen.getByRole("button", { name: "What do these symbols mean?" }));
      // Long past any window: the sample doesn't expire while the legend stays open.
      act(() => vi.advanceTimersByTime(10 * 60_000));
      const fresh = glyphs().find(([text]) => text.includes("Idle, just finished"))![1];
      expect(RATTLES.breathe.frames, `window ${windowMs}`).toContain(fresh);
      if (reduce) {
        // U+2800 is braille's blank cell, which `trim` keeps.
        for (const [text, glyph] of glyphs()) expect(["", "\u2800"], text).not.toContain(glyph.trim());
      }
      cleanup();
      vi.useRealTimers();
    }
  });
});
