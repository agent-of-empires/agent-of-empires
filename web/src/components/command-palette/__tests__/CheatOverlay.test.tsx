// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { CheatOverlay } from "../CheatOverlay";
import type { CheatEffect } from "../../../lib/cheats";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe("CheatOverlay", () => {
  it("renders a fly sprite with the direction-specific animation", () => {
    const effect: CheatEffect = { kind: "fly", emoji: "🚗", dir: "ltr" };
    render(<CheatOverlay effect={effect} onDone={() => {}} />);
    const overlay = screen.getByTestId("cheat-overlay");
    expect(overlay.querySelector(".animate-cheat-fly-ltr")?.textContent).toBe("🚗");
  });

  it("uses the rtl animation when dir is rtl", () => {
    render(<CheatOverlay effect={{ kind: "fly", emoji: "🚚", dir: "rtl" }} onDone={() => {}} />);
    expect(screen.getByTestId("cheat-overlay").querySelector(".animate-cheat-fly-rtl")).not.toBeNull();
  });

  it("rains a full set of confetti sprites", () => {
    render(<CheatOverlay effect={{ kind: "confetti", emoji: "🪨" }} onDone={() => {}} />);
    const pieces = screen.getByTestId("cheat-overlay").querySelectorAll(".animate-cheat-confetti-fall");
    expect(pieces.length).toBe(14);
    expect(pieces[0].textContent).toBe("🪨");
  });

  it("renders a flash tinted with the effect color", () => {
    render(<CheatOverlay effect={{ kind: "flash", color: "#3b82f6" }} onDone={() => {}} />);
    const flash = screen.getByTestId("cheat-overlay").querySelector<HTMLElement>(".animate-cheat-flash");
    expect(flash).not.toBeNull();
    expect(flash?.style.background).toBe("rgb(59, 130, 246)");
  });

  it("renders a pulse sprite", () => {
    render(<CheatOverlay effect={{ kind: "pulse", emoji: "🗺️" }} onDone={() => {}} />);
    expect(screen.getByTestId("cheat-overlay").querySelector(".animate-cheat-pulse")?.textContent).toBe("🗺️");
  });

  it("never intercepts clicks so the palette stays interactive", () => {
    render(<CheatOverlay effect={{ kind: "flash", color: "#000" }} onDone={() => {}} />);
    expect(screen.getByTestId("cheat-overlay").className).toContain("pointer-events-none");
  });

  it("self-cleans after the effect duration and not before", () => {
    vi.useFakeTimers();
    const onDone = vi.fn();
    render(<CheatOverlay effect={{ kind: "flash", color: "#000" }} onDone={onDone} />);
    vi.advanceTimersByTime(599);
    expect(onDone).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(onDone).toHaveBeenCalledTimes(1);
  });

  it("waits the longer confetti duration before cleaning", () => {
    vi.useFakeTimers();
    const onDone = vi.fn();
    render(<CheatOverlay effect={{ kind: "confetti", emoji: "🎉" }} onDone={onDone} />);
    vi.advanceTimersByTime(2199);
    expect(onDone).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(onDone).toHaveBeenCalledTimes(1);
  });
});
