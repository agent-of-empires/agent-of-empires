import { describe, expect, it } from "vitest";

import { reducer, transcriptDeltaAction } from "../../hooks/useAcpSession";
import { emptyAcpState, type AcpState, type ActivityRow, type ToolCall, type TranscriptRow } from "../../lib/acpTypes";
import { resolveAgentProfile } from "../../lib/agentProfiles";
import {
  activityToThreadMessages,
  clearFoldGeneration,
  SUBAGENT_TASK_NAME,
  TODO_GROUP_NAME,
  TOOL_GROUP_NAME,
} from "./activityMessages";

const AT = "2026-05-12T00:00:00Z";
const row = (id: string, kind: ActivityRow["kind"], text: string, extra: Partial<ActivityRow> = {}): ActivityRow => ({
  id,
  kind,
  text,
  at: AT,
  ...extra,
});
const user = (text = "go", id = "u1") => row(id, "user_prompt", text);
const message = (text: string, id = "m1") => row(id, "message", text);

function toolStart(id: string, over: Partial<ToolCall> = {}): ActivityRow {
  const tool: ToolCall = {
    id,
    name: "Read",
    kind: "read",
    args_preview: JSON.stringify({ path: `/tmp/${id}.txt` }),
    started_at: AT,
    ...over,
  };
  return row(`start-${id}`, "tool_start", tool.name, { toolCallId: id, tool });
}
const todo = (
  id: string,
  todos: { content: string; status: string }[] = [{ content: "a", status: "pending" }],
  name?: string,
) =>
  toolStart(id, {
    name: name ?? `Update TODOs: ${todos.map((t) => t.content).join(", ")}`,
    kind: "think",
    args_preview: JSON.stringify({ todos }),
  });
const child = (id: string, parent: string) => toolStart(id, { parent_tool_call_id: parent });

type Part = {
  type: string;
  toolName?: string;
  toolCallId?: string;
  argsText?: string;
  result?: { stopped?: boolean };
  isError?: boolean;
  text?: string;
};

function assistantParts(rows: ActivityRow[], ...opts: [boolean?, boolean?]): Part[] {
  const messages = activityToThreadMessages([user(), ...rows], false, ...opts);
  return messages.filter((m) => m.role === "assistant").flatMap((m) => m.content as Part[]);
}
const toolParts = (rows: ActivityRow[], ...opts: [boolean?, boolean?]) =>
  assistantParts(rows, ...opts).filter((p) => p.type === "tool-call");
const names = (parts: Part[]) => parts.map((p) => p.toolName);
const payload = (part: Part) => JSON.parse(part.argsText!);

describe("clearFoldGeneration", () => {
  const TURNS = [user("q1"), message("a1"), user("q2", "u2"), message("a2", "m2")];
  const cleared = [...TURNS, row("c1", "session_cleared", "cleared")];
  it.each([
    ["no clear", TURNS, false, "none"],
    ["turn appended", [...TURNS, message("a3", "m3")], false, "none"],
    ["cleared shown", cleared, true, "all"],
    ["folded", cleared, false, "c1"],
    ["folded twice", [...cleared, row("c2", "session_cleared", "again")], false, "c2"],
  ])("%s", (_label, rows, showCleared, expected) => {
    expect(clearFoldGeneration(rows, showCleared)).toBe(expected);
  });
});

