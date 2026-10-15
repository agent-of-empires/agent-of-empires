import { describe, expect, it } from "vitest";

import {
  applyEvent,
  emptyAcpState,
  normaliseTurnState,
  type AcpFrame,
  type AcpState,
  type ActivityRow,
  type Approval,
  type Elicitation,
  type ToolCall,
} from "../../lib/acpTypes";
import type { ServerQueuedPrompt } from "../../lib/api";
import { classifyResolveResponse, reducer, type Action } from "./reducer";

const run = (state: AcpState, ...actions: Action[]) => actions.reduce(reducer, state);
const empty = emptyAcpState;

describe("classifyResolveResponse", () => {
  it.each([
    ["approval", true, 204, "", "resolved"],
    ["approval", false, 404, "no pending approval with nonce n-1", "resolved"],
    ["approval", false, 404, "No Pending Approval with nonce n-1", "resolved"],
    ["approval", false, 404, "no pending approval with nonce other-99", "error"],
    ["approval", false, 404, "session has no running agent", "error"],
    ["approval", false, 500, "boom", "error"],
    ["elicitation", true, 204, "", "resolved"],
    ["elicitation", false, 404, "no pending elicitation with nonce n-1", "resolved"],
    ["elicitation", false, 404, "no pending elicitation with nonce other", "error"],
    ["elicitation", false, 404, "no pending approval with nonce n-1", "error"],
  ] as const)("%s ok=%s %i %j -> %s", (target, ok, status, detail, kind) => {
    expect(classifyResolveResponse(target, ok, status, detail, "n-1").kind).toBe(kind);
  });

  it("names the status and target in the error message", () => {
    expect(classifyResolveResponse("approval", false, 404, "gone", "n")).toEqual({
      kind: "error",
      message: "Could not resolve approval (404). gone",
    });
    expect(classifyResolveResponse("elicitation", false, 500, "", "n")).toEqual({
      kind: "error",
      message: "Could not resolve question (500).",
    });
  });
});

describe("locally resolved cards", () => {
  const approval = (nonce: string) => ({ nonce }) as unknown as Approval;
  const elicitation = (nonce: string, withQuestion = false): Elicitation => ({
    nonce,
    message: "Pick",
    tool_call_id: null,
    questions: withQuestion
      ? [
          {
            field_key: "question_0",
            title: "Proceed?",
            required: true,
            kind: "single_select",
            options: [
              { value: "yes_internal", label: "Yes" },
              { value: "no_internal", label: "No" },
            ],
          },
        ]
      : [],
    requested_at: new Date().toISOString(),
    resolved: null,
  });

  it("an approval removes its card and clears the error; an unknown nonce keeps both", () => {
    const state = { ...empty(), lastError: "stale", pendingApprovals: [approval("n-1"), approval("n-2")] };
    const next = reducer(state, { kind: "approval_resolved_locally", nonce: "n-1" });
    expect(next.pendingApprovals.map((a) => a.nonce)).toEqual(["n-2"]);
    expect(next.lastError).toBeNull();
    const noop = reducer(state, { kind: "approval_resolved_locally", nonce: "missing" });
    expect(noop.pendingApprovals).toHaveLength(2);
    expect(noop.lastError).toBe("stale");
  });

  it("an accepted elicitation drops the card and records an optimistic answer row (#2209)", () => {
    const state = { ...empty(), pendingElicitations: [elicitation("e-1", true), elicitation("e-2")] };
    const next = reducer(state, {
      kind: "elicitation_resolved_locally",
      nonce: "e-1",
      resolution: { action: "accept", answers: { question_0: "yes_internal" } },
    });
    expect(next.pendingElicitations.map((e) => e.nonce)).toEqual(["e-2"]);
    const row = next.optimisticRows.find((r) => r.kind === "elicitation_answered");
    expect(row?.id).toBe("elicitation-e-1");
    expect(row?.elicitationAnswers).toEqual([{ question: "Proceed?", answer: "Yes" }]);
    expect(next.activity).toHaveLength(0);
  });

  it.each([
    ["the card is already gone", [] as Elicitation[], "accept"],
    ["the question was declined", [elicitation("e-1", true)], "decline"],
  ] as const)("adds no answer row when %s", (_label, pending, action) => {
    const resolution =
      action === "accept" ? { action, answers: { question_0: "Yes" } } : ({ action } as { action: "decline" });
    const next = reducer(
      { ...empty(), pendingElicitations: [...pending] },
      { kind: "elicitation_resolved_locally", nonce: "e-1", resolution },
    );
    expect(next.optimisticRows.some((r) => r.kind === "elicitation_answered")).toBe(false);
  });
});

