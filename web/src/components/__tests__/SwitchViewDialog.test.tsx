// @vitest-environment jsdom
//
// Keyboard-affordance + capability-aware-copy tests for SwitchViewDialog. The
// dialog opens from the workspace sidebar "Switch to terminal / structured
// view" item and mirrors the TUI confirmation. Enter confirms, Escape cancels,
// and the body copy depends on direction + whether the pairing keeps context.

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, waitFor } from "@testing-library/react";

import { SwitchViewDialog } from "../SwitchViewDialog";

function setup(overrides?: {
  toStructured?: boolean;
  keepsContext?: boolean;
  onConfirm?: () => Promise<void>;
  onCancel?: () => void;
}) {
  const onConfirm = overrides?.onConfirm ?? vi.fn().mockResolvedValue(undefined);
  const onCancel = overrides?.onCancel ?? vi.fn();
  const utils = render(
    <SwitchViewDialog
      sessionTitle="my-session"
      toStructured={overrides?.toStructured ?? false}
      keepsContext={overrides?.keepsContext ?? true}
      onConfirm={onConfirm}
      onCancel={onCancel}
    />,
  );
  return { ...utils, onConfirm, onCancel };
}

describe("SwitchViewDialog", () => {
  it("claude to-terminal copy says the conversation continues", () => {
    const { container } = setup({ toStructured: false, keepsContext: true });
    expect(container.textContent).toMatch(/Switch to terminal/);
    expect(container.textContent).toMatch(/continues in the terminal/);
  });

  it("claude to-structured copy says the conversation continues", () => {
    const { container } = setup({ toStructured: true, keepsContext: true });
    expect(container.textContent).toMatch(/Switch to structured view/);
    expect(container.textContent).toMatch(/continues in structured view/);
  });

  it("non-resumable to-terminal copy warns of a fresh restart", () => {
    const { container } = setup({ toStructured: false, keepsContext: false });
    expect(container.textContent).toMatch(/fresh terminal pane/);
  });

  it("focuses the confirm button on mount", () => {
    const { getByTestId } = setup();
    expect(document.activeElement).toBe(getByTestId("switch-view-confirm"));
  });

  it("clicking confirm calls onConfirm", () => {
    const onConfirm = vi.fn().mockResolvedValue(undefined);
    const { getByTestId } = setup({ onConfirm });
    fireEvent.click(getByTestId("switch-view-confirm"));
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it("Enter inside the dialog calls onConfirm once", () => {
    const onConfirm = vi.fn().mockResolvedValue(undefined);
    setup({ onConfirm });
    fireEvent.keyDown(document, { key: "Enter" });
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it("Escape calls onCancel", () => {
    const onCancel = vi.fn();
    setup({ onCancel });
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it("clicking the overlay cancels; clicking the panel does not", () => {
    const onCancel = vi.fn();
    const { getByTestId } = setup({ onCancel });
    const dialog = getByTestId("switch-view-dialog");
    fireEvent.click(dialog);
    expect(onCancel).toHaveBeenCalledTimes(1);
    // A click that bubbles from the inner panel is stopped.
    fireEvent.click(dialog.firstElementChild as Element);
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it("re-enables the confirm button when onConfirm rejects", async () => {
    const onConfirm = vi.fn().mockRejectedValue(new Error("boom"));
    const { getByTestId } = setup({ onConfirm });
    const btn = getByTestId("switch-view-confirm") as HTMLButtonElement;
    fireEvent.click(btn);
    expect(onConfirm).toHaveBeenCalledTimes(1);
    await waitFor(() => expect(btn.disabled).toBe(false));
  });

  it("exposes an accessible dialog named by its title", () => {
    const { getByRole } = setup();
    // getByRole resolves the accessible name via aria-labelledby, so this
    // verifies role, the modal flag, and the label linkage in one query.
    const dialog = getByRole("dialog", { name: /Switch to terminal/ });
    expect(dialog.getAttribute("aria-modal")).toBe("true");
  });
});