describe("tool-call grouping", () => {
  const readRun = (prefix: string, n: number) => Array.from({ length: n }, (_, i) => toolStart(`${prefix}${i + 1}`));
  const todoRun = (prefix: string, n: number) => Array.from({ length: n }, (_, i) => todo(`${prefix}${i + 1}`));

  it.each<[string, ActivityRow[], (string | undefined)[], boolean?]>([
    ["folds 3+ tool calls", readRun("t", 4), [TOOL_GROUP_NAME]],
    ["keeps 1-2 inline", readRun("t", 2), ["read", "read"]],
    [
      "text splits runs",
      [...readRun("a", 3), message("Found it."), ...readRun("b", 3)],
      [TOOL_GROUP_NAME, TOOL_GROUP_NAME],
    ],
    ["folds 3+ todo snapshots", todoRun("td", 3), [TODO_GROUP_NAME]],
    // An empty clear with the bare TodoWrite title is still a snapshot.
    [
      "folds a run ending in an empty clear",
      [todo("td1", undefined, "TodoWrite"), todo("td2", undefined, "TodoWrite"), todo("td3", [], "TodoWrite")],
      [TODO_GROUP_NAME],
    ],
    ["keeps 2 todo snapshots inline", todoRun("td", 2), ["think", "think"]],
    // A status update among real work stays in the timeline.
    [
      "keeps todo mixed with work inline",
      [...readRun("a", 2), todo("td1"), toolStart("c")],
      ["read", "read", "think", "read"],
    ],
    ["keeps todo-then-work inline", [...todoRun("td", 2), toolStart("r1")], ["think", "think", "read"]],
    [
      "text splits todo runs below the threshold",
      [...todoRun("a", 2), message("Working."), ...todoRun("b", 2)],
      ["think", "think", "think", "think"],
    ],
  ])("%s", (_label, rows, expected) => {
    expect(names(toolParts(rows))).toEqual(expected);
  });

  it("uses the generic group for todo-shaped runs when todos are disabled", () => {
    expect(names(toolParts(todoRun("td", 3), false, false))).toEqual([TOOL_GROUP_NAME]);
  });

  it("preserves snapshot order in the todo payload", () => {
    const [group] = toolParts([
      todo("td1", undefined, "TodoWrite"),
      todo("td2", undefined, "TodoWrite"),
      todo("td3", [], "TodoWrite"),
    ]);
    expect(payload(group!).children.map((c: Part) => c.toolCallId)).toEqual(["td1", "td2", "td3"]);
  });

  // Anchored on the first child so a growing run keeps its card and expand state.
  it.each([
    [readRun("t", 3), readRun("t", 4), "group-t1"],
    [todoRun("td", 3), todoRun("td", 4), "todogroup-td1"],
  ])("keeps the group id stable as the run grows (%#)", (three, four, id) => {
    expect(toolParts(three)[0]!.toolCallId).toBe(id);
    expect(toolParts(four)[0]!.toolCallId).toBe(id);
  });

  it("gives text-split runs distinct ids", () => {
    const parts = toolParts([...readRun("a", 3), message("Found it."), ...readRun("b", 3)]);
    expect(parts.map((p) => p.toolCallId)).toEqual(["group-a1", "group-b1"]);
  });

  it("does not group across user prompts", () => {
    const messages = activityToThreadMessages(
      [user("first"), ...readRun("t", 2), user("second", "u2"), toolStart("t3")],
      false,
    );
    const assistants = messages.filter((m) => m.role === "assistant");
    expect(assistants).toHaveLength(2);
    expect(assistants.flatMap((m) => names(m.content as Part[]))).not.toContain(TOOL_GROUP_NAME);
  });

  // A reused tool_call_id gets seq-disambiguated completion rows; message ids must stay unique.
  it("keeps message ids unique when a tool_call_id is reused across turns", () => {
    const id = "toolu_reused";
    const done = (rowId: string) => row(rowId, "tool_complete", "result", { toolCallId: id });
    const messages = activityToThreadMessages(
      [
        user("first"),
        toolStart(id, { kind: "execute" }),
        done(`done-${id}`),
        user("second", "u2"),
        done(`done-${id}-6`),
        user("third", "u3"),
        done(`done-${id}-12`),
        message("limit", "m-final"),
      ],
      false,
    );
    const ids = messages.map((m) => m.id);
    expect(new Set(ids).size).toBe(ids.length);
  });
});

