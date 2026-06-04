import { describe, expect, it } from "vitest";
import { diffPair } from "./diffPair";

describe("diffPair counts", () => {
  it("returns 0/0 for two empty strings", () => {
    const r = diffPair("", "");
    expect(r.adds).toBe(0);
    expect(r.dels).toBe(0);
    expect(r.hunk.lines).toHaveLength(0);
  });

  it("counts a one-line single-character change as 1/1", () => {
    const r = diffPair("line 1\nline 2\nline 3", "line 1\nline TWO\nline 3");
    expect(r.adds).toBe(1);
    expect(r.dels).toBe(1);
  });

  it("does not double-count shared context lines", () => {
    const a = Array.from({ length: 50 }, (_, i) => `line ${i}`).join("\n");
    const b = a.replace("line 25", "line TWENTY-FIVE");
    const r = diffPair(a, b);
    expect(r.adds).toBe(1);
    expect(r.dels).toBe(1);
  });

  it("counts a pure append as +N / -0", () => {
    const r = diffPair("a\nb\nc", "a\nb\nc\nd\ne");
    expect(r.adds).toBe(2);
    expect(r.dels).toBe(0);
  });

  it("counts a pure deletion as +0 / -N", () => {
    const r = diffPair("a\nb\nc\nd", "a\nd");
    expect(r.adds).toBe(0);
    expect(r.dels).toBe(2);
  });

  it("treats a single trailing newline as not adding a line", () => {
    expect(diffPair("a\nb", "a\nb\n")).toMatchObject({ adds: 0, dels: 0 });
    expect(diffPair("a\nb\n", "a\nb")).toMatchObject({ adds: 0, dels: 0 });
  });

  it("treats a pure-write (empty old) as adds-only", () => {
    const r = diffPair("", "a\nb\nc");
    expect(r.adds).toBe(3);
    expect(r.dels).toBe(0);
  });

  it("treats a full-delete (empty new) as dels-only", () => {
    const r = diffPair("a\nb\nc", "");
    expect(r.adds).toBe(0);
    expect(r.dels).toBe(3);
  });
});

describe("diffPair hunk shape", () => {
  it("emits add/delete/equal line types in interleaved order", () => {
    const r = diffPair("a\nb\nc", "a\nB\nc");
    expect(r.hunk.lines.map((l) => l.type)).toEqual([
      "equal",
      "delete",
      "add",
      "equal",
    ]);
  });

  it("assigns line numbers per side (null on the absent side)", () => {
    const r = diffPair("a\nc", "a\nb\nc");
    // a: equal (1,1), b: add (null,2), c: equal (2,3)
    expect(r.hunk.lines).toEqual([
      { type: "equal", old_line_num: 1, new_line_num: 1, content: "a" },
      { type: "add", old_line_num: null, new_line_num: 2, content: "b" },
      { type: "equal", old_line_num: 2, new_line_num: 3, content: "c" },
    ]);
  });

  it("reports hunk old_lines / new_lines matching the tallies", () => {
    const r = diffPair("a\nb\nc", "a\nB\nc\nd");
    expect(r.hunk.old_lines).toBe(3);
    expect(r.hunk.new_lines).toBe(4);
    expect(r.hunk.old_start).toBe(1);
    expect(r.hunk.new_start).toBe(1);
  });

  it("yields an empty hunk for both-empty input", () => {
    const r = diffPair("", "");
    expect(r.hunk).toEqual({
      old_start: 0,
      old_lines: 0,
      new_start: 0,
      new_lines: 0,
      lines: [],
    });
  });
});

describe("diffPair CRLF line endings", () => {
  it("strips the carriage return from identical CRLF content", () => {
    const r = diffPair("a\r\nb\r\nc", "a\r\nb\r\nc");
    expect(r.hunk.lines).toEqual([
      { type: "equal", old_line_num: 1, new_line_num: 1, content: "a" },
      { type: "equal", old_line_num: 2, new_line_num: 2, content: "b" },
      { type: "equal", old_line_num: 3, new_line_num: 3, content: "c" },
    ]);
  });

  it("strips the carriage return from changed CRLF content", () => {
    const r = diffPair("a\r\nb\r\nc", "a\r\nB\r\nc");
    expect(r.hunk.lines).toEqual([
      { type: "equal", old_line_num: 1, new_line_num: 1, content: "a" },
      { type: "delete", old_line_num: 2, new_line_num: null, content: "b" },
      { type: "add", old_line_num: null, new_line_num: 2, content: "B" },
      { type: "equal", old_line_num: 3, new_line_num: 3, content: "c" },
    ]);
  });

  it("leaves no stray carriage returns in any line content", () => {
    const r = diffPair("x\r\ny\r\nz", "x\r\ny2\r\nz");
    for (const line of r.hunk.lines) {
      expect(line.content).not.toContain("\r");
    }
  });
});
