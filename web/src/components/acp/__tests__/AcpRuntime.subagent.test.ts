// Pipeline coverage for off-protocol subagent classification (#3070):
// opencode's `task` tool has no streamed children and no _meta linkage, so it
// must be normalized into the synthetic _aoe_subagent_task part by raw_name,
// kept inline (not folded into a generic tool group), and only for agents
// whose profile declares the subagent tool name.
//
// The transcript is server-owned now (Tier 4) and the server's ToolCall drops
// raw_name. The client recovers the wire identity from the FIRST transcript
// delta (the Append carries `name: "task"`) and preserves it across the
// retitling Patch, so we drive the live delta path through the reducer here.

import { describe, expect, it } from "vitest";
import { activityToThreadMessages, SUBAGENT_TASK_NAME, TOOL_GROUP_NAME } from "../AcpRuntime";
import { emptyAcpState, type ActivityRow, type AcpState, type TranscriptRow } from "../../../lib/acpTypes";
import { reducer, transcriptDeltaAction } from "../../../hooks/useAcpSession";
import { resolveAgentProfile } from "../../../lib/agentProfiles";

const SID = "s";

function apply(state: AcpState, delta: Parameters<typeof transcriptDeltaAction>[0]): AcpState {
  const act = transcriptDeltaAction(delta, SID);
  return act ? reducer(state, act) : state;
}

// The server Appends a `task` tool_start (wire name "task"), then Patches it
// with the retitled name once the update lands.
function taskStartAppend(state: AcpState, id: string): AcpState {
  const row: TranscriptRow = {
    id: `start-${id}`,
    group_id: `tool-${id}`,
    kind: "tool_start",
    at: "2026-01-01T00:00:00Z",
    text: "task",
    tool_call_id: id,
    tool: { id, name: "task", kind: "think", args_preview: "{}", started_at: "2026-01-01T00:00:00Z" },
  };
  return apply(state, { Append: row });
}

function taskRetitlePatch(state: AcpState, id: string): AcpState {
  const row: TranscriptRow = {
    id: `start-${id}`,
    group_id: `tool-${id}`,
    kind: "tool_start",
    at: "2026-01-01T00:00:00Z",
    text: "Trace clear session resets",
    tool_call_id: id,
    tool: {
      id,
      name: "Trace clear session resets",
      kind: "think",
      args_preview: JSON.stringify({ description: "Trace clear session resets", prompt: "Research only" }),
      started_at: "2026-01-01T00:00:00Z",
    },
  };
  return apply(state, { Patch: { id: `start-${id}`, row } });
}

function taskComplete(state: AcpState, id: string): AcpState {
  const row: TranscriptRow = {
    id: `done-${id}`,
    group_id: `tool-${id}`,
    kind: "tool_complete",
    at: "2026-01-01T00:00:01Z",
    text: '<task id="ses_1" state="completed"><task_result>ok</task_result></task>',
    tool_call_id: id,
  };
  return apply(state, { Append: row });
}

function bash(state: AcpState, id: string): AcpState {
  const row: TranscriptRow = {
    id: `start-${id}`,
    group_id: `tool-${id}`,
    kind: "tool_start",
    at: "2026-01-01T00:00:00Z",
    text: "bash",
    tool_call_id: id,
    tool: { id, name: "bash", kind: "execute", args_preview: "{}", started_at: "2026-01-01T00:00:00Z" },
  };
  return apply(state, { Append: row });
}

function toolCallParts(rows: ActivityRow[], toolKey: string) {
  const messages = activityToThreadMessages(rows, false, false, true, resolveAgentProfile(toolKey));
  return messages.flatMap((m) => (Array.isArray(m.content) ? m.content : [])).filter((p) => p.type === "tool-call");
}

describe("off-protocol subagent classification (#3070)", () => {
  it("normalizes an opencode `task` into the synthetic subagent part", () => {
    let state = taskStartAppend(emptyAcpState(), "t1");
    state = taskRetitlePatch(state, "t1");
    state = taskComplete(state, "t1");

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
    // Three bash calls plus the task make a >=3 run that would normally fold;
    // the SUBAGENT_TASK_NAME part must keep the run inline.
    let state = bash(emptyAcpState(), "b1");
    state = bash(state, "b2");
    state = bash(state, "b3");
    state = taskStartAppend(state, "t1");
    state = taskRetitlePatch(state, "t1");
    state = taskComplete(state, "t1");

    const parts = toolCallParts(state.activity, "opencode");
    expect(parts.some((p) => "toolName" in p && p.toolName === SUBAGENT_TASK_NAME)).toBe(true);
    expect(parts.some((p) => "toolName" in p && p.toolName === TOOL_GROUP_NAME)).toBe(false);
  });

  it("does not classify `task` for an agent that doesn't declare it", () => {
    let state = taskStartAppend(emptyAcpState(), "t1");
    state = taskRetitlePatch(state, "t1");
    state = taskComplete(state, "t1");

    const parts = toolCallParts(state.activity, "codex");
    expect(parts.some((p) => "toolName" in p && p.toolName === SUBAGENT_TASK_NAME)).toBe(false);
  });
});