describe("subagents", () => {
  const task = (id = "task-1") =>
    toolStart(id, { name: "Task", kind: "think", args_preview: JSON.stringify({ description: "go" }) });

  it("collapses a parent Task and its children into one part, not a generic group", () => {
    const parts = toolParts([task(), child("a", "task-1"), child("b", "task-1"), child("c", "task-1")]);
    expect(names(parts)).toEqual([SUBAGENT_TASK_NAME]);
    const p = payload(parts[0]!);
    expect(p.parent.toolCallId).toBe("task-1");
    expect(p.children.map((c: Part) => c.toolCallId)).toEqual(["a", "b", "c"]);
  });

  it("leaves orphan children in place", () => {
    expect(names(toolParts([child("ch-1", "elsewhere")]))).toEqual(["read"]);
  });

  it("collapses a childless async launch with async set", () => {
    const done = row("done-task-1", "tool_complete", "Async agent launched\nagentId: secret", {
      toolCallId: "task-1",
      asyncSubagent: true,
    });
    const parts = toolParts([task(), done]);
    expect(names(parts)).toEqual([SUBAGENT_TASK_NAME]);
    expect(payload(parts[0]!)).toMatchObject({ async: true, children: [], parent: { toolCallId: "task-1" } });
  });

  it("threads stopped onto top-level results and through subagent payloads", () => {
    const stopped = (id: string) => row(`stopped-${id}-9`, "tool_stopped", "", { toolCallId: id });
    const [top] = toolParts([toolStart("t1"), stopped("t1")]);
    expect(top!.result?.stopped).toBe(true);
    expect(top!.isError).toBeFalsy();
    const [sub] = toolParts([task(), child("ch-1", "task-1"), stopped("ch-1")]);
    expect(payload(sub!).children[0].result.stopped).toBe(true);
  });

  it.each([
    ["_aoe_parent_tool_call_id", { parent_tool_call_id: "task-parent-1" }, "task-parent-1"],
    [
      "_aoe_memory_recall",
      { memory_recall: { mode: "synthesize" as const, synthesized_text: "remembered" } },
      { mode: "synthesize", synthesized_text: "remembered" },
    ],
  ])("smuggles %s through argsText", (key, over, expected) => {
    const [part] = toolParts([toolStart("x", over)]);
    expect(payload(part!)[key]).toEqual(expected);
  });

  // The server transcript drops raw_name; the client keeps the wire name from the first
  // Append across the retitling Patch, so drive the reducer's delta path.
  describe("off-protocol subagent tools", () => {
    const apply = (state: AcpState, delta: Parameters<typeof transcriptDeltaAction>[0]) => {
      const action = transcriptDeltaAction(delta, "s");
      return action ? reducer(state, action) : state;
    };
    const start = (id: string, name: string, kind: string, args = "{}"): TranscriptRow => ({
      id: `start-${id}`,
      group_id: `tool-${id}`,
      kind: "tool_start",
      at: AT,
      text: name,
      tool_call_id: id,
      tool: { id, name, kind, args_preview: args, started_at: AT },
    });
    function taskRows(state = emptyAcpState()) {
      state = apply(state, { Append: start("t1", "task", "think") });
      const retitled = start(
        "t1",
        "Trace resets",
        "think",
        JSON.stringify({ description: "Trace resets", prompt: "Research" }),
      );
      state = apply(state, { Patch: { id: "start-t1", row: retitled } });
      const done: TranscriptRow = {
        id: "done-t1",
        group_id: "tool-t1",
        kind: "tool_complete",
        at: AT,
        text: "<task><task_result>ok</task_result></task>",
        tool_call_id: "t1",
      };
      return apply(state, { Append: done });
    }
    const partsFor = (state: AcpState, toolKey: string) =>
      activityToThreadMessages(state.activity, false, false, true, resolveAgentProfile(toolKey))
        .flatMap((m) => m.content as Part[])
        .filter((p) => p.type === "tool-call");

    it("normalizes a declared subagent tool into a childless, non-async part", () => {
      const [sub] = partsFor(taskRows(), "opencode");
      expect(sub!.toolName).toBe(SUBAGENT_TASK_NAME);
      const p = payload(sub!);
      expect(p.children).toEqual([]);
      expect(p.async).toBeUndefined();
      expect(p.parent.argsText).toContain("Trace resets");
      expect(p.parent.argsText).toContain("_aoe_raw_tool_name");
    });

    it("keeps it inline in a run that would otherwise fold", () => {
      let state = emptyAcpState();
      for (const id of ["b1", "b2", "b3"]) state = apply(state, { Append: start(id, "bash", "execute") });
      expect(names(partsFor(taskRows(state), "opencode"))).toEqual([
        "execute",
        "execute",
        "execute",
        SUBAGENT_TASK_NAME,
      ]);
    });

    it("does not classify the tool for an agent that does not declare it", () => {
      expect(names(partsFor(taskRows(), "codex"))).not.toContain(SUBAGENT_TASK_NAME);
    });
  });
});

describe("user and callout rows", () => {
  it.each([
    ["user_diff_comments", "diffComments", { intro: "Take a look:", comments: [{ id: "c-1" }] }],
    ["elicitation_answered", "elicitationAnswers", [{ question: "Proceed?", answer: "Yes" }]],
  ] as const)("renders %s as a user message with its payload on metadata, or none", (kind, key, value) => {
    const [withPayload] = activityToThreadMessages([row("r1", kind, "body text", { [key]: value })], false);
    expect(withPayload!.role).toBe("user");
    expect(withPayload!.content).toEqual([{ type: "text", text: "body text" }]);
    expect((withPayload!.metadata as { custom: Record<string, unknown> }).custom[key]).toEqual(value);
    const [without] = activityToThreadMessages([row("r2", kind, "plain")], false);
    expect(without!.metadata).toBeUndefined();
  });

  it("renders a summary as a quoted callout", () => {
    const parts = assistantParts([row("sum-1", "summary", "- fixed the login bug\n- next: wire the UI")]);
    expect(parts[0]!.text).toBe(
      "> 📝 **Summary of conversation so far**\n>\n> - fixed the login bug\n> - next: wire the UI",
    );
  });
});
