import { describe, expect, it } from "vitest";
import { anchoredScrollTop, scrollAnchor, type RowLayout } from "./pacing";

const LINE_H = 10;

/** Distinct parsed lines, shared between layouts the way the parse cache shares unchanged lines across frames. */
const LINES = Array.from({ length: 12 }, () => [] as unknown[]);

/** A layout of `lines` whose lines wrap to the given row counts. */
function layout(rowsPerLine: number[], lines: readonly unknown[] = LINES.slice(0, rowsPerLine.length)): RowLayout {
  const lineStartRow: number[] = [];
  const source: RowLayout["source"] = [];
  rowsPerLine.forEach((count, line) => {
    lineStartRow.push(source.length);
    for (let wrap = 0; wrap < count; wrap++) source.push({ line, wrap });
  });
  return { lines, lineStartRow, source };
}

describe("scroll anchor across a relayout", () => {
  it.each([
    {
      name: "a loaded line keeps its place when wrapped history loads above it",
      before: { top: 55, spacer: 5, layout: layout([1, 1], LINES.slice(5, 7)) },
      after: { spacer: 3, layout: layout([1, 3, 1, 1], LINES.slice(3, 7)) },
      expected: (3 + 4 + 0.5) * LINE_H,
    },
    {
      name: "a spacer row becomes its loaded line",
      before: { top: 42, spacer: 5, layout: layout([1], LINES.slice(5, 6)) },
      after: { spacer: 3, layout: layout([2, 3, 1], LINES.slice(3, 6)) },
      expected: (3 + 2 + 0.2) * LINE_H,
    },
    {
      name: "a line still in the spacer stays put",
      before: { top: 10, spacer: 5, layout: layout([1], LINES.slice(5, 6)) },
      after: { spacer: 3, layout: layout([2, 2, 1], LINES.slice(3, 6)) },
      expected: 10,
    },
    {
      name: "a wrap row past a narrower rewrap lands on the line's last row",
      before: { top: 20, spacer: 0, layout: layout([1, 3]) },
      after: { spacer: 0, layout: layout([1, 2]) },
      expected: 20,
    },
    {
      name: "a line follows its text when the oldest scrollback is trimmed",
      before: { top: 45, spacer: 0, layout: layout([2, 2, 1, 1, 1, 1]) },
      after: { spacer: 0, layout: layout([1, 1, 1, 1, 1], LINES.slice(2, 7)) },
      expected: 0.5 * LINE_H,
    },
    {
      name: "the same layout keeps the same position",
      before: { top: 37, spacer: 2, layout: layout([1, 2, 1]) },
      after: { spacer: 2, layout: layout([1, 2, 1]) },
      expected: 37,
    },
  ])("$name", ({ before, after, expected }) => {
    const anchor = scrollAnchor(before.top, before.spacer, before.layout, LINE_H);
    expect(anchor).not.toBeNull();
    expect(anchoredScrollTop(anchor!, after.spacer, after.layout, LINE_H)).toBeCloseTo(expected);
  });

  it("gives up on a position no layout can place", () => {
    expect(scrollAnchor(100, 2, layout([1, 1]), LINE_H)).toBeNull();
    expect(anchoredScrollTop({ line: 9, wrap: 0, fraction: 0, context: [] }, 2, layout([1, 1]), LINE_H)).toBeNull();
  });
});
