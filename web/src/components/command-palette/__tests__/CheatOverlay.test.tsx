// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { CheatOverlay } from "../CheatOverlay";
import type { CheatEffect } from "../../../lib/cheats";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

const overlay = () => screen.getByTestId("cheat-overlay");

describe("CheatOverlay", () => {
  it.each<[CheatEffect, string, string]>([
    [{ kind: "fly", emoji: "🚗", dir: "ltr" }, ".animate-cheat-fly-ltr", "🚗"],
    [{ kind: "fly", emoji: "🚚", dir: "rtl" }, ".animate-cheat-fly-rtl", "🚚"],
    [{ kind: "pulse", emoji: "🗺️" }, ".animate-cheat-pulse", "🗺️"],
  ])("renders %j", (effect, selector, text) => {
    render(<CheatOverlay effect={effect} onDone={() => {}} />);
    expect(overlay().querySelector(selector)?.textContent).toBe(text);
  });

  it("rains a full set of confetti sprites", () => {
    render(<CheatOverlay effect={{ kind: "confetti", emoji: "🪨" }} onDone={() => {}} />);
    const pieces = overlay().querySelectorAll(".animate-cheat-confetti-fall");
    expect(pieces).toHaveLength(14);
    expect(pieces[0]!.textContent).toBe("🪨");
  });

  it("renders a click-through flash tinted with the effect color", () => {
    render(<CheatOverlay effect={{ kind: "flash", color: "#3b82f6" }} onDone={() => {}} />);
    expect(overlay().querySelector<HTMLElement>(".animate-cheat-flash")?.style.background).toBe("rgb(59, 130, 246)");
    expect(overlay().className).toContain("pointer-events-none");
  });

  it.each<[CheatEffect, number]>([
    [{ kind: "flash", color: "#000" }, 600],
    [{ kind: "confetti", emoji: "🎉" }, 2200],
  ])("self-cleans %j after its duration and not before", (effect, ms) => {
    vi.useFakeTimers();
    const onDone = vi.fn();
    render(<CheatOverlay effect={effect} onDone={onDone} />);
    vi.advanceTimersByTime(ms - 1);
    expect(onDone).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(onDone).toHaveBeenCalledTimes(1);
  });
});
