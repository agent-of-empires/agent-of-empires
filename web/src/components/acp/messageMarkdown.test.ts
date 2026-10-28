import { marked } from "marked";
import { describe, expect, it } from "vitest";

import type { ToolOutputBlock } from "../../lib/acpTypes";
import { SUBAGENT_TASK_NAME, TOOL_GROUP_NAME } from "./activityMessages";
import { messageToMarkdown, type MessagePart } from "./messageMarkdown";

const text = (t: string): MessagePart => ({ type: "text", text: t });
const tool = (toolName: string, argsText: string, content?: string): MessagePart => ({
  type: "tool-call",
  toolName,
  argsText,
  result: content === undefined ? undefined : { content },
});

describe("messageToMarkdown", () => {
  it("joins text parts with a blank line and skips empty and non-text parts", () => {
    const parts = [text("one"), text("  "), { type: "image" }, text("two\n")];
    expect(messageToMarkdown(parts, { includeTools: false })).toBe("one\n\ntwo");
  });

  it("leaves tool calls out unless asked", () => {
    const parts = [text("before"), tool("Bash", '{"command":"ls"}', "a.txt"), text("after")];
    expect(messageToMarkdown(parts, { includeTools: false })).toBe("before\n\nafter");
  });

  it("renders each tool call between the text, minus the _aoe_ bookkeeping keys", () => {
    const parts = [
      text("before"),
      tool("Bash", '{"_aoe_title":"List files","_aoe_started_at":"t","command":"ls"}', "a.txt"),
      text("after"),
    ];
    expect(messageToMarkdown(parts, { includeTools: true })).toBe(
      ["before", '**List files**\n\n```json\n{\n  "command": "ls"\n}\n```\n\n```\na.txt\n```', "after"].join("\n\n"),
    );
  });

  it.each([
    ["no result yet", tool("Bash", '{"command":"ls"}'), '**Bash**\n\n```json\n{\n  "command": "ls"\n}\n```'],
    ["only bookkeeping args", tool("Bash", '{"_aoe_title":"x"}', "out"), "**x**\n\n```\nout\n```"],
    ["non-JSON args", tool("Bash", "ls -la", "out"), "**Bash**\n\n```json\nls -la\n```\n\n```\nout\n```"],
  ])("handles %s", (_name, part, expected) => {
    expect(messageToMarkdown([part], { includeTools: true })).toBe(expected);
  });

  it("fences output that contains backticks with a longer fence", () => {
    const out = "```rust\nfn main() {}\n```";
    expect(messageToMarkdown([tool("Read", "{}", out)], { includeTools: true })).toBe(
      `**Read**\n\n\`\`\`\`\n${out}\n\`\`\`\``,
    );
  });

  it("flattens group and subagent parts into their calls", () => {
    const child = (name: string, content: string) => ({ toolName: name, argsText: "{}", result: { content } });
    const group = tool(TOOL_GROUP_NAME, JSON.stringify({ children: [child("A", "a"), child("B", "b")] }));
    const subagent = tool(
      SUBAGENT_TASK_NAME,
      JSON.stringify({ parent: child("Task", "done"), children: [child("C", "c")] }),
    );
    const md = messageToMarkdown([group, subagent], { includeTools: true });
    expect([...md.matchAll(/^\*\*(.+)\*\*$/gm)].map((m) => m[1])).toEqual(["A", "B", "Task", "C"]);
    expect(md).not.toContain(TOOL_GROUP_NAME);
  });
});

