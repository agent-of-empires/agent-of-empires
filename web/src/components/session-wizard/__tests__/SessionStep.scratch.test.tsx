// @vitest-environment jsdom
//
// Vitest coverage for the SessionStep scratch branches (#1324).
// Exercises the rendering swap on `data.scratch === true` (worktree
// section replaced by the explanatory note) and the label-vs-Toggle
// double-toggle guard introduced in commit 0b761c00.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, fireEvent } from "@testing-library/react";

import { SessionStep } from "../steps/SessionStep";
import { initialData } from "../wizardReducer";

vi.mock("../../../lib/api", () => ({
  fetchBranches: vi.fn().mockResolvedValue([]),
}));

afterEach(() => {
  cleanup();
});

function renderStep(
  overrides: { scratch?: boolean; useWorktree?: boolean } = {},
) {
  const onChange = vi.fn();
  const utils = render(
    <SessionStep
      data={{
        ...initialData,
        path: overrides.scratch ? "" : "/repo/alpha",
        scratch: overrides.scratch ?? false,
        useWorktree: overrides.useWorktree ?? false,
      }}
      onChange={onChange}
    />,
  );
  // #1514 folds the worktree controls + scratch note behind a top-level
  // "Advanced" disclosure that defaults closed. Expand it so the branches
  // these tests exercise actually render.
  fireEvent.click(utils.getByRole("button", { name: "Advanced" }));
  return { onChange, ...utils };
}

describe("SessionStep scratch rendering (#1324)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("with scratch=true, the worktree toggle is hidden and a note is rendered", () => {
    const { queryByRole, getByText } = renderStep({ scratch: true });
    // The "Create a worktree" switch (role=switch) must not render when
    // scratch is on. The replacement note has the dedicated
    // aria-label below.
    expect(queryByRole("switch")).toBeNull();
    expect(
      getByText("Scratch sessions do not use git worktrees."),
    ).toBeTruthy();
  });

  it("with scratch=false, the worktree toggle is rendered", () => {
    const { getByRole } = renderStep({ scratch: false });
    expect(getByRole("switch")).toBeTruthy();
  });

  it("clicking the worktree switch does not double-toggle via the label", () => {
    // The label's onClick toggles useWorktree, and the inner Toggle's
    // onChange ALSO toggles useWorktree. Without the
    // `closest('button[role="switch"]')` guard at SessionStep.tsx:86,
    // a click on the switch fires both handlers and lands back on the
    // original value. Assert exactly one onChange.
    const { onChange, getByRole } = renderStep({
      scratch: false,
      useWorktree: false,
    });
    fireEvent.click(getByRole("switch"));
    expect(onChange).toHaveBeenCalledTimes(1);
    expect(onChange).toHaveBeenCalledWith("useWorktree", true);
  });
});
