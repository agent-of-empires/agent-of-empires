// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { WorktreePanel } from "../steps/WorktreePanel";
import { initialData, type WizardData } from "../wizardReducer";

vi.mock("../../../lib/api", () => ({
  fetchBranches: vi.fn().mockResolvedValue([]),
}));

afterEach(cleanup);

function renderPanel(overrides: Partial<WizardData> = {}) {
  const onChange = vi.fn();
  render(
    <WorktreePanel
      data={{ ...initialData, path: "/repo/alpha", useWorktree: true, ...overrides }}
      onChange={onChange}
    />,
  );
  return { onChange };
}

describe("WorktreePanel", () => {
  it("emits branch, attach and base branch changes", () => {
    const { onChange } = renderPanel();
    fireEvent.change(screen.getByPlaceholderText("Uses session title if empty"), { target: { value: "feat/x" } });
    fireEvent.click(screen.getByText("Attach to existing branch"));
    fireEvent.change(screen.getByLabelText("Base branch"), { target: { value: "release" } });
    expect(onChange).toHaveBeenCalledWith("worktreeBranch", "feat/x");
    expect(onChange).toHaveBeenCalledWith("attachExisting", true);
    expect(onChange).toHaveBeenCalledWith("baseBranch", "release");
  });

  it("hides the base branch picker when attaching to an existing branch", () => {
    renderPanel({ attachExisting: true });
    expect(screen.queryByLabelText("Base branch")).toBeNull();
  });
});
