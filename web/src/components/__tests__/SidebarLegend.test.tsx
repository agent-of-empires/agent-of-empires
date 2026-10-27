// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";

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
});
