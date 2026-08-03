// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { EmptyTrashConfirm } from "../EmptyTrashConfirm";

afterEach(cleanup);

describe("EmptyTrashConfirm (#3167)", () => {
  it("fires onConfirm once under a synchronous double confirm (firedRef guard)", () => {
    // Rendered standalone with an inert onConfirm that does not unmount, so the
    // confirm button stays mounted and a second synchronous click re-enters
    // confirm(); firedRef swallows it. Through the real sidebar this is
    // unreachable because onEmptyTrash unmounts the dialog on the first confirm.
    const onConfirm = vi.fn();
    render(<EmptyTrashConfirm sessionCount={2} onConfirm={onConfirm} onCancel={vi.fn()} />);

    const button = screen.getByTestId("empty-trash-confirm");
    fireEvent.click(button);
    fireEvent.click(button);

    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it("Escape calls onCancel", () => {
    const onCancel = vi.fn();
    render(<EmptyTrashConfirm sessionCount={2} onConfirm={vi.fn()} onCancel={onCancel} />);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it("Enter dispatched from the dialog body calls onConfirm", () => {
    // Fire from document (not the auto-focused confirm button), whose tagName is
    // undefined, so the INPUT/TEXTAREA/BUTTON guard is skipped and the keydown
    // path reaches confirm(). On the focused button, Enter is the native click.
    const onConfirm = vi.fn();
    render(<EmptyTrashConfirm sessionCount={2} onConfirm={onConfirm} onCancel={vi.fn()} />);
    fireEvent.keyDown(document, { key: "Enter" });
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });
});
