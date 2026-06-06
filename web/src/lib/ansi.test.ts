import { describe, expect, it } from "vitest";

import { collapseCarriageReturns, hasAnsi, parseAnsi, stripAnsi } from "./ansi";

const ESC = String.fromCharCode(0x1b);

describe("hasAnsi / stripAnsi", () => {
  it("detects and strips SGR sequences", () => {
    const text = `${ESC}[01;34mfoo${ESC}[0m`;
    expect(hasAnsi(text)).toBe(true);
    expect(stripAnsi(text)).toBe("foo");
    expect(hasAnsi("plain text")).toBe(false);
  });

  it("strips non-SGR CSI noise", () => {
    // Cursor up + line erase
    const noisy = `${ESC}[2K${ESC}[1Aredraw`;
    expect(stripAnsi(noisy)).toBe("redraw");
  });

  it("hasAnsi requires a real CSI shape, not just ESC[", () => {
    // Markdown blob discussing ANSI codes contains the literal
    // characters but no actual sequence. Triggering the ANSI fast
    // path here would render the prose without Shiki highlighting.
    expect(hasAnsi(`docs say: prefix is "${ESC}[" then params`)).toBe(false);
    // Real SGR still detected.
    expect(hasAnsi(`${ESC}[31mred${ESC}[0m`)).toBe(true);
  });
});

describe("collapseCarriageReturns", () => {
  it("keeps only the last fragment of each line", () => {
    expect(collapseCarriageReturns("p:1/3\rp:2/3\rp:3/3")).toBe("p:3/3");
  });
  it("preserves multi-line input", () => {
    expect(collapseCarriageReturns("a\nb\rc\nd")).toBe("a\nc\nd");
  });
  it("is a no-op when there are no carriage returns", () => {
    expect(collapseCarriageReturns("plain\nlines")).toBe("plain\nlines");
  });
  it("preserves CRLF line endings", () => {
    // Windows-style CRLF — the trailing \r is part of the line ending,
    // not a redraw marker. Stripping it would corrupt the text.
    expect(collapseCarriageReturns("line1\r\nline2\r\n")).toBe(
      "line1\r\nline2\r\n",
    );
  });
  it("collapses redraws within a CRLF-terminated line", () => {
    // Mixed: redraws in the middle of a line, CRLF at the end.
    expect(collapseCarriageReturns("p:1/3\rp:2/3\rp:3/3\r\nnext")).toBe(
      "p:3/3\r\nnext",
    );
  });
});

describe("parseAnsi", () => {
  it("returns a single segment with no style for plain text", () => {
    const segs = parseAnsi("hello world");
    expect(segs).toHaveLength(1);
    expect(segs[0].text).toBe("hello world");
    expect(segs[0].style).toEqual({});
  });

  it("splits text at SGR boundaries and applies fg colors", () => {
    // ls --color output shape: reset, then "[01;34mApplications[0m"
    const text = `${ESC}[0m${ESC}[01;34mApplications${ESC}[0m\nbin`;
    const segs = parseAnsi(text);
    // Reset segment is empty, filtered out. Then a styled "Applications",
    // then a plain "\nbin".
    expect(segs.map((s) => s.text)).toEqual(["Applications", "\nbin"]);
    expect(segs[0].style.bold).toBe(true);
    expect(segs[0].style.fg).toBe("#2472c8");
    expect(segs[1].style).toEqual({});
  });

  it("handles 256-color and truecolor params", () => {
    const text = `${ESC}[38;5;82mlime${ESC}[0m ${ESC}[38;2;10;20;30mrgb${ESC}[0m`;
    const segs = parseAnsi(text);
    expect(segs[0].text).toBe("lime");
    // 82 is in the 6x6x6 cube: i = 66, r = 1, g = 5, b = 0 → (51, 255, 0)
    expect(segs[0].style.fg).toBe("rgb(51, 255, 0)");
    expect(segs[2].text).toBe("rgb");
    expect(segs[2].style.fg).toBe("rgb(10, 20, 30)");
  });

  it("collapses carriage returns before parsing", () => {
    const text = "progress: 1/3\rprogress: 2/3\rprogress: 3/3";
    const segs = parseAnsi(text);
    expect(segs).toHaveLength(1);
    expect(segs[0].text).toBe("progress: 3/3");
  });

  it("treats empty SGR (ESC [m) as a full reset", () => {
    const text = `${ESC}[31mred${ESC}[mreset`;
    const segs = parseAnsi(text);
    expect(segs[0].style.fg).toBe("#cd3131");
    expect(segs[1].style).toEqual({});
  });
});
