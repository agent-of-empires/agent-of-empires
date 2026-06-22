import { describe, it, expect } from "vitest";
import { targetScrollFraction } from "../scrollFraction";

type Meta = Parameters<typeof targetScrollFraction>[0];

// Minimal FileDiffMetadata shape that `changedLines` walks: one hunk with a
// single "change" segment. `additionLineIndex` is 0-based; changedLines emits
// 1-based new-side line numbers (index + 1).
function metaWithNewLines(firstNewLine: number, count: number): Meta {
  const additionLines = Array.from({ length: firstNewLine - 1 + count }, (_, i) => `line ${i + 1}\n`);
  return {
    hunks: [
      {
        hunkContent: [
          { type: "change", deletions: 0, additions: count, deletionLineIndex: 0, additionLineIndex: firstNewLine - 1 },
        ],
      },
    ],
    deletionLines: [],
    additionLines,
  } as unknown as Meta;
}

function metaDeletionOnly(firstOldLine: number, count: number): Meta {
  const deletionLines = Array.from({ length: firstOldLine - 1 + count }, (_, i) => `line ${i + 1}\n`);
  return {
    hunks: [
      {
        hunkContent: [
          { type: "change", deletions: count, additions: 0, deletionLineIndex: firstOldLine - 1, additionLineIndex: 0 },
        ],
      },
    ],
    deletionLines,
    additionLines: [],
  } as unknown as Meta;
}

describe("targetScrollFraction", () => {
  it("maps an exact changed new line to its rank among changed rows", () => {
    const meta = metaWithNewLines(10, 3); // changed new lines 10, 11, 12
    expect(targetScrollFraction(meta, 10, 100)).toBe(0);
    expect(targetScrollFraction(meta, 11, 100)).toBe(0.5);
    expect(targetScrollFraction(meta, 12, 100)).toBe(1);
  });

  it("snaps to the nearest changed new line when the target is unchanged", () => {
    const meta = metaWithNewLines(10, 3);
    expect(targetScrollFraction(meta, 11, 100)).toBe(0.5); // closest to 11
    expect(targetScrollFraction(meta, 500, 100)).toBe(1); // beyond last changed -> last row
    expect(targetScrollFraction(meta, 1, 100)).toBe(0); // before first changed -> first row
  });

  it("returns 0 when there is a single changed row", () => {
    const meta = metaWithNewLines(42, 1);
    expect(targetScrollFraction(meta, 42, 100)).toBe(0);
    expect(targetScrollFraction(meta, 9, 100)).toBe(0);
  });

  it("falls back to a clamped file-line fraction with no changed new rows", () => {
    const meta = metaDeletionOnly(5, 4); // only deletions, no new-side rows
    expect(targetScrollFraction(meta, 50, 100)).toBeCloseTo(0.49, 5); // (50-1)/100
    expect(targetScrollFraction(meta, 1, 100)).toBe(0);
    expect(targetScrollFraction(meta, 999, 100)).toBe(1); // clamped
    expect(targetScrollFraction(meta, 10, 0)).toBe(1); // guards divide-by-zero -> clamped
  });
});
