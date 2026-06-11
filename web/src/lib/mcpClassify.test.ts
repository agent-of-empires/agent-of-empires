import { describe, expect, it } from "vitest";
import { classifyMcp, humanizeServer, humanizeVerb } from "./mcpClassify";
import type { ToolCall } from "./acpTypes";

function tool(name: string, args: Record<string, unknown> = {}): ToolCall {
  return {
    id: "tc-1",
    name,
    kind: "other",
    args_preview: JSON.stringify(args),
    started_at: "2026-01-01T00:00:00Z",
  };
}

describe("classifyMcp", () => {
  it("recognises a plain mcp__server__verb name", () => {
    const r = classifyMcp(tool("mcp__sentry__get_sentry_resource"));
    expect(r.isMcp).toBe(true);
    if (r.isMcp) {
      expect(r.server).toBe("sentry");
      expect(r.verb).toBe("get_sentry_resource");
    }
  });

  it("handles servers with underscores in their name", () => {
    const r = classifyMcp(tool("mcp__claude_ai_HubSpot__get_user_details"));
    expect(r.isMcp).toBe(true);
    if (r.isMcp) {
      expect(r.server).toBe("claude_ai_HubSpot");
      expect(r.verb).toBe("get_user_details");
    }
  });

  it("handles servers with hyphens in their name", () => {
    const r = classifyMcp(tool("mcp__db-toolbox-preprod__preprod_cluster_dbsize"));
    expect(r.isMcp).toBe(true);
    if (r.isMcp) {
      expect(r.server).toBe("db-toolbox-preprod");
      expect(r.verb).toBe("preprod_cluster_dbsize");
    }
  });

  it("falls back to _aoe_title when tool.name is empty", () => {
    const t = tool("", { _aoe_title: "mcp__sentry__find_issues" });
    const r = classifyMcp(t);
    expect(r.isMcp).toBe(true);
    if (r.isMcp) expect(r.server).toBe("sentry");
  });

  it("rejects non-mcp names", () => {
    expect(classifyMcp(tool("Bash")).isMcp).toBe(false);
    expect(classifyMcp(tool("Read")).isMcp).toBe(false);
    expect(classifyMcp(tool("")).isMcp).toBe(false);
  });

  it("rejects malformed mcp names missing parts", () => {
    expect(classifyMcp(tool("mcp__sentry")).isMcp).toBe(false);
    expect(classifyMcp(tool("mcp____foo")).isMcp).toBe(false);
    expect(classifyMcp(tool("mcp__sentry__")).isMcp).toBe(false);
  });
});

describe("humanizeServer", () => {
  it("title-cases all-lowercase chunks", () => {
    expect(humanizeServer("sentry")).toBe("Sentry");
    expect(humanizeServer("db-toolbox-preprod")).toBe("Db Toolbox Preprod");
  });

  it("preserves existing mixed case", () => {
    expect(humanizeServer("claude_ai_HubSpot")).toBe("Claude Ai HubSpot");
  });
});

describe("humanizeVerb", () => {
  it("converts snake_case to sentence case", () => {
    expect(humanizeVerb("get_sentry_resource")).toBe("Get sentry resource");
    expect(humanizeVerb("create_event")).toBe("Create event");
    expect(humanizeVerb("whoami")).toBe("Whoami");
  });
});
