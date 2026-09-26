// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { SessionStep } from "../steps/SessionStep";
import { initialData, type WizardData } from "../wizardReducer";

vi.mock("../../../lib/api", () => ({
  fetchBranches: vi.fn().mockResolvedValue([]),
}));

afterEach(cleanup);

function renderStep(overrides: Partial<WizardData> = {}) {
  const onChange = vi.fn();
  render(
    <SessionStep data={{ ...initialData, path: "/repo/alpha", useWorktree: true, ...overrides }} onChange={onChange} />,
  );
  return { onChange };
}

describe("SessionStep", () => {
  it("renders worktree, branch, attach, base branch and group controls that emit changes", () => {
    const { onChange } = renderStep();
    expect(screen.getByRole("switch", { name: /Create a worktree/ })).toBeTruthy();
    fireEvent.change(screen.getByPlaceholderText("Uses session title if empty"), { target: { value: "feat/x" } });
    expect(onChange).toHaveBeenCalledWith("worktreeBranch", "feat/x");
    fireEvent.click(screen.getByText("Attach to existing branch"));
    expect(onChange).toHaveBeenCalledWith("attachExisting", true);
    fireEvent.change(screen.getByPlaceholderText("Optional, for organizing related sessions"), {
      target: { value: "backend" },
    });
    expect(onChange).toHaveBeenCalledWith("group", "backend");
    fireEvent.click(screen.getByRole("button", { name: "Base branch" }));
    expect(screen.getByLabelText("Base branch")).toBeTruthy();
  });

  it("hides the base branch picker when attaching to an existing branch", () => {
    renderStep({ attachExisting: true });
    expect(screen.queryByRole("button", { name: "Base branch" })).toBeNull();
  });

  it("replaces the worktree toggle with a note for scratch sessions", () => {
    renderStep({ scratch: true, path: "" });
    expect(screen.queryByRole("switch")).toBeNull();
    expect(screen.getByText("Scratch sessions do not use git worktrees.")).toBeTruthy();
  });

  it.each([true, false])("gates the worktree toggle on pathIsGitRepo=%s", (pathIsGitRepo) => {
    const { onChange } = renderStep({ useWorktree: false, pathIsGitRepo });
    const toggle = screen.getByRole("switch") as HTMLButtonElement;
    expect(toggle.disabled).toBe(!pathIsGitRepo);
    expect(!!screen.queryByLabelText("Worktree disabled: not a git repository")).toBe(!pathIsGitRepo);
    fireEvent.click(toggle);
    expect(onChange.mock.calls).toEqual(pathIsGitRepo ? [["useWorktree", true]] : []);
  });
});