describe("in-flight prompt settlement", () => {
  const sent = (id: string) => reducer(empty(), { kind: "user_prompt", id, text: "hi" });

  it("user_prompt records the id and shows the spinner immediately", () => {
    const state = sent("p1");
    expect(state.inflightPromptIds).toEqual(["p1"]);
    expect(state.turnActive).toBe(true);
    expect(state.serverTurnActive).toBe(false);
    expect(state.promptSeq).toBe(1);
  });

  it.each([
    ["prompt_send_rejected", true],
    ["settle_inflight_prompt", true],
    ["rollback_optimistic_prompt", false],
  ] as const)("%s settles its id (keeps overlay row: %s)", (kind, keepsRow) => {
    const next = reducer(sent("p1"), { kind, id: "p1" });
    expect(next.inflightPromptIds).toEqual([]);
    expect(next.turnActive).toBe(false);
    expect(next.optimisticRows.length > 0).toBe(keepsRow);
  });

  it("settling one prompt does not retire another or close a server-running turn", () => {
    const two = run(
      sent("p1"),
      { kind: "user_prompt", id: "p2", text: "second" },
      { kind: "settle_inflight_prompt", id: "p1" },
    );
    expect(two.inflightPromptIds).toEqual(["p2"]);
    expect(two.turnActive).toBe(true);

    const echoed = applyEvent(sent("p1"), {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "hi", prompt_id: "p1" } },
    });
    expect(reducer(echoed, { kind: "settle_inflight_prompt", id: "p1" }).turnActive).toBe(true);
  });
});

describe("prompt queue", () => {
  const serverRow = (id: string, seq: number, text: string, atts?: ServerQueuedPrompt["attachments"]) => ({
    id,
    seq,
    text,
    created_at: "2026-01-01T00:00:00.000Z",
    ...(atts ? { attachments: atts } : {}),
  });
  const image = { kind: "image" as const, mimeType: "image/png", dataB64: "REALBYTES", name: "a.png" };

  it("enqueue appends pending rows; edit, dequeue, confirm, and clear mutate them", () => {
    let s = run(
      empty(),
      { kind: "enqueue_prompt", id: "a", text: "first", attachments: [image] },
      { kind: "enqueue_prompt", id: "b", text: "second", attachments: [] },
    );
    expect(s.queuedPrompts.map((q) => [q.id, q.text, q.pending])).toEqual([
      ["a", "first", true],
      ["b", "second", true],
    ]);
    expect(s.queuedPrompts[0]?.attachments?.[0]?.name).toBe("a.png");
    expect(s.queuedPrompts[1]).not.toHaveProperty("attachments");
    s = run(
      s,
      { kind: "edit_queued_prompt", id: "b", text: "edited" },
      { kind: "confirm_queued_prompt", id: "b" },
      { kind: "dequeue_prompt", id: "a" },
    );
    expect(s.queuedPrompts).toMatchObject([{ id: "b", text: "edited", pending: false }]);
    expect(reducer(s, { kind: "clear_queue" }).queuedPrompts).toEqual([]);
  });

  it("hydrate replaces confirmed rows with the server snapshot but keeps in-flight ones", () => {
    const local = run(
      empty(),
      { kind: "enqueue_prompt", id: "old", text: "gone from server" },
      { kind: "confirm_queued_prompt", id: "old" },
      { kind: "enqueue_prompt", id: "inflight", text: "just typed" },
    );
    const next = reducer(local, {
      kind: "hydrate_server_queue",
      rows: [serverRow("a", 0, "one"), serverRow("b", 1, "two")],
    });
    expect(next.queuedPrompts.map((q) => q.text)).toEqual(["one", "two", "just typed"]);
  });

  it("hydrate keeps local attachment bytes, else builds a metadata-only view", () => {
    const ref = { id: "att1", kind: "image" as const, mime_type: "image/png", name: "a.png", size: 9 };
    const local = run(empty(), { kind: "enqueue_prompt", id: "q1", text: "img", attachments: [image] });
    const kept = reducer(local, { kind: "hydrate_server_queue", rows: [serverRow("q1", 0, "img", [ref])] });
    expect(kept.queuedPrompts[0]?.attachments?.[0]?.dataB64).toBe("REALBYTES");

    const reloaded = reducer(empty(), {
      kind: "hydrate_server_queue",
      rows: [serverRow("q2", 0, "reloaded", [{ ...ref, kind: "resource", mime_type: "text/plain", name: "n.txt" }])],
    });
    expect(reloaded.queuedPrompts[0]?.attachments?.[0]).toMatchObject({
      kind: "resource",
      mimeType: "text/plain",
      name: "n.txt",
      dataB64: "",
    });
  });
});

