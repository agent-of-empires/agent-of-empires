// @vitest-environment jsdom
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { FindBar } from "./FindBar";
import type { FindMatch, SearchableLine } from "./findMatches";

const LINES: SearchableLine[] = [
  { side: "old", lineNumber: 2, text: "beta" },
  { side: "new", lineNumber: 1, text: "alpha beta" },
  { side: "new", lineNumber: 2, text: "gamma beta" },
];

function setup() {
  const onJump = vi.fn<(m: FindMatch | null) => void>();
  const onClose = vi.fn();
  render(<FindBar lines={LINES} onJump={onJump} onClose={onClose} />);
  const input = screen.getByRole("textbox", {
    name: "Find in diff",
  }) as HTMLInputElement;
  return { onJump, onClose, input };
}

describe("FindBar", () => {
  it("jumps to the first match and shows the count", () => {
    const { onJump, input } = setup();
    fireEvent.change(input, { target: { value: "beta" } });
    // old:1 + new:1 + new:1 = 3 matches across both sides.
    expect(screen.getByText("1/3")).toBeTruthy();
    const last = onJump.mock.calls.at(-1)?.[0];
    expect(last?.lineNumber).toBe(2); // "beta" first on old side, line 2
    expect(last?.side).toBe("old");
  });

  it("steps to the next match on Enter", () => {
    const { onJump, input } = setup();
    fireEvent.change(input, { target: { value: "beta" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(screen.getByText("2/3")).toBeTruthy();
    const last = onJump.mock.calls.at(-1)?.[0];
    expect(last?.side).toBe("new");
  });

  it("wraps backwards on Shift+Enter", () => {
    const { input } = setup();
    fireEvent.change(input, { target: { value: "beta" } });
    fireEvent.keyDown(input, { key: "Enter", shiftKey: true });
    expect(screen.getByText("3/3")).toBeTruthy();
  });

  it("shows 0/0 with no matches and reports null to the host", () => {
    const { onJump, input } = setup();
    fireEvent.change(input, { target: { value: "zzz" } });
    expect(screen.getByText("0/0")).toBeTruthy();
    expect(onJump.mock.calls.at(-1)?.[0]).toBeNull();
  });

  it("surfaces an invalid regex instead of throwing", () => {
    const { input } = setup();
    fireEvent.click(screen.getByRole("button", { name: "Regular expression" }));
    fireEvent.change(input, { target: { value: "(" } });
    expect(screen.getByText("Invalid pattern")).toBeTruthy();
  });

  it("closes on Escape", () => {
    const { onClose, input } = setup();
    fireEvent.keyDown(input, { key: "Escape" });
    expect(onClose).toHaveBeenCalled();
  });
});
