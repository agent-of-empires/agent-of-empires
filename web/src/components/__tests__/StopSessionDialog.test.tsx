// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { StopSessionDialog } from "../StopSessionDialog";
import { expectRestoresFocus } from "./dialogTestUtils";

function setup(onConfirm: () => Promise<void> = vi.fn().mockResolvedValue(undefined)) {
  const onCancel = vi.fn();
  const utils = render(<StopSessionDialog sessionTitle="my-session" onConfirm={onConfirm} onCancel={onCancel} />);
  return { ...utils, onConfirm, onCancel, stop: screen.getByRole("button", { name: "Stop" }) };
}

afterEach(cleanup);

describe("StopSessionDialog", () => {
  it("focuses Stop, confirms on Enter only once while in flight, and cancels on Escape", () => {
    let resolve = () => {};
    const onConfirm = vi.fn(() => new Promise<void>((r) => (resolve = r)));
    const { stop, onCancel } = setup(onConfirm);
    expect(document.activeElement).toBe(stop);
    fireEvent.keyDown(document, { key: "Enter" });
    fireEvent.keyDown(document, { key: "Enter" });
    expect(onConfirm).toHaveBeenCalledTimes(1);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onCancel).toHaveBeenCalledTimes(1);
    resolve();
  });

  it("leaves Enter on a focused button to its native click", () => {
    const { stop, onConfirm, onCancel } = setup();
    fireEvent.keyDown(stop, { key: "Enter" });
    fireEvent.click(stop);
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

  it("is a modal dialog named by its title and restores focus on unmount", () => {
    expect(
      setup()
        .getByRole("dialog", { name: /Stop Session/ })
        .getAttribute("aria-modal"),
    ).toBe("true");
    cleanup();
    expectRestoresFocus(() => setup().unmount);
  });
});
