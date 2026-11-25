// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cloneRepo } from "../../../lib/api";
import { CloneRepoForm } from "./CloneRepoForm";

vi.mock("../../../lib/api", () => ({ cloneRepo: vi.fn() }));

afterEach(() => {
  cleanup();
  vi.mocked(cloneRepo).mockReset();
});

describe("CloneRepoForm", () => {
  it.each([
    ["keyCode 229", { key: "Enter", keyCode: 229 }],
    ["isComposing", { key: "Enter", isComposing: true }],
  ])("does not clone on Enter while an IME composes (%s)", async (_n, init) => {
    vi.mocked(cloneRepo).mockResolvedValue({ ok: false, error: "nope" });
    render(<CloneRepoForm onCloned={vi.fn()} />);
    const input = screen.getByLabelText("Repository URL");
    fireEvent.change(input, { target: { value: "https://example.com/r.git" } });
    fireEvent.keyDown(input, init);
    await act(async () => {});
    expect(cloneRepo).not.toHaveBeenCalled();
    fireEvent.keyDown(input, { key: "Enter" });
    expect(cloneRepo).toHaveBeenCalledTimes(1);
    await act(async () => {});
  });
});
