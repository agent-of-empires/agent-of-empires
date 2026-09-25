// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { DeleteSessionDialog } from "../DeleteSessionDialog";
import { expectRestoresFocus } from "./dialogTestUtils";

type Overrides = Partial<Omit<React.ComponentProps<typeof DeleteSessionDialog>, "onConfirm" | "onTrash">> & {
  onConfirm?: ReturnType<typeof vi.fn>;
};

function setup(overrides: Overrides = {}) {
  const onConfirm = overrides.onConfirm ?? vi.fn().mockResolvedValue(undefined);
  const onTrash = vi.fn().mockResolvedValue(undefined);
  const onCancel = overrides.onCancel ?? vi.fn();
  const utils = render(
    <DeleteSessionDialog
      sessionTitle="my-session"
      branchName="feature/foo"
      hasManagedWorktree
      isSandboxed={false}
      isScratch={false}
      cleanupDefaults={{ delete_worktree: true, delete_branch: false, delete_sandbox: false, delete_to_trash: false }}
      defaultToTrash={false}
      {...overrides}
      onConfirm={onConfirm}
      onTrash={onTrash}
      onCancel={onCancel}
    />,
  );
  return { ...utils, onConfirm, onTrash, onCancel };
}

const defaults = { delete_worktree: true, delete_branch: false, delete_sandbox: false, delete_to_trash: false };
const box = (id: string) => screen.queryByTestId(id) as HTMLLabelElement | null;
const toggle = (id: string) => fireEvent.click(box(id)!.querySelector("span")!);
const enter = () => fireEvent.keyDown(document, { key: "Enter" });
const cleanupBoxes = () => document.querySelectorAll('[data-testid^="delete-session-checkbox-"]');
const body = (over: Record<string, boolean> = {}) => ({
  delete_worktree: true,
  delete_branch: false,
  delete_sandbox: false,
  force_delete: false,
  ...over,
});
const twoSessions = (sandboxed: [boolean, boolean] = [false, false]) => [
  { id: "sess-a", title: "agent-alpha", isSandboxed: sandboxed[0] },
  { id: "sess-b", title: "agent-beta", isSandboxed: sandboxed[1] },
];

afterEach(cleanup);

describe("DeleteSessionDialog keyboard and a11y", () => {
  it("focuses Delete, confirms on Enter once while in flight, and cancels on Escape", () => {
    let resolve = () => {};
    const onConfirm = vi.fn(() => new Promise<void>((r) => (resolve = r)));
    const { onCancel } = setup({ onConfirm });
    expect(document.activeElement).toBe(screen.getByTestId("delete-session-confirm"));
    enter();
    enter();
    expect(onConfirm).toHaveBeenCalledTimes(1);
    expect(onConfirm).toHaveBeenCalledWith(body());
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onCancel).toHaveBeenCalledTimes(1);
    resolve();
  });

  it("leaves Enter on a focused button to the native click", () => {
    const { onConfirm, onCancel } = setup();
    const confirm = screen.getByTestId("delete-session-confirm");
    fireEvent.keyDown(confirm, { key: "Enter" });
    fireEvent.click(confirm);
    expect(onConfirm).toHaveBeenCalledTimes(1);
    expect(onCancel).not.toHaveBeenCalled();
    cleanup();
    const second = setup();
    const cancel = screen.getByRole("button", { name: "Cancel" });
    cancel.focus();
    fireEvent.keyDown(cancel, { key: "Enter" });
    fireEvent.click(cancel);
    expect(second.onConfirm).not.toHaveBeenCalled();
    expect(second.onCancel).toHaveBeenCalledTimes(1);
  });

  it("is a modal dialog named by its title", () => {
    expect(setup() && screen.getByRole("dialog", { name: /Delete Session/ }).getAttribute("aria-modal")).toBe("true");
  });

  it("restores focus to the trigger on unmount", () => {
    expectRestoresFocus(() => setup().unmount);
  });
});

describe("DeleteSessionDialog presentation", () => {
  it("keeps the single-session presentation for one affected session", () => {
    const { container } = setup({ affectedSessions: [{ id: "sess-a", title: "my-session", isSandboxed: false }] });
    expect(screen.getByRole("heading").textContent).toBe("Delete Session");
    expect(container.textContent).toMatch(/Delete my-session\?/);
    expect(screen.queryByTestId("delete-session-affected-count")).toBeNull();
    expect(screen.queryByTestId("delete-session-affected-list")).toBeNull();
  });

  it("renders workspace-shaped copy for multi-session workspaces", () => {
    const { container } = setup({ affectedSessions: twoSessions([true, true]), isSandboxed: true, isScratch: true });
    expect(screen.getByRole("heading").textContent).toBe("Delete Workspace");
    expect(screen.getByTestId("delete-session-affected-count").textContent).toMatch(/all 2 sessions/);
    expect(screen.getByTestId("delete-session-affected-list").textContent).toBe("agent-alphaagent-beta");
    const text = container.textContent;
    expect(text).not.toMatch(/Delete my-session\?/);
    for (const copy of [
      "Permanently delete this workspace?",
      'Removes the workspace worktree for branch "feature/foo"',
      'Removes the workspace branch "feature/foo"',
      "Delete containers",
      "Removes Docker sandbox containers, and any private agent store, for all sessions in this workspace",
      "Keep scratch directories",
      "Leaves scratch directories on disk; session records are still removed",
    ]) {
      expect(text).toContain(copy);
    }
  });

  it("describes sandbox cleanup for single, mixed-workspace, and branchless cases", () => {
    setup({ hasManagedWorktree: false, isSandboxed: true });
    expect(document.body.textContent).toContain(
      "Removes the Docker sandbox container and any private agent store it has (including the saved agent login)",
    );
    cleanup();
    setup({ affectedSessions: twoSessions([true, false]), isSandboxed: true });
    expect(document.body.textContent).toContain(
      "Removes Docker sandbox containers, and any private agent store, for 1 sandboxed session in this workspace",
    );
    cleanup();
    setup({ branchName: null, affectedSessions: twoSessions() });
    expect(box("delete-session-checkbox-branch")!.textContent).toBe("Delete branch");
  });
});