describe("config options", () => {
  const set = (value: string): Action => ({ kind: "set_pending_config_option", configId: "model", value });
  const clearIf = (value: string): Action => ({
    kind: "clear_pending_config_option_if_match",
    configId: "model",
    value,
  });

  it.each([
    ["set records the latest click", [set("a"), set("b")], { configId: "model", value: "b" }],
    ["clear drops it", [set("a"), { kind: "clear_pending_config_option" }], null],
    ["a matching failure clears it", [set("a"), clearIf("a")], null],
    [
      "a stale failure keeps the newer click (#1403)",
      [set("a"), set("b"), clearIf("a")],
      { configId: "model", value: "b" },
    ],
    ["a failure with nothing pending is a no-op", [clearIf("a")], null],
  ] as [string, Action[], unknown][])("%s", (_label, actions, expected) => {
    expect(run(empty(), ...actions).pendingConfigOption).toEqual(expected);
  });

  it("normaliseTurnState backfills config-option fields missing from an older entry", () => {
    const stale = { ...empty() } as Record<string, unknown>;
    delete stale.configOptions;
    delete stale.configOptionSwitchFailed;
    delete stale.pendingConfigOption;
    const next = normaliseTurnState(stale as unknown as AcpState);
    expect([next.configOptions, next.configOptionSwitchFailed, next.pendingConfigOption]).toEqual([[], null, null]);
  });
});

describe("recent-first paging", () => {
  const prompt = (seq: number, text: string): AcpFrame => ({
    session_id: "s",
    seq,
    event: { UserPromptSent: { text } },
  });
  const promptRow = (seq: number, text: string): ActivityRow => ({
    id: `user-seq-${seq}`,
    kind: "user_prompt",
    text,
    at: "2026-01-01T00:00:00Z",
  });

  it("the tail seeds oldestSeq once; live frames never lower it", () => {
    const s = run(
      empty(),
      { kind: "frames", frames: [prompt(5, "e"), prompt(6, "f")], oldestSeq: 5 },
      { kind: "frames", frames: [prompt(7, "g")], oldestSeq: 7 },
    );
    expect(s.oldestSeq).toBe(5);
  });

  it("prepend adds older rows and lowers oldestSeq without touching queue or approvals", () => {
    let s = run(
      empty(),
      { kind: "frames", frames: [prompt(5, "e")], rows: [promptRow(5, "e")], oldestSeq: 5 },
      { kind: "enqueue_prompt", id: "q-1", text: "queued" },
    );
    const approvals = [{ nonce: "n1" } as unknown as Approval];
    s = reducer({ ...s, pendingApprovals: approvals }, { kind: "prepend", rows: [promptRow(2, "b")], oldestSeq: 2 });
    expect(s.oldestSeq).toBe(2);
    expect(s.activity.map((r) => r.id)).toEqual(["user-seq-2", "user-seq-5"]);
    expect(s.queuedPrompts.map((q) => q.text)).toEqual(["queued"]);
    expect(s.pendingApprovals).toBe(approvals);
  });

  it("handshake backfills empty fields but never overwrites loaded values", () => {
    const caps: AcpFrame = {
      session_id: "s",
      seq: 1,
      event: { PromptCapabilities: { image: true, audio: false, embedded_context: true } },
    };
    const loaded = { image: false, audio: false, embeddedContext: false, steering: false };
    const kept = reducer({ ...empty(), promptCapabilities: loaded }, { kind: "handshake", frames: [caps] });
    expect(kept.promptCapabilities).toEqual(loaded);
    const filled = reducer(empty(), { kind: "handshake", frames: [caps] });
    expect(filled.promptCapabilities).toEqual({ image: true, audio: false, embeddedContext: true, steering: false });
    expect(filled.activity).toHaveLength(0);
  });
});

describe("prepend seam dedupe (#2711)", () => {
  const tc = (over: Partial<ToolCall> & { id: string }): ToolCall => ({
    name: "Read",
    kind: "read",
    args_preview: "{}",
    started_at: "2024-01-01T00:00:00Z",
    ...over,
  });
  const startRow = (tool: ToolCall): ActivityRow => ({
    id: `start-${tool.id}`,
    kind: "tool_start",
    text: tool.name,
    toolCallId: tool.id,
    tool,
    at: tool.started_at,
  });
  const tail = () => ({
    ...empty(),
    activity: [
      startRow(tc({ id: "call_X", name: "tool call", kind: "other", args_preview: "" })),
      { id: "done-call_X", kind: "tool_complete", text: "done", toolCallId: "call_X", at: "2024-01-01T00:05:00Z" },
    ] as ActivityRow[],
  });
  const starts = (rows: ActivityRow[], id: string) =>
    rows.filter((r) => r.kind === "tool_start" && r.toolCallId === id);

  it("merges the older page's real start into the tail's synthesized start", () => {
    const real = startRow(tc({ id: "call_X", args_preview: '{"path":"/etc/hosts"}' }));
    const merged = starts(reducer(tail(), { kind: "prepend", rows: [real], oldestSeq: 5 }).activity, "call_X");
    expect(merged).toHaveLength(1);
    expect(merged[0]!.tool).toMatchObject({ name: "Read", kind: "read", started_at: "2024-01-01T00:00:00Z" });
  });

  it("prepends a non-overlapping older start as its own row", () => {
    const other = startRow(tc({ id: "call_Y", name: "Bash", kind: "execute" }));
    const next = reducer(tail(), { kind: "prepend", rows: [other], oldestSeq: 5 });
    expect(starts(next.activity, "call_Y")).toHaveLength(1);
    expect(starts(next.activity, "call_X")).toHaveLength(1);
  });
});
