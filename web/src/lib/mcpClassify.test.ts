import { expect, it } from "vitest";
import { classifyMcp, humanizeServer, humanizeVerb } from "./mcpClassify";
import type { ToolCall } from "./acpTypes";

function tool(name: string, args: Record<string, unknown> = {}): ToolCall {
  return { id: "tc-1", name, kind: "other", args_preview: JSON.stringify(args), started_at: "2026-01-01T00:00:00Z" };
}

it("classifyMcp splits mcp__server__verb names and rejects everything else", () => {
  const cases: [ToolCall, { server: string; verb: string } | null][] = [
    [tool("mcp__sentry__get_sentry_resource"), { server: "sentry", verb: "get_sentry_resource" }],
    [tool("mcp__claude_ai_HubSpot__get_user_details"), { server: "claude_ai_HubSpot", verb: "get_user_details" }],
    [tool("mcp__db-toolbox-preprod__preprod_dbsize"), { server: "db-toolbox-preprod", verb: "preprod_dbsize" }],
    [tool("", { _aoe_title: "mcp__sentry__find_issues" }), { server: "sentry", verb: "find_issues" }],
    ...["Bash", "", "mcp__sentry", "mcp____foo", "mcp__sentry__"].map((n): [ToolCall, null] => [tool(n), null]),
  ];
  for (const [t, expected] of cases) {
    const r = classifyMcp(t);
    expect(r.isMcp ? { server: r.server, verb: r.verb } : null, t.name || t.args_preview).toEqual(expected);
  }
});

it("humanizes server and verb names", () => {
  expect(humanizeServer("sentry")).toBe("Sentry");
  expect(humanizeServer("db-toolbox-preprod")).toBe("Db Toolbox Preprod");
  expect(humanizeServer("claude_ai_HubSpot")).toBe("Claude Ai HubSpot");
  expect(humanizeVerb("get_sentry_resource")).toBe("Get sentry resource");
  expect(humanizeVerb("whoami")).toBe("Whoami");
});
