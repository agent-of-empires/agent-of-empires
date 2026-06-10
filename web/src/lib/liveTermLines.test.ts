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
});
