// @vitest-environment jsdom
//
// Keyboard-affordance + a11y tests for StopSessionDialog. The dialog opens
// from the workspace sidebar right-click "Stop" item and mirrors the TUI's
// `x` confirmation. Enter confirms, Escape cancels, and Enter must not
// double-fire while a stop is in flight or when focus is on a button.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";

import { StopSessionDialog } from "../StopSessionDialog";

function setup(overrides?: { onConfirm?: () => Promise<void>; onCancel?: () => void }) {
  const onConfirm = overrides?.onConfirm ?? vi.fn().mockResolvedValue(undefined);
  const onCancel = overrides?.onCancel ?? vi.fn();
  const utils = render(<StopSessionDialog sessionTitle="my-session" onConfirm={onConfirm} onCancel={onCancel} />);
  return { ...utils, onConfirm, onCancel };
}

afterEach(() => {
  cleanup();
});

describe("StopSessionDialog", () => {
  it("focuses the Stop button on mount so Enter activates it natively", () => {
    const { container } = setup();
    const stopBtn = Array.from(container.querySelectorAll("button")).find(
      (b) => b.textContent?.includes("Stop") && !b.textContent.includes("Stopping"),
    );
    expect(stopBtn).toBeTruthy();
    expect(document.activeElement).toBe(stopBtn);
  });

  it("Enter pressed inside the dialog calls onConfirm once", () => {
    const onConfirm = vi.fn().mockResolvedValue(undefined);
    setup({ onConfirm });
    fireEvent.keyDown(document, { key: "Enter" });
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it("Escape pressed inside the dialog calls onCancel", () => {
    const onCancel = vi.fn();
    setup({ onCancel });
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it("Enter does not fire onConfirm a second time while a stop is in flight", () => {
    let resolveConfirm: (() => void) | null = null;
    const onConfirm = vi.fn(
      () =>
        new Promise<void>((resolve) => {
          resolveConfirm = () => resolve();
        }),
    );
    setup({ onConfirm });
    fireEvent.keyDown(document, { key: "Enter" });
    fireEvent.keyDown(document, { key: "Enter" });
    expect(onConfirm).toHaveBeenCalledTimes(1);
    resolveConfirm?.();
  });

  it("Enter while focus is on the Stop button does not double-fire onConfirm", () => {
    const onConfirm = vi.fn().mockResolvedValue(undefined);
    const { container } = setup({ onConfirm });
    const stopBtn = Array.from(container.querySelectorAll("button")).find(
      (b) => b.textContent?.includes("Stop") && !b.textContent.includes("Stopping"),
    )!;
    expect(document.activeElement).toBe(stopBtn);
    fireEvent.keyDown(stopBtn, { key: "Enter" });
    fireEvent.click(stopBtn);
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it("Enter while focus is on Cancel cancels rather than confirms", () => {
    const onConfirm = vi.fn().mockResolvedValue(undefined);
    const onCancel = vi.fn();
    const { container } = setup({ onConfirm, onCancel });
    const cancelBtn = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.trim() === "Cancel")!;
    cancelBtn.focus();
    fireEvent.keyDown(cancelBtn, { key: "Enter" });
    fireEvent.click(cancelBtn);
    expect(onConfirm).not.toHaveBeenCalled();
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it("has role=dialog, aria-modal, and aria-labelledby pointing at the title", () => {
    const { container } = setup();
    const dialog = container.querySelector('[role="dialog"]');
    expect(dialog).toBeTruthy();
    expect(dialog?.getAttribute("aria-modal")).toBe("true");
    const labelId = dialog?.getAttribute("aria-labelledby");
    expect(labelId).toBeTruthy();
    const titleEl = container.querySelector(`#${labelId}`);
    expect(titleEl?.textContent).toMatch(/Stop Session/);
  });

  it("restores focus to the previously focused element when the dialog unmounts", () => {
    const trigger = document.createElement("button");
    trigger.textContent = "trigger";
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
