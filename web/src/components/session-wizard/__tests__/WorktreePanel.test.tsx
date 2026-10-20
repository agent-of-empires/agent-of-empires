// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { WorktreePanel } from "../steps/WorktreePanel";
import { initialData, type WizardData } from "../wizardReducer";

const fetchWorktrees = vi.hoisted(() => vi.fn().mockResolvedValue([]));
vi.mock("../../../lib/api", () => ({
  fetchBranches: vi.fn().mockResolvedValue([]),
  fetchWorktrees,
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

  it("lists existing worktrees when attaching and fills the branch on pick", async () => {
    fetchWorktrees.mockResolvedValue([
      { branch: "main", path: "/repo/alpha" },
      { branch: "feat/x", path: "/repo/alpha-wt/feat-x" },
    ]);
    const { onChange } = renderPanel({ attachExisting: true });
    fireEvent.click(await screen.findByText("feat/x"));
    expect(fetchWorktrees).toHaveBeenCalledWith("/repo/alpha");
    expect(screen.getByText("/repo/alpha-wt/feat-x")).toBeTruthy();
    expect(onChange).toHaveBeenCalledWith("worktreeBranch", "feat/x");
  });

  it("does not fetch worktrees when creating, and says so when none exist", async () => {
    fetchWorktrees.mockClear();
    renderPanel();
    expect(fetchWorktrees).not.toHaveBeenCalled();
    cleanup();
    fetchWorktrees.mockResolvedValue([]);
    renderPanel({ attachExisting: true });
    await waitFor(() => expect(screen.getByText("No existing worktrees found.")).toBeTruthy());
    cleanup();
    fetchWorktrees.mockResolvedValue(null);
    renderPanel({ attachExisting: true });
    await waitFor(() => expect(screen.getByText("Could not list worktrees.")).toBeTruthy());
  });

  it("disables attach mode while extra repositories are selected", () => {
    const { onChange } = renderPanel({ extraRepoPaths: ["/repo/beta"] });
    const toggle = screen.getByRole("switch", { name: /attach to existing branch/i });
    fireEvent.click(toggle);
    expect(toggle.hasAttribute("disabled") || toggle.getAttribute("aria-disabled") === "true").toBe(true);
    expect(onChange).not.toHaveBeenCalledWith("attachExisting", true);
  });
});
