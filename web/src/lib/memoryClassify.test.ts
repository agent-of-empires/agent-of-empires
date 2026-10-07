import { describe, expect, it } from "vitest";
import { cleanRecalledMemory, classifyMemory, isMemoryPath, parseMemoryFrontmatter } from "./memoryClassify";
import type { ToolCall } from "./acpTypes";

const DIR = "/Users/jules/.claude/projects/-Users-jules-foo/memory";
const tool = (name: string, kind: ToolCall["kind"], args: Record<string, unknown>): ToolCall => ({
  id: "tc-1",
  name,
  kind,
  args_preview: JSON.stringify(args),
  started_at: "2026-01-01T00:00:00Z",
});

it.each([
  [`${DIR}/user_role.md`, true],
  [`${DIR}/MEMORY.md`, true],
  [`${DIR}/notes.txt`, false],
  ["/Users/jules/memory/notes.md", false],
  ["/Users/jules/.claude/projects/foo/memory.md", false],
  ["/tmp/projects/foo/memory/user.md", false],
])("isMemoryPath(%j) is %s", (path, expected) => {
  expect(isMemoryPath(path)).toBe(expected);
});

it.each<[string, ToolCall, ReturnType<typeof classifyMemory>]>([
  [
    "a Read as recalled",
    tool("Read", "read", { file_path: `${DIR}/feedback_testing.md` }),
    expect.objectContaining({ isMemory: true, verb: "recalled", basename: "feedback_testing.md", isIndex: false }),
  ],
  [
    "a Write as saved",
    tool("Write", "edit", { file_path: `${DIR}/a.md` }),
    expect.objectContaining({ isMemory: true, verb: "saved" }),
  ],
  [
    "an Edit as updated",
    tool("Edit", "edit", { file_path: `${DIR}/a.md` }),
    expect.objectContaining({ isMemory: true, verb: "updated" }),
  ],
  [
    "MEMORY.md as the index",
    tool("Read", "read", { file_path: `${DIR}/MEMORY.md` }),
    expect.objectContaining({ isMemory: true, isIndex: true, basename: "MEMORY.md" }),
  ],
  ["the `path` arg", tool("Read", "read", { path: `${DIR}/a.md` }), expect.objectContaining({ isMemory: true })],
  [
    "a file outside the memory dir",
    tool("Read", "read", { file_path: "/Users/jules/foo.md" }),
    expect.objectContaining({ isMemory: false }),
  ],
  [
    "an unmappable tool",
    tool("Glob", "search", { file_path: `${DIR}/a.md` }),
    expect.objectContaining({ isMemory: false }),
  ],
])("classifyMemory: %s", (_name, call, expected) => {
  expect(classifyMemory(call)).toEqual(expected);
});

describe("parseMemoryFrontmatter", () => {
  it.each([
    [
      ["---", "name: Testing approach", "description: hit a real database", "type: feedback", "---", "", "Body."].join(
        "\n",
      ),
      { name: "Testing approach", description: "hit a real database", type: "feedback", body: "Body." },
    ],
    [
      ["---", 'name: "Quoted name"', "type: 'user'", "---", "", "body"].join("\n"),
      { name: "Quoted name", type: "user" },
    ],
  ])("parses %j", (text, expected) => {
    expect(parseMemoryFrontmatter(text)).toMatchObject(expected);
  });

  it.each(["Just a body, no frontmatter.", "---\nname: foo\n\nno closing fence here"])("fails soft on %j", (text) => {
    expect(parseMemoryFrontmatter(text)).toMatchObject({ name: null, description: null, type: null, body: text });
  });
});

it.each([
  ["<system-reminder>\nrecalled body\n</system-reminder>", "recalled body"],
  ["     1\t# Title\n     2\t\n     3\tbody line", "# Title\n\nbody line"],
  ["<system-reminder>\n     1\t# Title\n     2\t- item\n</system-reminder>", "# Title\n- item"],
  ["# Title\n\nplain body", "# Title\n\nplain body"],
  ["version 12 shipped\n3 retries left", "version 12 shipped\n3 retries left"],
])("cleanRecalledMemory(%j)", (text, expected) => {
  expect(cleanRecalledMemory(text)).toBe(expected);
});
