// @vitest-environment jsdom
//
// Branch coverage for DiffFileViewer that the split-layout spec does not reach:
// the loading / error / no-contents / binary / truncated / no-changes states,
// the renamed-file header (old_path → path), the Find toggle and Cmd+F
// shortcut, the stale-comments block, active comment annotations, and the
// draft-comment flow (line selection -> CommentForm -> save / cancel).
//
// The Pierre renderer and worker pool never run under jsdom, so
// `@pierre/diffs/react` is mocked with a stand-in that exposes the passed
// annotations and lets a test fire `options.onLineSelected` to start a draft.
// Comment anchoring is mocked so each test can pick the active / stale shape it
// needs without constructing real diff metadata.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { DiffFileViewer } from "../DiffFileViewer";
import type { RichFileContentsResponse } from "../../../lib/types";
import type { UseDiffCommentsResult } from "../../../hooks/useDiffComments";

const mock = vi.hoisted(() => ({
  contents: undefined as RichFileContentsResponse | undefined,
  loading: false,
  error: null as string | null,
  anchored: [] as Array<{ status: "active" | "stale"; comment: Record<string, unknown> }>,
  snippet: "captured" as string | null,
}));

vi.mock("../../../hooks/useFileContents", () => ({
  useFileContents: () => ({
    contents: mock.contents,
    loading: mock.loading,
    error: mock.error,
    refresh: vi.fn(),
  }),
}));

vi.mock("../../../hooks/useShikiTheme", () => ({
  useShikiTheme: () => ({ theme: "dark" }),
}));

vi.mock("../comments/anchorToContents", () => ({
  anchorCommentsToContents: () => mock.anchored,
}));

vi.mock("../comments/extractSnippetFromContents", () => ({
  extractSnippetFromContents: () => mock.snippet,
}));

vi.mock("../comments/language", () => ({
  extensionToLanguage: () => "typescript",
}));

// Stand-ins for the comment / find subcomponents so we can observe props and
// fire their callbacks without rendering the real (heavier) UI.
vi.mock("../comments/CommentCard", () => ({
  CommentCard: ({ anchored }: { anchored: { comment: { id: string } } }) => (
    <div data-testid="comment-card">card:{anchored.comment.id}</div>
  ),
}));

vi.mock("../comments/CommentForm", () => ({
  CommentForm: ({ onSave, onCancel }: { onSave: (b: string) => void; onCancel: () => void }) => (
    <div data-testid="comment-form">
      <button onClick={() => onSave("hi")}>form-save</button>
      <button onClick={onCancel}>form-cancel</button>
    </div>
  ),
}));

vi.mock("../find/FindBar", () => ({
  FindBar: ({ onClose }: { onClose: () => void }) => (
    <div data-testid="find-bar">
      <button onClick={onClose}>close-find</button>
    </div>
  ),
}));

vi.mock("../find/changedLines", () => ({
  changedLines: () => [],
}));

vi.mock("../pierre/DiffWorkerPoolProvider", () => ({
  DiffWorkerPoolProvider: ({ children }: { children: React.ReactNode }) => <>{children}</>,
}));

vi.mock("@pierre/diffs", () => ({
  processFile: () => ({ name: "parsed" }),
}));

// FileDiff stand-in: render the annotations it was handed (so card / form
// branches surface) and expose a button that fires onLineSelected to start a
// comment draft.
type Ann = { metadata: { kind: "card" | "form"; anchored?: { comment: { id: string } } } };
vi.mock("@pierre/diffs/react", () => ({
  FileDiff: ({
    options,
    lineAnnotations,
    renderAnnotation,
  }: {
    options: { diffStyle: string; onLineSelected: (r: unknown) => void; enableLineSelection: boolean };
    lineAnnotations: Ann[];
    renderAnnotation: (a: Ann) => React.ReactNode;
  }) => (
    <div
      data-testid="pierre-diff"
      data-diff-style={options.diffStyle}
      data-selection={String(options.enableLineSelection)}
    >
      <button
        data-testid="select-line"
        onClick={() => options.onLineSelected({ start: 2, end: 3, side: "additions", endSide: "additions" })}
      >
        select
      </button>
      {lineAnnotations.map((a, i) => (
        <div key={i}>{renderAnnotation(a)}</div>
      ))}
    </div>
  ),
  Virtualizer: ({ children }: { children: React.ReactNode }) => <div data-testid="virtualizer">{children}</div>,
}));

const baseContents: RichFileContentsResponse = {
  file: { path: "a.ts", old_path: null, status: "modified", additions: 2, deletions: 1 },
  old_content: "ctx\nold\n",
  new_content: "ctx\nnew\n",
  patch: "--- a/a.ts\n+++ b/a.ts\n@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n",
  is_binary: false,
  truncated: false,
};

function commentsStore(): UseDiffCommentsResult {
  return {
    comments: [],
    addComment: vi.fn(),
    updateComment: vi.fn(),
    deleteComment: vi.fn(),
    clearAfterSend: false,
    setClearAfterSend: vi.fn(),
    introDraft: "",
    outroDraft: "",
    setIntroDraft: vi.fn(),
    setOutroDraft: vi.fn(),
    clearAll: vi.fn(),
  } as unknown as UseDiffCommentsResult;
}

beforeEach(() => {
  window.localStorage.clear();
  mock.contents = baseContents;
  mock.loading = false;
  mock.error = null;
  mock.anchored = [];
  mock.snippet = "captured";
  // A generous width so split layout can engage if selected.
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
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  window.localStorage.clear();
});

