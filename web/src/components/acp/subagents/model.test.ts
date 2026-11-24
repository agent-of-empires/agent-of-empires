import { describe, expect, it } from "vitest";

import type { ActivityRow, BackgroundAgent, SubagentRecord, ToolCall } from "../../../lib/acpTypes";
import type { ToolCardProps } from "../ToolCardChrome";
import { fromBackground, fromInline, taskPrompt } from "./model";

const agent = (over: Partial<BackgroundAgent>): BackgroundAgent => ({
  agentId: "a",
  toolCallId: "t",
  description: "d",
  prompt: "p",
  model: "",
  status: "completed",
  startedAt: "2026-01-01T00:00:00Z",
  endedAt: "2026-01-01T00:01:00Z",
  toolCount: 1,
  tools: [{ name: "Read", title: "a.ts" }],
  lastTool: "Read",
  lastText: null,
  result: null,
  warning: null,
  ...over,
});

describe("fromBackground", () => {
  it("shows an unresolved call as stopped once the agent ended, running while it may recover", () => {
    expect(fromBackground(agent({})).tools[0]!.status).toBe("stopped");
    const stalled = agent({ status: "stalled", endedAt: null });
    expect(fromBackground(stalled).tools[0]!.status).toBe("running");
  });

  it("keeps every line of a title for expansion and only the first on the row", () => {
    const tool = { name: "Bash", title: "cd repo\ncargo test", ok: true };
    const [t] = fromBackground(agent({ tools: [tool] })).tools;
    expect(t!.target).toBe("cd repo");
    expect(t!.detail).toBe("cd repo\ncargo test");
  });

  it("shows a failed child's own error over the generic fallback", () => {
    const failed = agent({ status: "error", error: "Authentication required" });
    expect(fromBackground(failed).error).toBe("Authentication required");
    expect(fromBackground(agent({ status: "error" })).error).toBe("background agent error");
  });

  it("keeps the total and newest call when the list is capped", () => {
    const record = fromBackground(agent({ toolCount: 300, lastTool: "Bash" }));
    expect(record.toolCount).toBe(300);
    expect(record.lastTool).toBe("Bash");
  });
});

describe("fromInline", () => {
  it("takes the Task status from its own result, not from a child still unresolved", () => {
    const tool: ToolCall = {
      id: "task",
      name: "Task",
      kind: "other",
      args_preview: "{}",
      started_at: "2026-01-01T00:00:00Z",
    };
    const child: ToolCall = { ...tool, id: "c", name: "Read" };
    const result: ActivityRow = { id: "r", kind: "tool_complete", text: "report" };
    const record = fromInline(tool, result, [{ tool: child, result: undefined }]);
    expect(record.status).toBe("done");
    expect(record.report).toBe("report");
    expect(record.tools[0]!.status).toBe("stopped");
  });

  it("shows a background child as background and running until it reports", () => {
    const tool: ToolCall = {
      id: "launch",
      name: "Explore",
      kind: "think",
      args_preview: "{}",
      started_at: "2026-01-01T00:00:00Z",
    };
    const roster: SubagentRecord = {
      session_id: "child",
      parent_session_id: "root",
      parent_tool_call_id: "launch",
      state: "running",
      cancel_capable: true,
      background: true,
    };
    const record = fromInline(tool, undefined, [], roster);
    expect(record.background).toBe(true);
    expect(record.status).toBe("running");
    expect(record.report).toBe("");
  });
});

describe("fromInline activity rows", () => {
  const launch: ToolCall = {
    id: "task",
    name: "Task",
    kind: "other",
    args_preview: "{}",
    started_at: "2026-01-01T00:00:00Z",
  };
  const child = (id: string, name: string, args: Record<string, string>, kind = "other"): ToolCall => ({
    id,
    name,
    kind,
    args_preview: JSON.stringify(args),
    started_at: "2026-01-01T00:00:00Z",
  });

  it("names a call by its wire name, shows the command once, and labels MCP calls", () => {
    const cmd = "cargo test --all --features web";
    const rows = fromInline(launch, undefined, [
      { tool: child("a", cmd, { command: cmd, _aoe_raw_tool_name: "Bash" }, "execute") },
      { tool: child("b", cmd, { command: cmd }, "execute") },
      { tool: child("c", "Navigate", { url: "https://x", _aoe_raw_tool_name: "mcp__playwright__browser_navigate" }) },
    ]).tools;
    expect(rows.map((t) => [t.name, t.target, t.kind])).toEqual([
      ["Bash", cmd, "execute"],
      [cmd, null, "execute"],
      ["Playwright Browser navigate", "https://x", "mcp"],
    ]);
  });

  it("ends a foreground card the turn stopped, at the stop time, with its open calls stopped", () => {
    const stopped: ActivityRow = { id: "s", kind: "tool_stopped", text: "", at: "2026-01-01T00:02:00Z" };
    const record = fromInline(launch, stopped, [{ tool: child("a", "Read", {}) }]);
    expect(record.status).toBe("stopped");
    expect(record.endedAt).toBe("2026-01-01T00:02:00Z");
    expect(record.report).toBe("");
    expect(record.tools[0]!.status).toBe("stopped");
  });
});

