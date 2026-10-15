// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { CheatOverlay } from "../CheatOverlay";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

const overlay = () => screen.getByTestId("cheat-overlay");

describe("CheatOverlay", () => {
  it("renders a click-through flash tinted with the effect color", () => {
    render(<CheatOverlay effect={{ kind: "flash", color: "#3b82f6" }} onDone={() => {}} />);
    expect(overlay().querySelector<HTMLElement>(".animate-cheat-flash")?.style.background).toBe("rgb(59, 130, 246)");
    expect(overlay().className).toContain("pointer-events-none");
  });

  it("self-cleans after its duration and not before", () => {
    const ms = 2200;
    vi.useFakeTimers();
    const onDone = vi.fn();
    render(<CheatOverlay effect={{ kind: "confetti", emoji: "🎉" }} onDone={onDone} />);
    vi.advanceTimersByTime(ms - 1);
    expect(onDone).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(onDone).toHaveBeenCalledTimes(1);
  });
});