describe("DiffFileViewer status states", () => {
  it("shows the loading screen when loading with no contents", () => {
    mock.contents = undefined;
    mock.loading = true;
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" />);
    expect(screen.getByText("Loading diff...")).toBeTruthy();
  });

  it("shows the error message when the fetch fails", () => {
    mock.contents = undefined;
    mock.error = "boom";
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" />);
    expect(screen.getByText("boom")).toBeTruthy();
  });

  it("prompts to select a file when there are no contents and no error", () => {
    mock.contents = undefined;
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" />);
    expect(screen.getByText("Select a file to view changes")).toBeTruthy();
  });

  it("renders the binary-file placeholder", () => {
    mock.contents = { ...baseContents, is_binary: true };
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" />);
    expect(screen.getByText("Binary file changed")).toBeTruthy();
  });

  it("renders the too-large placeholder when truncated", () => {
    mock.contents = { ...baseContents, truncated: true };
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" />);
    expect(screen.getByText("File too large to diff inline")).toBeTruthy();
  });

  it("renders the no-changes placeholder when old and new contents match", () => {
    mock.contents = { ...baseContents, old_content: "same\n", new_content: "same\n" };
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" />);
    expect(screen.getByText("No changes in this file")).toBeTruthy();
  });
});

describe("DiffFileViewer header", () => {
  it("renders the status label and +/- counts", () => {
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" />);
    expect(screen.getByText("Modified")).toBeTruthy();
    expect(screen.getByText("+2")).toBeTruthy();
    expect(screen.getByText("-1")).toBeTruthy();
  });

  it("renders the rename arrow for a renamed file", () => {
    mock.contents = {
      ...baseContents,
      file: { path: "new.ts", old_path: "old.ts", status: "renamed", additions: 0, deletions: 0 },
    };
    render(<DiffFileViewer sessionId="s1" filePath="new.ts" />);
    expect(screen.getByText("Renamed")).toBeTruthy();
    expect(screen.getByText("old.ts → new.ts")).toBeTruthy();
  });

  it("invokes onClose from the Terminal back button", () => {
    const onClose = vi.fn();
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" onClose={onClose} />);
    fireEvent.click(screen.getByRole("button", { name: "Back to terminal" }));
    expect(onClose).toHaveBeenCalled();
  });

  it("does not render a back button without onClose", () => {
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" />);
    expect(screen.queryByRole("button", { name: "Back to terminal" })).toBeNull();
  });
});

describe("DiffFileViewer find", () => {
  it("toggles the FindBar via the Find button", () => {
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" />);
    expect(screen.queryByTestId("find-bar")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Find in diff" }));
    expect(screen.getByTestId("find-bar")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Find in diff" }).getAttribute("aria-pressed")).toBe("true");
    // Close via the FindBar's own close callback.
    fireEvent.click(screen.getByText("close-find"));
    expect(screen.queryByTestId("find-bar")).toBeNull();
  });

  it("opens find on Cmd/Ctrl+F", () => {
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" />);
    const root = screen.getByText("Modified").closest("div")!.parentElement!;
    fireEvent.keyDown(root, { key: "f", metaKey: true });
    expect(screen.getByTestId("find-bar")).toBeTruthy();
  });

  it("does not show the FindBar for a binary file even when toggled open", () => {
    mock.contents = { ...baseContents, is_binary: true };
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" />);
    fireEvent.click(screen.getByRole("button", { name: "Find in diff" }));
    expect(screen.queryByTestId("find-bar")).toBeNull();
  });
});

describe("DiffFileViewer comments", () => {
  it("renders the stale-comments block with a count", () => {
    mock.anchored = [
      { status: "stale", comment: { id: "c1", side: "new", endLine: 1 } },
      { status: "stale", comment: { id: "c2", side: "new", endLine: 2 } },
    ];
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" commentsEnabled commentsStore={commentsStore()} />);
    expect(screen.getByText(/2 stale comments/)).toBeTruthy();
    expect(screen.getAllByTestId("comment-card").length).toBe(2);
  });

  it("renders active comment annotations through the Pierre renderer", () => {
    mock.anchored = [{ status: "active", comment: { id: "c9", side: "new", endLine: 3 } }];
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" commentsEnabled commentsStore={commentsStore()} />);
    expect(screen.getByText("card:c9")).toBeTruthy();
    // Line selection is enabled when comments are active.
    expect(screen.getByTestId("pierre-diff").getAttribute("data-selection")).toBe("true");
  });

  it("starts a draft on line selection, then saves it through the store", () => {
    const store = commentsStore();
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" commentsEnabled commentsStore={store} />);
    fireEvent.click(screen.getByTestId("select-line"));
    expect(screen.getByTestId("comment-form")).toBeTruthy();
    fireEvent.click(screen.getByText("form-save"));
    expect(store.addComment).toHaveBeenCalledWith(
      expect.objectContaining({ filePath: "a.ts", side: "new", startLine: 2, endLine: 3, body: "hi" }),
    );
  });

  it("cancels a draft, removing the form", () => {
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" commentsEnabled commentsStore={commentsStore()} />);
    fireEvent.click(screen.getByTestId("select-line"));
    expect(screen.getByTestId("comment-form")).toBeTruthy();
    fireEvent.click(screen.getByText("form-cancel"));
    expect(screen.queryByTestId("comment-form")).toBeNull();
  });

  it("does not start a draft when snippet extraction returns null", () => {
    mock.snippet = null;
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" commentsEnabled commentsStore={commentsStore()} />);
    fireEvent.click(screen.getByTestId("select-line"));
    expect(screen.queryByTestId("comment-form")).toBeNull();
  });

  it("leaves line selection off when comments are disabled and find is closed", () => {
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" />);
    expect(screen.getByTestId("pierre-diff").getAttribute("data-selection")).toBe("false");
  });
});
