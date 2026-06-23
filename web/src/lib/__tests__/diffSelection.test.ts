import { describe, expect, it } from "vitest";
import { diffSelectionStale } from "../diffSelection";
import type { RichDiffFile } from "../types";

const file = (path: string, repo_name?: string): RichDiffFile => ({
  path,
  old_path: null,
  status: "modified",
  additions: 0,
  deletions: 0,
  repo_name,
});

describe("diffSelectionStale", () => {
  it("is false when there is no selection", () => {
    expect(diffSelectionStale(null, false, [file("a.ts")])).toBe(false);
  });

  it("is false for a cited selection even when absent from the diff", () => {
    expect(diffSelectionStale({ path: "b.ts", cited: true }, false, [file("a.ts")])).toBe(false);
  });

  it("is false while the diff list is still loading", () => {
    expect(diffSelectionStale({ path: "b.ts" }, true, [])).toBe(false);
  });

  it("is true when a plain selection is absent from the diff list", () => {
    expect(diffSelectionStale({ path: "b.ts" }, false, [file("a.ts")])).toBe(true);
  });

  it("is false when the selection is present (path and repo match)", () => {
    expect(diffSelectionStale({ path: "a.ts", repoName: "api" }, false, [file("a.ts", "api")])).toBe(false);
  });

  it("is true when the path matches but the repo differs", () => {
    expect(diffSelectionStale({ path: "a.ts", repoName: "web" }, false, [file("a.ts", "api")])).toBe(true);
  });
});