describe("tool media", () => {
  const withOutput = (output: ToolOutputBlock[], content = "") => ({
    type: "tool-call",
    toolName: "Read",
    argsText: "{}",
    result: { content, output },
  });

  it("renders links, resource text and linked images, and names bytes it will not inline", () => {
    const md = messageToMarkdown(
      [
        withOutput(
          [
            { kind: "text", text: "Captured" },
            { kind: "resource_link", uri: "https://example.com/r", name: "Full report" },
            { kind: "resource", uri: "file:///notes.md", mime_type: "text/markdown", text: "# Notes\n- one" },
            { kind: "resource", uri: "file:///a.bin", data: "AAAA" },
            { kind: "image", mime_type: "image/png", uri: "https://example.com/i.png" },
            { kind: "image", mime_type: "image/png", data: "AAAA" },
            { kind: "audio", mime_type: "audio/wav", data: "AAAA" },
          ],
          "Captured",
        ),
      ],
      { includeTools: true },
    );
    expect(md).toBe(
      [
        "**Read**",
        "```\nCaptured\n```",
        "[Full report](https://example.com/r)",
        "`file:///notes.md`\n\n```\n# Notes\n- one\n```",
        "[resource: file:///a.bin]",
        "![](https://example.com/i.png)",
        "[image: image/png]",
        "[audio: audio/wav]",
      ].join("\n\n"),
    );
  });

  it("does not repeat a text block, which the output fence already shows, and is silent without media", () => {
    const md = messageToMarkdown([withOutput([{ kind: "text", text: "Captured" }], "Captured")], {
      includeTools: true,
    });
    expect(md).toBe("**Read**\n\n```\nCaptured\n```");
  });

  it("keeps a text block that content does not contain, such as an embedded terminal's placeholder", () => {
    const md = messageToMarkdown(
      [
        withOutput(
          [
            { kind: "text", text: "Captured" },
            { kind: "text", text: "[terminal t-1]" },
          ],
          "Captured",
        ),
      ],
      { includeTools: true },
    );
    expect(md).toBe("**Read**\n\n```\nCaptured\n```\n\n```\n[terminal t-1]\n```");
  });

  it("leaves media out of a text-only copy", () => {
    const md = messageToMarkdown(
      [{ type: "text", text: "hi" }, withOutput([{ kind: "audio", mime_type: "audio/wav", data: "AAAA" }])],
      { includeTools: false },
    );
    expect(md).toBe("hi");
  });
});

describe("fidelity of copied content", () => {
  it("keeps the indentation of a first line, which marks a code block, and drops only surrounding blank space", () => {
    expect(messageToMarkdown([text("\n\n    indented code\nnext\n\n")], { includeTools: false })).toBe(
      "    indented code\nnext",
    );
  });

  it("keeps the indentation of tool output", () => {
    const md = messageToMarkdown([tool("Bash", "{}", "\n  indented\nplain\n")], { includeTools: true });
    expect(md).toBe("**Bash**\n\n```\n  indented\nplain\n```");
  });

  it("copies output with a very large number of backtick runs", () => {
    const content = "a`".repeat(200_000);
    const md = messageToMarkdown([tool("Bash", "{}", content)], { includeTools: true });
    expect(md.startsWith("**Bash**\n\n```\na`a`")).toBe(true);
  });
});

