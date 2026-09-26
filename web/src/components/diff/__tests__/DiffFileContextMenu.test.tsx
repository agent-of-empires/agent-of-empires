// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { DiffFileContextMenu, type PathMenuState } from "../DiffFileContextMenu";
import { toastBus } from "../../../lib/toastBus";
import type { RichDiffFile } from "../../../lib/types";

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

const file = (over: Partial<RichDiffFile> = {}): RichDiffFile => ({
  path: "assets/logo.png",
  old_path: null,
  status: "modified",
  additions: 0,
  deletions: 0,
  ...over,
});

function openMenu(menu: Partial<PathMenuState> = {}, sessionId: string | null = "s1") {
  const onClose = vi.fn();
  render(
    <DiffFileContextMenu
      menu={{ x: 12, y: 34, path: "src/app/foo.rs", ...menu }}
      sessionId={sessionId}
      onClose={onClose}
    />,
  );
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

describe("DiffFileContextMenu", () => {
  it("renders nothing without a menu", () => {
    render(<DiffFileContextMenu menu={null} onClose={() => {}} />);
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

  it("opens the file's worktree copy from its own repo in a new tab", async () => {
    const tab = stubTab();
    const png = new Response("png", { headers: { "Content-Type": "image/png" } });
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(png));
    vi.stubGlobal("URL", { createObjectURL: vi.fn(() => "blob:logo"), revokeObjectURL: vi.fn() });
    const toast = stubToast();
    const target = file({ path: "assets/logo.png", repo_name: "web" });
    const onClose = openMenu({ path: target.path, file: target });

    const items = screen.getAllByRole("menuitem").map((b) => b.textContent);
    expect(items).toEqual(["Open file", "Copy relative path"]);
    fireEvent.click(screen.getByText("Open file"));
    // Opened within the click, before the fetch resolves.
    expect(window.open).toHaveBeenCalledWith("about:blank", "_blank");
    expect(onClose).toHaveBeenCalledTimes(1);
    await settle();
    expect(fetch).toHaveBeenCalledWith("/api/sessions/s1/diff/file/raw?path=assets%2Flogo.png&repo=web");
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
    openMenu({ path: "gone.bin", file: file({ path: "gone.bin" }) });
    fireEvent.click(screen.getByText("Open file"));
    await vi.waitFor(() => expect(toast.error).toHaveBeenCalledWith(message));
    expect(fetch).toHaveBeenCalledWith("/api/sessions/s1/diff/file/raw?path=gone.bin");
    expect(tab.close).toHaveBeenCalled();
  });

  it("keeps Open file visible but disabled for a deleted file, and still copies", async () => {
    vi.stubGlobal("open", vi.fn());
    const { writeText } = stubClipboard(true);
    openMenu({ path: "old.pdf", file: file({ path: "old.pdf", status: "deleted" }) });
    const openItem = screen.getByRole("menuitem", { name: "Open file" });
    expect(openItem).toHaveProperty("disabled", true);
    fireEvent.click(openItem);
    expect(window.open).not.toHaveBeenCalled();
    await clickCopy();
    expect(writeText).toHaveBeenCalledWith("old.pdf");
  });

  it.each<[string, Partial<PathMenuState>, string | null]>([
    ["a directory row", { path: "src/app" }, "s1"],
    ["a menu without a session", { file: file() }, null],
  ])("only copies for %s", (_, menu, sessionId) => {
    openMenu(menu, sessionId);
    expect(screen.getAllByRole("menuitem").map((b) => b.textContent)).toEqual(["Copy relative path"]);
  });
});
