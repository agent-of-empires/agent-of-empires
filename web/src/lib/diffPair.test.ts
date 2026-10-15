import { expect, it } from "vitest";
import { diffPair } from "./diffPair";

it("counts adds and dels without double-counting context", () => {
  const fifty = Array.from({ length: 50 }, (_, i) => `line ${i}`).join("\n");
  const cases: [string, string, string, number, number][] = [
    ["both empty", "", "", 0, 0],
    ["one-line change", "line 1\nline 2\nline 3", "line 1\nline TWO\nline 3", 1, 1],
    ["change amid context", fifty, fifty.replace("line 25", "line TWENTY-FIVE"), 1, 1],
    ["pure append", "a\nb\nc", "a\nb\nc\nd\ne", 2, 0],
    ["pure deletion", "a\nb\nc\nd", "a\nd", 0, 2],
    ["added trailing newline", "a\nb", "a\nb\n", 0, 0],
    ["removed trailing newline", "a\nb\n", "a\nb", 0, 0],
    ["pure write", "", "a\nb\nc", 3, 0],
    ["full delete", "a\nb\nc", "", 0, 3],
  ];
  for (const [name, a, b, adds, dels] of cases) expect(diffPair(a, b), name).toMatchObject({ adds, dels });
});

it("numbers lines per side, with null on the absent side", () => {
  expect(diffPair("a\nc", "a\nb\nc").hunk.lines).toEqual([
    { type: "equal", old_line_num: 1, new_line_num: 1, content: "a" },
    { type: "add", old_line_num: null, new_line_num: 2, content: "b" },
    { type: "equal", old_line_num: 2, new_line_num: 3, content: "c" },
  ]);
});

it("reports hunk bounds matching the tallies, and an empty hunk for empty input", () => {
  expect(diffPair("a\nb\nc", "a\nB\nc\nd").hunk).toMatchObject({
    old_start: 1,
    old_lines: 3,
    new_start: 1,
    new_lines: 4,
  });
  expect(diffPair("", "").hunk).toEqual({ old_start: 0, old_lines: 0, new_start: 0, new_lines: 0, lines: [] });
});

it("strips carriage returns from CRLF content", () => {
  expect(diffPair("a\r\nb\r\nc", "a\r\nB\r\nc").hunk.lines).toEqual([
    { type: "equal", old_line_num: 1, new_line_num: 1, content: "a" },
    { type: "delete", old_line_num: 2, new_line_num: null, content: "b" },
    { type: "add", old_line_num: null, new_line_num: 2, content: "B" },
    { type: "equal", old_line_num: 3, new_line_num: 3, content: "c" },
  ]);
  expect(diffPair("a\r\nb", "a\r\nb")).toMatchObject({ adds: 0, dels: 0 });
});
