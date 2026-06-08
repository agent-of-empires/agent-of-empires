// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { CopyPathContextMenu } from "../CopyPathContextMenu";
import { toastBus } from "../../../lib/toastBus";

function setSecureContext(secure: boolean) {
  Object.defineProperty(window, "isSecureContext", {
    value: secure,
    configurable: true,
  });
}

function stubClipboard() {
  setSecureContext(true);
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", {
    value: { writeText },
    configurable: true,
  });
  return writeText;
}

function stubToasts() {
  const info = vi.fn();
  const error = vi.fn();
  toastBus.handler = { push: vi.fn(), info, error };
  return { info, error };
}

// The dismiss listeners attach on the next animation frame (so the opening
// right-click doesn't immediately re-trigger them); wait one frame before
// dispatching events that should close the menu.
async function flushFrame() {
  await act(async () => {
    await new Promise<void>((resolve) =>
      requestAnimationFrame(() => resolve()),
    );
  });
}

// The copy path resolves a promise before toasting; flush microtasks so the
// toast assertion sees the settled result.
async function flushMicrotasks() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

function openMenu(onClose: () => void) {
  return render(
    <CopyPathContextMenu
      menu={{ x: 12, y: 34, path: "src/app/foo.rs" }}
      onClose={onClose}
    />,
  );
}

afterEach(() => {
  vi.restoreAllMocks();
  toastBus.handler = null;
});

describe("CopyPathContextMenu", () => {
  it("renders nothing when there is no open menu", () => {
    const { container } = render(
      <CopyPathContextMenu menu={null} onClose={() => {}} />,
    );
    expect(container.querySelector("button")).toBeNull();
    expect(screen.queryByText("Copy relative path")).toBeNull();
  });

  it("copies the path to the clipboard and closes on click", () => {
    const writeText = stubClipboard();
    const onClose = vi.fn();
    openMenu(onClose);

    fireEvent.click(screen.getByText("Copy relative path"));

    expect(writeText).toHaveBeenCalledWith("src/app/foo.rs");
    expect(onClose).toHaveBeenCalled();
  });

  it("toasts a confirmation after a successful copy", async () => {
    stubClipboard();
    const { info, error } = stubToasts();
    openMenu(vi.fn());

    fireEvent.click(screen.getByText("Copy relative path"));
    await flushMicrotasks();

    expect(info).toHaveBeenCalledWith("Copied src/app/foo.rs");
    expect(error).not.toHaveBeenCalled();
  });

  it("falls back to execCommand and toasts when the clipboard API is unavailable", async () => {
    // Insecure context (plain-HTTP `aoe serve`): navigator.clipboard is absent.
    setSecureContext(false);
    Object.defineProperty(navigator, "clipboard", {
      value: undefined,
      configurable: true,
    });
    const execCommand = vi.fn().mockReturnValue(true);
    // jsdom has no execCommand by default; install a stub.
    (document as unknown as { execCommand: typeof execCommand }).execCommand =
      execCommand;
    const { info, error } = stubToasts();
    openMenu(vi.fn());

    fireEvent.click(screen.getByText("Copy relative path"));
    await flushMicrotasks();

    expect(execCommand).toHaveBeenCalledWith("copy");
    expect(info).toHaveBeenCalledWith("Copied src/app/foo.rs");
    expect(error).not.toHaveBeenCalled();
  });

  it("toasts an error when copying fails entirely", async () => {
    setSecureContext(false);
    Object.defineProperty(navigator, "clipboard", {
      value: undefined,
      configurable: true,
    });
    (document as unknown as { execCommand: () => boolean }).execCommand = vi
      .fn()
      .mockReturnValue(false);
    const { info, error } = stubToasts();
    openMenu(vi.fn());

    fireEvent.click(screen.getByText("Copy relative path"));
    await flushMicrotasks();

    expect(error).toHaveBeenCalledWith("Couldn't copy path to clipboard");
    expect(info).not.toHaveBeenCalled();
  });

  it("closes on an outside click once the listeners attach", async () => {
    const onClose = vi.fn();
    openMenu(onClose);
    await flushFrame();

    fireEvent.click(document.body);
    expect(onClose).toHaveBeenCalled();
  });

  it("closes on Escape", async () => {
    const onClose = vi.fn();
    openMenu(onClose);
    await flushFrame();

    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalled();
  });

  it("closes on another right-click", async () => {
    const onClose = vi.fn();
    openMenu(onClose);
    await flushFrame();

    fireEvent.contextMenu(document.body);
    expect(onClose).toHaveBeenCalled();
  });

  it("does not double-close when the click lands inside the menu", async () => {
    const writeText = stubClipboard();
    const onClose = vi.fn();
    openMenu(onClose);
    await flushFrame();

    // The document listener must skip clicks inside the menu (menuRef guard);
    // the item's own onClick performs the copy + the single close.
    fireEvent.click(screen.getByText("Copy relative path"));
    expect(writeText).toHaveBeenCalledWith("src/app/foo.rs");
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});
