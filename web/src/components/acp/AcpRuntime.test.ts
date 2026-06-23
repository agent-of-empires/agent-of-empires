import { describe, expect, it } from "vitest";
import { SUBAGENT_TASK_NAME, TODO_GROUP_NAME, TOOL_GROUP_NAME, activityToThreadMessages } from "./AcpRuntime";
import type { ActivityRow, ToolCall } from "../../lib/acpTypes";

function userRow(text: string, id = "u1"): ActivityRow {
  return {
    id,
    kind: "user_prompt",
    text,
    at: "2026-05-12T00:00:00Z",
  };
}

function toolStart(id: string, kind = "read"): ActivityRow {
  const tool: ToolCall = {
    id,
    name: "Read",
    kind,
    args_preview: JSON.stringify({ path: `/tmp/${id}.txt` }),
    started_at: "2026-05-12T00:00:00Z",
  };
  return {
    id: `start-${id}`,
    kind: "tool_start",
    text: "Read",
    toolCallId: id,
    tool,
    at: "2026-05-12T00:00:00Z",
  };
}

function todoStart(id: string, todos: Array<{ content: string; status: string }>): ActivityRow {
  const tool: ToolCall = {
    id,
    name: `Update TODOs: ${todos.map((t) => t.content).join(", ")}`,
    kind: "think",
    args_preview: JSON.stringify({ todos }),
    started_at: "2026-05-12T00:00:00Z",
  };
  return {
    id: `start-${id}`,
    kind: "tool_start",
    text: "Update TODOs",
    toolCallId: id,
    tool,
    at: "2026-05-12T00:00:00Z",
  };
}

function messageRow(text: string, id = "m1"): ActivityRow {
  return {
    id,
    kind: "message",
    text,
    at: "2026-05-12T00:00:00Z",
  };
}

