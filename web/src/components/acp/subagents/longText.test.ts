import { describe, expect, it } from "vitest";

import { previewParagraphs, revealLines } from "./longText";

const lines = (n: number) => Array.from({ length: n }, (_, i) => `line ${i + 1}`).join("\n");

describe("revealLines", () => {
  it("returns short text whole and counts what a cut hides", () => {
    expect(revealLines(lines(5), 40)).toEqual({ text: lines(5), remaining: 0 });
    const cut = revealLines(lines(100), 40);
    expect(cut.remaining).toBe(60);
    expect(cut.text.split("\n")).toHaveLength(40);
  });

  it("closes a code fence the cut left open", () => {
    const text = ["```", "a", "b", "c", "```"].join("\n");
    expect(revealLines(text, 3).text.endsWith("```")).toBe(true);
  });

  it("closes with the marker that opened the fence", () => {
    expect(revealLines(["~~~", "a", "b", "~~~"].join("\n"), 2).text.endsWith("\n~~~")).toBe(true);
    const four = ["````", "```", "x", "````"].join("\n");
    expect(revealLines(four, 3).text.endsWith("\n````")).toBe(true);
  });
});

describe("previewParagraphs", () => {
  it("stops at a paragraph boundary near four lines", () => {
    const text = "one\ntwo\n\nthree\n\nfour\nfive\nsix";
    expect(previewParagraphs(text)).toEqual({ text: "one\ntwo\n\nthree", remaining: 4 });
  });

  it("cuts a long first paragraph by line and leaves short text alone", () => {
    expect(previewParagraphs(lines(10))).toMatchObject({ remaining: 6 });
    expect(previewParagraphs("short")).toEqual({ text: "short", remaining: 0 });
  });
});
