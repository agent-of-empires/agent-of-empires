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
      render(<SidebarLegend anchor={{ x: 0, y: 0 }} onClose={() => {}} opener={null} />);
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
          <SidebarLegend anchor={{ x: 0, y: 0 }} onClose={() => {}} opener={null} />
        </IdleDecayWindowContext.Provider>,
      );
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

  it("returns focus to its opener after a click on blank space, not after a click on a control", () => {
    vi.useFakeTimers();
    stubReducedMotion(true);
    // (where the outside click lands, focus back on the opener)
    const cases: [string, boolean][] = [
      // Like Safari, jsdom doesn't focus a clicked button, so focus is on <body> either way.
      ["Other control", false],
      ["Blank space", true],
    ];
    for (const [target, refocused] of cases) {
      const onClose = vi.fn();
      const opener = document.createElement("button");
      document.body.append(opener);
      render(
        <>
          <button type="button">Other control</button>
          <p>Blank space</p>
          <SidebarLegend anchor={{ x: 0, y: 0 }} onClose={onClose} opener={opener} />
        </>,
      );
      // The outside-click listener attaches a frame after opening.
      act(() => vi.advanceTimersByTime(50));
      expect(screen.getByTestId("sidebar-legend")).toBeTruthy();
      act(() => (document.activeElement as HTMLElement | null)?.blur());
      const el = screen.getByText(target);
      fireEvent.pointerDown(el);
      fireEvent.click(el);
      expect(screen.queryByTestId("sidebar-legend"), target).toBeNull();
      expect(onClose, target).toHaveBeenCalledTimes(1);
      expect(document.activeElement === opener, target).toBe(refocused);
      cleanup();
      opener.remove();
    }
  });
});