describe("activityToThreadMessages; tool-call grouping (#1057)", () => {
  it("folds a run of ≥3 consecutive tool calls into one group", () => {
    const messages = activityToThreadMessages(
      [userRow("go"), toolStart("t1"), toolStart("t2"), toolStart("t3"), toolStart("t4")],
      false,
    );
    const assistant = messages.find((m) => m.role === "assistant");
    expect(assistant).toBeDefined();
    const parts = assistant!.content as Array<{
      type: string;
      toolName?: string;
    }>;
    const toolParts = parts.filter((p) => p.type === "tool-call");
    expect(toolParts).toHaveLength(1);
    expect(toolParts[0]!.toolName).toBe(TOOL_GROUP_NAME);
  });

  it("does not group runs of 1 or 2 tool calls", () => {
    const messages = activityToThreadMessages([userRow("go"), toolStart("t1"), toolStart("t2")], false);
    const assistant = messages.find((m) => m.role === "assistant")!;
    const parts = assistant.content as Array<{
      type: string;
      toolName?: string;
    }>;
    const toolParts = parts.filter((p) => p.type === "tool-call");
    expect(toolParts).toHaveLength(2);
    for (const p of toolParts) expect(p.toolName).not.toBe(TOOL_GROUP_NAME);
  });

  it("text between tool calls splits two runs", () => {
    const messages = activityToThreadMessages(
      [
        userRow("go"),
        toolStart("a1"),
        toolStart("a2"),
        toolStart("a3"),
        messageRow("Found it."),
        toolStart("b1"),
        toolStart("b2"),
        toolStart("b3"),
      ],
      false,
    );
    const assistant = messages.find((m) => m.role === "assistant")!;
    const parts = assistant.content as Array<{
      type: string;
      toolName?: string;
    }>;
    const groups = parts.filter((p) => p.type === "tool-call" && p.toolName === TOOL_GROUP_NAME);
    expect(groups).toHaveLength(2);
  });

  it("exempts TodoWrite calls from folding (#1064)", () => {
    const todoTool: ToolCall = {
      id: "td-1",
      name: "Update TODOs: a, b",
      kind: "think",
      args_preview: JSON.stringify({
        todos: [
          { content: "a", status: "pending" },
          { content: "b", status: "in_progress" },
        ],
      }),
      started_at: "2026-05-12T00:00:00Z",
    };
    const todoRow: ActivityRow = {
      id: "start-td-1",
      kind: "tool_start",
      text: "Update TODOs",
      toolCallId: "td-1",
      tool: todoTool,
      at: "2026-05-12T00:00:00Z",
    };
    const messages = activityToThreadMessages(
      [userRow("go"), toolStart("a"), toolStart("b"), todoRow, toolStart("c")],
      false,
    );
    const assistant = messages.find((m) => m.role === "assistant")!;
    const parts = assistant.content as Array<{
      type: string;
      toolName?: string;
    }>;
    const groups = parts.filter((p) => p.type === "tool-call" && p.toolName === TOOL_GROUP_NAME);
    expect(groups).toHaveLength(0);
    const toolParts = parts.filter((p) => p.type === "tool-call");
    expect(toolParts).toHaveLength(4);
  });

  it("folds ≥3 consecutive TodoWrite snapshots into one todo group (#1468)", () => {
    const messages = activityToThreadMessages(
      [
        userRow("go"),
        todoStart("td1", [{ content: "a", status: "pending" }]),
        todoStart("td2", [{ content: "a", status: "in_progress" }]),
        todoStart("td3", [{ content: "a", status: "completed" }]),
      ],
      false,
    );
    const assistant = messages.find((m) => m.role === "assistant")!;
    const parts = assistant.content as Array<{
      type: string;
      toolName?: string;
    }>;
    const toolParts = parts.filter((p) => p.type === "tool-call");
    expect(toolParts).toHaveLength(1);
    expect(toolParts[0]!.toolName).toBe(TODO_GROUP_NAME);
    // The folded payload preserves each snapshot in original order so
    // the expand-history view can replay the plan's evolution.
    const payload = JSON.parse((toolParts[0] as { argsText?: string }).argsText!);
    expect(payload.children.map((c: { toolCallId: string }) => c.toolCallId)).toEqual(["td1", "td2", "td3"]);
  });

  it("folds a run ending in an empty TodoWrite clear into the todo group (#2003)", () => {
    // The real regression carries the bare tool name "TodoWrite", so the
    // `_aoe_title` "Update TODOs" rescue branch does NOT fire and the
    // empty clear must be recognized purely by its `todos: []` array.
    const todoWrite = (id: string, todos: Array<{ content: string; status: string }>): ActivityRow => ({
      id: `start-${id}`,
      kind: "tool_start",
      text: "TodoWrite",
      toolCallId: id,
      tool: {
        id,
        name: "TodoWrite",
        kind: "think",
        args_preview: JSON.stringify({ todos }),
        started_at: "2026-05-12T00:00:00Z",
      },
      at: "2026-05-12T00:00:00Z",
    });
    const messages = activityToThreadMessages(
      [
        userRow("go"),
        todoWrite("td1", [{ content: "a", status: "pending" }]),
        todoWrite("td2", [{ content: "a", status: "in_progress" }]),
        todoWrite("td3", []),
      ],
      false,
    );
    const assistant = messages.find((m) => m.role === "assistant")!;
    const parts = assistant.content as Array<{
      type: string;
      toolName?: string;
    }>;
    const toolParts = parts.filter((p) => p.type === "tool-call");
    // The empty clear is a real todo snapshot, so it stays in the fold
    // instead of leaking out as a separate ungrouped think card.
    expect(toolParts).toHaveLength(1);
    expect(toolParts[0]!.toolName).toBe(TODO_GROUP_NAME);
    const payload = JSON.parse((toolParts[0] as { argsText?: string }).argsText!);
    expect(payload.children.map((c: { toolCallId: string }) => c.toolCallId)).toEqual(["td1", "td2", "td3"]);
  });

  it("uses the generic group for todo-shaped runs when todos are disabled", () => {
    const messages = activityToThreadMessages(
      [
        userRow("go"),
        todoStart("td1", [{ content: "a", status: "pending" }]),
        todoStart("td2", [{ content: "a", status: "in_progress" }]),
        todoStart("td3", [{ content: "a", status: "completed" }]),
      ],
      false,
      false,
      false,
    );
    const assistant = messages.find((m) => m.role === "assistant")!;
    const parts = assistant.content as Array<{
      type: string;
      toolName?: string;
    }>;
    const toolParts = parts.filter((p) => p.type === "tool-call");
    expect(toolParts).toHaveLength(1);
    expect(toolParts[0]!.toolName).toBe(TOOL_GROUP_NAME);
    expect(toolParts[0]!.toolName).not.toBe(TODO_GROUP_NAME);
  });

  it("leaves 2 consecutive TodoWrite snapshots inline (#1468)", () => {
    const messages = activityToThreadMessages(
      [
        userRow("go"),
        todoStart("td1", [{ content: "a", status: "pending" }]),
        todoStart("td2", [{ content: "a", status: "completed" }]),
      ],
      false,
    );
    const assistant = messages.find((m) => m.role === "assistant")!;
    const parts = assistant.content as Array<{
      type: string;
      toolName?: string;
    }>;
    const toolParts = parts.filter((p) => p.type === "tool-call");
    expect(toolParts).toHaveLength(2);
    for (const p of toolParts) {
      expect(p.toolName).not.toBe(TODO_GROUP_NAME);
      expect(p.toolName).not.toBe(TOOL_GROUP_NAME);
    }
  });

  it("keeps a ≥3 run mixing TodoWrite with real tool work inline (#1468)", () => {
    const messages = activityToThreadMessages(
      [
        userRow("go"),
        todoStart("td1", [{ content: "a", status: "pending" }]),
        todoStart("td2", [{ content: "a", status: "in_progress" }]),
        toolStart("r1"),
      ],
      false,
    );
    const assistant = messages.find((m) => m.role === "assistant")!;
    const parts = assistant.content as Array<{
      type: string;
      toolName?: string;
    }>;
    const toolParts = parts.filter((p) => p.type === "tool-call");
    expect(toolParts).toHaveLength(3);
    for (const p of toolParts) {
      expect(p.toolName).not.toBe(TODO_GROUP_NAME);
      expect(p.toolName).not.toBe(TOOL_GROUP_NAME);
    }
  });

  it("text between TodoWrite snapshots splits the fold (#1468)", () => {
    const messages = activityToThreadMessages(
      [
        userRow("go"),
        todoStart("a1", [{ content: "a", status: "pending" }]),
        todoStart("a2", [{ content: "a", status: "in_progress" }]),
        messageRow("Working on it."),
        todoStart("b1", [{ content: "a", status: "in_progress" }]),
        todoStart("b2", [{ content: "a", status: "completed" }]),
      ],
      false,
    );
    const assistant = messages.find((m) => m.role === "assistant")!;
    const parts = assistant.content as Array<{
      type: string;
      toolName?: string;
    }>;
    const groups = parts.filter((p) => p.type === "tool-call" && p.toolName === TODO_GROUP_NAME);
    // Each side of the text is a run of 2, below the fold threshold.
    expect(groups).toHaveLength(0);
  });

  it("smuggles parent_tool_call_id through args_preview as _aoe_parent_tool_call_id (#1041)", () => {
    const childTool: ToolCall = {
      id: "ch-1",
      name: "Read",
      kind: "read",
      args_preview: JSON.stringify({ path: "/tmp/x" }),
      started_at: "2026-05-12T00:00:00Z",
      parent_tool_call_id: "task-parent-1",
    };
    const row: ActivityRow = {
      id: "start-ch-1",
      kind: "tool_start",
      text: "Read",
      toolCallId: "ch-1",
      tool: childTool,
      at: "2026-05-12T00:00:00Z",
    };
    const messages = activityToThreadMessages([userRow("go"), row], false);
    const assistant = messages.find((m) => m.role === "assistant")!;
    const parts = assistant.content as Array<{
      type: string;
      argsText?: string;
    }>;
    const child = parts.find((p) => p.type === "tool-call")!;
    const parsed = JSON.parse(child.argsText!);
    expect(parsed._aoe_parent_tool_call_id).toBe("task-parent-1");
  });

  it("smuggles memory_recall through args_preview as _aoe_memory_recall (#2142)", () => {
    const memTool: ToolCall = {
      id: "mem-1",
      name: "Recalled synthesized memory",
      kind: "read",
      args_preview: "{}",
      started_at: "2026-05-12T00:00:00Z",
      memory_recall: { mode: "synthesize", synthesized_text: "remembered" },
    };
    const row: ActivityRow = {
      id: "start-mem-1",
      kind: "tool_start",
      text: "Recalled synthesized memory",
      toolCallId: "mem-1",
      tool: memTool,
      at: "2026-05-12T00:00:00Z",
    };
    const messages = activityToThreadMessages([userRow("go"), row], false);
    const assistant = messages.find((m) => m.role === "assistant")!;
    const parts = assistant.content as Array<{ type: string; argsText?: string }>;
    const part = parts.find((p) => p.type === "tool-call")!;
    const parsed = JSON.parse(part.argsText!);
    expect(parsed._aoe_memory_recall).toEqual({ mode: "synthesize", synthesized_text: "remembered" });
  });

  it("collapses a parent Task + its children into a _aoe_subagent_task part (#1041)", () => {
    const parent: ToolCall = {
      id: "task-1",
      name: "Investigate auth bug",
      kind: "think",
      args_preview: JSON.stringify({
        description: "Investigate auth bug",
        _aoe_title: "Investigate auth bug",
      }),
      started_at: "2026-05-12T00:00:00Z",
    };
    const parentRow: ActivityRow = {
      id: "start-task-1",
      kind: "tool_start",
      text: "Task",
      toolCallId: "task-1",
      tool: parent,
      at: "2026-05-12T00:00:00Z",
    };
    const child: ToolCall = {
      id: "ch-1",
      name: "Read",
      kind: "read",
      args_preview: JSON.stringify({ path: "/x" }),
      started_at: "2026-05-12T00:00:01Z",
      parent_tool_call_id: "task-1",
    };
    const childRow: ActivityRow = {
      id: "start-ch-1",
      kind: "tool_start",
      text: "Read",
      toolCallId: "ch-1",
      tool: child,
      at: "2026-05-12T00:00:01Z",
    };
    const messages = activityToThreadMessages([userRow("go"), parentRow, childRow], false);
    const assistant = messages.find((m) => m.role === "assistant")!;
    const parts = assistant.content as Array<{
      type: string;
      toolName?: string;
      argsText?: string;
    }>;
    const subagentParts = parts.filter((p) => p.type === "tool-call" && p.toolName === SUBAGENT_TASK_NAME);
    expect(subagentParts).toHaveLength(1);
    const payload = JSON.parse(subagentParts[0]!.argsText!);
    expect(payload.parent.toolCallId).toBe("task-1");
    expect(payload.children).toHaveLength(1);
    expect(payload.children[0].toolCallId).toBe("ch-1");
    // The original child part should not appear as a top-level tool-call.
    const directChild = parts.find((p) => p.type === "tool-call" && p.toolName !== SUBAGENT_TASK_NAME);
    expect(directChild).toBeUndefined();
  });

  it("leaves orphan children in place when their parent is absent", () => {
    const orphanChild: ToolCall = {
      id: "ch-1",
      name: "Read",
      kind: "read",
      args_preview: JSON.stringify({ path: "/x" }),
      started_at: "2026-05-12T00:00:00Z",
      parent_tool_call_id: "task-elsewhere",
    };
    const childRow: ActivityRow = {
      id: "start-ch-1",
      kind: "tool_start",
      text: "Read",
      toolCallId: "ch-1",
      tool: orphanChild,
      at: "2026-05-12T00:00:00Z",
    };
    const messages = activityToThreadMessages([userRow("go"), childRow], false);
    const assistant = messages.find((m) => m.role === "assistant")!;
    const parts = assistant.content as Array<{
      type: string;
      toolName?: string;
    }>;
    const subagentParts = parts.filter((p) => p.type === "tool-call" && p.toolName === SUBAGENT_TASK_NAME);
    expect(subagentParts).toHaveLength(0);
    const toolParts = parts.filter((p) => p.type === "tool-call");
    expect(toolParts).toHaveLength(1);
  });

  it("does not fold subagent parent + children into the generic tool group", () => {
    // Three children would otherwise hit TOOL_GROUP_MIN_RUN=3.
    const parent: ToolCall = {
      id: "task-1",
      name: "Task",
      kind: "think",
      args_preview: JSON.stringify({ description: "go" }),
      started_at: "2026-05-12T00:00:00Z",
    };
    const parentRow: ActivityRow = {
      id: "start-task-1",
      kind: "tool_start",
      text: "Task",
      toolCallId: "task-1",
      tool: parent,
      at: "2026-05-12T00:00:00Z",
    };
    const mkChild = (id: string): ActivityRow => ({
      id: `start-${id}`,
      kind: "tool_start",
      text: "Read",
      toolCallId: id,
      tool: {
        id,
        name: "Read",
        kind: "read",
        args_preview: JSON.stringify({ path: `/${id}` }),
        started_at: "2026-05-12T00:00:00Z",
        parent_tool_call_id: "task-1",
      },
      at: "2026-05-12T00:00:00Z",
    });
    const messages = activityToThreadMessages(
      [userRow("go"), parentRow, mkChild("a"), mkChild("b"), mkChild("c")],
      false,
    );
    const assistant = messages.find((m) => m.role === "assistant")!;
    const parts = assistant.content as Array<{
      type: string;
      toolName?: string;
    }>;
    const groups = parts.filter((p) => p.type === "tool-call" && p.toolName === TOOL_GROUP_NAME);
    expect(groups).toHaveLength(0);
    const subagents = parts.filter((p) => p.type === "tool-call" && p.toolName === SUBAGENT_TASK_NAME);
    expect(subagents).toHaveLength(1);
  });

  it("does not group across user-prompt boundaries (separate messages)", () => {
    const messages = activityToThreadMessages(
      [userRow("first", "u1"), toolStart("t1"), toolStart("t2"), userRow("second", "u2"), toolStart("t3")],
      false,
    );
    // Each user_prompt starts a fresh assistant message; neither run is
    // long enough to fold on its own.
    const assistants = messages.filter((m) => m.role === "assistant");
    expect(assistants).toHaveLength(2);
    for (const m of assistants) {
      const parts = m.content as Array<{ type: string; toolName?: string }>;
      for (const p of parts.filter((p) => p.type === "tool-call")) {
        expect(p.toolName).not.toBe(TOOL_GROUP_NAME);
      }
    }
  });
});

