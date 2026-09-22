// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import {
  LineParseCache,
  ansiToLines,
  clusterSpanAt,
  findCursorCharIndex,
  isHttpUrl,
  lineText,
  splitCellRuns,
  splitUrls,
  textWidth,
  wrapLine,
} from "./liveTermLines";

const ZWJ = "\u200D";
const FLAG_US = "\u{1F1FA}\u{1F1F8}";
const FAMILY = `\u{1F468}${ZWJ}\u{1F469}${ZWJ}\u{1F467}`;
const DEV = `\u{1F9D1}${ZWJ}\u{1F4BB}`;
const seg = (text: string, fg?: string) => ({ text, style: fg ? { fg } : {} });
const texts = (rows: ReturnType<typeof wrapLine>) => rows.map((r) => lineText(r));

describe("ansiToLines", () => {
  it.each([
    ["one\ntwo\nthree\n", ["one", "two", "three"]],
    ["prompt\n\n\n", ["prompt", "", ""]],
    ["", [""]],
  ])("splits %j into rows, dropping the capture terminator", (content, expected) => {
    expect(ansiToLines(content).map(lineText)).toEqual(expected);
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

  it.each([
    ["https://example.com/pull/8", true],
    ["http://localhost:3000", true],
    ["HTTPS://EXAMPLE.COM", true],
    ["javascript:alert(1)", false],
    ["vscode://file/etc/passwd", false],
    ["ssh://host", false],
    ["file:///etc/passwd", false],
    ["mailto:a@b.c", false],
    ["https://example.com/\u001B]8;;x", false],
    ["https://", false],
  ])("isHttpUrl(%j) is %s", (url, expected) => {
    expect(isHttpUrl(url)).toBe(expected);
  });
});

describe("hyperlinks across lines", () => {
  const spanning = "\x1b]8;;https://example.com\x1b\\first\nsecond\x1b]8;;\x1b\\\nplain\n";
  const expected = [
    [{ text: "first", style: {}, url: "https://example.com" }],
    [{ text: "second", style: {}, url: "https://example.com" }],
    [{ text: "plain", style: {} }],
  ];

  it("carries a link target onto each line, with or without the cache", () => {
    expect(ansiToLines(spanning)).toEqual(expected);
    expect(new LineParseCache().lines(spanning)).toEqual(expected);
  });

  it("keys the cache on the open link, not just the style", () => {
    const content = ["same", "\x1b]8;;https://example.com\x1b\\", "same", "\x1b]8;;\x1b\\", "same", ""].join("\n");
    const rows = new LineParseCache().lines(content);
    expect([rows[0], rows[2], rows[4]]).toEqual([
      [{ text: "same", style: {} }],
      [{ text: "same", style: {}, url: "https://example.com" }],
      [{ text: "same", style: {} }],
    ]);
  });
});

describe("wrapLine", () => {
  it.each([
    ["within the limit", [seg("hello world")], 80],
    ["zero cols", [seg("abcdef")], 0],
    ["infinite cols", [seg("abcdef")], Number.POSITIVE_INFINITY],
    ["a mark that fits", [seg("e\u0301x")], 2],
  ])("is the identity %s", (_name, line, cols) => {
    expect(wrapLine(line, cols)).toEqual([line]);
  });

  it("returns one empty row for an empty line", () => {
    expect(wrapLine([], 10)).toEqual([[]]);
  });

  it("keeps a hyperlink target on every wrapped row", () => {
    const url = "https://example.com/long";
    expect(wrapLine([{ text: "aaaabbbb", style: {}, url }], 4)).toEqual([
      [{ text: "aaaa", style: {}, url }],
      [{ text: "bbbb", style: {}, url }],
    ]);
  });

  it("hard-wraps at the column boundary preserving styles", () => {
    const rows = wrapLine([seg("aaaa", "red"), seg("bbbb")], 3);
    expect(texts(rows)).toEqual(["aaa", "abb", "bb"]);
    expect([rows[0]![0]!.style.fg, rows[1]![0]!.style.fg, rows[1]![1]!.style.fg]).toEqual(["red", "red", undefined]);
  });

  it.each([
    ["an emoji surrogate pair", "a\u{1F600}\u{1F600}", 2, ["a", "\u{1F600}", "\u{1F600}"]],
    ["two-cell CJK", "你好世界", 4, ["你好", "世界"]],
    ["CJK that fits in code units", "你好世", 4, ["你好", "世"]],
    ["a combining mark with its base", "e\u0301x", 1, ["e\u0301", "x"]],
  ])("wraps %s whole", (_name, text, cols, expected) => {
    expect(texts(wrapLine([seg(text)], cols))).toEqual(expected);
  });

  it.each([2, 3, 4])("never splits a cluster at %s cols", (cols) => {
    const text = `${DEV}\u{1F44D}`;
    expect(
      wrapLine([seg(text)], cols)
        .flat()
        .map((s) => s.text)
        .join(""),
    ).toBe(text);
  });
});

it.each<[string, number, number | null]>([
  ["hello", 0, 0],
  ["hello", 4, 4],
  ["hello", 5, null],
  ["你好", 4, null],
  ["你好世界", 0, 0],
  ["你好世界", 2, 1],
  ["你好世界", 4, 2],
  ["你好世界", 6, 3],
  ["a\u{1F600}", 0, 0],
  ["a\u{1F600}", 1, 1],
  ["a\u{1F600}", 2, 1],
  ["e\u0301x", 0, 0],
  ["e\u0301x", 1, 2],
  [FLAG_US, 0, 0],
  [FLAG_US, 1, 0],
  [FLAG_US, 2, null],
  [FAMILY, 0, 0],
  [FAMILY, 1, 0],
  [FAMILY, 2, null],
])("findCursorCharIndex(%j, %s) is %s", (text, col, expected) => {
  expect(findCursorCharIndex(text, col)).toBe(expected);
});

it.each([
  ["no links here", [{ text: "no links here", url: null }]],
  ["https://github.com/o/r/pull/1", [{ text: "https://github.com/o/r/pull/1", url: "https://github.com/o/r/pull/1" }]],
  [
    "open http://localhost:3000 now",
    [
      { text: "open ", url: null },
      { text: "http://localhost:3000", url: "http://localhost:3000" },
      { text: " now", url: null },
    ],
  ],
  [
    "see https://example.com/a).",
    [
      { text: "see ", url: null },
      { text: "https://example.com/a", url: "https://example.com/a" },
      { text: ").", url: null },
    ],
  ],
  [
    "https://github.com/o/r를 확인",
    [
      { text: "https://github.com/o/r를", url: "https://github.com/o/r를" },
      { text: " 확인", url: null },
    ],
  ],
  [
    "https://a.com and https://b.com",
    [
      { text: "https://a.com", url: "https://a.com" },
      { text: " and ", url: null },
      { text: "https://b.com", url: "https://b.com" },
    ],
  ],
  ["localhost:3000 is up", [{ text: "localhost:3000 is up", url: null }]],
])("splitUrls(%j)", (text, expected) => {
  expect(splitUrls(text)).toEqual(expected);
});

describe("LineParseCache", () => {
  it.each([
    "one\ntwo\nthree\n",
    "prompt\n\n\n",
    "\x1b[31mred\nstill-red\x1b[0m plain\n",
    "",
    "no-trailing-newline",
    "\x1b[1;38;5;208mbold orange\x1b[0m\nnext\n",
    "a\n\x1b[0m",
    "\x1b[7minverse\x1b[27m\n\x1b[4munder\x1b[24m\n",
  ])("matches ansiToLines for %j, including a repeat frame", (content) => {
    const cache = new LineParseCache();
    expect(cache.lines(content)).toEqual(ansiToLines(content));
    expect(cache.lines(content)).toEqual(ansiToLines(content));
  });

  it("keeps segment-array identity for unchanged and slid lines", () => {
    const cache = new LineParseCache();
    const a = cache.lines("\x1b[32mok\x1b[0m line\nsteady\ntail 1\n");
    const b = cache.lines("\x1b[32mok\x1b[0m line\nsteady\ntail 2\n");
    expect([b[0], b[1]]).toEqual([a[0], a[1]]);
    expect(b[0]).toBe(a[0]);
    expect(b[1]).toBe(a[1]);
    expect(b[2]).not.toBe(a[2]);
    const slid = new LineParseCache();
    const c = slid.lines("alpha\nbeta\ngamma\n");
    const d = slid.lines("beta\ngamma\ndelta\n");
    expect(d[0]).toBe(c[1]);
    expect(d[1]).toBe(c[2]);
  });

  it("does not confuse identical raw lines entered under different SGR state", () => {
    const lines = new LineParseCache().lines("text\n\x1b[31mred\ntext\n");
    expect(lines[0]![0]!.style.fg).toBeUndefined();
    expect(lines[2]![0]!.style.fg).toBeTruthy();
  });

  it("evicts entries unused for two frames", () => {
    const cache = new LineParseCache();
    const a = cache.lines("gone\n");
    cache.lines("other\n");
    cache.lines("another\n");
    const b = cache.lines("gone\n");
    expect(b[0]).toEqual(a[0]);
    expect(b[0]).not.toBe(a[0]);
  });
});

describe("cell runs and widths", () => {
  it("keeps printable ASCII as flow and coalesces non-ASCII stretches", () => {
    const runs = splitCellRuns("$ ok 한글 ⠋⠙ \u{E0B0}");
    expect(runs.map((r) => r.fixed)).toEqual([false, true, false, true, false, true]);
    expect(runs.filter((r) => r.fixed).map((r) => [r.text, r.cells])).toEqual([
      ["한글", 4],
      ["⠋⠙", 2],
      ["\u{E0B0}", 1],
    ]);
  });

  it.each([
    ["$ ls --color", { text: "$ ls --color", cells: 12, fixed: false }],
    ["سلام", { text: "سلام", cells: 4, fixed: true }],
    [FLAG_US, { text: FLAG_US, cells: 2, fixed: true }],
    ["\u{1F44D}\u{1F3FB}", { text: "\u{1F44D}\u{1F3FB}", cells: 2, fixed: true }],
    [DEV, { text: DEV, cells: 2, fixed: true }],
    ["#\uFE0F\u20E3", { text: "#\uFE0F\u20E3", cells: 2, fixed: false }],
  ])("keeps %j as one run", (text, run) => {
    expect(splitCellRuns(text)).toEqual([run]);
  });

  it.each<[string, number]>([
    ["⚠\uFE0F", 2],
    ["✔\uFE0F", 2],
    ["ℹ\uFE0F", 2],
    ["✏\uFE0F", 2],
    ["❤\uFE0F", 2],
    [FLAG_US, 2],
    [FAMILY, 2],
    [`\u{1F468}${ZWJ}\u{1F469}${ZWJ}\u{1F466}`, 2],
    ["\u{1F44D}\u{1F3FB}", 2],
    ["\u{1F600}", 2],
    ["\u{1F1EB}", 1],
    ["한", 2],
    ["⠋", 1],
    ["\u{E0B0}", 1],
    ["e\u0301", 1],
    ["\u{1F600}\u{1F3FB}", 4],
    ["漢\u{1F3FB}", 4],
    ["⠋\u{1F3FB}", 3],
    ["a\u{1F3FB}", 3],
    ["\u{1F3FB}", 2],
    ["\uFE0F", 0],
    [ZWJ, 0],
    [`a${ZWJ}b`, 2],
    ["\u20E3", 0],
    ["1\u20E3", 1],
    ["#\uFE0F\u20E3", 2],
    ["1\uFE0F\u20E3", 2],
    ["*\uFE0F\u20E3", 2],
  ])("textWidth(%j) matches tmux: %s", (input, expected) => {
    expect(textWidth(input)).toBe(expected);
  });

  it.each([
    "$ ready",
    "가각ᅟ⠋⠙",
    "\u{E0B0}\u{E0B2} powerline",
    "e\u0301glue",
    "emoji \u{1F600} done",
    `${FLAG_US} flag`,
    "tone \u{1F44D}\u{1F3FB}",
    `dev ${DEV} ops`,
    "arabic 漢\u0301 glue",
    "warn ⚠\uFE0F end",
  ])("runs of %j cover the text and sum to its width", (line) => {
    const runs = splitCellRuns(line);
    expect(runs.reduce((n, r) => n + r.cells, 0)).toBe(textWidth(line));
    expect(runs.map((r) => r.text).join("")).toBe(line);
  });
});

it.each<[string, number, [number, number]]>([
  ["한글", 0, [0, 1]],
  ["한글", 1, [1, 2]],
  [FLAG_US, 0, [0, 2]],
  [FLAG_US, 1, [0, 2]],
  ["\u{1F1E9}\u{1F1EA}\u{1F3FB}", 0, [0, 2]],
  ["\u{1F1E9}\u{1F1EA}\u{1F3FB}", 1, [0, 2]],
  ["\u{1F44D}\u{1F3FB}", 0, [0, 2]],
  [`a\u{1F468}${ZWJ}\u{1F469}${ZWJ}\u{1F466}b`, 1, [1, 6]],
  [`a\u{1F468}${ZWJ}\u{1F469}${ZWJ}\u{1F466}b`, 6, [6, 7]],
  [`${FLAG_US}\u{1F1E9}\u{1F1EA}`, 0, [0, 2]],
  [`${FLAG_US}\u{1F1E9}\u{1F1EA}`, 1, [0, 2]],
  [`${FLAG_US}\u{1F1E9}\u{1F1EA}`, 2, [2, 4]],
  [`${FLAG_US}\u{1F1E9}\u{1F1EA}`, 3, [2, 4]],
])("clusterSpanAt(%j, %s) is %o", (text, index, expected) => {
  expect(clusterSpanAt(text, index)).toEqual(expected);
});
