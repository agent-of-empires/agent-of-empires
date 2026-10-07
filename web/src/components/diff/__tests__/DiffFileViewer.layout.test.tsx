// @vitest-environment jsdom

import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DiffFileViewer } from "../DiffFileViewer";
import type { RichFileContentsResponse } from "../../../lib/types";

const tsContents: RichFileContentsResponse = {
  file: { path: "a.ts", old_path: null, status: "modified", additions: 1, deletions: 1 },
  old_content: "ctx\nold\n",
  new_content: "ctx\nnew\n",
  // Server-computed unified diff (similar-crate format).
  patch: "--- a/a.ts\n+++ b/a.ts\n@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n",
  is_binary: false,
  truncated: false,
};

const mdContents: RichFileContentsResponse = {
  file: { path: "notes.md", old_path: null, status: "modified", additions: 1, deletions: 0 },
  old_content: "old line\n",
  new_content: "# Heading\n\nbody text\n",
  patch: "--- a/notes.md\n+++ b/notes.md\n@@ -1 +1,3 @@\n-old line\n+# Heading\n+\n+body text\n",
  is_binary: false,
  truncated: false,
};

const mock = vi.hoisted(() => ({
  contents: undefined as RichFileContentsResponse | undefined,
  observe: vi.fn(),
}));

vi.mock("../../../hooks/useFileContents", () => ({
  useFileContents: () => ({
    contents: mock.contents,
    loading: mock.contents === undefined,
    error: null,
    refresh: vi.fn(),
  }),
}));

// Stand in for the Pierre renderer: surface the diffStyle on a data attribute
// and render the file name so the header assertions still resolve.
vi.mock("@pierre/diffs/react", () => ({
  FileDiff: ({ options, fileDiff }: { options: { diffStyle: string }; fileDiff: { name: string } }) => (
    <div data-testid="pierre-diff" data-diff-style={options.diffStyle}>
      {fileDiff.name}
    </div>
  ),
  Virtualizer: ({ children }: { children: React.ReactNode }) => <div data-testid="virtualizer">{children}</div>,
  WorkerPoolContextProvider: ({ children }: { children: React.ReactNode }) => <>{children}</>,
}));

beforeEach(() => {
  window.localStorage.clear();
  mock.contents = tsContents;
  mock.observe.mockClear();
  class WideRO {
    cb: ResizeObserverCallback;
    constructor(cb: ResizeObserverCallback) {
      this.cb = cb;
    }
    observe(el: Element) {
      mock.observe(el);
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

const splitButton = () => screen.getByRole("button", { name: "Split" });

describe("DiffFileViewer split layout", () => {
  it("defaults to unified with no markdown toggle, then switches to split and persists it", async () => {
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" />);
    await screen.findByText(/Modified/i);
    expect(splitButton().getAttribute("aria-pressed")).toBe("false");
    expect(screen.getByTestId("pierre-diff").getAttribute("data-diff-style")).toBe("unified");
    expect(screen.queryByRole("button", { name: "Rendered" })).toBeNull();

    fireEvent.click(splitButton());

    await waitFor(() => expect(splitButton().getAttribute("aria-pressed")).toBe("true"));
    expect(screen.getByTestId("pierre-diff").getAttribute("data-diff-style")).toBe("split");
    expect(JSON.parse(window.localStorage.getItem("aoe-web-settings") ?? "{}").diffViewLayout).toBe("split");
  });

  it("attaches the width observer when the diff container mounts after loading", async () => {
    mock.contents = undefined;
    const { rerender } = render(<DiffFileViewer sessionId="s1" filePath="a.ts" />);
    expect(mock.observe).not.toHaveBeenCalled();

    mock.contents = tsContents;
    rerender(<DiffFileViewer sessionId="s1" filePath="a.ts" />);
    await screen.findByText(/Modified/i);
    expect(mock.observe).toHaveBeenCalled();
  });
});

describe("DiffFileViewer markdown toggle", () => {
  it("renders a .md file as markdown without diff controls, then flips to Raw and persists it", async () => {
    mock.contents = mdContents;
    const { container } = render(<DiffFileViewer sessionId="s1" filePath="notes.md" />);

    await waitFor(() => expect(container.querySelector("h1")?.textContent).toBe("Heading"));
    // Rendered mode replaces the diff and suppresses diff-only controls.
    expect(screen.queryByTestId("pierre-diff")).toBeNull();
    expect(screen.queryByRole("button", { name: "Split" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Find in diff" })).toBeNull();
    expect(screen.getByRole("button", { name: "Rendered" }).getAttribute("aria-pressed")).toBe("true");

    fireEvent.click(screen.getByRole("button", { name: "Raw" }));

    await waitFor(() => expect(screen.getByTestId("pierre-diff")).toBeTruthy());
    expect(splitButton()).toBeTruthy();
    expect(JSON.parse(window.localStorage.getItem("aoe-web-settings") ?? "{}").markdownPreview).toBe("raw");
  });
});