describe("activityToThreadMessages; diff-comments user card (#1123)", () => {
  it("emits a user message with the structured payload on metadata.custom", () => {
    const row: ActivityRow = {
      id: "user-seq-1",
      kind: "user_diff_comments",
      text: "Take a look:\n\n## Diff comments\n\n...\n",
      diffComments: {
        intro: "Take a look:",
        outro: "Please address these comments.",
        isMultiRepo: true,
        comments: [
          {
            id: "c-1",
            repoName: "repoA",
            filePath: "src/main.rs",
            side: "new",
            startLine: 42,
            endLine: 45,
            body: "rename this",
            capturedSnippet: "fn main() {}",
            language: "rust",
            createdAt: "2026-01-01T00:00:00Z",
          },
        ],
      },
      at: "2026-05-12T00:00:00Z",
    };
    const messages = activityToThreadMessages([row], false);
    const user = messages.find((m) => m.role === "user")!;
    // The text part stays the assembled markdown so copy / fallback work.
    const parts = user.content as Array<{ type: string; text?: string }>;
    expect(parts[0]!.type).toBe("text");
    expect(parts[0]!.text).toContain("## Diff comments");
    // The structured payload rides on metadata.custom.diffComments for
    // UserText to render the rich card without parsing any sentinel.
    const custom = (user.metadata as { custom?: { diffComments?: { comments?: unknown[] } } } | undefined)?.custom;
    expect(custom?.diffComments?.comments).toHaveLength(1);
  });

  it("omits metadata when the structured payload is absent", () => {
    const row: ActivityRow = {
      id: "user-seq-2",
      kind: "user_diff_comments",
      text: "plain body\n",
      at: "2026-05-12T00:00:00Z",
    };
    const messages = activityToThreadMessages([row], false);
    const user = messages.find((m) => m.role === "user")!;
    expect(user.metadata).toBeUndefined();
  });
});

