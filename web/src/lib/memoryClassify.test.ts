import { describe, expect, it } from "vitest";
import { classifyMemory, isMemoryPath, parseMemoryFrontmatter } from "./memoryClassify";
import type { ToolCall } from "./acpTypes";

function tool(name: string, kind: ToolCall["kind"], args: Record<string, unknown> = {}): ToolCall {
  return {
    id: "tc-1",
    name,
    kind,
    args_preview: JSON.stringify(args),
    started_at: "2026-01-01T00:00:00Z",
  };
}

describe("isMemoryPath", () => {
  it("matches the canonical per-project memory dir", () => {
    expect(isMemoryPath("/Users/jules/.claude/projects/-Users-jules-foo/memory/user_role.md")).toBe(true);
  });

  it("matches MEMORY.md at the root of a memory dir", () => {
    expect(isMemoryPath("/Users/jules/.claude/projects/-Users-jules-foo/memory/MEMORY.md")).toBe(true);
  });

  it("rejects non-md files", () => {
    expect(isMemoryPath("/Users/jules/.claude/projects/-Users-jules-foo/memory/notes.txt")).toBe(false);
  });

  it("rejects unrelated paths that merely contain the word memory", () => {
    expect(isMemoryPath("/Users/jules/memory/notes.md")).toBe(false);
    expect(isMemoryPath("/Users/jules/.claude/projects/foo/memory.md")).toBe(false);
  });

  it("rejects paths outside the .claude/projects root", () => {
    expect(isMemoryPath("/tmp/projects/foo/memory/user.md")).toBe(false);
  });
});

describe("classifyMemory", () => {
  const path = "/Users/jules/.claude/projects/-Users-jules-foo/memory/feedback_testing.md";

  it("classifies a Read on a memory file as 'recalled'", () => {
    const r = classifyMemory(tool("Read", "read", { file_path: path }));
    expect(r.isMemory).toBe(true);
    if (r.isMemory) {
      expect(r.verb).toBe("recalled");
      expect(r.basename).toBe("feedback_testing.md");
      expect(r.isIndex).toBe(false);
    }
  });

  it("classifies a Write on a memory file as 'saved'", () => {
    const r = classifyMemory(tool("Write", "edit", { file_path: path }));
    expect(r.isMemory).toBe(true);
    if (r.isMemory) expect(r.verb).toBe("saved");
  });

  it("classifies an Edit on a memory file as 'updated'", () => {
    const r = classifyMemory(tool("Edit", "edit", { file_path: path }));
    expect(r.isMemory).toBe(true);
    if (r.isMemory) expect(r.verb).toBe("updated");
  });

  it("flags MEMORY.md as the index", () => {
    const idx = "/Users/jules/.claude/projects/-Users-jules-foo/memory/MEMORY.md";
    const r = classifyMemory(tool("Read", "read", { file_path: idx }));
    expect(r.isMemory).toBe(true);
    if (r.isMemory) {
      expect(r.isIndex).toBe(true);
      expect(r.basename).toBe("MEMORY.md");
    }
  });

  it("falls back to the 'path' arg name", () => {
    const r = classifyMemory(tool("Read", "read", { path }));
    expect(r.isMemory).toBe(true);
  });

  it("rejects file ops outside the memory dir", () => {
    const r = classifyMemory(tool("Read", "read", { file_path: "/Users/jules/foo.md" }));
    expect(r.isMemory).toBe(false);
  });

  it("rejects tools whose verb cannot be mapped", () => {
    const r = classifyMemory(tool("Glob", "search", { file_path: path }));
    expect(r.isMemory).toBe(false);
  });
});

describe("parseMemoryFrontmatter", () => {
  it("extracts name, description, and type", () => {
    const text = [
      "---",
      "name: Testing approach",
      "description: integration tests must hit a real database",
      "type: feedback",
      "---",
      "",
      "Body content here.",
    ].join("\n");
    const r = parseMemoryFrontmatter(text);
    expect(r.name).toBe("Testing approach");
    expect(r.description).toBe("integration tests must hit a real database");
    expect(r.type).toBe("feedback");
    expect(r.body).toBe("Body content here.");
  });

  it("strips matched surrounding quotes from values", () => {
    const text = ["---", 'name: "Quoted name"', "type: 'user'", "---", "", "body"].join("\n");
    const r = parseMemoryFrontmatter(text);
    expect(r.name).toBe("Quoted name");
    expect(r.type).toBe("user");
  });

  it("fails soft when frontmatter is missing", () => {
    const text = "Just a body, no frontmatter.";
    const r = parseMemoryFrontmatter(text);
    expect(r.name).toBeNull();
    expect(r.description).toBeNull();
    expect(r.type).toBeNull();
    expect(r.body).toBe(text);
  });

  it("fails soft when the closing fence is missing", () => {
    const text = "---\nname: foo\n\nno closing fence here";
    const r = parseMemoryFrontmatter(text);
    expect(r.name).toBeNull();
    expect(r.body).toBe(text);
  });
});
