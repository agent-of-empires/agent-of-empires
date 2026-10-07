// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { FilesPane } from "../FilesPane";
import * as api from "../../lib/api";

const filesMock = vi.hoisted(() => ({
  files: [] as string[],
  loading: false,
  error: false,
  reload: vi.fn(),
}));

vi.mock("../acp/useFilesIndex", () => ({
  useFilesIndex: () => filesMock,
  fuzzyFilter: <T,>(items: T[]) => items,
}));

const openInNewTab = vi.hoisted(() => vi.fn());
vi.mock("../../lib/openInNewTab", () => ({ openInNewTab }));

vi.mock("../../hooks/useShikiTheme", () => ({
  useShikiTheme: () => ({ theme: "github-dark", appearance: "dark" }),
}));
vi.mock("../../lib/snippetHighlighter", () => ({
  highlightSnippet: vi.fn().mockResolvedValue(null),
}));

beforeEach(() => {
  window.localStorage.clear();
  filesMock.files = ["docs/plan.md", "src/main.rs", "notes.md"];
  filesMock.loading = false;
  filesMock.error = false;
  filesMock.reload = vi.fn();
  openInNewTab.mockReset().mockResolvedValue({ ok: true });
});
afterEach(() => {
  vi.restoreAllMocks();
});

describe("FilesPane", () => {
  it("lists project files and filters them", () => {
    render(<FilesPane sessionId="s1" />);
    expect(screen.getByRole("button", { name: "docs/plan.md" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "src/main.rs" })).toBeTruthy();

    fireEvent.change(screen.getByLabelText("Filter files"), { target: { value: ".md" } });
    expect(screen.getByRole("button", { name: "docs/plan.md" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "src/main.rs" })).toBeNull();
  });

  it("opens a file in the viewer and returns focus to its row on close", async () => {
    vi.spyOn(api, "getSessionFile").mockResolvedValue({
      content: "# Plan",
      is_binary: false,
      truncated: false,
    });
    const { container } = render(<FilesPane sessionId="s1" />);
    fireEvent.click(screen.getByRole("button", { name: "docs/plan.md" }));
    await waitFor(() => {
      expect(container.querySelector("h1")?.textContent).toBe("Plan");
    });
    expect(api.getSessionFile).toHaveBeenCalledWith("s1", "docs/plan.md");

    fireEvent.click(screen.getByRole("button", { name: "Back to files" }));
    // The row the user opened regains focus, not the top of the pane.
    await waitFor(() => {
      expect(document.activeElement).toBe(screen.getByRole("button", { name: "docs/plan.md" }));
    });
  });

  it("opens a row's file in a new tab from its context menu without selecting it", () => {
    render(<FilesPane sessionId="s1" />);
    fireEvent.contextMenu(screen.getByRole("button", { name: "docs/plan.md" }));
    expect(screen.getAllByRole("menuitem").map((b) => b.textContent)).toEqual(["Open file", "Copy relative path"]);
    fireEvent.click(screen.getByRole("menuitem", { name: "Open file" }));
    expect(openInNewTab).toHaveBeenCalledWith("/api/sessions/s1/file/raw?path=docs%2Fplan.md", "plan.md");
    expect(screen.queryByRole("button", { name: "Back to files" })).toBeNull();
  });

  it("distinguishes a failed fetch from an empty session, and retries", () => {
    filesMock.files = [];
    filesMock.error = true;
    render(<FilesPane sessionId="s1" />);

    // Must not read as "this session has no files".
    expect(screen.getByText("Could not load the file list")).toBeTruthy();
    expect(screen.queryByText("No files in this session")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(filesMock.reload).toHaveBeenCalled();
    cleanup();

    filesMock.error = false;
    render(<FilesPane sessionId="s1" />);
    expect(screen.getByText("No files in this session")).toBeTruthy();
    expect(screen.queryByText("Could not load the file list")).toBeNull();
  });
});