describe("activityToThreadMessages; elicitation answer (#2209)", () => {
  it("emits a user message with the answers on metadata.custom", () => {
    const row: ActivityRow = {
      id: "elicitation-el-1",
      kind: "elicitation_answered",
      text: "Proceed?: Yes",
      elicitationAnswers: [{ question: "Proceed?", answer: "Yes" }],
      at: "2026-05-12T00:00:00Z",
    };
    const messages = activityToThreadMessages([row], false);
    const user = messages.find((m) => m.role === "user")!;
    const parts = user.content as Array<{ type: string; text?: string }>;
    expect(parts[0]!.type).toBe("text");
    expect(parts[0]!.text).toBe("Proceed?: Yes");
    const custom = (user.metadata as { custom?: { elicitationAnswers?: unknown[] } } | undefined)?.custom;
    expect(custom?.elicitationAnswers).toHaveLength(1);
  });

  it("omits metadata when the structured answers are absent", () => {
    const row: ActivityRow = {
      id: "elicitation-el-2",
      kind: "elicitation_answered",
      text: "Q: A",
      at: "2026-05-12T00:00:00Z",
    };
    const messages = activityToThreadMessages([row], false);
    const user = messages.find((m) => m.role === "user")!;
    expect(user.metadata).toBeUndefined();
  });
});

