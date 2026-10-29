// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, renderHook, screen, waitFor } from "@testing-library/react";
import { FileContentViewer } from "../FileContentViewer";
import * as api from "../../../lib/api";
import { useWebSettings } from "../../../hooks/useWebSettings";
import { fileDraftKey, setFileDraft } from "../../../lib/fileEditGuard";

vi.mock("../../../hooks/useShikiTheme", () => ({
  useShikiTheme: () => ({ theme: "github-dark", appearance: "dark" }),
}));

// Stubbed: the whole-file view renders through `@pierre/diffs`, which needs a
// real DOM and workers. This spec is about which branch is chosen, not paint.
vi.mock("../FullFileViewer", () => ({
  FullFileViewer: ({ content, filePath }: { content: string; filePath: string }) => (
    <div data-testid="full-file" data-path={filePath}>
      {content}
    </div>
  ),
}));

beforeEach(() => {
  window.localStorage.clear();
});
afterEach(() => {
  cleanup();
  setFileDraft(fileDraftKey("s1", "/repo/a.ts"), null);
  vi.restoreAllMocks();
  window.localStorage.clear();
});

describe("FileContentViewer", () => {
  it("renders a .md file as formatted markdown by default and toggles to raw", async () => {
    vi.spyOn(api, "getSessionFile").mockResolvedValue({
      content: "# Plan\n\nstep one",
      is_binary: false,
      truncated: false,
    });

    const { container } = render(<FileContentViewer sessionId="s1" filePath="/tmp/plan.md" />);
    await waitFor(() => {
      expect(container.querySelector("h1")?.textContent).toBe("Plan");
    });
    expect(screen.getByRole("button", { name: "Rendered" }).getAttribute("aria-pressed")).toBe("true");

    fireEvent.click(screen.getByRole("button", { name: "Raw" }));
    await waitFor(() => {
      // Raw mode renders the whole-file view, which shows the literal source
      // including the "#".
      expect(container.textContent).toContain("# Plan");
    });
    expect(container.querySelector("h1")).toBeNull();
  });

  it("renders a non-markdown file via the whole-file view (no toggle)", async () => {
    vi.spyOn(api, "getSessionFile").mockResolvedValue({
      content: "export const a = 1;",
      is_binary: false,
      truncated: false,
    });
    const { container } = render(<FileContentViewer sessionId="s1" filePath="/repo/a.ts" />);
    await waitFor(() => {
      expect(container.textContent).toContain("export const a = 1;");
    });
    expect(screen.queryByRole("button", { name: "Rendered" })).toBeNull();
  });

  it("hands a binary file to the binary view instead of the text view", async () => {
    vi.spyOn(api, "getSessionFile").mockResolvedValue({ content: "", is_binary: true, truncated: false });
    render(<FileContentViewer sessionId="s1" filePath="/tmp/a.zip" />);
    await screen.findByRole("button", { name: "Download" });
    expect(screen.queryByTestId("full-file")).toBeNull();
  });

  it("shows the filesystem restriction and disables editing when atomic replacement is unavailable", async () => {
    const reason = "This filesystem does not support atomic file replacement";
    vi.spyOn(api, "getSessionFile").mockResolvedValue({
      content: "original",
      version: "v1",
      is_binary: false,
      truncated: false,
      edit_disabled_reason: reason,
    });
    const settings = renderHook(() => useWebSettings());
    act(() => settings.result.current.update({ fileEditing: true }));
    render(<FileContentViewer sessionId="s1" filePath="/repo/a.ts" />);
    await screen.findByText("original");
    const edit = screen.getByRole("button", { name: "Edit", exact: true }) as HTMLButtonElement;
    expect(edit.disabled).toBe(true);
    expect(edit.title).toBe(reason);
    fireEvent.click(edit);
    expect(screen.queryByRole("textbox")).toBeNull();
  });

  it("parks a draft while editing is disabled and restores its opened version on re-enable", async () => {
    const original = { content: "original", version: "v1", is_binary: false, truncated: false };
    const get = vi.spyOn(api, "getSessionFile").mockResolvedValue(original);
    const save = vi.spyOn(api, "saveSessionFile").mockResolvedValue({ ok: true, status: 204, error: undefined });
    const settings = renderHook(() => useWebSettings());
    act(() => settings.result.current.update({ fileEditing: true }));
    render(<FileContentViewer sessionId="s1" filePath="/repo/a.ts" />);
    fireEvent.click(await screen.findByRole("button", { name: "Edit", exact: true }));
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "parked draft" } });
    get.mockResolvedValue({ ...original, content: "changed on disk", version: "v2" });
    act(() => settings.result.current.update({ fileEditing: false }));
    expect(screen.queryByRole("textbox")).toBeNull();
    expect(screen.queryByRole("button", { name: "Save", exact: true })).toBeNull();
    expect(screen.queryByRole("button", { name: "Edit", exact: true })).toBeNull();
    await screen.findByText("changed on disk");
    fireEvent.keyDown(window, { key: "s", ctrlKey: true });
    expect(save).not.toHaveBeenCalled();
    act(() => settings.result.current.update({ fileEditing: true }));
    const editor = screen.getByRole("textbox");
    expect((editor as HTMLTextAreaElement).value).toBe("parked draft");
    fireEvent.keyDown(editor, { key: "s", ctrlKey: true });
    await screen.findByText("File saved.");
    expect(save).toHaveBeenCalledWith("s1", "/repo/a.ts", "v1", "parked draft");
  });

  it("uses a late parked draft's opened version instead of the viewer's newer snapshot", async () => {
    const original = { content: "original", version: "v1", is_binary: false, truncated: false };
    vi.spyOn(api, "getSessionFile").mockResolvedValue({ ...original, content: "new disk content", version: "v2" });
    const save = vi.spyOn(api, "saveSessionFile").mockResolvedValue({ ok: true, status: 204, error: undefined });
    const settings = renderHook(() => useWebSettings());
    act(() => settings.result.current.update({ fileEditing: true }));
    render(<FileContentViewer sessionId="s1" filePath="/repo/a.ts" />);
    const edit = await screen.findByRole("button", { name: "Edit", exact: true });
    setFileDraft(fileDraftKey("s1", "/repo/a.ts"), { original, text: "parked edit", pendingSave: null });
    fireEvent.click(edit);
    expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe("parked edit");
    fireEvent.click(screen.getByRole("button", { name: "Save", exact: true }));
    await screen.findByText("File saved.");
    expect(save).toHaveBeenCalledWith("s1", "/repo/a.ts", "v1", "parked edit");
  });

  it("shows a parked draft only after editing is enabled", async () => {
    const original = { content: "original", version: "v1", is_binary: false, truncated: false };
    setFileDraft(fileDraftKey("s1", "/repo/a.ts"), { original, text: "restored draft", pendingSave: null });
    vi.spyOn(api, "getSessionFile").mockResolvedValue(original);
    const settings = renderHook(() => useWebSettings());
    render(<FileContentViewer sessionId="s1" filePath="/repo/a.ts" />);
    await screen.findByTestId("full-file");
    expect(screen.queryByRole("textbox")).toBeNull();
    act(() => settings.result.current.update({ fileEditing: true }));
    expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe("restored draft");
  });

  it("shows an error when the fetch fails", async () => {
    vi.spyOn(api, "getSessionFile").mockResolvedValue(null);
    render(<FileContentViewer sessionId="s1" filePath="/tmp/x.md" />);
    await screen.findByText("Failed to load file");
  });
});
