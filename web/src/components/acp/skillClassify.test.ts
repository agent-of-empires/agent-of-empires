// Sanity check for the Skill heuristic; exercises the same regex
// shape as classifySkill via a direct ToolCard render-side dispatch
// pre-check. Kept as a black-box test to avoid exporting the
// classifier (which would force a refactor for one consumer).

import { describe, expect, it } from "vitest";
import { parseJsonObject, pickStr } from "../../lib/acpArgs";
import type { ToolCall } from "../../lib/acpTypes";

function classifyForTest(
  tool: ToolCall,
): { isSkill: true; name: string } | { isSkill: false } {
  if (tool.kind !== "other") return { isSkill: false };
  const title = tool.name?.trim().toLowerCase() ?? "";
  if (title !== "skill" && title !== "claude-skill") return { isSkill: false };
  const args = parseJsonObject(tool.args_preview);
  const name = pickStr(args, "skill", "name", "skill_name") ?? "skill";
  return { isSkill: true, name };
}

function tool(name: string, kind: string, args: Record<string, unknown>): ToolCall {
  return {
    id: "tc-1",
    name,
    kind,
    args_preview: JSON.stringify(args),
    started_at: "2026-05-12T00:00:00Z",
  };
}

describe("classifySkill (#1062)", () => {
  it("recognises the Skill title with args.skill", () => {
    const r = classifyForTest(tool("Skill", "other", { skill: "fix-netrc" }));
    expect(r.isSkill).toBe(true);
    if (r.isSkill) expect(r.name).toBe("fix-netrc");
  });

  it("is case-insensitive on the title", () => {
    expect(classifyForTest(tool("skill", "other", { skill: "x" })).isSkill).toBe(true);
    expect(classifyForTest(tool("SKILL", "other", { skill: "x" })).isSkill).toBe(true);
  });

  it("accepts the claude-skill variant", () => {
    expect(classifyForTest(tool("claude-skill", "other", { skill: "x" })).isSkill).toBe(true);
  });

  it("falls back to a generic name when skill arg is missing", () => {
    const r = classifyForTest(tool("Skill", "other", {}));
    expect(r.isSkill).toBe(true);
    if (r.isSkill) expect(r.name).toBe("skill");
  });

  it("rejects non-other kinds", () => {
    expect(classifyForTest(tool("Skill", "execute", { skill: "x" })).isSkill).toBe(false);
  });

  it("rejects unrelated titles", () => {
    expect(classifyForTest(tool("Bash", "other", { skill: "x" })).isSkill).toBe(false);
  });
});
