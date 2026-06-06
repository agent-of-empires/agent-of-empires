import { describe, it, expect } from "vitest";
import type { RichDiffHunk } from "../../../lib/types";
import { anchorComments } from "./anchor";
import type { DiffComment } from "./types";

function mkHunk(): RichDiffHunk {
  return {
    old_start: 10,
    old_lines: 3,
    new_start: 10,
    new_lines: 3,
    lines: [
      { type: "equal", old_line_num: 10, new_line_num: 10, content: "a\n" },
      { type: "equal", old_line_num: 11, new_line_num: 11, content: "b\n" },
      { type: "equal", old_line_num: 12, new_line_num: 12, content: "c\n" },
    ],
  };
}

function mkComment(overrides: Partial<DiffComment>): DiffComment {
  return {
    id: "c1",
    filePath: "src/foo.rs",
    side: "new",
    startLine: 11,
    endLine: 11,
    body: "review",
    capturedSnippet: "b",
    createdAt: "2025-01-01T00:00:00Z",
    ...overrides,
  };
}

describe("anchorComments", () => {
  it("marks comments active when the range resolves", () => {
    const [a] = anchorComments([mkComment({})], "src/foo.rs", undefined, [
      mkHunk(),
    ]);
    expect(a.status).toBe("active");
    expect(a.hunkIndex).toBe(0);
    expect(a.contentChanged).toBe(false);
  });

  it("marks comments stale when the range is missing", () => {
    const [a] = anchorComments(
      [mkComment({ startLine: 99, endLine: 99 })],
      "src/foo.rs",
      undefined,
      [mkHunk()],
    );
    expect(a.status).toBe("stale");
  });

  it("flags contentChanged when current snippet differs from captured", () => {
    const [a] = anchorComments(
      [mkComment({ capturedSnippet: "STALE" })],
      "src/foo.rs",
      undefined,
      [mkHunk()],
    );
    expect(a.status).toBe("active");
    expect(a.contentChanged).toBe(true);
  });

  it("filters by repoName and filePath", () => {
    const result = anchorComments(
      [
        mkComment({ id: "match", filePath: "src/foo.rs", repoName: "r" }),
        mkComment({ id: "wrongRepo", filePath: "src/foo.rs", repoName: "x" }),
        mkComment({ id: "wrongFile", filePath: "src/bar.rs", repoName: "r" }),
      ],
      "src/foo.rs",
      "r",
      [mkHunk()],
    );
    expect(result.map((a) => a.comment.id)).toEqual(["match"]);
  });
});
