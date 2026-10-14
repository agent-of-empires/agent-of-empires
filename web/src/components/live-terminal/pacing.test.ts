import { describe, expect, it } from "vitest";
import { anchoredScrollTop, scrollAnchor, type RowLayout } from "./pacing";

const LINE_H = 10;
/** Three rows tall. */
const VIEW_H = 30;

const LINES = Array.from({ length: 20 }, (_, i) => `line ${i}`);

/** A layout of the plain-text lines `keys`, wrapping to the given row counts, whose last `screenRows` lines are
 *  the live screen. */
function layout(rowsPerLine: number[], keys: readonly string[] = LINES.slice(0, rowsPerLine.length), screenRows = 0) {
  const lineStartRow: number[] = [];
  const source: RowLayout["source"] = [];
  rowsPerLine.forEach((count, line) => {
    lineStartRow.push(source.length);
    for (let wrap = 0; wrap < count; wrap++) source.push({ line, wrap });
  });
  return { keys, lineStartRow, source, screenStart: keys.length - screenRows };
}

const ones = (n: number) => Array.from({ length: n }, () => 1);

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
      after: { spacer: 0, layout: layout(ones(5), LINES.slice(2, 7)) },
      expected: 0.5 * LINE_H,
    },
    {
      name: "a trim prefers the line's full context over a repeat of it near the end",
      before: { top: 40, spacer: 0, layout: layout(ones(8), ["q", "r", "s", "t", "a", "b", "c", "a"]) },
      after: { spacer: 0, layout: layout(ones(5), ["t", "a", "b", "c", "a"]) },
      expected: 10,
    },
    {
      name: "a trim with a screen repaint matches on history lines only",
      before: { top: 20, spacer: 0, layout: layout(ones(6), ["a", "b", "c", "d", "s0", "s1"], 2) },
      after: { spacer: 0, layout: layout(ones(4), ["c", "d", "x0", "x1"], 2) },
      expected: 0,
    },
    // The VT grid keeps 2000 lines while tmux capture keeps the whole scrollback, so a line's number from the top
    // differs between them; here the VT grid holds the last 10 of tmux's 20.
    {
      name: "a loaded line keeps its place when reading switches from the VT grid to tmux",
      before: { top: 65, spacer: 6, layout: layout(ones(4), LINES.slice(16, 20), 2) },
      after: { spacer: 0, layout: layout(ones(20), LINES, 2) },
      expected: (16 + 0.5) * LINE_H,
    },
    {
      name: "a loaded line keeps its place when reading switches from the VT grid to tmux with wrapped history",
      before: { top: 65, spacer: 6, layout: layout(ones(4), LINES.slice(16, 20), 2) },
      after: { spacer: 0, layout: layout([...Array.from({ length: 16 }, () => 2), 1, 1, 1, 1], LINES, 2) },
      expected: (32 + 0.5) * LINE_H,
    },
    {
      name: "a screen row keeps its place when reading switches from the VT grid to tmux",
      before: { top: 85, spacer: 6, layout: layout(ones(4), LINES.slice(16, 20), 2) },
      after: { spacer: 0, layout: layout(ones(20), LINES, 2) },
      expected: (18 + 0.5) * LINE_H,
    },
    {
      name: "a spacer row moves with the loaded text in view when reading switches from the VT grid to tmux",
      before: { top: 50, spacer: 6, layout: layout(ones(4), LINES.slice(16, 20), 2) },
      after: { spacer: 0, layout: layout(ones(20), LINES, 2) },
      expected: 15 * LINE_H,
    },
    {
      name: "the oldest boundary stays put when reading switches from the VT grid to tmux",
      before: { top: 0, spacer: 6, layout: layout(ones(4), LINES.slice(16, 20), 2) },
      after: { spacer: 0, layout: layout(ones(20), LINES, 2) },
      expected: 0,
    },
    {
      name: "the oldest boundary stays put with loaded text in view when reading switches from the VT grid to tmux",
      before: { top: 0, spacer: 2, layout: layout(ones(4), LINES.slice(16, 20), 2) },
      after: { spacer: 0, layout: layout(ones(20), LINES, 2) },
      expected: 0,
    },
    {
      name: "a view of only spacer keeps its scrollbar position when reading switches from the VT grid to tmux",
      before: { top: 20, spacer: 6, layout: layout(ones(4), LINES.slice(16, 20), 2) },
      after: { spacer: 0, layout: layout(ones(20), LINES, 2) },
      expected: 2 * LINE_H,
    },
    {
      name: "a spacer row stays put when output appends and no history is loaded",
      before: { top: 30, spacer: 5, layout: layout(ones(2), ["s0", "s1"], 2) },
      after: { spacer: 6, layout: layout(ones(2), ["n0", "n1"], 2) },
      expected: 30,
    },
    {
      name: "a spacer row stays put when output appends past the loaded history",
      before: { top: 30, spacer: 5, layout: layout(ones(4), ["h5", "h6", "s0", "s1"], 2) },
      after: { spacer: 6, layout: layout(ones(4), ["h6", "h7", "n0", "n1"], 2) },
      expected: 30,
    },
    {
      name: "the same layout keeps the same position",
      before: { top: 37, spacer: 2, layout: layout([1, 2, 1]) },
      after: { spacer: 2, layout: layout([1, 2, 1]) },
      expected: 37,
    },
  ])("$name", ({ before, after, expected }) => {
    const anchor = scrollAnchor(before.top, VIEW_H, before.spacer, before.layout, LINE_H);
    expect(anchor).not.toBeNull();
    expect(anchoredScrollTop(anchor!, after.spacer, after.layout, LINE_H)).toBeCloseTo(expected);
  });

  it("gives up on a position no layout can place", () => {
    expect(scrollAnchor(100, VIEW_H, 2, layout([1, 1]), LINE_H)).toBeNull();
    const pastTheEnd = { line: 9, fromEnd: -5, wrap: 0, fraction: 0, probe: 0, context: [] };
    expect(anchoredScrollTop(pastTheEnd, 2, layout([1, 1]), LINE_H)).toBeNull();
  });
});