describe("DeleteSessionDialog confirm body", () => {
  it.each([
    ["delete-worktree off hides force", "delete-session-checkbox-worktree", body({ delete_worktree: false })],
    ["force delete", "delete-session-checkbox-force", body({ force_delete: true })],
    ["delete branch", "delete-session-checkbox-branch", body({ delete_branch: true })],
  ])("%s", (_name, id, expected) => {
    const { onConfirm } = setup();
    expect(box(id)!.dataset.checked).toBe(id.endsWith("worktree") ? "true" : "false");
    toggle(id);
    expect(box(id)!.dataset.checked).toBe(id.endsWith("worktree") ? "false" : "true");
    if (id.endsWith("worktree")) expect(box("delete-session-checkbox-force")).toBeNull();
    enter();
    expect(onConfirm).toHaveBeenCalledWith(expected);
  });

  it("renders only the sandbox checkbox without a managed worktree", () => {
    const { onConfirm } = setup({ hasManagedWorktree: false, isSandboxed: true });
    expect(cleanupBoxes()).toHaveLength(1);
    expect(box("delete-session-checkbox-sandbox")!.dataset.checked).toBe("false");
    toggle("delete-session-checkbox-sandbox");
    enter();
    expect(onConfirm).toHaveBeenCalledWith(body({ delete_worktree: false, delete_sandbox: true }));
  });

  it("confirms an all-false body with no options", () => {
    const { onConfirm } = setup({ hasManagedWorktree: false });
    expect(cleanupBoxes()).toHaveLength(0);
    enter();
    expect(onConfirm).toHaveBeenCalledWith(body({ delete_worktree: false }));
    expect(onConfirm.mock.calls[0]![0].keep_scratch).toBeUndefined();
  });

  it("sends keep_scratch only for scratch sessions, false until checked", () => {
    const { onConfirm } = setup({ hasManagedWorktree: false, isScratch: true });
    expect(box("delete-session-checkbox-keep-scratch")!.dataset.checked).toBe("false");
    enter();
    expect(onConfirm.mock.calls[0]![0].keep_scratch).toBe(false);
    cleanup();
    const second = setup({ hasManagedWorktree: false, isScratch: true });
    toggle("delete-session-checkbox-keep-scratch");
    enter();
    expect(second.onConfirm).toHaveBeenCalledWith({ ...body({ delete_worktree: false }), keep_scratch: true });
  });

  it.each([
    [["agent-beta"], '"agent-beta" still uses it'],
    [["agent-beta", "agent-gamma"], "2 other sessions still use it"],
  ])("says the worktree and branch are kept when %j still use them (#4084)", (sharedWith, text) => {
    const { onConfirm } = setup({
      worktreeSharedWith: sharedWith,
      cleanupDefaults: { ...defaults, delete_branch: true },
    });
    expect(screen.getByTestId("delete-session-shared-worktree").textContent).toContain(text);
    expect(box("delete-session-checkbox-worktree")).toBeNull();
    expect(box("delete-session-checkbox-branch")).toBeNull();
    enter();
    expect(onConfirm).toHaveBeenCalledWith(body({ delete_worktree: false }));
  });
});

describe("DeleteSessionDialog trash-first", () => {
  it("a bare Delete trashes with the cleanup options hidden", () => {
    const { onConfirm, onTrash } = setup({ defaultToTrash: true });
    expect(screen.getByRole("heading").textContent).toBe("Delete Session");
    expect(box("delete-session-permanent")!.dataset.checked).toBe("false");
    expect(cleanupBoxes()).toHaveLength(0);
    enter();
    expect(onTrash).toHaveBeenCalledTimes(1);
    expect(onConfirm).not.toHaveBeenCalled();
  });

  it.each([
    ["session", undefined, "Delete Session"],
    ["workspace", twoSessions(), "Delete Workspace"],
  ])("checking Delete permanently reveals options and confirms (%s)", (kind, affectedSessions, title) => {
    const { onConfirm, onTrash, container } = setup({ defaultToTrash: true, affectedSessions });
    if (kind === "workspace") expect(container.textContent).toMatch(/Move this workspace to Trash\?/);
    toggle("delete-session-permanent");
    expect(box("delete-session-permanent")!.dataset.checked).toBe("true");
    expect(screen.getByRole("heading").textContent).toBe(title);
    if (kind === "workspace") expect(container.textContent).toMatch(/Permanently delete this workspace\?/);
    expect(box("delete-session-checkbox-worktree")).not.toBeNull();
    enter();
    expect(onConfirm).toHaveBeenCalledWith(body());
    expect(onTrash).not.toHaveBeenCalled();
  });

  it("without trash-first there is no opt-in and Delete purges directly", () => {
    const { onConfirm, onTrash } = setup();
    expect(box("delete-session-permanent")).toBeNull();
    expect(box("delete-session-checkbox-worktree")).not.toBeNull();
    enter();
    expect(onConfirm).toHaveBeenCalledTimes(1);
    expect(onTrash).not.toHaveBeenCalled();
  });
});
