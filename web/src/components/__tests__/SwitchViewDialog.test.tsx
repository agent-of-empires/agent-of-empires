// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { SwitchViewDialog } from "../SwitchViewDialog";

function setup({
  toStructured = false,
  keepsContext = true,
  onConfirm = vi.fn().mockResolvedValue(undefined),
}: { toStructured?: boolean; keepsContext?: boolean; onConfirm?: () => Promise<void> } = {}) {
  const onCancel = vi.fn();
  const utils = render(
    <SwitchViewDialog
      sessionTitle="my-session"
      toStructured={toStructured}
      keepsContext={keepsContext}
      onConfirm={onConfirm}
      onCancel={onCancel}
    />,
  );
  return { ...utils, onConfirm, onCancel, confirm: screen.getByTestId("switch-view-confirm") as HTMLButtonElement };
}

afterEach(cleanup);

describe("SwitchViewDialog", () => {
  it.each([
    [false, true, /Switch to terminal[\s\S]*continues in the terminal/],
    [true, true, /Switch to structured view[\s\S]*continues in structured view/],
    [false, false, /fresh terminal pane/],
  ])("toStructured=%s keepsContext=%s copy", (toStructured, keepsContext, copy) => {
    expect(setup({ toStructured, keepsContext }).container.textContent).toMatch(copy);
  });

  it("is a focused modal named by its title; Enter confirms and Escape cancels", () => {
    const { confirm, onConfirm, onCancel } = setup();
    expect(screen.getByRole("dialog", { name: /Switch to terminal/ }).getAttribute("aria-modal")).toBe("true");
    expect(document.activeElement).toBe(confirm);
    fireEvent.keyDown(document, { key: "Enter" });
    expect(onConfirm).toHaveBeenCalledTimes(1);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it("cancels on an overlay click but not a panel click", () => {
    const { onCancel } = setup();
    const dialog = screen.getByTestId("switch-view-dialog");
    fireEvent.click(dialog.firstElementChild!);
    expect(onCancel).not.toHaveBeenCalled();
    fireEvent.click(dialog);
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it("re-enables confirm when onConfirm rejects", async () => {
    const { confirm, onConfirm } = setup({ onConfirm: vi.fn().mockRejectedValue(new Error("boom")) });
    fireEvent.click(confirm);
    expect(onConfirm).toHaveBeenCalledTimes(1);
    await waitFor(() => expect(confirm.disabled).toBe(false));
  });
});
