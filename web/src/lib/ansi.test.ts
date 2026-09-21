import { describe, expect, it } from "vitest";

import { collapseCarriageReturns, hasAnsi, parseAnsi, stripAnsi, type AnsiStyle } from "./ansi";

const ESC = String.fromCharCode(0x1b);
const link = (url: string, text: string, terminator = `${ESC}\\`) =>
  `${ESC}]8;;${url}${terminator}${text}${ESC}]8;;${terminator}`;

it.each([
  [`${ESC}[01;34mfoo${ESC}[0m`, true, "foo"],
  [`${ESC}[1;31mbold red${ESC}[0m`, true, "bold red"],
  [`${ESC}[2K${ESC}[1Aredraw`, true, "redraw"],
  ["plain text", false, "plain text"],
  [`docs say: prefix is "${ESC}[" then params`, false, undefined],
  [link("https://x.com", "click"), true, undefined],
  [`${ESC}]0;title${ESC}\\`, true, undefined],
])("hasAnsi/stripAnsi(%j)", (text, has, stripped) => {
  expect(hasAnsi(text)).toBe(has);
  if (stripped !== undefined) expect(stripAnsi(text)).toBe(stripped);
});

it.each([
  ["p:1/3\rp:2/3\rp:3/3", "p:3/3"],
  ["a\nb\rc\nd", "a\nc\nd"],
  ["plain\nlines", "plain\nlines"],
  ["line1\r\nline2\r\n", "line1\r\nline2\r\n"],
  ["p:1/3\rp:2/3\rp:3/3\r\nnext", "p:3/3\r\nnext"],
])("collapseCarriageReturns(%j) is %j", (text, expected) => {
  expect(collapseCarriageReturns(text)).toBe(expected);
});

describe("parseAnsi", () => {
  it.each<[string, string, [string, AnsiStyle][]]>([
    ["plain text", "hello world", [["hello world", {}]]],
    ["collapsed carriage returns", "progress: 1/3\rprogress: 2/3\rprogress: 3/3", [["progress: 3/3", {}]]],
    [
      "bold fg and reset",
      `${ESC}[0m${ESC}[01;34mApplications${ESC}[0m\nbin`,
      [
        ["Applications", { bold: true, fg: "#2472c8" }],
        ["\nbin", {}],
      ],
    ],
    [
      "256-color and truecolor",
      `${ESC}[38;5;82mlime${ESC}[0m ${ESC}[38;2;10;20;30mrgb${ESC}[0m`,
      [
        ["lime", { fg: "rgb(51, 255, 0)" }],
        [" ", {}],
        ["rgb", { fg: "rgb(10, 20, 30)" }],
      ],
    ],
    [
      "empty SGR as a reset",
      `${ESC}[31mred${ESC}[mreset`,
      [
        ["red", { fg: "#cd3131" }],
        ["reset", {}],
      ],
    ],
    [
      "every attribute",
      `${ESC}[1m${ESC}[2m${ESC}[3m${ESC}[4m${ESC}[7mstyled`,
      [["styled", { bold: true, dim: true, italic: true, underline: true, inverse: true }]],
    ],
    [
      "attribute clears",
      `${ESC}[1;2;3;4;7mon${ESC}[22ma${ESC}[23mb${ESC}[24mc${ESC}[27md`,
      [
        ["on", { bold: true, dim: true, italic: true, underline: true, inverse: true }],
        ["a", { italic: true, underline: true, inverse: true }],
        ["b", { underline: true, inverse: true }],
        ["c", { inverse: true }],
        ["d", {}],
      ],
    ],
    [
      "fg/bg clears",
      `${ESC}[31;41;1mboth${ESC}[39mno-fg${ESC}[49mno-bg`,
      [
        ["both", { fg: "#cd3131", bg: "#cd3131", bold: true }],
        ["no-fg", { bg: "#cd3131", bold: true }],
        ["no-bg", { bold: true }],
      ],
    ],
    ["16-color background", `${ESC}[42mgreen-bg`, [["green-bg", { bg: "#0dbc79" }]]],
    ["unsupported code 53", `${ESC}[53mtext`, [["text", {}]]],
    ["low 256 index", `${ESC}[38;5;4mblue`, [["blue", { fg: "#2472c8" }]]],
    ["grayscale 232", `${ESC}[38;5;232mgray`, [["gray", { fg: "rgb(8, 8, 8)" }]]],
    ["grayscale 255", `${ESC}[38;5;255mlight`, [["light", { fg: "rgb(238, 238, 238)" }]]],
    ["256-color background", `${ESC}[48;5;82mbg`, [["bg", { bg: "rgb(51, 255, 0)" }]]],
    ["truecolor background", `${ESC}[48;2;1;2;3mtc`, [["tc", { bg: "rgb(1, 2, 3)" }]]],
    ["unknown extended mode", `${ESC}[38;9mnoop`, [["noop", {}]]],
    ["missing 256 params", `${ESC}[38;5mfill`, [["fill", { fg: "#000000" }]]],
    ["missing truecolor params", `${ESC}[38;2mfill`, [["fill", { fg: "rgb(0, 0, 0)" }]]],
  ])("%s", (_name, text, expected) => {
    expect(parseAnsi(text).map((s) => [s.text, s.style])).toEqual(expected);
  });
});

describe("OSC 8 hyperlinks and other OSC sequences", () => {
  it.each<[string, string, { text: string; style: AnsiStyle; url?: string }[]]>([
    [
      "tags the link's visible text",
      `Created PR ${link("https://github.com/x/y/pull/8", "here")}, done`,
      [
        { text: "Created PR ", style: {} },
        { text: "here", style: {}, url: "https://github.com/x/y/pull/8" },
        { text: ", done", style: {} },
      ],
    ],
    [
      "keeps SGR inside a link",
      link("https://x.com", `${ESC}[31mred link${ESC}[0m`),
      [{ text: "red link", style: { fg: "#cd3131" }, url: "https://x.com" }],
    ],
    [
      "supports a BEL terminator",
      link("https://x.com", "click", "\x07"),
      [{ text: "click", style: {}, url: "https://x.com" }],
    ],
    [
      "skips an id= parameter",
      `${ESC}]8;id=k16z3m;https://x.com/pull/8${ESC}\\click${ESC}]8;;${ESC}\\`,
      [{ text: "click", style: {}, url: "https://x.com/pull/8" }],
    ],
    [
      "runs an unterminated link to the end",
      `${ESC}]8;;https://x.com${ESC}\\trailing text`,
      [{ text: "trailing text", style: {}, url: "https://x.com" }],
    ],
    [
      "anchors a link opened after color",
      `Styled: ${ESC}[31m${ESC}]8;;https://example.com/red${ESC}\\red link${ESC}[39m${ESC}]8;;${ESC}\\`,
      [
        { text: "Styled: ", style: {} },
        { text: "red link", style: { fg: "#cd3131" }, url: "https://example.com/red" },
      ],
    ],
    [
      "keeps the link off trailing text",
      `${ESC}[32m${ESC}]8;;https://example.com${ESC}\\link${ESC}]8;;${ESC}\\ tail`,
      [
        { text: "link", style: { fg: "#0dbc79" }, url: "https://example.com" },
        { text: " tail", style: { fg: "#0dbc79" } },
      ],
    ],
    [
      "drops other OSC sequences",
      `${ESC}]0;window title${ESC}\\before${ESC}]52;c;aGk=\x07after`,
      [{ text: "beforeafter", style: {} }],
    ],
  ])("%s", (_name, text, expected) => {
    expect(parseAnsi(text)).toEqual(expected);
  });
});