function toolStopped(id: string, text = ""): ActivityRow {
  return {
    id: `stopped-${id}-9`,
    kind: "tool_stopped",
    text,
    toolCallId: id,
    at: "2026-05-12T00:00:02Z",
  };
}

describe("activityToThreadMessages; stopped status threading (#1646)", () => {
  it("carries stopped onto a top-level tool-call part's result", () => {
    const messages = activityToThreadMessages([userRow("go"), toolStart("t1"), toolStopped("t1")], false);
    const assistant = messages.find((m) => m.role === "assistant")!;
    const parts = assistant.content as Array<{
      type: string;
      result?: { stopped?: boolean };
      isError?: boolean;
    }>;
    const tool = parts.find((p) => p.type === "tool-call")!;
    expect(tool.result?.stopped).toBe(true);
    // Stopped is not an error; assistant-ui's isError stays falsy.
    expect(tool.isError).toBeFalsy();
  });

  it("preserves stopped on a subagent child through the payload round-trip", () => {
    const parent: ToolCall = {
      id: "task-1",
      name: "Task",
      kind: "think",
      args_preview: JSON.stringify({ _aoe_title: "Task" }),
      started_at: "2026-05-12T00:00:00Z",
    };
    const parentRow: ActivityRow = {
      id: "start-task-1",
      kind: "tool_start",
      text: "Task",
      toolCallId: "task-1",
      tool: parent,
      at: "2026-05-12T00:00:00Z",
    };
    const child: ToolCall = {
      id: "ch-1",
      name: "Read",
      kind: "read",
      args_preview: JSON.stringify({ path: "/x" }),
      started_at: "2026-05-12T00:00:01Z",
      parent_tool_call_id: "task-1",
    };
    const childRow: ActivityRow = {
      id: "start-ch-1",
      kind: "tool_start",
      text: "Read",
      toolCallId: "ch-1",
      tool: child,
      at: "2026-05-12T00:00:01Z",
    };
    const messages = activityToThreadMessages([userRow("go"), parentRow, childRow, toolStopped("ch-1")], false);
    const assistant = messages.find((m) => m.role === "assistant")!;
    const parts = assistant.content as Array<{
      type: string;
      toolName?: string;
      argsText?: string;
    }>;
    const subagent = parts.find((p) => p.type === "tool-call" && p.toolName === SUBAGENT_TASK_NAME)!;
    const payload = JSON.parse(subagent.argsText!);
    expect(payload.children[0].result.stopped).toBe(true);
  });
});
