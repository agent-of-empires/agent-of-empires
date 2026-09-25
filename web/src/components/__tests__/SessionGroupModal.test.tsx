// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { SessionGroupModal } from "../SessionGroupModal";
import { expectRestoresFocus } from "./dialogTestUtils";

function setup(currentGroup = "work", onSave: (group: string) => Promise<boolean> = vi.fn().mockResolvedValue(true)) {
  const onClose = vi.fn();
  const utils = render(
    <SessionGroupModal sessionTitle="alpha" currentGroup={currentGroup} onSave={onSave} onClose={onClose} />,
  );
  return {
    ...utils,
    onSave,
    onClose,
    input: screen.getByTestId("session-group-modal-input") as HTMLInputElement,
    saveBtn: screen.getByTestId("session-group-modal-save") as HTMLButtonElement,
    cancelBtn: screen.getByRole("button", { name: "Cancel" }) as HTMLButtonElement,
  };
}

const errorText = () => screen.queryByTestId("session-group-modal-error")?.textContent;
const pending = () => {
  let resolve: (ok: boolean) => void = () => {};
  const onSave = vi.fn(() => new Promise<boolean>((r) => (resolve = r)));
  return { onSave, resolve: (ok: boolean) => resolve(ok) };
};

afterEach(cleanup);

describe("SessionGroupModal", () => {
  it("is a modal named by its title with the current group focused", () => {
    const { input, container } = setup("work/projects");
    expect(screen.getByRole("dialog", { name: "Edit group" }).getAttribute("aria-modal")).toBe("true");
    expect(container.textContent).toContain("alpha");
    expect(input.value).toBe("work/projects");
    expect(document.activeElement).toBe(input);
  });

  it("saves the trimmed value then closes, or closes without saving when unchanged", async () => {
    for (const [current, typed, saved, via] of [
      ["", "  work/api  ", "work/api", "click"],
      ["old", "new", "new", "enter"],
      // A blank value ungroups.
      ["work", "   ", "", "click"],
      ["work", "  work  ", null, "click"],
    ] as const) {
      const { input, saveBtn, onSave, onClose } = setup(current);
      fireEvent.change(input, { target: { value: typed } });
      if (via === "enter") fireEvent.keyDown(input, { key: "Enter" });
      else fireEvent.click(saveBtn);
      if (saved == null) expect(onSave).not.toHaveBeenCalled();
      else expect(onSave).toHaveBeenCalledWith(saved);
      await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
      cleanup();
    }
  });

  it("a failed save shows an update or clear error and stays open", async () => {
    for (const [current, typed, message] of [
      ["", "work", "Failed to update group."],
      ["work", "", "Failed to clear group."],
    ]) {
      const { input, saveBtn, onClose } = setup(current, vi.fn().mockResolvedValue(false));
      fireEvent.change(input, { target: { value: typed } });
      fireEvent.click(saveBtn);
      await waitFor(() => expect(errorText()).toBe(message));
      expect(onClose).not.toHaveBeenCalled();
      expect(saveBtn.disabled).toBe(false);
      expect(document.activeElement).toBe(input);
      fireEvent.change(input, { target: { value: "again" } });
      expect(errorText()).toBeUndefined();
      cleanup();
    }
  });

  it("disables controls and ignores repeat saves while one is in flight", async () => {
    const { onSave, resolve } = pending();
    const { input, saveBtn, cancelBtn, onClose } = setup("", onSave);
    fireEvent.change(input, { target: { value: "work" } });
    fireEvent.keyDown(input, { key: "Enter" });
    await Promise.resolve();
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSave).toHaveBeenCalledTimes(1);
    expect(saveBtn.disabled).toBe(true);
    expect(cancelBtn.disabled).toBe(true);
    expect(saveBtn.textContent).toContain("Saving...");
    resolve(true);
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it("closes without saving via Cancel, Escape, and the backdrop but not the panel", () => {
    const { cancelBtn, input, onClose, onSave } = setup();
    fireEvent.click(cancelBtn);
    fireEvent.keyDown(input, { key: "Escape" });
    const backdrop = screen.getByTestId("session-group-modal");
    fireEvent.click(backdrop.firstElementChild!);
    expect(onClose).toHaveBeenCalledTimes(2);
    fireEvent.click(backdrop);
    expect(onClose).toHaveBeenCalledTimes(3);
    expect(onSave).not.toHaveBeenCalled();
  });

  it("restores focus on unmount", () => {
    expectRestoresFocus(() => setup().unmount);
  });
});
