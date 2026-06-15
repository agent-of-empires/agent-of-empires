// @vitest-environment jsdom
//
// Behavior tests for SessionGroupModal. The modal opens from the workspace
// sidebar context menu; the user edits a session's group path, then saves
// (Enter or the Save button) or cancels (Escape, Cancel, or backdrop click).
// Saving trims the value, short-circuits unchanged values to a plain close,
// surfaces save failures inline, and disables controls while in flight.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";

import { SessionGroupModal } from "../SessionGroupModal";

function setup(overrides?: {
  sessionTitle?: string;
  currentGroup?: string;
  onSave?: (group: string) => Promise<boolean>;
  onClose?: () => void;
}) {
  const onSave = overrides?.onSave ?? vi.fn().mockResolvedValue(true);
  const onClose = overrides?.onClose ?? vi.fn();
  const utils = render(
    <SessionGroupModal
      sessionTitle={overrides?.sessionTitle ?? "my-session"}
      currentGroup={overrides?.currentGroup ?? "work"}
      onSave={onSave}
      onClose={onClose}
    />,
  );
  const input = utils.container.querySelector<HTMLInputElement>('[data-testid="session-group-modal-input"]')!;
  const saveBtn = utils.container.querySelector<HTMLButtonElement>('[data-testid="session-group-modal-save"]')!;
  const cancelBtn = Array.from(utils.container.querySelectorAll("button")).find(
    (b) => b.textContent?.trim() === "Cancel",
  ) as HTMLButtonElement;
  return { ...utils, onSave, onClose, input, saveBtn, cancelBtn };
}

afterEach(() => {
  cleanup();
});

