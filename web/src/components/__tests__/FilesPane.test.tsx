// @vitest-environment jsdom
//
// FilesPane contract (#3088): lists a session's project files, filters them,
// and opens one in the FileContentViewer on click.

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

vi.mock("../../hooks/useShikiTheme", () => ({
  useShikiTheme: () => ({ theme: "github-dark", appearance: "dark" }),
}));
vi.mock("../../lib/highlighter", () => ({
  ensureThemeLoaded: vi.fn().mockResolvedValue("github-dark"),
  getHighlighter: vi.fn().mockResolvedValue({ codeToHtml: (c: string) => `<pre>${c}</pre>` }),
  langKeyForExt: (s: string) => s,
  loadLanguage: vi.fn().mockResolvedValue(undefined),
}));

beforeEach(() => {
  window.localStorage.clear();
  filesMock.files = ["docs/plan.md", "src/main.rs", "notes.md"];
  filesMock.loading = false;
  filesMock.error = false;
  filesMock.reload = vi.fn();
});
afterEach(() => {
  cleanup();
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

  it("opens a file in the viewer on click", async () => {
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
  });

  it("shows an empty state without a session", () => {
    render(<FilesPane sessionId={null} />);
    expect(screen.getByText("No active session")).toBeTruthy();
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
  });

  it("says the session is empty when the fetch succeeded with no files", () => {
    filesMock.files = [];
    filesMock.error = false;
    render(<FilesPane sessionId="s1" />);
    expect(screen.getByText("No files in this session")).toBeTruthy();
    expect(screen.queryByText("Could not load the file list")).toBeNull();
  });

  it("returns focus to the opened row when the viewer closes", async () => {
    vi.spyOn(api, "getSessionFile").mockResolvedValue({
      content: "# Plan",
      is_binary: false,
      truncated: false,
    });
    render(<FilesPane sessionId="s1" />);

    fireEvent.click(screen.getByRole("button", { name: "docs/plan.md" }));
    const back = await screen.findByRole("button", { name: "Back to files" });
    fireEvent.click(back);

    // The row the user opened regains focus, not the top of the pane.
    await waitFor(() => {
      expect(document.activeElement).toBe(screen.getByRole("button", { name: "docs/plan.md" }));
    });
  });
});
