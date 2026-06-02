import { describe, expect, it } from "vitest";
import { buildSplitRows, type SplitRow } from "../splitDiff";
import type { RichDiffHunk, RichDiffLine } from "../types";

function line(
  type: RichDiffLine["type"],
  oldNum: number | null,
  newNum: number | null,
  content: string,
): RichDiffLine {
  return { type, old_line_num: oldNum, new_line_num: newNum, content };
}

function hunk(lines: RichDiffLine[]): RichDiffHunk {
  return { old_start: 1, old_lines: 1, new_start: 1, new_lines: 1, lines };
}

describe("buildSplitRows", () => {
  it("puts a context line on both sides with its original index", () => {
    const rows = buildSplitRows(hunk([line("equal", 1, 1, "ctx")]));
    expect(rows).toEqual<SplitRow[]>([
      {
        left: line("equal", 1, 1, "ctx"),
        right: line("equal", 1, 1, "ctx"),
        leftIndex: 0,
        rightIndex: 0,
      },
    ]);
  });

  it("places a pure deletion on the left with an empty right", () => {
    const rows = buildSplitRows(hunk([line("delete", 5, null, "gone")]));
    expect(rows).toEqual<SplitRow[]>([
      {
        left: line("delete", 5, null, "gone"),
        right: null,
        leftIndex: 0,
        rightIndex: null,
      },
    ]);
  });

  it("places a pure addition on the right with an empty left", () => {
    const rows = buildSplitRows(hunk([line("add", null, 5, "new")]));
    expect(rows).toEqual<SplitRow[]>([
      {
        left: null,
        right: line("add", null, 5, "new"),
        leftIndex: null,
        rightIndex: 0,
      },
    ]);
  });

  it("pairs a delete+add block into modified rows", () => {
    const rows = buildSplitRows(
      hunk([
        line("delete", 5, null, "old a"),
        line("delete", 6, null, "old b"),
        line("add", null, 5, "new a"),
        line("add", null, 6, "new b"),
      ]),
    );
    expect(rows.map((r) => [r.left?.content, r.right?.content])).toEqual([
      ["old a", "new a"],
      ["old b", "new b"],
    ]);
    expect(rows.map((r) => [r.leftIndex, r.rightIndex])).toEqual([
      [0, 2],
      [1, 3],
    ]);
  });

  it("pads the shorter side of an uneven block with placeholders", () => {
    const rows = buildSplitRows(
      hunk([
        line("delete", 5, null, "old a"),
        line("add", null, 5, "new a"),
        line("add", null, 6, "new b"),
      ]),
    );
    expect(rows.map((r) => [r.left?.content ?? null, r.right?.content ?? null])).toEqual([
      ["old a", "new a"],
      [null, "new b"],
    ]);
  });

  it("flushes blocks independently around context lines", () => {
    const rows = buildSplitRows(
      hunk([
        line("equal", 1, 1, "ctx1"),
        line("delete", 2, null, "del"),
        line("add", null, 2, "ins"),
        line("equal", 3, 3, "ctx2"),
        line("add", null, 4, "tail add"),
      ]),
    );
    expect(rows.map((r) => [r.left?.content ?? null, r.right?.content ?? null])).toEqual([
      ["ctx1", "ctx1"],
      ["del", "ins"],
      ["ctx2", "ctx2"],
      [null, "tail add"],
    ]);
  });
});
