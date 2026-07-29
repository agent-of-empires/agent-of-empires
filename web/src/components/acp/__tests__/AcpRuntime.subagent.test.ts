// Pipeline coverage for off-protocol subagent classification (#3070):
// opencode's `task` tool has no streamed children and no _meta linkage,
// so it must be normalized into the synthetic _aoe_subagent_task part by
// raw_name, kept inline (not folded into a generic tool group), and only
// for agents whose profile declares the subagent tool name.

import { describe, expect, it } from "vitest";
import { activityToThreadMessages, SUBAGENT_TASK_NAME, TOOL_GROUP_NAME } from "../AcpRuntime";
import { applyEvent, emptyAcpState, type AcpState } from "../../../lib/acpTypes";
import { resolveAgentProfile } from "../../../lib/agentProfiles";

function taskStart(state: AcpState, id: string, seq: number): AcpState {
  return applyEvent(state, {
    session_id: "s",
    seq,
    event: {
      ToolCallStarted: {
        tool_call: { id, name: "task", kind: "think", args_preview: "{}", started_at: "2026-01-01T00:00:00Z" },
      },
    },
  });
}

function taskUpdate(state: AcpState, id: string, seq: number): AcpState {
  return applyEvent(state, {
    session_id: "s",
    seq,
    event: {
      ToolCallUpdated: {
        tool_call_id: id,
        title: "Trace clear session resets",
        args_preview: JSON.stringify({ description: "Trace clear session resets", prompt: "Research only" }),
      },
    },
  });
}

function taskComplete(state: AcpState, id: string, seq: number): AcpState {
  return applyEvent(state, {
    session_id: "s",
    seq,
    event: {
      ToolCallCompleted: {
        tool_call_id: id,
        is_error: false,
        content: '<task id="ses_1" state="completed"><task_result>ok</task_result></task>',
      },
    },
  });
}

function bash(state: AcpState, id: string, seq: number): AcpState {
  return applyEvent(state, {
    session_id: "s",
    seq,
    event: {
      ToolCallStarted: {
        tool_call: { id, name: "bash", kind: "execute", args_preview: "{}", started_at: "2026-01-01T00:00:00Z" },
      },
    },
  });
}

function toolCallParts(rows: AcpState["activity"], toolKey: string) {
  const messages = activityToThreadMessages(rows, false, false, true, resolveAgentProfile(toolKey));
  return messages.flatMap((m) => (Array.isArray(m.content) ? m.content : [])).filter((p) => p.type === "tool-call");
}

describe("off-protocol subagent classification (#3070)", () => {
  it("normalizes an opencode `task` into the synthetic subagent part", () => {
    let state = taskStart(emptyAcpState(), "t1", 1);
    state = taskUpdate(state, "t1", 2);
    state = taskComplete(state, "t1", 3);

    const parts = toolCallParts(state.activity, "opencode");
    const subagent = parts.find((p) => "toolName" in p && p.toolName === SUBAGENT_TASK_NAME);
    expect(subagent).toBeDefined();
    const payload = JSON.parse((subagent as { argsText: string }).argsText);
    expect(payload.children).toEqual([]);
    expect(payload.async).toBeUndefined();
    expect(payload.parent.argsText).toContain("Trace clear session resets");
    expect(payload.parent.argsText).toContain("_aoe_raw_tool_name");
  });

  it("keeps the subagent inline instead of folding it into a tool group", () => {
    // Three bash calls plus the task make a >=3 run that would normally
    // fold; the SUBAGENT_TASK_NAME part must keep the run inline.
    let state = bash(emptyAcpState(), "b1", 10);
    state = bash(state, "b2", 11);
    state = bash(state, "b3", 12);
    state = taskStart(state, "t1", 13);
    state = taskUpdate(state, "t1", 14);
    state = taskComplete(state, "t1", 15);

    const parts = toolCallParts(state.activity, "opencode");
    expect(parts.some((p) => "toolName" in p && p.toolName === SUBAGENT_TASK_NAME)).toBe(true);
    expect(parts.some((p) => "toolName" in p && p.toolName === TOOL_GROUP_NAME)).toBe(false);
  });

  it("does not classify `task` for an agent that doesn't declare it", () => {
    let state = taskStart(emptyAcpState(), "t1", 1);
    state = taskUpdate(state, "t1", 2);
    state = taskComplete(state, "t1", 3);

    const parts = toolCallParts(state.activity, "codex");
    expect(parts.some((p) => "toolName" in p && p.toolName === SUBAGENT_TASK_NAME)).toBe(false);
  });
});
