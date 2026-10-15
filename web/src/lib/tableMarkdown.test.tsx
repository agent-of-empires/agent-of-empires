// @vitest-environment jsdom

import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import { tableSource, type TableNode } from "./tableMarkdown";

/** The hast node react-markdown hands each table when it parses `parsed`. */
function tableNodes(parsed: string): TableNode[] {
  const nodes: TableNode[] = [];
  renderToStaticMarkup(
    <ReactMarkdown
      remarkPlugins={[remarkGfm]}
      components={{
        table: ({ node }) => {
          if (node) nodes.push(node);
          return null;
        },
      }}
    >
      {parsed}
    </ReactMarkdown>,
  );
  return nodes;
}

function copies(text: string, parsed = text, complete = true): (string | null)[] {
  return tableNodes(parsed).map((node) => tableSource(text, node, complete));
}

const PLAIN = "| a | b |\n|---|---|\n| 1 | 2 |";

describe("tableSource", () => {
  it.each([
    ["plain, between paragraphs", `intro\n\n${PLAIN}\n\noutro`, PLAIN],
    ["aligned columns keep their colons", "| a | b |\n|:--|--:|\n| 1 | 2 |", "| a | b |\n|:--|--:|\n| 1 | 2 |"],
    ["no leading or trailing pipes", "a | b\n--|--\n1 | 2", "a | b\n--|--\n1 | 2"],
    ["header only", "| a | b |\n|---|---|", "| a | b |\n|---|---|"],
    ["escaped pipe inside code", "| a |\n|---|\n| `x \\| y` |", "| a |\n|---|\n| `x \\| y` |"],
    ["CRLF line endings", "| a |\r\n|---|\r\n| 1 |\r\n", "| a |\n|---|\n| 1 |"],
    ["blockquote", "> Results:\n>\n> | a | b |\n> |---|---|\n> | 1 | 2 |", PLAIN],
    ["nested blockquote", "> > | a |\n> > |---|\n> > | 1 |", "| a |\n|---|\n| 1 |"],
    ["blockquote with uneven spacing", "> | a |\n>|---|\n>   | 1 |", "| a |\n|---|\n| 1 |"],
    ["list item", "- Results:\n\n  | a | b |\n  |---|---|\n  | 1 | 2 |", PLAIN],
    ["nested list item", "- a\n  - b\n\n    | a | b |\n    |---|---|\n    | 1 | 2 |", PLAIN],
    ["ordered list item", "1. x\n\n   | a |\n   |---|\n   | 1 |", "| a |\n|---|\n| 1 |"],
    ["blockquote inside a list", "- > | a |\n  > |---|\n  > | 1 |", "| a |\n|---|\n| 1 |"],
    ["no trailing newline", `text\n\n${PLAIN}`, PLAIN],
  ])("%s", (_name, text, expected) => {
    expect(copies(text)).toEqual([expected]);
  });

  it("returns each table's own source when several share content", () => {
    expect(copies(`${PLAIN}\n\nbetween\n\n${PLAIN}`)).toEqual([PLAIN, PLAIN]);
  });

  it("copies a table that is in the parsed prefix once the rest of the text has arrived", () => {
    const full = `${PLAIN}\n\nlater paragraph`;
    expect(copies(full, PLAIN)).toEqual([PLAIN]);
  });

  it("never copies a half-streamed row when more text has already arrived, at any prefix length", () => {
    const full = `> ${PLAIN.split("\n").join("\n> ")}`;
    const complete = new Set([PLAIN, "| a | b |\n|---|---|"]);
    for (let n = 0; n <= full.length; n++) {
      for (const copy of copies(full, full.slice(0, n), false)) {
        if (copy !== null) expect(complete, `prefix ${JSON.stringify(full.slice(0, n))}`).toContain(copy);
      }
    }
  });

  it("while streaming, waits for the line ending of the last received row, at any prefix length", () => {
    const full = `> ${PLAIN.split("\n").join("\n> ")}\n`;
    const complete = new Set([PLAIN, "| a | b |\n|---|---|"]);
    for (let n = 0; n <= full.length; n++) {
      const received = full.slice(0, n);
      for (const copy of copies(received, received, false)) {
        if (copy === null) continue;
        expect(complete).toContain(copy);
        const lineEndings = received.match(/\n/g)?.length ?? 0;
        expect(lineEndings, `prefix ${JSON.stringify(received)}`).toBeGreaterThanOrEqual(copy.split("\n").length);
      }
    }
  });

  it("accepts the end of the text as a complete row once the message has finished", () => {
    const received = "| a | b |\n|---|---|\n| 1 | 2 |";
    expect(copies(received, received, false)).toEqual([null]);
    expect(copies(received, received, true)).toEqual([received]);
  });
});
