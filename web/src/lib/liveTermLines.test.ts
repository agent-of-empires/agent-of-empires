// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { ansiToLines, lineText, wrapLine } from "./liveTermLines";

describe("ansiToLines", () => {
  it("splits plain text into lines and drops the capture trailing terminator", () => {
    const lines = ansiToLines("one\ntwo\nthree\n");
    expect(lines.map(lineText)).toEqual(["one", "two", "three"]);
  });

  it("preserves blank screen rows in the middle and at the end", () => {
    // capture-pane keeps trailing blank rows of the screen; only the
    // final `\n` terminator is an artifact.
    const lines = ansiToLines("prompt\n\n\n");
    expect(lines.map(lineText)).toEqual(["prompt", "", ""]);
  });

  it("carries SGR style across newlines", () => {
    const lines = ansiToLines("\x1b[31mred\nstill-red\x1b[0m plain\n");
    expect(lines).toHaveLength(2);
    expect(lines[0]![0]!.style.fg).toBeTruthy();
    expect(lines[1]![0]!.text).toBe("still-red");
    expect(lines[1]![0]!.style.fg).toBe(lines[0]![0]!.style.fg);
    expect(lines[1]![1]!.text).toBe(" plain");
    expect(lines[1]![1]!.style.fg).toBeUndefined();
  });

  it("renders an empty frame as a single empty line", () => {
    expect(ansiToLines("").map(lineText)).toEqual([""]);
  });
});

describe("wrapLine", () => {
  const seg = (text: string, fg?: string) => ({ text, style: fg ? { fg } : {} });

  it("is the identity for lines within the column limit", () => {
    const line = [seg("hello world")];
    expect(wrapLine(line, 80)).toEqual([line]);
  });

  it("hard-wraps at the column boundary preserving styles", () => {
    const rows = wrapLine([seg("aaaa", "red"), seg("bbbb")], 3);
    expect(rows.map((r) => lineText(r))).toEqual(["aaa", "abb", "bb"]);
    expect(rows[0]![0]!.style.fg).toBe("red");
    expect(rows[1]![0]!.style.fg).toBe("red");
    expect(rows[1]![1]!.style.fg).toBeUndefined();
  });

  it("treats zero or non-finite cols as no-wrap", () => {
    const line = [seg("abcdef")];
    expect(wrapLine(line, 0)).toEqual([line]);
    expect(wrapLine(line, Number.POSITIVE_INFINITY)).toEqual([line]);
  });

  it("returns one empty row for an empty line", () => {
    expect(wrapLine([], 10)).toEqual([[]]);
  });

  it("never splits an emoji's surrogate pair and counts it two cells", () => {
    // "a" (1 cell) + grinning face U+1F600 (2 cells) at cols=2: the
    // emoji wraps whole, leaving the first row's last cell empty.
    const rows = wrapLine([seg("a\u{1F600}\u{1F600}")], 2);
    expect(rows.map((r) => lineText(r))).toEqual(["a", "\u{1F600}", "\u{1F600}"]);
  });

  it("counts CJK as two cells when wrapping", () => {
    // Four CJK chars are eight cells; at cols=4 they wrap two per row.
    const rows = wrapLine([seg("\u4F60\u597D\u4E16\u754C")], 4);
    expect(rows.map((r) => lineText(r))).toEqual(["\u4F60\u597D", "\u4E16\u754C"]);
  });

  it("treats CJK width as identity-breaking even when code units fit", () => {
    // Three CJK chars are 3 UTF-16 units but 6 cells; cols=4 must wrap.
    const rows = wrapLine([seg("\u4F60\u597D\u4E16")], 4);
    expect(rows.length).toBe(2);
  });

  it("keeps combining marks attached to their base character", () => {
    // e + combining acute (zero cells) + "x" is 2 cells; at cols=2 the
    // line is identity, and at cols=1 the mark stays with the e.
    const line = [seg("e\u0301x")];
    expect(wrapLine(line, 2)).toEqual([line]);
    const rows = wrapLine(line, 1);
    expect(rows.map((r) => lineText(r))).toEqual(["e\u0301", "x"]);
  });
});
