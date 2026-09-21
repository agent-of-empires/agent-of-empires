// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { CopyPathContextMenu } from "../CopyPathContextMenu";
import { toastBus } from "../../../lib/toastBus";

function stubClipboard(secure: boolean, execResult = true) {
  Object.defineProperty(window, "isSecureContext", { value: secure, configurable: true });
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", { value: secure ? { writeText } : undefined, configurable: true });
  const execCommand = vi.fn().mockReturnValue(execResult);
  (document as unknown as { execCommand: typeof execCommand }).execCommand = execCommand;
  const toast = { push: vi.fn(), info: vi.fn(), error: vi.fn() };
  toastBus.handler = toast;
  return { writeText, execCommand, toast };
}

function openMenu() {
  const onClose = vi.fn();
  render(<CopyPathContextMenu menu={{ x: 12, y: 34, path: "src/app/foo.rs" }} onClose={onClose} />);
  return onClose;
}

const clickCopy = async () => {
  fireEvent.click(screen.getByText("Copy relative path"));
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
};

// Dismiss listeners attach on the next frame so the opening right-click does not close the menu.
const flushFrame = () =>
  act(async () => {
    await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
  });

afterEach(() => {
  vi.restoreAllMocks();
  toastBus.handler = null;
});

describe("CopyPathContextMenu", () => {
  it("renders nothing without a menu", () => {
    render(<CopyPathContextMenu menu={null} onClose={() => {}} />);
    expect(screen.queryByText("Copy relative path")).toBeNull();
  });

  it("copies via the clipboard API, closes once, and confirms", async () => {
    const { writeText, toast } = stubClipboard(true);
    const onClose = openMenu();
    await flushFrame();
    await clickCopy();
    expect(writeText).toHaveBeenCalledWith("src/app/foo.rs");
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(toast.info).toHaveBeenCalledWith("Copied src/app/foo.rs");
    expect(toast.error).not.toHaveBeenCalled();
  });

  it.each([
    [true, "info", "Copied src/app/foo.rs"],
    [false, "error", "Couldn't copy path to clipboard"],
  ] as const)("falls back to execCommand in an insecure context (succeeds=%s)", async (ok, kind, message) => {
    const { execCommand, toast } = stubClipboard(false, ok);
    openMenu();
    await clickCopy();
    expect(execCommand).toHaveBeenCalledWith("copy");
    expect(toast[kind]).toHaveBeenCalledWith(message);
    expect(toast[kind === "info" ? "error" : "info"]).not.toHaveBeenCalled();
  });

  it.each<[string, () => void]>([
    ["an outside click", () => fireEvent.click(document.body)],
    ["Escape", () => fireEvent.keyDown(document, { key: "Escape" })],
    ["another right-click", () => fireEvent.contextMenu(document.body)],
  ])("closes on %s", async (_, dismiss) => {
    const onClose = openMenu();
    await flushFrame();
    dismiss();
    expect(onClose).toHaveBeenCalled();
  });
});