describe("fromInline resumed background child", () => {
  const launch: ToolCall = {
    id: "launch",
    name: "Explore",
    kind: "think",
    args_preview: "{}",
    started_at: "2026-01-01T00:00:00Z",
  };
  const first: ActivityRow = {
    id: "r",
    kind: "tool_complete",
    text: "first report",
    at: "2026-01-01T00:01:00Z",
  };
  const call = (startedAt: string, endedAt?: string): ToolCardProps => ({
    tool: { id: startedAt, name: "Read", kind: "read", args_preview: "{}", started_at: startedAt },
    result: endedAt ? { id: `d-${startedAt}`, kind: "tool_complete", text: "", at: endedAt } : undefined,
  });
  const roster = (over: Partial<SubagentRecord> = {}): SubagentRecord => ({
    session_id: "child",
    parent_session_id: "root",
    parent_tool_call_id: "launch",
    state: "running",
    cancel_capable: true,
    background: true,
    ...over,
  });

  it("follows a running or waiting roster past the first result and keeps that report apart", () => {
    const running = fromInline(launch, first, [], roster());
    expect(running.status).toBe("running");
    expect(running.endedAt).toBeUndefined();
    expect(running.report).toBe("");
    expect(running.earlierReport).toBe("first report");
    expect(fromInline(launch, first, [], roster({ state: "requires_action" })).status).toBe("waiting");
  });

  it("settles an idle child that ran again at its last call, not the first result", () => {
    const calls = [call("2026-01-01T00:02:00Z", "2026-01-01T00:03:00Z")];
    const record = fromInline(launch, first, calls, roster({ state: "idle", stop_reason: "end_turn" }));
    expect(record.status).toBe("done");
    expect(record.endedAt).toBe("2026-01-01T00:03:00Z");
    expect(record.report).toBe("");
    expect(record.earlierReport).toBe("first report");
  });

  it("keeps an empty first report as earlier output so the missing final report is not blamed on the child", () => {
    const empty: ActivityRow = { ...first, text: "" };
    const calls = [call("2026-01-01T00:02:00Z", "2026-01-01T00:03:00Z")];
    const record = fromInline(launch, empty, calls, roster({ state: "idle", stop_reason: "end_turn" }));
    expect(record.status).toBe("done");
    expect(record.earlierReport).toBe("");
  });

  it("settles a text-only resume from its running stamp, ending at the last update", () => {
    const idle = roster({
      state: "idle",
      stop_reason: "end_turn",
      running_since: "2026-01-01T00:02:00Z",
      updated_at: "2026-01-01T00:04:00Z",
    });
    const record = fromInline(launch, first, [], idle);
    expect(record.status).toBe("done");
    expect(record.report).toBe("");
    expect(record.earlierReport).toBe("first report");
    expect(record.endedAt).toBe("2026-01-01T00:04:00Z");
  });

  it("keeps the report when the child last ran before its result, or carries no stamps", () => {
    const before = roster({ state: "idle", stop_reason: "end_turn", running_since: "2026-01-01T00:00:30Z" });
    for (const idle of [before, roster({ state: "idle", stop_reason: "end_turn" })]) {
      const record = fromInline(launch, first, [], idle);
      expect(record.report).toBe("first report");
      expect(record.earlierReport).toBeUndefined();
      expect(record.endedAt).toBe(first.at);
    }
  });

  it("keeps the report of an idle child with no call after its result", () => {
    const calls = [call("2026-01-01T00:00:30Z", "2026-01-01T00:00:40Z")];
    const record = fromInline(launch, first, calls, roster({ state: "idle", stop_reason: "end_turn" }));
    expect(record.status).toBe("done");
    expect(record.endedAt).toBe(first.at);
    expect(record.report).toBe("first report");
    expect(record.earlierReport).toBeUndefined();
  });

  it("fails an idle resumed child from its roster error, not the first result", () => {
    const calls = [call("2026-01-01T00:02:00Z", "2026-01-01T00:03:00Z")];
    const idle = roster({ state: "idle", stop_reason: "error", error: "boom" });
    const record = fromInline(launch, first, calls, idle);
    expect(record.status).toBe("failed");
    expect(record.error).toBe("boom");
  });

  it("leaves a child with an unknown roster state done by its result", () => {
    const record = fromInline(launch, first, [], roster({ state: "unknown" }));
    expect(record.status).toBe("done");
    expect(record.report).toBe("first report");
    expect(record.earlierReport).toBeUndefined();
  });

  it("still follows a running roster when the launch call was stopped", () => {
    const stopped: ActivityRow = { id: "s", kind: "tool_stopped", text: "", at: "2026-01-01T00:01:00Z" };
    const record = fromInline(launch, stopped, [], roster());
    expect(record.status).toBe("running");
    expect(record.earlierReport).toBeUndefined();
  });

  it("does not treat a result without a time as superseded", () => {
    const untimed: ActivityRow = { id: "r", kind: "tool_complete", text: "first report" };
    const calls = [call("2026-01-01T00:02:00Z", "2026-01-01T00:03:00Z")];
    const record = fromInline(launch, untimed, calls, roster({ state: "idle", stop_reason: "end_turn" }));
    expect(record.status).toBe("done");
    expect(record.report).toBe("first report");
    expect(record.earlierReport).toBeUndefined();
  });

  it("leaves a foreground card done by its result", () => {
    const record = fromInline(launch, first, [], roster({ background: false }));
    expect(record.status).toBe("done");
    expect(record.report).toBe("first report");
    expect(record.earlierReport).toBeUndefined();
  });
});

describe("taskPrompt", () => {
  it("reads whole args and recovers the prompt from a preview cut at the cap", () => {
    expect(taskPrompt('{"prompt":"do it"}')).toBe("do it");
    const cut = '{"description":"d","prompt":"line one\\nline \\"two\\" and more…[truncated]';
    expect(taskPrompt(cut)).toBe('line one\nline "two" and more\n\n…[truncated]');
    expect(taskPrompt("{}")).toBe("");
  });
});
