import { describe, it, expect } from "vitest";
import { anchorCommentsToContents } from "./anchorToContents";
import type { DiffComment } from "./types";

const OLD = "old1\nold2\nold3\n";
const NEW = "new1\nnew2\nnew3\nnew4\n";

function comment(over: Partial<DiffComment>): DiffComment {
  return {
    id: "c1",
    filePath: "a.ts",
    side: "new",
    startLine: 1,
    endLine: 2,
    body: "b",
    capturedSnippet: "new1\nnew2",
    createdAt: "2026-01-01T00:00:00Z",
    ...over,
  };
}

describe("anchorCommentsToContents", () => {
  it("marks an in-bounds comment active", () => {
    const out = anchorCommentsToContents(
      [comment({})],
      "a.ts",
      undefined,
      OLD,
      NEW,
    );
    expect(out[0]?.status).toBe("active");
    expect(out[0]?.contentChanged).toBe(false);
  });

  it("flags contentChanged when the snippet drifted", () => {
    const out = anchorCommentsToContents(
      [comment({ capturedSnippet: "stale text" })],
      "a.ts",
      undefined,
      OLD,
      NEW,
    );
    expect(out[0]?.status).toBe("active");
    expect(out[0]?.contentChanged).toBe(true);
  });

  it("marks an out-of-bounds range stale", () => {
    const out = anchorCommentsToContents(
      [comment({ side: "old", startLine: 3, endLine: 5 })],
      "a.ts",
      undefined,
      OLD,
      NEW,
    );
    expect(out[0]?.status).toBe("stale");
  });

  it("filters by filePath and repoName", () => {
    const comments = [
      comment({ id: "keep" }),
      comment({ id: "wrong-file", filePath: "b.ts" }),
      comment({ id: "wrong-repo", repoName: "other" }),
    ];
    const out = anchorCommentsToContents(comments, "a.ts", undefined, OLD, NEW);
    expect(out.map((a) => a.comment.id)).toEqual(["keep"]);
  });
});
