// @vitest-environment jsdom
//
// Tests the worktree-toggle gate in SessionStep: a session pointed at a
// folder that is not a git repository (e.g. a root picked via "Use this
// folder") must not be able to enable worktree mode, which the server
// rejects. See #2680 follow-up.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

// SessionStep imports fetchBranches for its base-branch picker; stub it so
// the module graph resolves. The gate under test does not depend on it.
vi.mock("../../../../lib/api", () => ({
  fetchBranches: vi.fn().mockResolvedValue([]),
}));

import { SessionStep } from "../SessionStep";
import { initialData } from "../../wizardReducer";

function renderStep(overrides: Partial<typeof initialData>) {
  const onChange = vi.fn();
  // embedded mode renders the worktree controls flat, so the toggle is
  // visible without expanding the Advanced fold.
  render(<SessionStep data={{ ...initialData, ...overrides }} onChange={onChange} embedded />);
  return { onChange };
}

afterEach(cleanup);

describe("SessionStep worktree gate", () => {
  it("enables the worktree toggle for a git-repo path", () => {
    renderStep({ path: "/repos/app", pathIsGitRepo: true });
    // This repo's component tests do not load jest-dom, so assert on the
    // plain DOM `disabled` property and query-null presence.
    expect((screen.getByRole("switch") as HTMLButtonElement).disabled).toBe(false);
    expect(screen.queryByLabelText("Worktree disabled: not a git repository")).toBeNull();
  });

  it("disables the worktree toggle and explains why for a non-repo path", () => {
    const { onChange } = renderStep({ path: "/home/user/projects", pathIsGitRepo: false });
    const toggle = screen.getByRole("switch") as HTMLButtonElement;
    expect(toggle.disabled).toBe(true);
    expect(screen.getByLabelText("Worktree disabled: not a git repository")).toBeTruthy();

    // A click on the disabled toggle must not flip useWorktree on.
    fireEvent.click(toggle);
    expect(onChange).not.toHaveBeenCalledWith("useWorktree", true);
  });
});
