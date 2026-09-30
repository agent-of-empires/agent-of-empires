// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { FileContextMenu, type PathMenuState } from "../FileContextMenu";
import { toastBus } from "../../../lib/toastBus";

function stubClipboard(secure: boolean, execResult = true) {
  Object.defineProperty(window, "isSecureContext", { value: secure, configurable: true });
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", { value: secure ? { writeText } : undefined, configurable: true });
  const execCommand = vi.fn().mockReturnValue(execResult);
  (document as unknown as { execCommand: typeof execCommand }).execCommand = execCommand;
  return { writeText, execCommand, toast: stubToast() };
}

function stubTab() {
  const tab = { location: { href: "" }, close: vi.fn() };
  const open = vi.fn(() => tab);
  vi.stubGlobal("open", open);
  return tab;
}

function stubToast() {
  const toast = { push: vi.fn(), info: vi.fn(), error: vi.fn() };
  toastBus.handler = toast;
  return toast;
}

function openMenu(menu: Partial<PathMenuState> = {}) {
  const onClose = vi.fn();
  render(<FileContextMenu menu={{ x: 12, y: 34, path: "src/app/foo.rs", ...menu }} onClose={onClose} />);
  return onClose;
}

const settle = () =>
  act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });

const clickCopy = async () => {
  fireEvent.click(screen.getByText("Copy relative path"));
  await settle();
};

// Dismiss listeners attach on the next frame so the opening right-click does not close the menu.
const flushFrame = () =>
  act(async () => {
    await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
  });

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  toastBus.handler = null;
});

describe("FileContextMenu", () => {
  it("renders nothing without a menu", () => {
    render(<FileContextMenu menu={null} onClose={() => {}} />);
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

  it("opens the file's URL in a new tab", async () => {
    const tab = stubTab();
    const png = new Response("png", { headers: { "Content-Type": "image/png" } });
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(png));
    vi.stubGlobal("URL", { createObjectURL: vi.fn(() => "blob:logo"), revokeObjectURL: vi.fn() });
    const toast = stubToast();
    const onClose = openMenu({ path: "assets/logo.png", open: { url: "/raw/logo.png" } });

    const items = screen.getAllByRole("menuitem").map((b) => b.textContent);
    expect(items).toEqual(["Open file", "Copy relative path"]);
    fireEvent.click(screen.getByText("Open file"));
    // Opened within the click, before the fetch resolves.
    expect(window.open).toHaveBeenCalledWith("about:blank", "_blank");
    expect(onClose).toHaveBeenCalledTimes(1);
    await settle();
    expect(fetch).toHaveBeenCalledWith("/raw/logo.png");
    await vi.waitFor(() => expect(tab.location.href).toBe("blob:logo"));
    expect(toast.error).not.toHaveBeenCalled();
  });

  it.each([
    [404, "gone.bin is not in the worktree"],
    [413, "gone.bin is too large to open (over 50 MiB)"],
    [500, "Couldn't open gone.bin"],
  ])("toasts why the file cannot be opened (HTTP %i)", async (status, message) => {
    const tab = stubTab();
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(null, { status })));
    const toast = stubToast();
    openMenu({ path: "gone.bin", open: { url: "/raw/gone.bin" } });
    fireEvent.click(screen.getByText("Open file"));
    await vi.waitFor(() => expect(toast.error).toHaveBeenCalledWith(message));
    expect(tab.close).toHaveBeenCalled();
  });

  it("keeps a disabled Open file visible, and still copies", async () => {
    vi.stubGlobal("open", vi.fn());
    const { writeText } = stubClipboard(true);
    openMenu({ path: "old.pdf", open: { url: "/raw/old.pdf", disabled: true } });
    const openItem = screen.getByRole("menuitem", { name: "Open file" });
    expect(openItem).toHaveProperty("disabled", true);
    fireEvent.click(openItem);
    expect(window.open).not.toHaveBeenCalled();
    await clickCopy();
    expect(writeText).toHaveBeenCalledWith("old.pdf");
  });

  it("only copies for a directory row", () => {
    openMenu({ path: "src/app" });
    expect(screen.getAllByRole("menuitem").map((b) => b.textContent)).toEqual(["Copy relative path"]);
  });
});
