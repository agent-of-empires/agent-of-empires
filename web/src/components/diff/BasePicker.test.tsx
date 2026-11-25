// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fetchBranches, setSessionDiffBase } from "../../lib/api";
import { BasePicker } from "./BasePicker";

vi.mock("../../lib/api", () => ({ fetchBranches: vi.fn(), setSessionDiffBase: vi.fn() }));

afterEach(() => {
  cleanup();
  vi.mocked(fetchBranches).mockReset();
  vi.mocked(setSessionDiffBase).mockReset();
});

describe("BasePicker", () => {
  it.each([
    ["keyCode 229", { key: "Enter", keyCode: 229 }],
    ["isComposing", { key: "Enter", isComposing: true }],
  ])("does not apply the typed base on Enter while an IME composes (%s)", async (_n, init) => {
    vi.mocked(fetchBranches).mockResolvedValue([]);
    vi.mocked(setSessionDiffBase).mockResolvedValue(null);
    render(<BasePicker sessionId="s1" repoPath="/r" currentBase="main" hasOverride={false} />);
    fireEvent.click(screen.getByRole("button", { name: /Change diff base/ }));
    await act(async () => {});
    const input = screen.getByPlaceholderText("Search branches...");
    fireEvent.change(input, { target: { value: "release/1" } });
    fireEvent.keyDown(input, init);
    await act(async () => {});
    expect(setSessionDiffBase).not.toHaveBeenCalled();
    fireEvent.keyDown(input, { key: "Enter" });
    expect(setSessionDiffBase).toHaveBeenCalledWith("s1", "release/1", undefined);
    await act(async () => {});
  });
});
