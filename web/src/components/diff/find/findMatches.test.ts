import { describe, it, expect } from "vitest";
import { findMatches, type SearchableLine } from "./findMatches";

// Changed lines in rendered order (deletions before additions per change).
const LINES: SearchableLine[] = [
  { side: "old", lineNumber: 2, text: "alpha beta" },
  { side: "old", lineNumber: 3, text: "BETA done" },
  { side: "new", lineNumber: 2, text: "alpha BETA" },
  { side: "new", lineNumber: 3, text: "delta beta beta" },
];

describe("findMatches", () => {
  it("returns nothing for an empty query", () => {
    expect(findMatches(LINES, "")).toEqual([]);
  });

  it("matches case-insensitively across the supplied lines in order, with a contiguous index", () => {
    const m = findMatches(LINES, "beta");
    expect(m.map((x) => [x.side, x.lineNumber, x.startCol, x.endCol, x.index])).toEqual([
      ["old", 2, 6, 10, 0],
      ["old", 3, 0, 4, 1],
      ["new", 2, 6, 10, 2],
      ["new", 3, 6, 10, 3],
      ["new", 3, 11, 15, 4],
    ]);
  });

  it("respects caseSensitive", () => {
    const m = findMatches(LINES, "BETA", { caseSensitive: true });
    expect(m.map((x) => [x.side, x.lineNumber, x.startCol])).toEqual([
      ["old", 3, 0],
      ["new", 2, 6],
    ]);
  });

  it("finds non-overlapping literal matches", () => {
    const m = findMatches([{ side: "old", lineNumber: 1, text: "aaaa" }], "aa");
    expect(m.map((x) => [x.startCol, x.endCol])).toEqual([
      [0, 2],
      [2, 4],
    ]);
  });

  it("supports regex search without looping on zero-width matches", () => {
    const d = findMatches([{ side: "old", lineNumber: 1, text: "foo123bar" }], "\\d+", { regex: true });
    expect(d.map((x) => [x.startCol, x.endCol])).toEqual([[3, 6]]);

    const m = findMatches([{ side: "old", lineNumber: 1, text: "abc" }], "x*", {
      regex: true,
    });
    expect(m.length).toBeGreaterThan(0);
    expect(m.every((x) => x.startCol === x.endCol)).toBe(true);
  });
});
