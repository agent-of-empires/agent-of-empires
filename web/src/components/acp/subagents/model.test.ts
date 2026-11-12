import { describe, expect, it } from "vitest";

import type { ActivityRow, BackgroundAgent, ToolCall } from "../../../lib/acpTypes";
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
});

describe("taskPrompt", () => {
  it("reads whole args and recovers the prompt from a preview cut at the cap", () => {
    expect(taskPrompt('{"prompt":"do it"}')).toBe("do it");
    const cut = '{"description":"d","prompt":"line one\\nline \\"two\\" and more…[truncated]';
    expect(taskPrompt(cut)).toBe('line one\nline "two" and more\n\n…[truncated]');
    expect(taskPrompt("{}")).toBe("");
  });
});
