// @vitest-environment jsdom
//
// Vitest coverage for the SessionStep "Advanced" disclosure (#1514).
// The fold moved the worktree toggle, branch input, attach-existing
// toggle, base-branch picker, and group input behind a top-level
// "Advanced" disclosure that defaults closed. These tests render the
// component directly, expand the disclosure, and exercise each folded
// control so the moved render + handler lines stay covered.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, fireEvent } from "@testing-library/react";

import { SessionStep } from "../steps/SessionStep";
import { initialData, type WizardData } from "../wizardReducer";

vi.mock("../../../lib/api", () => ({
  fetchBranches: vi.fn().mockResolvedValue([]),
}));

afterEach(() => {
  cleanup();
});

function renderStep(overrides: Partial<WizardData> = {}) {
  const onChange = vi.fn();
  const utils = render(
    <SessionStep
      data={{
        ...initialData,
        path: "/repo/alpha",
        useWorktree: true,
        scratch: false,
        ...overrides,
      }}
      onChange={onChange}
    />,
  );
  const expandAdvanced = () => fireEvent.click(utils.getByRole("button", { name: "Advanced" }));
  return { onChange, expandAdvanced, ...utils };
}

describe("SessionStep Advanced disclosure (#1514)", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("hides every non-title control until Advanced is expanded", () => {
    const { queryByRole, queryByPlaceholderText, getByPlaceholderText } = renderStep();
    // Title input is always visible.
    expect(getByPlaceholderText("Auto-generated if empty")).toBeTruthy();
    // Folded controls are absent before expanding.
    expect(queryByRole("switch")).toBeNull();
    expect(queryByPlaceholderText("Uses session title if empty")).toBeNull();
    expect(queryByPlaceholderText("Optional, for organizing related sessions")).toBeNull();
  });

  it("reveals the worktree toggle, branch input, attach toggle, and group when expanded", () => {
    const { expandAdvanced, getByPlaceholderText, getByRole } = renderStep();
    expandAdvanced();
    expect(getByRole("switch", { name: /Create a worktree/ })).toBeTruthy();
    expect(getByPlaceholderText("Uses session title if empty")).toBeTruthy();
    expect(getByRole("switch", { name: /Attach to existing branch/ })).toBeTruthy();
    // The base-branch picker's disclosure button (its input shares the
    // same accessible name but is not a button).
    expect(getByRole("button", { name: "Base branch" })).toBeTruthy();
    expect(getByPlaceholderText("Optional, for organizing related sessions")).toBeTruthy();
  });

  it("editing the branch input emits a worktreeBranch change", () => {
    const { expandAdvanced, onChange, getByPlaceholderText } = renderStep();
    expandAdvanced();
    fireEvent.change(getByPlaceholderText("Uses session title if empty"), {
      target: { value: "feat/x" },
    });
    expect(onChange).toHaveBeenCalledWith("worktreeBranch", "feat/x");
  });

  it("toggling attach-existing emits an attachExisting change", () => {
    const { expandAdvanced, onChange, getByText } = renderStep();
    expandAdvanced();
    fireEvent.click(getByText("Attach to existing branch"));
    expect(onChange).toHaveBeenCalledWith("attachExisting", true);
  });

  it("hides the Base branch picker once attach-existing is on", () => {
    const { expandAdvanced, queryByRole } = renderStep({
      attachExisting: true,
    });
    expandAdvanced();
    expect(queryByRole("button", { name: "Base branch" })).toBeNull();
  });

  it("expanding the Base branch picker renders the base-branch input", () => {
    const { expandAdvanced, getByRole, getByLabelText } = renderStep();
    expandAdvanced();
    fireEvent.click(getByRole("button", { name: "Base branch" }));
    expect(getByLabelText("Base branch")).toBeTruthy();
  });

  it("editing the group input emits a group change", () => {
    const { expandAdvanced, onChange, getByPlaceholderText } = renderStep();
    expandAdvanced();
    fireEvent.change(getByPlaceholderText("Optional, for organizing related sessions"), {
      target: { value: "backend" },
    });
    expect(onChange).toHaveBeenCalledWith("group", "backend");
  });
});
