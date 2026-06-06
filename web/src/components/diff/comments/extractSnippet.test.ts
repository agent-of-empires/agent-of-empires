import { describe, it, expect } from "vitest";
import type { RichDiffHunk } from "../../../lib/types";
import { extractSnippetFromHunks } from "./extractSnippet";

function hunk(): RichDiffHunk {
  return {
    old_start: 10,
    old_lines: 5,
    new_start: 10,
    new_lines: 6,
    lines: [
      { type: "equal", old_line_num: 10, new_line_num: 10, content: "a\n" },
      { type: "equal", old_line_num: 11, new_line_num: 11, content: "b\n" },
      {
        type: "delete",
        old_line_num: 12,
        new_line_num: null,
        content: "old\n",
      },
      { type: "add", old_line_num: null, new_line_num: 12, content: "new1\n" },
      { type: "add", old_line_num: null, new_line_num: 13, content: "new2\n" },
      { type: "equal", old_line_num: 13, new_line_num: 14, content: "c\n" },
      { type: "equal", old_line_num: 14, new_line_num: 15, content: "d\n" },
    ],
  };
}

describe("extractSnippetFromHunks", () => {
  it("extracts a new-side range, skipping deleted rows", () => {
    const res = extractSnippetFromHunks([hunk()], "new", 12, 14);
    expect(res?.snippet).toBe("new1\nnew2\nc");
  });

  it("extracts an old-side range, skipping added rows", () => {
    const res = extractSnippetFromHunks([hunk()], "old", 11, 13);
    expect(res?.snippet).toBe("b\nold\nc");
  });

  it("returns null when range straddles the hunk boundary", () => {
    const res = extractSnippetFromHunks([hunk()], "new", 9, 11);
    expect(res).toBeNull();
  });

  it("normalises reversed ranges", () => {
    const a = extractSnippetFromHunks([hunk()], "new", 14, 12);
    const b = extractSnippetFromHunks([hunk()], "new", 12, 14);
    expect(a?.snippet).toBe(b?.snippet);
  });

  it("returns the row index of the last matching line", () => {
    const res = extractSnippetFromHunks([hunk()], "new", 14, 14);
    // Row index 5 == the equal row at new_line_num=14
    expect(res?.endRowIndex).toBe(5);
  });

  it("returns null when range falls outside any hunk", () => {
    const res = extractSnippetFromHunks([hunk()], "new", 50, 52);
    expect(res).toBeNull();
  });
});
