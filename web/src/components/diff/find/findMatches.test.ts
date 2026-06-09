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

  it("matches case-insensitively across the supplied lines in order", () => {
    const m = findMatches(LINES, "beta");
    expect(m.map((x) => [x.side, x.lineNumber, x.startCol, x.endCol])).toEqual([
      ["old", 2, 6, 10],
      ["old", 3, 0, 4],
      ["new", 2, 6, 10],
      ["new", 3, 6, 10],
      ["new", 3, 11, 15],
    ]);
  });

  it("assigns a contiguous global index in match order", () => {
    expect(findMatches(LINES, "beta").map((x) => x.index)).toEqual([0, 1, 2, 3, 4]);
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

  it("only searches the lines it is given (changed lines)", () => {
    // A line not in the set is never matched.
    const m = findMatches([{ side: "new", lineNumber: 5, text: "needle here" }], "needle");
    expect(m).toEqual([{ side: "new", lineNumber: 5, startCol: 0, endCol: 6, index: 0 }]);
  });

  it("supports regex search", () => {
    const m = findMatches([{ side: "old", lineNumber: 1, text: "foo123bar" }], "\\d+", { regex: true });
    expect(m.map((x) => [x.startCol, x.endCol])).toEqual([[3, 6]]);
  });

  it("does not loop forever on zero-width regex matches", () => {
    const m = findMatches([{ side: "old", lineNumber: 1, text: "abc" }], "x*", {
      regex: true,
    });
    expect(m.length).toBeGreaterThan(0);
    expect(m.every((x) => x.startCol === x.endCol)).toBe(true);
  });

  it("throws on an invalid regex so callers can show an error", () => {
    expect(() =>
      findMatches([{ side: "old", lineNumber: 1, text: "x" }], "(", {
        regex: true,
      }),
    ).toThrow();
  });
});