describe("carried structured diffs", () => {
  const diff = (path: string, old_text: string | null, new_text: string | null) => ({
    path,
    old_text,
    new_text,
    created_at: "2026-10-21T00:00:00Z",
  });
  // The producer's shape: bookkeeping keys beside the real input, and an empty textual result.
  const write = (args: Record<string, unknown>, content = "") =>
    tool("Write", JSON.stringify({ _aoe_title: "Write /tmp/out.sh", ...args }), content);

  it("copies a write's file content, which the input does not carry", () => {
    const md = messageToMarkdown(
      [write({ file_path: "/tmp/out.sh", _aoe_diffs: [diff("/tmp/out.sh", null, "echo hi\n")] })],
      { includeTools: true },
    );
    expect(md).toContain("**Write /tmp/out.sh**");
    expect(md).toContain("```diff\n--- /dev/null\n+++ /tmp/out.sh\n");
    expect(md).toContain("\n+echo hi\n");
    expect(md).not.toContain("_aoe_diffs");
  });

  it("copies an edit as old and new lines, one diff per file", () => {
    const md = messageToMarkdown(
      [
        write({
          _aoe_diffs: [diff("src/a.rs", "a\nb\n", "a\nB\n"), diff("src/b.rs", "x\n", "y\n")],
        }),
      ],
      { includeTools: true },
    );
    expect(md).toContain("+++ src/a.rs");
    expect(md).toContain("\n-b\n+B\n");
    expect(md).toContain("+++ src/b.rs");
    expect(md).toContain("\n-x\n+y\n");
  });

  it("does not repeat content the visible input already carries", () => {
    const md = messageToMarkdown(
      [
        write({
          file_path: "/tmp/out.sh",
          content: "echo hi\n",
          _aoe_diffs: [diff("/tmp/out.sh", null, "echo hi\n")],
        }),
      ],
      { includeTools: true },
    );
    expect(md).not.toContain("```diff");
    expect(md).toContain("echo hi");
  });

  it("keeps the removed lines of an overwrite, though the input also carries the new content", () => {
    const md = messageToMarkdown(
      [
        write({
          file_path: "a.txt",
          content: "new\n",
          _aoe_diffs: [diff("a.txt", "old\n", "new\n")],
        }),
      ],
      { includeTools: true },
    );
    expect(md).toContain("```diff\n--- a.txt\n+++ a.txt\n");
    expect(md).toContain("\n-old\n+new\n");
  });

  it.each([
    ["malformed entries", [null, { path: 1 }]],
    ["an empty list", []],
    ["no change", [diff("/tmp/same", "same\n", "same\n")]],
  ])("ignores %s", (_name, diffs) => {
    const md = messageToMarkdown([write({ file_path: "/tmp/x", _aoe_diffs: diffs })], { includeTools: true });
    expect(md).not.toContain("```diff");
    expect(md).not.toContain("_aoe_diffs");
  });
});

describe("markdown-safe links in copied media", () => {
  // What a markdown reader makes of the copied text, not just the generated string.
  const parsed = (md: string) => {
    const found: { type: string; href: string; text: string }[] = [];
    marked.walkTokens(marked.lexer(md), (t) => {
      if (t.type === "link" || t.type === "image") found.push({ type: t.type, href: t.href, text: t.text });
    });
    return found;
  };
  const copy = (output: ToolOutputBlock[]) =>
    messageToMarkdown([{ type: "tool-call", toolName: "Read", argsText: "{}", result: { content: "", output } }], {
      includeTools: true,
    });

  it.each([
    ["an unmatched closing paren", "https://example.com/report).pdf"],
    ["balanced parens", "https://example.com/a(1).pdf"],
    ["a space", "https://example.com/my report.pdf"],
    ["a backslash before a paren", "https://example.com/a\\(b"],
  ])("keeps the whole destination of a link with %s", (_name, uri) => {
    const links = parsed(copy([{ kind: "resource_link", uri, name: "report" }]));
    expect(links).toHaveLength(1);
    expect(decodeURI(links[0]!.href)).toBe(uri);
    expect(links[0]!.text).toBe("report");
  });

  it("keeps a label with brackets as one link", () => {
    const links = parsed(copy([{ kind: "resource_link", uri: "https://example.com/r", name: "Report [final]" }]));
    expect(links).toHaveLength(1);
    expect(links[0]!.text).toBe("Report [final]");
  });

  it("keeps the whole destination of a linked image and of an unfetchable resource", () => {
    const uri = "https://example.com/i(1).png)";
    const links = parsed(
      copy([
        { kind: "image", mime_type: "image/png", uri },
        { kind: "resource", uri: "https://example.com/doc).md" },
      ]),
    );
    expect(links.map((l) => [l.type, decodeURI(l.href)])).toEqual([
      ["image", uri],
      ["link", "https://example.com/doc).md"],
    ]);
  });
});

describe("markdown-safe code spans in copied media", () => {
  it("delimits a resource uri that contains backticks", () => {
    const md = messageToMarkdown(
      [
        {
          type: "tool-call",
          toolName: "Read",
          argsText: "{}",
          result: { content: "", output: [{ kind: "resource", uri: "file:///a`b.md", text: "x" }] },
        },
      ],
      { includeTools: true },
    );
    const code: string[] = [];
    marked.walkTokens(marked.lexer(md), (t) => {
      if (t.type === "codespan") code.push(t.text);
    });
    expect(code).toEqual(["file:///a`b.md"]);
  });
});
