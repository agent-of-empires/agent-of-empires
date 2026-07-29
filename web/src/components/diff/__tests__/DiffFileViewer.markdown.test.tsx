// @vitest-environment jsdom
//
// Covers the Rendered/Raw Markdown toggle in DiffFileViewer (#3088): a `.md`
// file renders formatted by default, the toggle flips back to the diff and
// persists via useWebSettings, and non-Markdown files expose no toggle.
//
// The Pierre renderer is mocked (as in DiffFileViewer.split.test.tsx) since it
// needs real DOM + workers; MarkdownFileView (react-markdown) runs for real.

import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DiffFileViewer } from "../DiffFileViewer";
import type { RichFileContentsResponse } from "../../../lib/types";

const mdContents: RichFileContentsResponse = {
  file: { path: "notes.md", old_path: null, status: "modified", additions: 1, deletions: 0 },
  old_content: "old line\n",
  new_content: "# Heading\n\nbody text\n",
  patch: "--- a/notes.md\n+++ b/notes.md\n@@ -1 +1,3 @@\n-old line\n+# Heading\n+\n+body text\n",
  is_binary: false,
  truncated: false,
};

const tsContents: RichFileContentsResponse = {
  file: { path: "a.ts", old_path: null, status: "modified", additions: 1, deletions: 1 },
  old_content: "old\n",
  new_content: "new\n",
  patch: "--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n-old\n+new\n",
  is_binary: false,
  truncated: false,
};

const mock = vi.hoisted(() => ({ contents: undefined as RichFileContentsResponse | undefined }));

vi.mock("../../../hooks/useFileContents", () => ({
  useFileContents: () => ({
    contents: mock.contents,
    loading: mock.contents === undefined,
    error: null,
    refresh: vi.fn(),
  }),
}));

vi.mock("@pierre/diffs/react", () => ({
  FileDiff: ({ fileDiff }: { fileDiff: { name: string } }) => <div data-testid="pierre-diff">{fileDiff.name}</div>,
  Virtualizer: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
  WorkerPoolContextProvider: ({ children }: { children: React.ReactNode }) => <>{children}</>,
}));

beforeEach(() => {
  window.localStorage.clear();
  class WideRO {
    cb: ResizeObserverCallback;
    constructor(cb: ResizeObserverCallback) {
      this.cb = cb;
    }
    observe() {
      this.cb([{ contentRect: { width: 1000 } } as ResizeObserverEntry], this as unknown as ResizeObserver);
    }
    unobserve() {}
    disconnect() {}
  }
  vi.stubGlobal("ResizeObserver", WideRO);
});

afterEach(() => {
  vi.unstubAllGlobals();
  window.localStorage.clear();
});

describe("DiffFileViewer markdown toggle", () => {
  it("renders a .md file as formatted markdown by default and hides diff controls", async () => {
    mock.contents = mdContents;
    const { container } = render(<DiffFileViewer sessionId="s1" filePath="notes.md" />);

    await waitFor(() => {
      expect(container.querySelector("h1")?.textContent).toBe("Heading");
    });
    // Rendered mode replaces the diff and suppresses diff-only controls.
    expect(screen.queryByTestId("pierre-diff")).toBeNull();
    expect(screen.queryByRole("button", { name: "Split" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Find in diff" })).toBeNull();
    expect(screen.getByRole("button", { name: "Rendered" }).getAttribute("aria-pressed")).toBe("true");
  });

  it("flips to Raw, shows the diff, and persists the preference", async () => {
    mock.contents = mdContents;
    render(<DiffFileViewer sessionId="s1" filePath="notes.md" />);
    await screen.findByRole("button", { name: "Raw" });

    fireEvent.click(screen.getByRole("button", { name: "Raw" }));

    await waitFor(() => {
      expect(screen.getByTestId("pierre-diff")).toBeTruthy();
    });
    expect(screen.getByRole("button", { name: "Split" })).toBeTruthy();
    expect(JSON.parse(window.localStorage.getItem("aoe-web-settings") ?? "{}").markdownPreview).toBe("raw");
  });

  it("shows no Rendered/Raw toggle for a non-markdown file", async () => {
    mock.contents = tsContents;
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" />);
    await screen.findByText(/Modified/i);
    expect(screen.queryByRole("button", { name: "Rendered" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Raw" })).toBeNull();
  });
});