describe("SessionGroupModal", () => {
  it("seeds the input with the current group, focuses and selects it on mount", () => {
    const { input } = setup({ currentGroup: "work/projects" });
    expect(input.value).toBe("work/projects");
    expect(document.activeElement).toBe(input);
  });

  it("renders the dialog with a11y attributes pointing at the title", () => {
    const { container } = setup({ sessionTitle: "alpha" });
    const dialog = container.querySelector('[role="dialog"]');
    expect(dialog).toBeTruthy();
    expect(dialog?.getAttribute("aria-modal")).toBe("true");
    const labelId = dialog?.getAttribute("aria-labelledby");
    expect(container.querySelector(`#${labelId}`)?.textContent).toMatch(/Edit group/);
    expect(container.textContent).toContain("alpha");
  });

  it("clicking Save persists the trimmed value then closes", async () => {
    const onSave = vi.fn().mockResolvedValue(true);
    const onClose = vi.fn();
    const { input, saveBtn } = setup({ currentGroup: "", onSave, onClose });
    fireEvent.change(input, { target: { value: "  work/api  " } });
    fireEvent.click(saveBtn);
    expect(onSave).toHaveBeenCalledTimes(1);
    expect(onSave).toHaveBeenCalledWith("work/api");
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it("pressing Enter in the input saves", async () => {
    const onSave = vi.fn().mockResolvedValue(true);
    const onClose = vi.fn();
    const { input } = setup({ currentGroup: "old", onSave, onClose });
    fireEvent.change(input, { target: { value: "new" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSave).toHaveBeenCalledWith("new");
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it("an empty (trimmed) value ungroups by sending an empty string", async () => {
    const onSave = vi.fn().mockResolvedValue(true);
    const { input, saveBtn } = setup({ currentGroup: "work", onSave });
    fireEvent.change(input, { target: { value: "   " } });
    fireEvent.click(saveBtn);
    expect(onSave).toHaveBeenCalledWith("");
    await Promise.resolve();
  });

  it("an unchanged value closes without calling onSave", () => {
    const onSave = vi.fn().mockResolvedValue(true);
    const onClose = vi.fn();
    const { saveBtn } = setup({ currentGroup: "work", onSave, onClose });
    fireEvent.click(saveBtn);
    expect(onSave).not.toHaveBeenCalled();
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("a value that only differs by surrounding whitespace is treated as unchanged", () => {
    const onSave = vi.fn().mockResolvedValue(true);
    const onClose = vi.fn();
    const { input, saveBtn } = setup({ currentGroup: "work", onSave, onClose });
    fireEvent.change(input, { target: { value: "  work  " } });
    fireEvent.click(saveBtn);
    expect(onSave).not.toHaveBeenCalled();
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("surfaces an update error and keeps the modal open when onSave returns false", async () => {
    const onSave = vi.fn().mockResolvedValue(false);
    const onClose = vi.fn();
    const { input, saveBtn, container } = setup({ currentGroup: "", onSave, onClose });
    fireEvent.change(input, { target: { value: "work" } });
    fireEvent.click(saveBtn);
    await waitFor(() =>
      expect(container.querySelector('[data-testid="session-group-modal-error"]')?.textContent).toBe(
        "Failed to update group.",
      ),
    );
    expect(onClose).not.toHaveBeenCalled();
    // Controls re-enabled and focus returned to the input for a retry.
    expect(saveBtn.disabled).toBe(false);
    expect(document.activeElement).toBe(input);
  });

  it("surfaces a clear error when ungrouping fails", async () => {
    const onSave = vi.fn().mockResolvedValue(false);
    const { input, saveBtn, container } = setup({ currentGroup: "work", onSave });
    fireEvent.change(input, { target: { value: "" } });
    fireEvent.click(saveBtn);
    await waitFor(() =>
      expect(container.querySelector('[data-testid="session-group-modal-error"]')?.textContent).toBe(
        "Failed to clear group.",
      ),
    );
  });

  it("editing the input clears a previously shown error", async () => {
    const onSave = vi.fn().mockResolvedValue(false);
    const { input, saveBtn, container } = setup({ currentGroup: "", onSave });
    fireEvent.change(input, { target: { value: "work" } });
    fireEvent.click(saveBtn);
    await waitFor(() => expect(container.querySelector('[data-testid="session-group-modal-error"]')).toBeTruthy());
    fireEvent.change(input, { target: { value: "work2" } });
    expect(container.querySelector('[data-testid="session-group-modal-error"]')).toBeNull();
  });

  it("disables both buttons while a save is in flight", async () => {
    let resolveSave: ((ok: boolean) => void) | null = null;
    const onSave = vi.fn(() => new Promise<boolean>((resolve) => (resolveSave = resolve)));
    const { input, saveBtn, cancelBtn, onClose } = setup({ currentGroup: "", onSave });
    fireEvent.change(input, { target: { value: "work" } });
    fireEvent.click(saveBtn);
    await Promise.resolve();
    expect(saveBtn.disabled).toBe(true);
    expect(cancelBtn.disabled).toBe(true);
    expect(saveBtn.textContent).toContain("Saving...");
    resolveSave?.(true);
    // Let the post-resolution state settle (modal closes on success) so the
    // update doesn't land outside the test window and flake.
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it("does not fire a second save while one is in flight", async () => {
    let resolveSave: ((ok: boolean) => void) | null = null;
    const onSave = vi.fn(() => new Promise<boolean>((resolve) => (resolveSave = resolve)));
    const { input, onClose } = setup({ currentGroup: "", onSave });
    fireEvent.change(input, { target: { value: "work" } });
    fireEvent.keyDown(input, { key: "Enter" });
    await Promise.resolve();
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSave).toHaveBeenCalledTimes(1);
    resolveSave?.(true);
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it("Cancel button closes without saving", () => {
    const onSave = vi.fn().mockResolvedValue(true);
    const onClose = vi.fn();
    const { cancelBtn } = setup({ onSave, onClose });
    fireEvent.click(cancelBtn);
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(onSave).not.toHaveBeenCalled();
  });

  it("Escape in the input closes the modal", () => {
    const onClose = vi.fn();
    const { input } = setup({ onClose });
    fireEvent.keyDown(input, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("clicking the backdrop closes, clicking the panel does not", () => {
    const onClose = vi.fn();
    const { container } = setup({ onClose });
    const backdrop = container.querySelector('[data-testid="session-group-modal"]') as HTMLElement;
    const panel = backdrop.querySelector("div") as HTMLElement;
    fireEvent.click(panel);
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.click(backdrop);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("restores focus to the previously focused element on unmount", () => {
    const trigger = document.createElement("button");
    document.body.appendChild(trigger);
    trigger.focus();
    expect(document.activeElement).toBe(trigger);
    const { unmount } = setup();
    expect(document.activeElement).not.toBe(trigger);
    unmount();
    expect(document.activeElement).toBe(trigger);
    trigger.remove();
  });
});
