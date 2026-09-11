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
    expect(collapseCarriageReturns("line1\r\nline2\r\n")).toBe("line1\r\nline2\r\n");
  });
  it("collapses redraws within a CRLF-terminated line", () => {
    // Mixed: redraws in the middle of a line, CRLF at the end.
    expect(collapseCarriageReturns("p:1/3\rp:2/3\rp:3/3\r\nnext")).toBe("p:3/3\r\nnext");
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

describe("OSC 8 hyperlinks and other OSC sequences", () => {
  const link = (url: string, text: string, terminator = `${ESC}\\`) =>
    `${ESC}]8;;${url}${terminator}${text}${ESC}]8;;${terminator}`;

  it("tags the visible text with the link target, not a regex guess", () => {
    const segs = parseAnsi(`Created PR ${link("https://github.com/x/y/pull/8", "here")}, done`);
    expect(segs.map((s) => [s.text, s.url])).toEqual([
      ["Created PR ", undefined],
      ["here", "https://github.com/x/y/pull/8"],
      [", done", undefined],
    ]);
  });

  it("keeps SGR styling inside a link's visible text and still tags it", () => {
    const styled = link("https://x.com", `${ESC}[31mred link${ESC}[0m`);
    const segs = parseAnsi(styled);
    expect(segs).toEqual([{ text: "red link", style: { fg: "#cd3131" }, url: "https://x.com" }]);
  });

  it("supports a BEL terminator instead of ST", () => {
    const segs = parseAnsi(link("https://x.com", "click", "\x07"));
    expect(segs).toEqual([{ text: "click", style: {}, url: "https://x.com" }]);
  });

  it("carries an id= parameter before the URL", () => {
    const withId = `${ESC}]8;id=k16z3m;https://x.com/pull/8${ESC}\\click${ESC}]8;;${ESC}\\`;
    const segs = parseAnsi(withId);
    expect(segs).toEqual([{ text: "click", style: {}, url: "https://x.com/pull/8" }]);
  });

  it("runs an unterminated link to the end of the text instead of dropping it", () => {
    const segs = parseAnsi(`${ESC}]8;;https://x.com${ESC}\\trailing text`);
    expect(segs).toEqual([{ text: "trailing text", style: {}, url: "https://x.com" }]);
  });

  it("anchors the link to its own text when color opens before the link", () => {
    // The shape `tmux capture-pane -e` emits: the SGR change lands BEFORE
    // the OSC 8 opener, so counting escape bytes as visible text shifted
    // the link right by the length of the color sequence.
    const line = `Styled: ${ESC}[31m${ESC}]8;;https://example.com/red${ESC}\\red link${ESC}[39m${ESC}]8;;${ESC}\\`;
    expect(parseAnsi(line)).toEqual([
      { text: "Styled: ", style: {} },
      { text: "red link", style: { fg: "#cd3131" }, url: "https://example.com/red" },
    ]);
  });

  it("keeps the link off text that follows the closing sequence", () => {
    const line = `${ESC}[32m${ESC}]8;;https://example.com${ESC}\\link${ESC}]8;;${ESC}\\ tail`;
    expect(parseAnsi(line)).toEqual([
      { text: "link", style: { fg: "#0dbc79" }, url: "https://example.com" },
      { text: " tail", style: { fg: "#0dbc79" } },
    ]);
  });

  it("detects output whose only escape sequence is a hyperlink", () => {
    // Output with a link but no color must still reach this parser; the
    // caller renders it as plain text otherwise and the bytes leak.
    expect(hasAnsi(link("https://x.com", "click"))).toBe(true);
    expect(hasAnsi(`${ESC}]0;title${ESC}\\`)).toBe(true);
  });

  it("drops non-hyperlink OSC sequences instead of leaking their escape bytes", () => {
    const segs = parseAnsi(`${ESC}]0;window title${ESC}\\before${ESC}]52;c;aGk=\x07after`);
    expect(segs.map((s) => s.text)).toEqual(["beforeafter"]);
  });
});
