// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { fetchGroups } from "../../lib/api";
import type { GroupInfo } from "../../lib/types";
import { SessionGroupModal } from "../SessionGroupModal";
import { expectRestoresFocus } from "./dialogTestUtils";

vi.mock("../../lib/api", () => ({ fetchGroups: vi.fn() }));

const group = (path: string): GroupInfo => ({ path, session_count: 2 });

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

beforeEach(() => vi.mocked(fetchGroups).mockResolvedValue([]));
afterEach(cleanup);

describe("SessionGroupModal", () => {
  it("is a modal named by its title with the current group focused", () => {
    const { input, container } = setup("work/projects");
    expect(screen.getByRole("dialog", { name: "Edit group" }).getAttribute("aria-modal")).toBe("true");
    expect(container.textContent).toContain("alpha");
    expect(input.value).toBe("work/projects");
    expect(document.activeElement).toBe(input);
    // The prompt, not the input or hint, describes the dialog.
    screen.getByRole("dialog", { description: "Move alpha to a group." });
  });

  it.each([
    ["Save click", "", "  work/api  ", "work/api", "click"],
    ["Enter", "old", "new", "new", "enter"],
    ["blank value ungroups", "work", "   ", "", "click"],
    ["unchanged value closes without saving", "work", "  work  ", null, "click"],
  ] as const)("%s: saves the trimmed value then closes", async (_name, current, typed, saved, via) => {
    const { input, saveBtn, onSave, onClose } = setup(current);
    fireEvent.change(input, { target: { value: typed } });
    if (via === "enter") {
      // The open list takes the first Enter; the second saves.
      fireEvent.keyDown(input, { key: "Enter" });
      fireEvent.keyDown(input, { key: "Enter" });
    } else fireEvent.click(saveBtn);
    if (saved == null) expect(onSave).not.toHaveBeenCalled();
    else {
      expect(onSave).toHaveBeenCalledTimes(1);
      expect(onSave).toHaveBeenCalledWith(saved);
    }
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it.each([
    ["keyCode 229", { key: "Enter", keyCode: 229 }],
    ["isComposing", { key: "Enter", isComposing: true }],
  ] as const)("ignores Enter while an IME composes (%s)", async (_n, init) => {
    const { input, onSave, onClose } = setup("old");
    fireEvent.change(input, { target: { value: "new" } });
    fireEvent.keyDown(input, init);
    expect(onSave).not.toHaveBeenCalled();
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onSave).toHaveBeenCalledWith("new");
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it.each([
    ["", "work", "Failed to update group."],
    ["work", "", "Failed to clear group."],
  ])("from %j to %j a failed save shows %j and stays open", async (current, typed, message) => {
    const { input, saveBtn, onClose } = setup(current, vi.fn().mockResolvedValue(false));
    fireEvent.change(input, { target: { value: typed } });
    input.blur();
    fireEvent.click(saveBtn);
    await waitFor(() => expect(errorText()).toBe(message));
    expect(onClose).not.toHaveBeenCalled();
    expect(saveBtn.disabled).toBe(false);
    expect(document.activeElement).toBe(input);
    expect(screen.queryByTestId("group-combobox-popup")).toBeNull();
    fireEvent.change(input, { target: { value: "again" } });
    expect(errorText()).toBeUndefined();
  });

  it("disables controls and ignores repeat saves while one is in flight", async () => {
    const { onSave, resolve } = pending();
    const { input, saveBtn, cancelBtn, onClose } = setup("", onSave);
    fireEvent.change(input, { target: { value: "work" } });
    fireEvent.keyDown(input, { key: "Enter" });
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
    // The first Escape closes the open list, the second the dialog.
    fireEvent.keyDown(input, { key: "Escape" });
    fireEvent.keyDown(input, { key: "Escape" });
    const backdrop = screen.getByTestId("session-group-modal");
    fireEvent.click(backdrop.firstElementChild!);
    expect(onClose).toHaveBeenCalledTimes(2);
    fireEvent.click(backdrop);
    expect(onClose).toHaveBeenCalledTimes(3);
    expect(onSave).not.toHaveBeenCalled();
  });

  it("lists existing groups and saves the picked path", async () => {
    vi.mocked(fetchGroups).mockResolvedValue([group("work/api"), group("play")]);
    const { input, saveBtn, onSave } = setup("");
    fireEvent.change(input, { target: { value: "wor" } });
    fireEvent.mouseDown(await screen.findByRole("option", { name: /work\/api/ }));
    fireEvent.click(saveBtn);
    expect(onSave).toHaveBeenCalledWith("work/api");
  });

  it("Remove from group saves a blank group and is absent without one", async () => {
    const { onSave, onClose, unmount } = setup("work");
    fireEvent.click(screen.getByRole("button", { name: "Remove from group" }));
    expect(onSave).toHaveBeenCalledWith("");
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
    unmount();
    setup("");
    expect(screen.queryByRole("button", { name: "Remove from group" })).toBeNull();
  });

  it("restores focus on unmount", () => {
    expectRestoresFocus(() => setup().unmount);
  });
});
