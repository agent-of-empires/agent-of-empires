// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { SplitHunkView } from "../SplitHunkView";
import type { RichDiffHunk } from "../../../lib/types";
import type { AnchoredComment } from "../comments/types";

// A modify row: delete at line index 0 (old side), add at index 1 (new side).
const modifyHunk: RichDiffHunk = {
  old_start: 1,
  old_lines: 1,
  new_start: 1,
  new_lines: 1,
  lines: [
    {
      type: "delete",
      old_line_num: 1,
      new_line_num: null,
      content: "old line\n",
    },
    { type: "add", old_line_num: null, new_line_num: 1, content: "new line\n" },
  ],
};

// Context + a pure deletion (placeholder on the right) + another context +
// a pure addition (placeholder on the left).
const richHunk: RichDiffHunk = {
  old_start: 1,
  old_lines: 3,
  new_start: 1,
  new_lines: 3,
  lines: [
    { type: "equal", old_line_num: 1, new_line_num: 1, content: "ctx\n" },
    {
      type: "delete",
      old_line_num: 2,
      new_line_num: null,
      content: "removed\n",
    },
    { type: "equal", old_line_num: 3, new_line_num: 2, content: "mid\n" },
    { type: "add", old_line_num: null, new_line_num: 3, content: "added\n" },
  ],
};

function card(side: "old" | "new", body: string): AnchoredComment {
  return {
    status: "active",
    comment: {
      id: `c-${side}`,
      filePath: "a.ts",
      side,
      startLine: 1,
      endLine: 1,
      body,
      capturedSnippet: "snippet",
      createdAt: "2025-01-01T00:00:00Z",
    },
  } as AnchoredComment;
}

const noop = () => {};

type Props = Parameters<typeof SplitHunkView>[0];

function renderHunk(overrides: Partial<Props> = {}) {
  const base: Props = {
    hunk: modifyHunk,
    hunkIndex: 0,
    commentsEnabled: false,
    cardsByEndRow: new Map(),
    formRowIndex: null,
    draftSide: null,
    draftStartLine: null,
    draftEndLine: null,
    rangeStart: null,
    onPlusClick: noop,
    onCommentSave: noop,
    onCommentDelete: noop,
    onDraftSave: noop,
    onDraftCancel: noop,
  };
  return render(<SplitHunkView {...base} {...overrides} />);
}

describe("SplitHunkView", () => {
  it("renders the hunk header, context rows, and pure add/delete rows", () => {
    renderHunk({ hunk: richHunk });
    expect(screen.getByText(/-1,3 \+1,3/)).toBeTruthy(); // hunk header
    expect(screen.getByText("removed")).toBeTruthy(); // delete-only row (left)
    expect(screen.getByText("added")).toBeTruthy(); // add-only row (right)
    // Context lines render on both the old and new side.
    expect(screen.getAllByText("ctx")).toHaveLength(2);
    expect(screen.getAllByText("mid")).toHaveLength(2);
  });

  it("renders syntax tokens when provided", () => {
    renderHunk({
      hunk: richHunk,
      lineTokens: [[{ content: "CTXTOKEN", color: "#abcdef" }]],
    });
    // The token-backed context line renders on both sides.
    expect(screen.getAllByText("CTXTOKEN")).toHaveLength(2);
  });

  it("shows the active + gutter button for a started range", () => {
    renderHunk({
      hunk: richHunk,
      commentsEnabled: true,
      rangeStart: { side: "old", line: 2 },
    });
    expect(screen.getByLabelText("Add comment on old line 2")).toBeTruthy();
  });

  it("renders the draft form at the target row", () => {
    // Draft on the new side of the modify row (rightIndex = 1).
    renderHunk({
      hunk: modifyHunk,
      commentsEnabled: true,
      formRowIndex: 1,
      draftSide: "new",
      draftStartLine: 1,
      draftEndLine: 1,
      onDraftSave: vi.fn(),
    });
    expect(screen.getByPlaceholderText(/Leave a comment/)).toBeTruthy();
  });

  it("renders an old-side card on a modify row (left index)", () => {
    renderHunk({
      hunk: modifyHunk,
      commentsEnabled: true,
      cardsByEndRow: new Map([[0, [card("old", "OLD SIDE NOTE")]]]),
    });
    expect(screen.getByText("OLD SIDE NOTE")).toBeTruthy();
  });

  it("renders a new-side card on a modify row (right index)", () => {
    renderHunk({
      hunk: modifyHunk,
      commentsEnabled: true,
      cardsByEndRow: new Map([[1, [card("new", "NEW SIDE NOTE")]]]),
    });
    expect(screen.getByText("NEW SIDE NOTE")).toBeTruthy();
  });
});
